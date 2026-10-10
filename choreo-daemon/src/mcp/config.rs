//! MCP server config parsing and expansion for both tiers.
//!
//! Parses the standard MCP `mcpServers` shape — `command`/`args`/`env`/`cwd`
//! for a stdio server, `url`/`headers` for a remote one, with an explicit
//! `transport` overriding inference from the keys present — expands `${VAR}`
//! references and a leading `~` in `cwd`, and produces the `choreo-mcp`
//! server config the manager connects with. A project tier's entries are
//! expanded only when the project root is trusted (an untrusted project is
//! read for status but never expanded and never spawned).

use anyhow::{Context, Result};
use choreo_mcp::{McpProtocolMode, McpServerConfig, McpTransport, McpTransportKind};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

/// Top-level structure matching the standard MCP `mcpServers` config format.
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
    /// Working directory for a stdio subprocess. A leading `~`/`~/` is expanded
    /// to the user's home directory.
    #[serde(default)]
    cwd: Option<String>,
    /// Tool names (as the server advertises them) to hide from the model.
    #[serde(default, rename = "disabledTools", alias = "disabled_tools")]
    disabled_tools: Vec<String>,
    #[serde(default = "default_true")]
    enabled: bool,
    /// Whether this server's connection may be pooled and shared across every
    /// session that references it (`shared`, default `true`). `false` gives
    /// the server a private connection per session that uses it — the escape
    /// hatch for a stateful server whose state must not be shared between
    /// sessions. This is a daemon-layer pooling attribute, not a wire/transport
    /// concern (so it is not part of [`McpServerConfig`]).
    #[serde(default = "default_true")]
    shared: bool,
    /// Optional per-server request timeout, in seconds.
    #[serde(default)]
    timeout: Option<u64>,
    /// Optional protocol-era selector: `"auto"` (default), `"legacy"`, or
    /// `"modern"` (aliased by the explicit era string `"2026-07-28"`).
    #[serde(default)]
    protocol: Option<String>,
    /// Optional cap on concurrent in-flight tool calls to this server
    /// (`maxConcurrentCalls`; `max_concurrent_calls` is accepted as an alias).
    /// When unset, the client's default applies.
    #[serde(default, rename = "maxConcurrentCalls", alias = "max_concurrent_calls")]
    max_concurrent_calls: Option<usize>,
    /// Optional cap on consecutive transport-rebuild attempts before the
    /// dispatcher stops reconnecting until the next request (`maxRestarts`;
    /// `max_restarts` is accepted as an alias). When unset, the client's
    /// default applies; `0` disables automatic reconnect.
    #[serde(default, rename = "maxRestarts", alias = "max_restarts")]
    max_restarts: Option<u32>,
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
fn resolve_transport(
    identity: &str,
    slug: &str,
    entry: &ServerEntry,
    expand_env: bool,
) -> Option<McpTransport> {
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
                env: maybe_expand_env_map(&entry.env, expand_env),
                cwd: entry.cwd.as_deref().map(expand_tilde),
                log_path: Some(server_log_path(identity, slug)),
            })
        }
        McpTransportKind::Http => {
            let Some(url) = entry.url.clone() else {
                tracing::warn!(server = %slug, "http MCP server has no `url`; skipping");
                return None;
            };
            Some(McpTransport::Http {
                url,
                headers: maybe_expand_env_map(&entry.headers, expand_env),
            })
        }
        // `Auto` is resolved to a concrete kind above.
        McpTransportKind::Auto => None,
    }
}

/// A resolved daemon-layer MCP server: its client config plus the pooling
/// attribute (`shared`) the daemon layers on top. `shared` is deliberately NOT
/// part of [`McpServerConfig`] — it governs how the daemon pools the
/// connection, not how the client speaks to the server.
#[derive(Debug, Clone)]
pub struct McpEntry {
    /// The resolved client config.
    pub config: McpServerConfig,
    /// Whether the connection may be shared across sessions (default `true`).
    pub shared: bool,
}

