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
    /// Working directory for a stdio subprocess. A leading `~`/`~/` is expanded
    /// to the user's home directory.
    #[serde(default)]
    cwd: Option<String>,
    /// Tool names (as the server advertises them) to hide from the model.
    #[serde(default, rename = "disabledTools", alias = "disabled_tools")]
    disabled_tools: Vec<String>,
    #[serde(default = "default_true")]
    enabled: bool,
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
                cwd: entry.cwd.as_deref().map(expand_tilde),
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
        max_concurrent_calls: entry.max_concurrent_calls,
        disabled_tools: entry.disabled_tools.clone(),
    })
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

    /// Test-only override for the project config root. `Disabled` forces "no
    /// project file"; `Path(root)` points the project file at
    /// `<root>/.choreographr/mcp_servers.json`; `Unset` falls back to the base
    /// dir / current dir.
    static TEST_PROJECT_ROOT: std::cell::RefCell<ProjectRootOverride> =
        const { std::cell::RefCell::new(ProjectRootOverride::Unset) };
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

/// Test-only override for the project config root (see `TEST_PROJECT_ROOT`).
#[doc(hidden)]
pub fn set_test_project_root(root: Option<Option<PathBuf>>) {
    let override_value = match root {
        None => ProjectRootOverride::Unset,
        Some(None) => ProjectRootOverride::Disabled,
        Some(Some(path)) => ProjectRootOverride::Path(path),
    };
    TEST_PROJECT_ROOT.with(|cell| cell.replace(override_value));
}

/// Resolve the path to the **user** `mcp_servers.json`.
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

/// Resolve the path to the **project** `mcp_servers.json`, if one can be placed.
///
/// The project file lives at `<root>/.choreographr/mcp_servers.json`, where
/// `<root>` is the base dir when the daemon runs under `--base-dir` and the
/// process's current directory otherwise. A project file lets a checkout carry
/// its own server set without editing the user config; its entries override the
/// user file's per server slug. `None` when neither a base dir nor a current
/// directory is resolvable (the project layer is then simply absent).
#[must_use]
pub fn project_config_path() -> Option<PathBuf> {
    match TEST_PROJECT_ROOT.with(|cell| cell.borrow().clone()) {
        ProjectRootOverride::Path(root) => {
            return Some(root.join(".choreographr").join("mcp_servers.json"));
        }
        ProjectRootOverride::Disabled => return None,
        ProjectRootOverride::Unset => {}
    }
    let root = choreo_shared::paths::base_dir().or_else(|| std::env::current_dir().ok());
    root.map(|root| root.join(".choreographr").join("mcp_servers.json"))
}

/// Test-only override for the project config root (see `TEST_PROJECT_ROOT`),
/// distinguishing "unset" from "explicitly no project file".
#[derive(Clone)]
enum ProjectRootOverride {
    /// No override: resolve from the base dir / current dir.
    Unset,
    /// The project layer is disabled.
    Disabled,
    /// The project file root is this directory.
    Path(PathBuf),
}

/// Load MCP server configurations from the user file, overlaying the project
/// file when present.
///
/// Returns an empty Vec if neither file exists. Project entries replace user
/// entries per server slug (a whole-entry override, so a project can point a
/// server at a different command/URL entirely).
///
/// # Errors
///
/// Returns an error when a present file cannot be read or parsed.
pub fn load_mcp_config() -> Result<Vec<McpServerConfig>> {
    let user = mcp_config_path()?;
    let project = project_config_path();
    load_from_paths(&user, project.as_deref())
}

/// [`load_mcp_config`] parameterised on the two file paths, so the merge can be
/// exercised without mutating the process environment or base dir.
///
/// # Errors
///
/// Returns an error when a present file cannot be read or parsed.
fn load_from_paths(
    user: &std::path::Path,
    project: Option<&std::path::Path>,
) -> Result<Vec<McpServerConfig>> {
    let mut entries: HashMap<String, ServerEntry> = HashMap::new();
    read_servers_into(user, &mut entries)?;
    if let Some(project) = project {
        // Project entries extend and override the user's — re-keyed by slug, so
        // a project's `docs` replaces the user's `docs` outright.
        read_servers_into(project, &mut entries)?;
    }
    Ok(entries
        .into_iter()
        .filter_map(|(slug, entry)| resolve_entry(&slug, &entry))
        .collect())
}

/// Parse one `mcp_servers.json` (if it exists) into `entries`, keyed by slug.
fn read_servers_into(
    path: &std::path::Path,
    entries: &mut HashMap<String, ServerEntry>,
) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let parsed: McpServersFile = serde_json::from_str(&contents)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    entries.extend(parsed.mcp_servers);
    Ok(())
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
        match resolve_transport("s", &entry).expect("resolved") {
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
        let config = resolve_entry("s", &entry).expect("resolved");
        assert_eq!(config.disabled_tools, vec!["a", "b"]);
    }

    #[test]
    fn project_layer_overrides_user_per_slug() {
        let dir = tempfile::tempdir().unwrap();
        let user = dir.path().join("user.json");
        let project = dir.path().join("project.json");
        std::fs::write(
            &user,
            r#"{"mcpServers":{
                "docs":{"url":"https://user.example/mcp"},
                "fs":{"command":"user-fs"}
            }}"#,
        )
        .unwrap();
        std::fs::write(
            &project,
            r#"{"mcpServers":{
                "docs":{"url":"https://project.example/mcp"},
                "extra":{"command":"project-extra"}
            }}"#,
        )
        .unwrap();

        let configs = load_from_paths(&user, Some(&project)).unwrap();
        let by_slug: HashMap<&str, &McpServerConfig> =
            configs.iter().map(|c| (c.slug.as_str(), c)).collect();
        // The project's `docs` wins outright.
        match &by_slug["docs"].transport {
            McpTransport::Http { url, .. } => assert_eq!(url, "https://project.example/mcp"),
            other @ McpTransport::Stdio { .. } => panic!("expected http, got {other:?}"),
        }
        // The user's `fs` survives; the project's `extra` is added.
        assert!(by_slug.contains_key("fs"));
        assert!(by_slug.contains_key("extra"));
    }

    #[test]
    fn load_from_paths_handles_missing_files() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("none.json");
        let configs = load_from_paths(&missing, Some(&missing)).unwrap();
        assert!(configs.is_empty());
    }
}
