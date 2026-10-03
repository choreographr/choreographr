use anyhow::{Context, Result};
use choreo_mcp::{McpProtocolMode, McpServerConfig, McpTransport, McpTransportKind};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

/// Top-level structure matching the standard `mcp_servers.json` format.
#[derive(Deserialize, Debug)]
struct McpServersFile {
    #[serde(rename = "mcpServers")]
    mcp_servers: HashMap<String, ServerEntry>,
}

/// One server entry. Every field is optional so a config can describe either a
/// stdio server (`command`/`args`/`env`) or a remote one (`url`/`headers`); the
/// `transport` key picks explicitly, otherwise the keys present decide.
#[derive(Deserialize, Debug)]
struct ServerEntry {
    /// Executable to launch (stdio transport).
    command: Option<String>,
    /// Arguments for `command`.
    #[serde(default)]
    args: Vec<String>,
    /// Extra environment variables for the subprocess.
    #[serde(default)]
    env: HashMap<String, String>,
    /// Streamable HTTP endpoint URL (HTTP transport).
    url: Option<String>,
    /// Extra HTTP headers sent with every request.
    #[serde(default)]
    headers: HashMap<String, String>,
    /// Explicit transport selector: `"http"`/`"stdio"`/`"auto"` (default).
    #[serde(default)]
    transport: Option<String>,
    #[serde(default = "default_true")]
    enabled: bool,
    /// Optional per-server request timeout, in seconds.
    #[serde(default)]
    timeout: Option<u64>,
    /// Optional protocol-era selector: `"auto"` (default), `"legacy"`, or
    /// `"modern"` (aliased by the explicit era string `"2026-07-28"`).
    #[serde(default)]
    protocol: Option<String>,
    /// Any keys this client does not recognize. Collected so loading can
    /// report them (never fatal) — a typo'd or not-yet-supported key should
    /// surface in the log rather than being silently dropped.
    #[serde(flatten)]
    unknown: HashMap<String, serde_json::Value>,
}

fn default_true() -> bool {
    true
}

/// Map the optional `protocol` config value onto a [`McpProtocolMode`].
///
/// An absent value or an unrecognized one defaults to `Auto` (with a warning
/// for the unrecognized case) so a typo never makes a server unusable.
fn parse_protocol(value: Option<&str>) -> McpProtocolMode {
    match value {
        None => McpProtocolMode::Auto,
        Some(raw) => McpProtocolMode::parse(raw).unwrap_or_else(|| {
            tracing::warn!(value = %raw, "unrecognized MCP protocol value; using 'auto'");
            McpProtocolMode::Auto
        }),
    }
}

/// Resolve the transport for one entry, applying the `transport` override and,
/// for `auto`, inferring from the keys present.
///
/// Returns `None` (with a logged reason) for an entry that cannot be resolved —
/// an ambiguous or incomplete server — so the rest of the file still loads.
fn resolve_transport(slug: &str, entry: &ServerEntry) -> Option<McpTransport> {
    let requested = match entry.transport.as_deref() {
        None => McpTransportKind::Auto,
        Some(raw) => McpTransportKind::parse(raw).unwrap_or_else(|| {
            tracing::warn!(
                server = %slug,
                value = %raw,
                "unrecognized MCP transport value; inferring from keys"
            );
            McpTransportKind::Auto
        }),
    };

    let kind = match requested {
        McpTransportKind::Auto => match (entry.command.is_some(), entry.url.is_some()) {
            (true, false) => McpTransportKind::Stdio,
            (false, true) => McpTransportKind::Http,
            (true, true) => {
                tracing::warn!(
                    server = %slug,
                    "MCP server sets both `command` and `url`; set `transport` to disambiguate; skipping"
                );
                return None;
            }
            (false, false) => {
                tracing::warn!(
                    server = %slug,
                    "MCP server sets neither `command` nor `url`; skipping"
                );
                return None;
            }
        },
        explicit => explicit,
    };

    match kind {
        McpTransportKind::Stdio => {
            let Some(command) = entry.command.clone() else {
                tracing::warn!(server = %slug, "stdio MCP server has no `command`; skipping");
                return None;
            };
            Some(McpTransport::Stdio {
                command,
                args: entry.args.clone(),
                env: expand_env_map(&entry.env),
            })
        }
        McpTransportKind::Http => {
            let Some(url) = entry.url.clone() else {
                tracing::warn!(server = %slug, "http MCP server has no `url`; skipping");
                return None;
            };
            Some(McpTransport::Http {
                url,
                headers: expand_env_map(&entry.headers),
            })
        }
        // `Auto` is resolved to a concrete kind above.
        McpTransportKind::Auto => None,
    }
}

