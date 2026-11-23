//! The `mcp` subcommand group: list/add/remove servers in the on-disk config,
//! and reconnect/reload servers on a running daemon over its local socket.
//!
//! The `mcp list/add/remove` operations read and write the same
//! `{"mcpServers": { … }}` file the daemon's `mcp` module loads (`mcp.json`),
//! but do so DIRECTLY rather than through that module: the module is compiled
//! only behind the daemon's `mcp` cargo feature (on by default; an embedder
//! opts out with `default-features = false`), while this CLI group is always
//! available. The path resolution and file shape are kept identical so a
//! server added here is picked up by a feature-enabled daemon unchanged.

use super::McpCliCommand;
use anyhow::Context;
use std::path::PathBuf;

/// Resolve the path to the **daemon-tier** `mcp.json`.
///
/// # Errors
///
/// Returns an error when the user's config directory cannot be determined.
fn user_mcp_config_path() -> anyhow::Result<PathBuf> {
    choreo_shared::paths::config_file("mcp.json").context("could not determine config directory")
}

/// Resolve the **project-tier** `.mcp.json` for the current directory, if one
/// can be placed: `<base_dir or cwd>/.mcp.json`. This is the offline CLI's
/// best-effort notion of "the project file"; the daemon resolves a SESSION's
/// project root by walking up from its working directory.
fn project_mcp_config_path() -> Option<PathBuf> {
    let root = choreo_shared::paths::base_dir().or_else(|| std::env::current_dir().ok());
    root.map(|root| root.join(".mcp.json"))
}

/// Read the `mcpServers` map from `path`, or an empty map when the file does
/// not exist. Any other top-level keys are ignored.
///
/// # Errors
///
/// Returns an error when a present file cannot be read or does not parse as
/// JSON with an object-valued `mcpServers` key.
fn read_mcp_servers(
    path: &std::path::Path,
) -> anyhow::Result<serde_json::Map<String, serde_json::Value>> {
    if !path.exists() {
        return Ok(serde_json::Map::new());
    }
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let value: serde_json::Value = serde_json::from_str(&contents)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    Ok(value
        .get("mcpServers")
        .and_then(serde_json::Value::as_object)
        .cloned()
        .unwrap_or_default())
}

/// Write `servers` into `path` as `{"mcpServers": { … }}`, creating parent
/// directories as needed.
///
/// # Errors
///
/// Returns an error when the parent cannot be created or the file cannot be
/// written.
fn write_mcp_servers(
    path: &std::path::Path,
    servers: &serde_json::Map<String, serde_json::Value>,
) -> anyhow::Result<()> {
    let mut root = serde_json::Map::new();
    root.insert(
        "mcpServers".to_string(),
        serde_json::Value::Object(servers.clone()),
    );
    let text = serde_json::to_string_pretty(&serde_json::Value::Object(root))
        .context("failed to serialize MCP server config")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    std::fs::write(path, format!("{text}\n"))
        .with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

/// Add (or, with `force`, overwrite) a stdio server entry in the user config
/// at `path`. The testable core of `mcp add`.
///
/// # Errors
///
/// Returns an error when `slug` already exists without `force`, or when the
/// file cannot be read or written.
fn mcp_add_to(
    path: &std::path::Path,
    slug: &str,
    command: &str,
    args: &[String],
    force: bool,
) -> anyhow::Result<()> {
    let mut servers = read_mcp_servers(path)?;
    if servers.contains_key(slug) && !force {
        anyhow::bail!(
            "MCP server {slug:?} already exists in {}; pass --force to overwrite",
            path.display()
        );
    }
    let mut entry = serde_json::Map::new();
    entry.insert(
        "command".to_string(),
        serde_json::Value::String(command.to_string()),
    );
    entry.insert(
        "args".to_string(),
        serde_json::Value::Array(
            args.iter()
                .map(|a| serde_json::Value::String(a.clone()))
                .collect(),
        ),
    );
    servers.insert(slug.to_string(), serde_json::Value::Object(entry));
    write_mcp_servers(path, &servers)?;
    Ok(())
}

/// Remove a server entry from the user config at `path`. Returns whether an
/// entry was present (and therefore removed). The testable core of `mcp
/// remove`.
///
/// # Errors
///
/// Returns an error when the file cannot be read or written.
fn mcp_remove_from(path: &std::path::Path, slug: &str) -> anyhow::Result<bool> {
    let mut servers = read_mcp_servers(path)?;
    let removed = servers.remove(slug).is_some();
    if removed {
        write_mcp_servers(path, &servers)?;
    }
    Ok(removed)
}

/// Describe one raw MCP server entry's transport label and target from its
/// config keys, mirroring the daemon's inference (explicit `transport` wins;
/// otherwise `command` ⇒ stdio, `url` ⇒ http).
fn describe_mcp_entry(entry: &serde_json::Value) -> (&'static str, String) {
    let command = entry.get("command").and_then(serde_json::Value::as_str);
    let url = entry.get("url").and_then(serde_json::Value::as_str);
    match entry.get("transport").and_then(serde_json::Value::as_str) {
        Some("http") => ("http", url.unwrap_or("").to_string()),
        Some("stdio") => ("stdio", command.unwrap_or("").to_string()),
        _ => match (command, url) {
            (Some(cmd), None) => ("stdio", cmd.to_string()),
            (None, Some(u)) => ("http", u.to_string()),
            (Some(cmd), Some(_)) => ("stdio", cmd.to_string()),
            (None, None) => ("?", String::new()),
        },
    }
}

/// Print the configured MCP servers (project entries override user entries by
/// slug). Offline: no daemon connection, so tool counts are unknown.
///
/// # Errors
///
/// Returns an error when a present config file cannot be read or parsed.
fn print_mcp_list() -> anyhow::Result<()> {
    let user = user_mcp_config_path()?;
    let mut servers = read_mcp_servers(&user)?;
    if let Some(project) = project_mcp_config_path() {
        for (slug, entry) in read_mcp_servers(&project)? {
            servers.insert(slug, entry);
        }
    }
    if servers.is_empty() {
        println!("no MCP servers configured");
        return Ok(());
    }
    let mut slugs: Vec<&String> = servers.keys().collect();
    slugs.sort();
    println!(
        "{:<20} {:<8} {:<40} {:<8} TOOLS",
        "SLUG", "TRANSPORT", "TARGET", "ENABLED"
    );
    for slug in slugs {
        // `slug` came from `slugs` (a key of `servers`), so the lookup is total.
        let Some(entry) = servers.get(slug) else {
            continue;
        };
        let (transport, target) = describe_mcp_entry(entry);
        let enabled = entry
            .get("enabled")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true);
        println!("{slug:<20} {transport:<8} {target:<40} {enabled:<8} unknown (offline)");
    }
    Ok(())
}