/// Resolve one entry into a config, or `None` when it is disabled or invalid.
///
/// `expand_env` gates `${VAR}` expansion in `env`/`headers` values: the
/// daemon tier always expands; a project entry expands only when its project
/// root is trusted (see [`crate::mcp::trust`]). An untrusted project entry is
/// never resolved into a spawnable config this way.
fn resolve_entry(
    identity: &str,
    slug: &str,
    entry: &ServerEntry,
    expand_env: bool,
) -> Option<McpEntry> {
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
    let transport = resolve_transport(identity, slug, entry, expand_env)?;
    Some(McpEntry {
        config: McpServerConfig {
            slug: slug.to_string(),
            transport,
            enabled: entry.enabled,
            timeout: entry.timeout.map(Duration::from_secs),
            protocol: parse_protocol(entry.protocol.as_deref()),
            max_concurrent_calls: entry.max_concurrent_calls,
            max_restarts: entry.max_restarts,
            disabled_tools: entry.disabled_tools.clone(),
        },
        shared: entry.shared,
    })
}

/// The per-server log file path for a stdio server.
///
/// The child's `stderr` is captured here (size-capped) so each server's own
/// diagnostics are isolated rather than mixed into the daemon's log. The stem is
/// `mcp-<safe-slug>-<hash>` — the slug sanitized to filename-safe characters and
/// disambiguated by a short hash of the tier-scoped `identity` plus the slug.
/// The path is resolved through the same [`choreo_shared::paths::log_file_path`]
/// as every other Choreographr log (`{base}/log` → `$XDG_STATE_HOME/choreographr`
/// → the platform temp dir), so there is always a file — never a silent
/// "child inherits stderr" on macOS/Windows.
fn server_log_path(identity: &str, slug: &str) -> PathBuf {
    choreo_shared::paths::log_file_path(&log_file_stem(identity, slug))
}

/// The per-server log file stem (`mcp-<safe-slug>-<hash>`): the slug sanitized
/// to filename-safe characters, disambiguated by a stable short hash of the
/// tier-scoped `identity` and the ORIGINAL slug.
///
/// `identity` scopes the log to the TIER that declared the server (the fixed
/// `"daemon"` string for the daemon tier, the project root for a project tier),
/// so two servers sharing one slug in different tiers — a daemon `docs` and a
/// project `docs`, or two projects each with a `docs` — get DISTINCT log files
/// rather than clobbering each other's.
fn log_file_stem(identity: &str, slug: &str) -> String {
    let safe: String = slug
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    // The sanitizer is many-to-one (`a.b` and `a_b` both become `a_b`), so two
    // distinct servers could otherwise share — and race on — one log file. The
    // stable FNV-1a short hash of identity+slug keeps each server's stem unique
    // while staying reproducible across runs and toolchains. The `\0` separator
    // between identity and slug ensures a slug like `a` with identity `bc`
    // cannot collide with slug `ab` and identity `c`.
    format!(
        "mcp-{safe}-{}",
        choreo_mcp::short_hash(&format!("{identity}\0{slug}"))
    )
}

/// Expand a leading `~` (or `~/`) in `path` to the user's home directory.
///
/// A config author writes `~/work` expecting a shell-like expansion, not a
/// literal directory named `~`. Expansion applies only to a *leading* `~`; an
/// embedded `~` is left alone (it is a valid path character). When the home
/// directory cannot be resolved the path is returned unchanged.
fn expand_tilde(path: &str) -> String {
    if path == "~" {
        return dirs::home_dir().map_or_else(
            || path.to_string(),
            |home| home.to_string_lossy().into_owned(),
        );
    }
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest).to_string_lossy().into_owned();
    }
    path.to_string()
}

/// Expand `${VAR}` references in every value of `map` from the environment.
fn expand_env_map(map: &HashMap<String, String>) -> HashMap<String, String> {
    map.iter()
        .map(|(key, value)| (key.clone(), expand_env(value)))
        .collect()
}