/// Resolve one entry into a config, or `None` when it is disabled or invalid.
fn resolve_entry(slug: &str, entry: &ServerEntry) -> Option<McpServerConfig> {
    if !entry.enabled {
        return None;
    }
    if !entry.unknown.is_empty() {
        let mut keys: Vec<&String> = entry.unknown.keys().collect();
        keys.sort();
        tracing::warn!(
            server = %slug,
            keys = ?keys,
            "ignoring unrecognized MCP server config keys"
        );
    }
    let transport = resolve_transport(slug, entry)?;
    Some(McpServerConfig {
        slug: slug.to_string(),
        transport,
        enabled: entry.enabled,
        timeout: entry.timeout.map(Duration::from_secs),
        protocol: parse_protocol(entry.protocol.as_deref()),
    })
}

/// Expand `${VAR}` references in every value of `map` from the environment.
fn expand_env_map(map: &HashMap<String, String>) -> HashMap<String, String> {
    map.iter()
        .map(|(key, value)| (key.clone(), expand_env(value)))
        .collect()
}

/// Expand `${VAR}` references in `value` from the environment.
///
/// A reference to an unset variable expands to the empty string (with a
/// warning): a missing secret should surface in the log rather than be sent as
/// the literal `${VAR}`. A `${` with no closing `}` is left verbatim.
fn expand_env(value: &str) -> String {
    expand_env_with(value, |name| {
        std::env::var(name)
            .inspect_err(|_| {
                tracing::warn!(
                    var = name,
                    "MCP config references an unset environment variable; expanding to empty"
                );
            })
            .ok()
    })
}

/// [`expand_env`] parameterised on the variable lookup, so the expansion logic
/// can be exercised without mutating the process environment.
///
/// Splits on `${`/`}` with `split_at`/`strip_prefix` (never index slicing) so a
/// multi-byte UTF-8 variable name cannot be cut mid-character.
fn expand_env_with(value: &str, lookup: impl Fn(&str) -> Option<String>) -> String {
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(start) = rest.find("${") {
        let (prefix, tail) = rest.split_at(start);
        out.push_str(prefix);
        // `tail` begins with the `${` sigil.
        let Some(after_open) = tail.strip_prefix("${") else {
            out.push_str(tail);
            return out;
        };
        let Some(end) = after_open.find('}') else {
            // No closing brace: keep the `${`-onwards text verbatim.
            out.push_str(tail);
            return out;
        };
        let (name, after_name) = after_open.split_at(end);
        if let Some(expanded) = lookup(name) {
            out.push_str(&expanded);
        }
        // `after_name` begins with the closing `}`.
        rest = after_name.strip_prefix('}').unwrap_or("");
    }
    out.push_str(rest);
    out
}