/// Send one MCP control request to a running daemon over its local Unix socket
/// and drive the reply loop.
///
/// A minimal one-shot client: the daemon's Unix transport takes framed
/// `ClientMessage`s directly (no Noise handshake — that applies only to TCP),
/// so this writes the request and reads messages until `handle` recognises the
/// reply and returns its outcome. Any message `handle` does not recognise (a
/// status reply for another action, unrelated broadcast traffic) is skipped, so
/// only the answer to *this* request ends the loop. `hint` is appended to the
/// "is a daemon running?" error so each caller can name the matching
/// connected-client escape hatch.
///
/// # Errors
///
/// Returns an error when the socket cannot be dialed, the request cannot be
/// written, or the stream ends with no matching reply. The reply's own outcome
/// (a failure reply) is returned by `handle`.
fn mcp_socket_request(
    request: &choreo_proto::ClientMessage,
    hint: &str,
    handle: impl Fn(choreo_proto::DaemonMessage) -> Option<anyhow::Result<()>>,
) -> anyhow::Result<()> {
    use choreo_proto::DaemonMessage;
    use std::io::{BufReader, BufWriter, Write};

    let path = choreo_proto::socket_path();
    let stream = choreo_proto::connect_unix(&path).with_context(|| {
        format!("could not connect to a daemon at {path} (is one running?); {hint}")
    })?;
    let mut writer = BufWriter::new(stream.try_clone().context("failed to clone socket")?);
    let mut reader = BufReader::new(stream);

    choreo_proto::write_message(&mut writer, request).context("failed to send request")?;
    writer.flush().context("failed to flush socket")?;

    loop {
        match choreo_proto::read_message::<_, DaemonMessage>(&mut reader) {
            Ok(msg) => {
                if let Some(outcome) = handle(msg) {
                    return outcome;
                }
                // Any other message is unrelated broadcast traffic; keep
                // reading for the reply that answers our request.
            }
            Err(e) => {
                anyhow::bail!("daemon disconnected before replying: {e}");
            }
        }
    }
}

/// Reconnect one MCP server on a running daemon over its local Unix socket.
///
/// On success the daemon answers with a refreshed `McpStatus` list; on failure
/// with `McpReconnectFailed`.
///
/// # Errors
///
/// Propagates the socket error, or the daemon's reconnect failure.
fn mcp_reconnect_via_socket(slug: &str) -> anyhow::Result<()> {
    use choreo_proto::{ClientMessage, ClientMessageType, DaemonMessageType};
    let request = ClientMessage::request(
        0,
        ClientMessageType::McpReconnect {
            slug: slug.to_string(),
        },
    );
    mcp_socket_request(
        &request,
        &format!("use `/mcp reconnect {slug}` in a connected client instead"),
        |msg| match msg.inner {
            DaemonMessageType::McpStatus { servers, .. } => {
                for server in &servers {
                    println!("[{}] {}", server.tier, server.summary());
                }
                Some(Ok(()))
            }
            DaemonMessageType::McpReconnectFailed { error, .. } => {
                Some(Err(anyhow::anyhow!("reconnect failed: {error}")))
            }
            _ => None,
        },
    )
}