/// [`expand_env_map`] gated on `expand`: when `expand` is false the map is
/// returned verbatim (a project entry whose root is UNTRUSTED must never have
/// its secrets pulled from the environment).
fn maybe_expand_env_map(map: &HashMap<String, String>, expand: bool) -> HashMap<String, String> {
    if expand {
        expand_env_map(map)
    } else {
        map.clone()
    }
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

/// The MCP server file name shared by both tiers.
///
/// The daemon tier lives at `<config dir>/choreographr/mcp.json`; a project
/// tier lives at `<project root>/.mcp.json` (a leading dot, the MCP-ecosystem
/// convention). `mcpServers` is the MCP-standard key inside both.
pub const DAEMON_CONFIG_FILE: &str = "mcp.json";

/// The log identity for the daemon tier: a fixed string (not a path) so the
/// daemon's own `mcp.json` servers log under a stable, project-independent
/// scope. Project tiers use the project root as their identity instead.
const DAEMON_LOG_IDENTITY: &str = "daemon";

/// Test-only override for the base config directory. Re-exported from the
/// module root (which owns the thread-local) so the existing
/// `mcp::config::set_test_config_root` call sites in `tests/` keep working.
#[doc(hidden)]
pub fn set_test_config_root(root: Option<PathBuf>) {
    super::set_test_config_root(root);
}

/// Resolve the path to the daemon-tier `mcp.json`.
///
/// # Errors
///
/// Returns an error when the config directory cannot be determined.
pub fn mcp_config_path() -> Result<PathBuf> {
    Ok(super::config_dir()?.join(DAEMON_CONFIG_FILE))
}

/// The project-tier `.mcp.json` path for a resolved project root.
#[must_use]
pub fn project_config_path(project_root: &std::path::Path) -> PathBuf {
    project_root.join(super::PROJECT_CONFIG_FILE)
}

/// Load the **daemon-tier** server set from `mcp.json`.
///
/// Returns an empty Vec if the file does not exist. Daemon-tier entries are
/// trusted unconditionally, so `${VAR}` expansion is always applied.
///
/// # Errors
///
/// Returns an error when a present file cannot be read or parsed.
pub fn load_daemon_config() -> Result<Vec<McpEntry>> {
    let path = mcp_config_path()?;
    Ok(read_entries_into(&path)?.map_or_else(Vec::new, |entries| {
        resolve_entries(DAEMON_LOG_IDENTITY, &entries, true)
    }))
}

/// Load the **project-tier** server set from `<project_root>/.mcp.json`.
///
/// Returns `None` when the file does not exist. `expand` gates `${VAR}`
/// expansion: only a TRUSTED project root expands. The caller decides whether
/// to spawn the entries; an untrusted root's entries are read here (unexpanded)
/// purely so status can report the slugs being ignored.
///
/// # Errors
///
/// Returns an error when a present file cannot be read or parsed.
pub fn load_project_config(
    project_root: &std::path::Path,
    expand: bool,
) -> Result<Option<Vec<McpEntry>>> {
    let path = project_config_path(project_root);
    // The project root scopes this tier's log files: it is stable for a given
    // project across loads, so a reconnecting project server reuses its log,
    // while two projects (or a project and the daemon) with the same slug keep
    // DISTINCT logs.
    let identity = project_root.to_string_lossy();
    Ok(read_entries_into(&path)?.map(|entries| resolve_entries(&identity, &entries, expand)))
}

/// Resolve a parsed slug→entry map into a deterministic, slug-sorted list.
///
/// `identity` scopes the per-server log files to the tier this map came from
/// (see [`log_file_stem`]).
fn resolve_entries(
    identity: &str,
    entries: &HashMap<String, ServerEntry>,
    expand: bool,
) -> Vec<McpEntry> {
    let mut resolved: Vec<(String, McpEntry)> = entries
        .iter()
        .filter_map(|(slug, entry)| {
            resolve_entry(identity, slug, entry, expand).map(|e| (slug.clone(), e))
        })
        .collect();
    resolved.sort_by(|a, b| a.0.cmp(&b.0));
    resolved.into_iter().map(|(_, e)| e).collect()
}

/// Parse one `mcp.json`/`.mcp.json` (if it exists) into `entries`, keyed by
/// slug. `None` when the file is absent.
fn read_entries_into(path: &std::path::Path) -> Result<Option<HashMap<String, ServerEntry>>> {
    if !path.exists() {
        return Ok(None);
    }
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let parsed: McpServersFile = serde_json::from_str(&contents)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    Ok(Some(parsed.mcp_servers))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_daemon_config_missing_file_returns_empty() {
        // Point the config root at a temp dir with no mcp.json so the real
        // user config is never read.
        let dir = tempfile::tempdir().unwrap();
        set_test_config_root(Some(dir.path().to_path_buf()));
        let configs = load_daemon_config().expect("load daemon config");
        set_test_config_root(None);
        assert!(configs.is_empty(), "no mcp.json yields no configs");
    }

    #[test]
    fn mcp_config_path_is_absolute() {
        let path = mcp_config_path().expect("should resolve config path");
        assert!(path.is_absolute());
        assert!(path.ends_with("mcp.json"));
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
        assert!(entry.shared, "shared defaults to true");
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
    fn server_entry_deserializes_max_concurrent_calls() {
        // camelCase is the documented spelling; the snake_case alias is accepted
        // for convenience.
        let camel: ServerEntry = serde_json::from_value(serde_json::json!({
            "command": "python",
            "maxConcurrentCalls": 2
        }))
        .expect("entry with maxConcurrentCalls");
        assert_eq!(camel.max_concurrent_calls, Some(2));
        assert!(camel.unknown.is_empty(), "recognized key is not 'unknown'");

        let snake: ServerEntry = serde_json::from_value(serde_json::json!({
            "command": "python",
            "max_concurrent_calls": 6
        }))
        .expect("entry with max_concurrent_calls alias");
        assert_eq!(snake.max_concurrent_calls, Some(6));
    }

    #[test]
    fn server_entry_deserializes_max_restarts() {
        let camel: ServerEntry = serde_json::from_value(serde_json::json!({
            "command": "python",
            "maxRestarts": 5
        }))
        .expect("entry with maxRestarts");
        assert_eq!(camel.max_restarts, Some(5));
        assert!(camel.unknown.is_empty(), "recognized key is not 'unknown'");

        let snake: ServerEntry = serde_json::from_value(serde_json::json!({
            "command": "python",
            "max_restarts": 0
        }))
        .expect("entry with max_restarts alias");
        assert_eq!(snake.max_restarts, Some(0));
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
        match resolve_transport("tier", "s", &entry, true).expect("resolved") {
            McpTransport::Stdio { command, .. } => assert_eq!(command, "npx"),
            other @ McpTransport::Http { .. } => panic!("expected stdio, got {other:?}"),
        }
    }

    #[test]
    fn auto_infers_http_from_url() {
        let entry = entry_from(serde_json::json!({"url": "https://example.com/mcp"}));
        match resolve_transport("tier", "s", &entry, true).expect("resolved") {
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
            resolve_transport("tier", "s", &entry, true),
            Some(McpTransport::Http { .. })
        ));
    }

    #[test]
    fn ambiguous_and_incomplete_entries_are_skipped() {
        let both = entry_from(serde_json::json!({
            "command": "npx",
            "url": "https://example.com/mcp"
        }));
        assert!(resolve_transport("tier", "s", &both, true).is_none());

        let neither = entry_from(serde_json::json!({}));
        assert!(resolve_transport("tier", "s", &neither, true).is_none());

        let http_without_url = entry_from(serde_json::json!({"transport": "http"}));
        assert!(resolve_transport("tier", "s", &http_without_url, true).is_none());
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
        let configs: Vec<McpEntry> = parsed
            .mcp_servers
            .into_iter()
            .filter_map(|(slug, entry)| resolve_entry("tier", &slug, &entry, true))
            .collect();
        assert_eq!(configs.len(), 1);
        assert_eq!(configs[0].config.slug, "enabled-server");
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

    #[test]
    fn server_entry_deserializes_cwd_and_disabled_tools() {
        // camelCase is documented; the snake_case alias is accepted too, and
        // both keys are recognized (not collected as "unknown").
        let camel: ServerEntry = serde_json::from_value(serde_json::json!({
            "command": "python",
            "cwd": "/work",
            "disabledTools": ["dangerous", "admin"]
        }))
        .expect("entry with cwd/disabledTools");
        assert_eq!(camel.cwd.as_deref(), Some("/work"));
        assert_eq!(camel.disabled_tools, vec!["dangerous", "admin"]);
        assert!(
            camel.unknown.is_empty(),
            "recognized keys are not 'unknown'"
        );

        let snake: ServerEntry = serde_json::from_value(serde_json::json!({
            "command": "python",
            "disabled_tools": ["x"]
        }))
        .expect("entry with disabled_tools alias");
        assert_eq!(snake.disabled_tools, vec!["x"]);
    }

    #[test]
    fn stdio_transport_carries_cwd() {
        let entry = entry_from(serde_json::json!({"command": "npx", "cwd": "/srv/mcp"}));
        match resolve_transport("tier", "s", &entry, true).expect("resolved") {
            McpTransport::Stdio { cwd, .. } => assert_eq!(cwd.as_deref(), Some("/srv/mcp")),
            other @ McpTransport::Http { .. } => panic!("expected stdio, got {other:?}"),
        }
    }

    #[test]
    fn expand_tilde_expands_leading_tilde_only() {
        // `~` and `~/x` expand; an embedded or mid-string `~` is untouched.
        if let Some(home) = dirs::home_dir() {
            let home_s = home.to_string_lossy();
            assert_eq!(expand_tilde("~"), home_s);
            assert_eq!(expand_tilde("~/work"), home.join("work").to_string_lossy());
        }
        assert_eq!(expand_tilde("/abs/path"), "/abs/path");
        assert_eq!(expand_tilde("rel/~/path"), "rel/~/path");
    }

    #[test]
    fn resolve_entry_carries_disabled_tools() {
        let entry = entry_from(serde_json::json!({
            "command": "python",
            "disabledTools": ["a", "b"]
        }));
        let config = resolve_entry("tier", "s", &entry, true).expect("resolved");
        assert_eq!(config.config.disabled_tools, vec!["a", "b"]);
    }

    #[test]
    fn project_load_reads_dot_mcp_json_and_gates_expansion() {
        let dir = tempfile::tempdir().unwrap();
        // A project root with a `.mcp.json` whose env requests expansion.
        std::fs::write(
            project_config_path(dir.path()),
            r#"{"mcpServers":{"docs":{"url":"https://project.example/mcp","headers":{"Authorization":"Bearer ${MCP_TEST_TOKEN}"}}}}"#,
        )
        .unwrap();

        // Trusted: expansion happens (the unset var collapses to empty).
        let trusted = load_project_config(dir.path(), true)
            .expect("load project")
            .expect("present");
        assert_eq!(trusted.len(), 1);
        match &trusted[0].config.transport {
            McpTransport::Http { headers, .. } => {
                assert_eq!(
                    headers.get("Authorization").map(String::as_str),
                    Some("Bearer ")
                );
            }
            other @ McpTransport::Stdio { .. } => panic!("expected http, got {other:?}"),
        }

        // Untrusted: the literal `${...}` is preserved (never expanded).
        let untrusted = load_project_config(dir.path(), false)
            .expect("load project")
            .expect("present");
        match &untrusted[0].config.transport {
            McpTransport::Http { headers, .. } => {
                assert_eq!(
                    headers.get("Authorization").map(String::as_str),
                    Some("Bearer ${MCP_TEST_TOKEN}")
                );
            }
            other @ McpTransport::Stdio { .. } => panic!("expected http, got {other:?}"),
        }
    }

    #[test]
    fn project_load_missing_file_is_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            load_project_config(dir.path(), true)
                .expect("load")
                .is_none(),
            "a project root with no .mcp.json loads as None"
        );
    }

    #[test]
    fn shared_false_is_carried_through_resolution() {
        let entry = entry_from(serde_json::json!({"command": "stateful", "shared": false}));
        let resolved = resolve_entry("tier", "s", &entry, true).expect("resolved");
        assert!(!resolved.shared);
    }

    #[test]
    fn parsing_malformed_config_never_panics() {
        // Fuzz-style: a deterministic corpus of truncated, adversarial, and
        // wrongly-typed JSON through the exact type `read_servers_into` parses.
        // A parse error is fine; a panic is not.
        let corpus = [
            "",
            "{",
            "}",
            "null",
            "[]",
            "{\"mcpServers\":\"not-an-object\"}",
            "{\"mcpServers\":[]}",
            "{\"mcpServers\":{\"s\":\"a-string\"}}",
            "{\"mcpServers\":{\"s\":{\"command\":42}}}",
            "{\"mcpServers\":{\"s\":{\"timeout\":-1,\"args\":\"x\"}}}",
            "{\"mcpServers\":{\"☃\":{\"url\":\"\",\"headers\":{\"a\":null}}}}",
            "{\"unknown_top\":1}",
        ];
        for raw in corpus {
            let _ = serde_json::from_str::<McpServersFile>(raw);
        }
        // Deeply nested input must fail cleanly (serde_json's recursion limit)
        // rather than overflow the stack.
        let deep = format!("{}{}", "[".repeat(10_000), "]".repeat(10_000));
        assert!(serde_json::from_str::<McpServersFile>(&deep).is_err());
    }

    #[test]
    fn expand_env_handles_adversarial_input() {
        // An empty/odd `${...}` sequence must never panic, and an unset variable
        // expands to nothing rather than the literal text.
        let lookup = |_name: &str| -> Option<String> { None };
        let cases = [
            "${",
            "${
}",
            "${x",
            "}",
            "a${b}c",
            "${}${}",
            "${a}${b}${c}",
            "プレースホルダ",
        ];
        for case in cases {
            let _ = expand_env_with(case, lookup);
        }
        assert_eq!(expand_env_with("${UNSET}", lookup), "");
        assert_eq!(expand_env_with("keep ${UNSET} me", lookup), "keep  me");
    }

    #[test]
    fn log_file_stem_sanitizes_the_slug() {
        // The sanitized slug is kept for readability, with the stable short
        // hash appended. `docs` needs no sanitizing; `my.server` and `a/b` map
        // their unsafe characters to `_`.
        assert_eq!(
            log_file_stem("tier", "docs"),
            format!("mcp-docs-{}", choreo_mcp::short_hash("tier\0docs"))
        );
        assert_eq!(
            log_file_stem("tier", "my.server"),
            format!(
                "mcp-my_server-{}",
                choreo_mcp::short_hash("tier\0my.server")
            )
        );
        assert_eq!(
            log_file_stem("tier", "a/b"),
            format!("mcp-a_b-{}", choreo_mcp::short_hash("tier\0a/b"))
        );
        // The stem is stable across calls (a reconnect writes the same file).
        assert_eq!(log_file_stem("tier", "docs"), log_file_stem("tier", "docs"));
        // Two slugs that sanitize to the SAME stem (`a.b` and `a_b`) must now
        // produce DIFFERENT stems, so their servers never race on one log file.
        assert_eq!(
            log_file_stem("tier", "a.b")
                .rsplit_once('-')
                .map(|(stem, _)| stem),
            log_file_stem("tier", "a_b")
                .rsplit_once('-')
                .map(|(stem, _)| stem),
            "both slug spellings must sanitize to the same readable prefix"
        );
        assert_ne!(log_file_stem("tier", "a.b"), log_file_stem("tier", "a_b"));
    }

    #[test]
    fn log_file_stem_is_keyed_by_tier_scoped_identity() {
        // (a) A daemon-tier and a project-tier server with the SAME slug are
        // distinct connections and must NOT share one log file.
        let daemon = log_file_stem("daemon", "docs");
        let project = log_file_stem("/home/me/proj", "docs");
        assert_ne!(daemon, project);
        // The readable sanitized-slug prefix is preserved in both.
        for stem in [&daemon, &project] {
            assert!(stem.starts_with("mcp-docs-"), "prefix kept: {stem}");
        }

        // (b) Two DIFFERENT project identities with the same slug must differ.
        assert_ne!(
            log_file_stem("/home/me/proj-a", "docs"),
            log_file_stem("/home/me/proj-b", "docs")
        );

        // (c) The same (identity, slug) is stable across calls.
        assert_eq!(
            log_file_stem("/home/me/proj", "docs"),
            log_file_stem("/home/me/proj", "docs")
        );

        // The `\0` separator prevents an identity/slug boundary collision:
        // ("bc", "a") and ("c", "ab") must not hash to the same stem.
        assert_ne!(log_file_stem("bc", "a"), log_file_stem("c", "ab"));
    }

    #[test]
    fn read_entries_missing_files_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("none.json");
        assert!(read_entries_into(&missing).unwrap().is_none());
    }
}