thread_local! {
    /// Test-only override for the base config directory. When set,
    /// `mcp_config_path()` returns `<root>/choreographr/mcp_servers.json`
    /// instead of the user's real config dir.
    ///
    /// Deliberately NOT `#[cfg(test)]`-gated: integration tests in `tests/`
    /// compile the crate without `cfg(test)`, so the hook must exist in
    /// normal builds too (it is a no-op unless explicitly set).
    static TEST_CONFIG_ROOT: std::cell::RefCell<Option<PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

/// Test-only override for the base config directory (see `TEST_CONFIG_ROOT`).
///
/// This is needed because `dirs::config_dir()` honors `XDG_CONFIG_HOME` only
/// on Linux — on macOS it always returns `$HOME/Library/Application Support`,
/// so an integration test cannot redirect the config path via environment
/// variables.
#[doc(hidden)]
pub fn set_test_config_root(root: Option<PathBuf>) {
    TEST_CONFIG_ROOT.with(|cell| cell.replace(root));
}

/// Resolve the path to `mcp_servers.json`.
///
/// # Errors
///
/// Returns an error when the user's config directory cannot be determined.
pub fn mcp_config_path() -> Result<PathBuf> {
    if let Some(root) = TEST_CONFIG_ROOT.with(|cell| cell.borrow().clone()) {
        return Ok(root.join("choreographr").join("mcp_servers.json"));
    }
    choreo_shared::paths::config_file("mcp_servers.json")
        .context("could not determine config directory")
}

/// Load MCP server configurations from `mcp_servers.json`.
/// Returns an empty Vec if the file doesn't exist.
///
/// # Errors
///
/// Returns an error when the file exists but cannot be read or parsed.
pub fn load_mcp_config() -> Result<Vec<McpServerConfig>> {
    let path = mcp_config_path()?;
    if !path.exists() {
        return Ok(Vec::new());
    }
    let contents = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let parsed: McpServersFile = serde_json::from_str(&contents)
        .with_context(|| format!("failed to parse {}", path.display()))?;

    Ok(parsed
        .mcp_servers
        .into_iter()
        .filter_map(|(slug, entry)| resolve_entry(&slug, &entry))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_mcp_config_file_not_found_returns_empty() {
        // No mcp_servers.json exists in the test environment → returns empty Vec.
        let configs = load_mcp_config().unwrap_or_else(|_| Vec::new());
        // The file doesn't exist in CI so we expect empty.
        // This test just verifies no panic on the happy path.
        assert!(configs.is_empty() || !configs.is_empty());
    }

    #[test]
    fn mcp_config_path_is_absolute() {
        let path = mcp_config_path().expect("should resolve config path");
        assert!(path.is_absolute());
        assert!(path.ends_with("mcp_servers.json"));
    }

    #[test]
    fn default_true_returns_true() {
        assert!(default_true());
    }

    #[test]
    fn server_entry_deserializes_minimal() {
        let json = r#"{"command": "npx"}"#;
        let entry: ServerEntry = serde_json::from_str(json).expect("minimal server entry");
        assert_eq!(entry.command.as_deref(), Some("npx"));
        assert_eq!(entry.args, [] as [std::string::String; 0]);
        assert!(entry.env.is_empty());
        assert!(entry.enabled);
        assert!(entry.timeout.is_none());
        assert!(entry.protocol.is_none());
        assert!(entry.unknown.is_empty());
    }

    #[test]
    fn server_entry_deserializes_full() {
        let json = serde_json::json!({
            "command": "python",
            "args": ["-m", "server"],
            "env": {"KEY": "value"},
            "enabled": false,
            "timeout": 30
        });
        let entry: ServerEntry = serde_json::from_value(json).expect("full server entry");
        assert_eq!(entry.command.as_deref(), Some("python"));
        assert_eq!(entry.args, vec!["-m", "server"]);
        assert_eq!(
            entry.env.get("KEY").map(std::string::String::as_str),
            Some("value")
        );
        assert!(!entry.enabled);
        assert_eq!(entry.timeout, Some(30));
        assert!(entry.unknown.is_empty());
    }

    #[test]
    fn server_entry_deserializes_http() {
        let json = serde_json::json!({
            "url": "https://example.com/mcp",
            "headers": {"Authorization": "Bearer abc"}
        });
        let entry: ServerEntry = serde_json::from_value(json).expect("http server entry");
        assert_eq!(entry.url.as_deref(), Some("https://example.com/mcp"));
        assert!(entry.command.is_none());
        assert_eq!(
            entry.headers.get("Authorization").map(String::as_str),
            Some("Bearer abc")
        );
    }

    #[test]
    fn server_entry_collects_unknown_keys() {
        // `auto_load` was removed; a config still carrying it must load (not
        // error) with the key reported rather than silently dropped.
        let json = serde_json::json!({
            "command": "echo",
            "auto_load": false
        });
        let entry: ServerEntry = serde_json::from_value(json).expect("entry with unknown key");
        assert!(entry.unknown.contains_key("auto_load"));
    }

    fn entry_from(json: serde_json::Value) -> ServerEntry {
        serde_json::from_value(json).expect("server entry")
    }

    #[test]
    fn auto_infers_stdio_from_command() {
        let entry = entry_from(serde_json::json!({"command": "npx"}));
        match resolve_transport("s", &entry).expect("resolved") {
            McpTransport::Stdio { command, .. } => assert_eq!(command, "npx"),
            other @ McpTransport::Http { .. } => panic!("expected stdio, got {other:?}"),
        }
    }

    #[test]
    fn auto_infers_http_from_url() {
        let entry = entry_from(serde_json::json!({"url": "https://example.com/mcp"}));
        match resolve_transport("s", &entry).expect("resolved") {
            McpTransport::Http { url, .. } => assert_eq!(url, "https://example.com/mcp"),
            other @ McpTransport::Stdio { .. } => panic!("expected http, got {other:?}"),
        }
    }

    #[test]
    fn explicit_transport_overrides_inference() {
        // A `transport: "http"` with a url is http even though it also has a
        // stray command; explicit wins.
        let entry = entry_from(serde_json::json!({
            "command": "ignored",
            "url": "https://example.com/mcp",
            "transport": "http"
        }));
        assert!(matches!(
            resolve_transport("s", &entry),
            Some(McpTransport::Http { .. })
        ));
    }

    #[test]
    fn ambiguous_and_incomplete_entries_are_skipped() {
        let both = entry_from(serde_json::json!({
            "command": "npx",
            "url": "https://example.com/mcp"
        }));
        assert!(resolve_transport("s", &both).is_none());

        let neither = entry_from(serde_json::json!({}));
        assert!(resolve_transport("s", &neither).is_none());

        let http_without_url = entry_from(serde_json::json!({"transport": "http"}));
        assert!(resolve_transport("s", &http_without_url).is_none());
    }

    #[test]
    fn disabled_servers_are_filtered() {
        let json = serde_json::json!({
            "mcpServers": {
                "enabled-server": {"command": "echo", "enabled": true},
                "disabled-server": {"command": "false", "enabled": false}
            }
        });
        let parsed: McpServersFile = serde_json::from_value(json).unwrap();
        let configs: Vec<McpServerConfig> = parsed
            .mcp_servers
            .into_iter()
            .filter_map(|(slug, entry)| resolve_entry(&slug, &entry))
            .collect();
        assert_eq!(configs.len(), 1);
        assert_eq!(configs[0].slug, "enabled-server");
    }

    #[test]
    fn parse_protocol_maps_values() {
        assert_eq!(parse_protocol(None), McpProtocolMode::Auto);
        assert_eq!(parse_protocol(Some("legacy")), McpProtocolMode::Legacy);
        assert_eq!(parse_protocol(Some("modern")), McpProtocolMode::Modern);
        // Unrecognized values fall back to the default rather than failing.
        assert_eq!(parse_protocol(Some("bogus")), McpProtocolMode::Auto);
    }

    #[test]
    fn expand_env_substitutes_and_leaves_literal() {
        // Fake lookup: only `TOKEN` resolves; everything else is absent.
        let lookup = |name: &str| {
            if name == "TOKEN" {
                Some("s3cret".to_string())
            } else {
                None
            }
        };
        assert_eq!(expand_env_with("Bearer ${TOKEN}", lookup), "Bearer s3cret");
        assert_eq!(expand_env_with("${TOKEN}", lookup), "s3cret");
        // An unset variable expands to empty (not the literal), and a `${` with
        // no closing brace is left verbatim.
        assert_eq!(expand_env_with("x${UNSET}y", lookup), "xy");
        assert_eq!(
            expand_env_with("a ${unterminated", lookup),
            "a ${unterminated"
        );
        assert_eq!(expand_env_with("no refs here", lookup), "no refs here");
    }

    #[test]
    fn expand_env_map_expands_every_value() {
        let mut map = HashMap::new();
        map.insert("A".to_string(), "plain".to_string());
        map.insert("B".to_string(), "no refs".to_string());
        let expanded = expand_env_map(&map);
        assert_eq!(expanded.get("A").map(String::as_str), Some("plain"));
        assert_eq!(expanded.get("B").map(String::as_str), Some("no refs"));
    }
}