/// Reload the MCP configuration on a running daemon over its local Unix socket.
///
/// On success the daemon answers with a reload summary plus the refreshed
/// status list; on a config read/parse failure with `McpReloadFailed`.
///
/// # Errors
///
/// Propagates the socket error, or the daemon's reload failure.
fn mcp_reload_via_socket() -> anyhow::Result<()> {
    use choreo_proto::{ClientMessage, ClientMessageType, DaemonMessageType};
    mcp_socket_request(
        &ClientMessage::request(0, ClientMessageType::McpReload),
        "use `/mcp reload` in a connected client instead",
        |msg| match msg.inner {
            DaemonMessageType::McpReloaded { summary, servers } => {
                println!("{summary}");
                for server in &servers {
                    println!("{}", server.summary());
                }
                Some(Ok(()))
            }
            DaemonMessageType::McpReloadFailed { error } => {
                Some(Err(anyhow::anyhow!("reload failed: {error}")))
            }
            _ => None,
        },
    )
}

/// Dispatch an `mcp` subcommand to its implementation (see `McpCliCommand`).
///
/// `list`/`add`/`remove` are offline file operations on the user config file;
/// `reconnect`/`reload` connect to a running daemon over its local socket.
///
/// # Errors
///
/// Propagates the failure of the chosen operation.
pub(super) fn run_mcp_cli(command: &McpCliCommand) -> anyhow::Result<()> {
    match command {
        McpCliCommand::List => print_mcp_list(),
        McpCliCommand::Add {
            slug,
            command,
            args,
            force,
        } => {
            let path = user_mcp_config_path()?;
            mcp_add_to(&path, slug, command, args, *force)?;
            println!("added MCP server {slug:?} to {}", path.display());
            Ok(())
        }
        McpCliCommand::Remove { slug } => {
            let path = user_mcp_config_path()?;
            if mcp_remove_from(&path, slug)? {
                println!("removed MCP server {slug:?} from {}", path.display());
                Ok(())
            } else {
                anyhow::bail!("no MCP server {slug:?} in {}", path.display())
            }
        }
        McpCliCommand::Reconnect { slug } => mcp_reconnect_via_socket(slug),
        McpCliCommand::Reload => mcp_reload_via_socket(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_add_creates_and_preserves_shape() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp.json");

        mcp_add_to(
            &path,
            "docs",
            "npx",
            &["-y".to_string(), "@scope/server".to_string()],
            false,
        )
        .unwrap();

        let servers = read_mcp_servers(&path).unwrap();
        assert_eq!(servers.len(), 1);
        let entry = &servers["docs"];
        assert_eq!(entry.get("command").and_then(|v| v.as_str()), Some("npx"));
        assert_eq!(
            entry.get("args").and_then(|v| v.as_array()).map(Vec::len),
            Some(2)
        );
        // The on-disk shape keeps the `mcpServers` wrapper the daemon loads.
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("\"mcpServers\""));
    }

    #[test]
    fn mcp_add_refuses_existing_without_force() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp.json");

        mcp_add_to(&path, "docs", "first", &[], false).unwrap();
        // A second add without --force is refused, leaving the original.
        assert!(mcp_add_to(&path, "docs", "second", &[], false).is_err());
        let servers = read_mcp_servers(&path).unwrap();
        assert_eq!(
            servers["docs"].get("command").and_then(|v| v.as_str()),
            Some("first")
        );

        // With --force the entry is replaced wholesale.
        mcp_add_to(&path, "docs", "second", &[], true).unwrap();
        let servers = read_mcp_servers(&path).unwrap();
        assert_eq!(
            servers["docs"].get("command").and_then(|v| v.as_str()),
            Some("second")
        );
    }

    #[test]
    fn mcp_remove_reports_presence() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp.json");

        mcp_add_to(&path, "docs", "npx", &[], false).unwrap();
        assert!(mcp_remove_from(&path, "docs").unwrap());
        assert!(!mcp_remove_from(&path, "docs").unwrap());
        assert!(read_mcp_servers(&path).unwrap().is_empty());
    }

    #[test]
    fn mcp_read_absent_file_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing.json");
        assert!(read_mcp_servers(&path).unwrap().is_empty());
    }

    #[test]
    fn describe_mcp_entry_infers_and_honours_override() {
        let stdio = serde_json::json!({"command": "npx", "args": []});
        assert_eq!(describe_mcp_entry(&stdio), ("stdio", "npx".to_string()));

        let http = serde_json::json!({"url": "https://example.com/mcp"});
        assert_eq!(
            describe_mcp_entry(&http),
            ("http", "https://example.com/mcp".to_string())
        );

        // An explicit transport wins over the key inference.
        let forced = serde_json::json!({
            "command": "ignored",
            "url": "https://example.com/mcp",
            "transport": "http"
        });
        assert_eq!(
            describe_mcp_entry(&forced),
            ("http", "https://example.com/mcp".to_string())
        );
    }
}
