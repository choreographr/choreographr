//! The `choreographr` CLI: argument parsing and the process entry point.
//!
//! [`main`] is the binary's entry point — it parses the command line, installs
//! logging, opens the daemon state, and then either runs the server (the
//! default, no-subcommand case) or dispatches one of the offline utility
//! subcommands (`acl-add`, `fingerprint`, `migrate`, `mcp`). The serve flags
//! (base-dir, listeners, log destination, auto-exit) live on the parent command
//! so `choreographr --tcp-addr …` keeps working without a subcommand.

use crate::config::{DaemonConfig, load_daemon_config};
use crate::daemon::DaemonState;
use anyhow::Context;
use choreo_proto::socket_path;
use choreo_shared::clap_styles;
use choreo_shared::logging::{ConsoleSink, LogOptions, Verbosity};
use choreo_transport::key::ensure_transport_keypair;
use clap::Parser;
use std::path::PathBuf;
use tracing::info;

#[derive(Parser)]
// `--version`/`-V` reports this crate's CARGO_PKG_VERSION (which the Homebrew
// formula test, installer, and smoke tests rely on) with the release name
// appended via `choreo_shared::release_name` — e.g. `0.2.0 (Lindy)`, or the
// bare version when the name file is empty. See `choreo-shared/release-name.txt`.
// `color` is explicitly `Auto` (clap's default) to document the intent that
// help/error output is colored only when stdout/stderr is a TTY.
#[command(
    name = "choreographr",
    version = choreo_shared::release_name::version_string(env!("CARGO_PKG_VERSION")),
    about = "Choreographr AI daemon",
    color = clap::ColorChoice::Auto,
    styles = clap_styles()
)]
struct Cli {
    // Increase logging verbosity (-v debug, -vv trace)
    #[command(flatten)]
    verbosity: Verbosity,

    /// Run the whole instance out of this base directory instead of the
    /// platform defaults: `{base}/config`, `{base}/data`, `{base}/run`
    /// (the socket), and `{base}/log`. Equivalent to exporting
    /// `CHOREOGRAPHR_BASE_DIR`. Global so it works before the CLI, the daemon
    /// (no subcommand), and the utility subcommands alike.
    #[arg(long = "base-dir", value_name = "PATH", global = true)]
    base_dir: Option<PathBuf>,

    /// Enable Prometheus metrics HTTP server on this socket address
    /// (e.g. 127.0.0.1:9464).  When absent no metrics server is started.
    ///
    /// Requires the `metrics` cargo feature (off by default; rebuild with
    /// `--features metrics`).  When the binary is built without it, passing
    /// this flag is a startup error rather than a silent no-op.
    #[arg(long = "metrics-addr")]
    metrics_addr: Option<String>,

    /// Enable TCP Noise IK listener on this socket address
    /// (e.g. 0.0.0.0:9443).  When absent no TCP listener is started.
    #[arg(long = "tcp-addr")]
    tcp_addr: Option<String>,

    /// Write the daemon's log file to this path instead of the default
    /// (`{base}/log/daemon-<pid>.log`, else `$XDG_STATE_HOME/choreographr`,
    /// else the platform temp dir). Diagnostics are always ALSO mirrored to
    /// stderr (the console or journald) — this only chooses the file's path;
    /// it never affects RUST_LOG/-v/-q level selection and never mutes the
    /// console. A log file that cannot be created or opened degrades to stderr
    /// (with a warning) rather than preventing the daemon from starting.
    #[arg(long = "log-file")]
    log_file: Option<String>,

    /// Exit automatically when the last client disconnects (used by
    /// choreo-tui, which spawns a private daemon); without this flag the
    /// daemon runs until interrupted. A daemon with this flag that has never
    /// had a client still runs forever — there is no idle timeout.
    #[arg(long = "auto-exit")]
    auto_exit: bool,

    /// Optional utility subcommand. When absent (the overwhelmingly common
    /// case) the daemon runs — `choreographr --tcp-addr 0.0.0.0:9443` keeps
    /// working unchanged because the serve flags stay on the parent command.
    #[command(subcommand)]
    command: Option<Command>,
}

/// Utility subcommands. The daemon itself is the default (no subcommand).
#[derive(clap::Subcommand)]
enum Command {
    /// Enroll a client key in the ACL: appends a `[[client]]` entry to
    /// `authorized_clients.toml` under the advisory file lock; a running
    /// daemon hot-reloads it, so the client can connect immediately without
    /// a restart. Works while the daemon is locked and needs no socket.
    AclAdd {
        /// Base64 of the client's 32-byte transport public key (the client
        /// prints it with `choreo-tui`'s help or reads its transport.pub)
        pubkey: String,
    },
    /// Print the human-comparable fingerprint of a transport public key —
    /// with no argument, this machine's own (read out to the client operator
    /// during enrollment); with a path, any key file (verify a copied
    /// server key or a pinned known-servers entry).
    Fingerprint {
        /// Path to a 32-byte raw transport public key file
        #[arg(default_value = None)]
        path: Option<String>,
    },
    /// Move an existing platform-default install (config + data) into a base
    /// dir, preserving the instance identity. Requires `--base-dir <PATH>`.
    Migrate {
        /// Move instead of copy (the default copy leaves the old layout in
        /// place, so the migration is reversible).
        #[arg(long = "move")]
        move_: bool,
        /// Report what would be transferred without touching the filesystem.
        #[arg(long = "dry-run")]
        dry_run: bool,
        /// Merge into a non-empty destination instead of refusing.
        #[arg(long = "force")]
        force: bool,
    },
    /// Manage MCP (Model Context Protocol) servers: list the configured set,
    /// add or remove a server in the user config file, reconnect a running
    /// daemon's server, or reload its configuration over its local socket.
    Mcp {
        #[command(subcommand)]
        command: McpCliCommand,
    },
}

/// The `mcp` subcommand group.
#[derive(clap::Subcommand)]
enum McpCliCommand {
    /// List the configured MCP servers (user + project layers). Offline — no
    /// daemon connection; tool counts are unknown without one.
    List,
    /// Add a server to the daemon-tier config file (`mcp.json`).
    Add {
        /// The server's slug (its config key and tool-name prefix).
        slug: String,
        /// The executable to launch (stdio transport).
        #[arg(long = "command", value_name = "CMD")]
        command: String,
        /// Arguments for the command, space-separated after `--args`.
        #[arg(long = "args", value_name = "ARG", num_args = 1.., allow_hyphen_values = true)]
        args: Vec<String>,
        /// Overwrite an existing entry for the same slug.
        #[arg(long = "force")]
        force: bool,
    },
    /// Remove a server from the user config file.
    Remove {
        /// The server's slug.
        slug: String,
    },
    /// Reconnect one MCP server on a running daemon over its local socket.
    Reconnect {
        /// The server's slug.
        slug: String,
    },
    /// Reload the MCP configuration on a running daemon over its local socket:
    /// re-read the daemon-tier `mcp.json` and the active session's project
    /// `.mcp.json`, connect added servers, disconnect removed ones, and
    /// reconnect changed ones — no restart.
    Reload,
}

/// Enroll `pubkey_b64` into the ACL file at `path`. The testable core of the
/// `acl-add` subcommand: the CLI resolves the path from the standard config
/// dir and delegates here.
fn acl_add_to(path: &std::path::Path, pubkey_b64: &str) -> anyhow::Result<usize> {
    use base64::Engine as _;
    let key: [u8; 32] = base64::engine::general_purpose::STANDARD
        .decode(pubkey_b64.trim())
        .map_err(|e| anyhow::anyhow!("invalid pubkey: not valid base64: {e}"))?
        .try_into()
        .map_err(|_| anyhow::anyhow!("invalid pubkey: must decode to exactly 32 bytes"))?;

    // Idempotency: an already-present key is a no-op (no duplicate entry).
    let existing = crate::server::acl::Acl::load(path);
    if existing.contains(&key) {
        info!("pubkey is already authorized; nothing to do");
        return Ok(existing.len());
    }

    // The advisory file lock makes this safe against a concurrent daemon
    // /acl add; the daemon's watcher + parse-compare reload makes the new
    // entry live without a restart.
    crate::server::acl::append_key_locked(path, &key).map_err(|e| anyhow::anyhow!(e))?;
    let count = crate::server::acl::Acl::load(path).len();
    info!(clients = count, "ACL: client key enrolled");
    Ok(count)
}

/// Print the fingerprint of a transport public key (see `Command::Fingerprint`).
fn fingerprint_cli(path: Option<&str>) -> anyhow::Result<()> {
    use choreo_transport::key::{fingerprint, fingerprint_of_file, read_server_pk};
    let fp = if let Some(p) = path {
        fingerprint_of_file(std::path::Path::new(p))
            .with_context(|| format!("failed to fingerprint key file {p}"))?
    } else {
        let pk = read_server_pk(None).context(
            "failed to read this machine's transport public key (has the daemon ever run here?)",
        )?;
        fingerprint(&pk)
    };
    println!("{fp}");
    Ok(())
}

// ── `mcp` subcommand ─────────────────────────────────────────────────
//
// The `mcp list/add/remove` operations read and write the same
// `{"mcpServers": { … }}` file the daemon's `mcp` module loads
// (`mcp.json`), but do so DIRECTLY rather than through that module:
// the module is compiled only behind the daemon's `mcp` cargo feature (on by
// default; an embedder opts out with `default-features = false`), while this
// CLI group is always available. The path resolution and
// file shape are kept identical so a server added here is picked up by a
// feature-enabled daemon unchanged.

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

const DEFAULT_MAX_TURNS: u32 = 0;

/// Dispatch an `mcp` subcommand to its implementation (see `McpCliCommand`).
///
/// `list`/`add`/`remove` are offline file operations on the user config file;
/// `reconnect`/`reload` connect to a running daemon over its local socket.
///
/// # Errors
///
/// Propagates the failure of the chosen operation.
fn run_mcp_cli(command: &McpCliCommand) -> anyhow::Result<()> {
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

/// Resolve the tool-loop iteration limit.
///
/// Resolution chain: `CHOREOGRAPHR_MAX_TURNS` env var → `config.toml` → default 0 (unlimited).
/// A value of `0` means *unlimited* — the agent loop will run until the
/// model produces a final answer, is cancelled, or hits an error.
///
/// The already-loaded `[config.toml]` is passed in (the CLI loads it once and
/// also reads its `[cache_warming]` table) rather than re-reading the file here.
///
/// A `CHOREOGRAPHR_MAX_TURNS` that is set but not a valid `u32` is a
/// configuration error: failing startup beats silently running unbounded.
fn resolve_max_turns(config: &DaemonConfig) -> anyhow::Result<u32> {
    match std::env::var("CHOREOGRAPHR_MAX_TURNS") {
        Ok(val) => return parse_max_turns_env(&val),
        Err(std::env::VarError::NotPresent) => {}
        Err(e) => {
            return Err(anyhow::anyhow!(
                "failed to read CHOREOGRAPHR_MAX_TURNS: {e}"
            ));
        }
    }
    if let Some(n) = config.max_turns {
        return Ok(n);
    }
    Ok(DEFAULT_MAX_TURNS)
}

/// Parse the `CHOREOGRAPHR_MAX_TURNS` value. Kept as a pure function so the
/// parsing behavior is unit-testable without touching process-global env.
///
/// # Errors
///
/// Returns Err if the value is not a valid `u32`.
fn parse_max_turns_env(val: &str) -> anyhow::Result<u32> {
    val.parse::<u32>()
        .map_err(|e| anyhow::anyhow!("CHOREOGRAPHR_MAX_TURNS={val:?} is not a valid u32: {e}"))
}

/// Daemon entry point: parse CLI args, initialize logging, and run the
/// daemon.
///
/// # Errors
///
/// Returns Err if argument parsing, config loading, daemon startup, or the
/// run loop fails; the error is reported to stderr before exit.
pub fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // Apply the `--base-dir` override FIRST: it must be in the environment
    // before any path (the log file, config.toml, the DB, the keystore, the
    // socket) is resolved. Single-threaded here, so the contained `set_var` is
    // sound.
    choreo_shared::paths::set_base_dir_from_cli(cli.base_dir.clone());

    // Logging init happens HERE — before any subcommand/state work — because
    // everything after it wants to log. The shared initializer writes a
    // hardened, pid-keyed file (`{base}/log/daemon-<pid>.log` under a base,
    // else the XDG state dir, else the platform temp dir) AND mirrors every
    // event to stderr unconditionally, so a `--base-dir` (or any file sink)
    // never silences the console or the platform log (journald/launchd).
    // `--log-file` chooses the file's path only. A file that cannot be opened
    // is never fatal: the daemon degrades to the stderr sink (with a warning)
    // rather than refusing to start, so an unwritable log directory — a
    // read-only `$XDG_STATE_HOME`, a bare container — cannot take the daemon
    // down. Diagnostics are never a startup precondition.
    let _ = choreo_shared::logging::init(LogOptions {
        binary: "daemon",
        verbosity: cli.verbosity,
        log_file: cli.log_file.as_deref(),
        console: ConsoleSink::Stderr,
        with_target: true,
        extra_directives: &[],
    });

    // Utility subcommands exit early — they are one-shot file operations and
    // never touch the DB, providers, or listeners below.
    match &cli.command {
        Some(Command::AclAdd { pubkey }) => {
            let path = choreo_keystore::paths::authorized_clients_path()
                .context("failed to resolve authorized_clients path")?;
            let count = acl_add_to(&path, pubkey)?;
            println!("client key authorized ({count} client(s) now trusted)");
            return Ok(());
        }
        Some(Command::Fingerprint { path }) => return fingerprint_cli(path.as_deref()),
        Some(Command::Migrate {
            move_,
            dry_run,
            force,
        }) => {
            let base = choreo_shared::paths::base_dir()
                .context("migrate requires --base-dir <PATH> (or CHOREOGRAPHR_BASE_DIR)")?;
            return crate::migrate::run(&base, *move_, *dry_run, *force);
        }
        Some(Command::Mcp { command }) => {
            return run_mcp_cli(command);
        }
        None => {}
    }

    // Serve path only: a fresh empty base that shadows an existing install is
    // almost certainly a mistake (a silent identity reset), so warn loudly
    // before opening state.
    choreo_shared::paths::warn_if_base_shadows_legacy_install();

    // The blockchain tools (EVM/Substrate) run on a tokio sidecar runtime owned
    // by the `choreo-blockchain` crate. Initialize it once at startup when the
    // `blockchain` feature is enabled (off by default); without the feature the
    // tools — and tokio itself — are compiled out entirely.
    #[cfg(feature = "blockchain")]
    choreo_blockchain::runtime::init()
        .map_err(|e| anyhow::anyhow!("failed to initialize blockchain tokio runtime: {e}"))?;

    // The Choreographr Coordination Platform tools also run on a tokio sidecar
    // runtime (owned by the `choreo-content` crate) used only for signed chain
    // writes via subxt. Initialize it once at startup when the `content`
    // feature is enabled (off by default); without the feature the tools — and
    // the sidecar — are compiled out entirely. A failure is NOT fatal: read
    // tools and IPFS/indexer still work, while content write tools would be unavailable
    // until the sidecar can be built.
    #[cfg(feature = "content")]
    match choreo_content::init() {
        Ok(()) => {}
        Err(e) => {
            tracing::warn!(
                error = %e,
                "failed to initialize the coordination platform tokio runtime; \
                 content write tools will be unavailable"
            );
        }
    }

    // The state construction (DB open/migrate/backup dance, tombstone purge,
    // session index, accounts, tool registry, MCP) lives in
    // `DaemonState::open` so the embedded daemon can share the exact same
    // sequence. The CLI supplies the standard paths and the unrestricted tool
    // policy, so its behavior is unchanged.
    //
    // The daemon-level config.toml is loaded ONCE here: its `max_turns` feeds
    // the loop limit and its `[cache_warming]` table rides on `DaemonState` so
    // `spawn_session` resolves each session's warm policy without re-reading
    // the file. A read/parse error logs and falls back to defaults, matching
    // `load_daemon_config`'s tolerant contract elsewhere.
    let daemon_config = match load_daemon_config() {
        Ok(config) => config,
        Err(e) => {
            tracing::warn!(error = %e, "failed to load config.toml; using defaults");
            DaemonConfig::default()
        }
    };
    let max_turns =
        resolve_max_turns(&daemon_config).context("failed to resolve tool-loop iteration limit")?;
    info!(max_turns, "tool loop iteration limit");
    // The release name (choreo-shared/release-name.txt) is part of the startup
    // banner alongside the crate version, so logs identify the exact series.
    info!(
        version = %choreo_shared::release_name::version_string(env!("CARGO_PKG_VERSION")),
        "choreographr starting (locked)"
    );

    let state = DaemonState::open(crate::daemon::OpenOptions {
        db_path: crate::db::db_path().context("failed to resolve database path")?,
        accounts_path: crate::accounts::accounts_config_path()
            .context("failed to resolve accounts config path")?,
        catalog_paths: crate::catalog::CatalogPaths::from_dirs(),
        tool_policy: crate::tools::ToolPolicy::Full,
        max_turns,
        cache_warming: daemon_config.cache_warming,
        // Desktop CLI: no platform-native tool host exists in this process.
        platform_tool_bridge: None,
    })
    .context("failed to open daemon state")?;

    // Load or generate the transport keypair for Noise IK.
    let (transport_sk, _transport_pk) =
        ensure_transport_keypair().context("failed to load/generate transport keypair")?;

    // Load the ACL of authorized client public keys.
    let acl_path = choreo_keystore::paths::authorized_clients_path()
        .context("failed to resolve authorized_clients path")?;
    let acl = crate::server::acl::SharedAcl::load(&acl_path);

    let socket_path = socket_path();
    crate::run_server(
        &socket_path,
        state,
        cli.metrics_addr.as_ref(),
        cli.tcp_addr.as_ref(),
        transport_sk,
        &acl,
        cli.auto_exit,
    )
    .context("failed to run server")
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── mcp CLI core ─────────────────────────────────────────────────

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

    #[test]
    fn parse_max_turns_env_accepts_positive() {
        assert_eq!(parse_max_turns_env("42").unwrap(), 42);
    }

    // ── acl-add CLI core ──────────────────────────────────────────────

    const CLI_KEY_A: [u8; 32] = [1u8; 32];
    const CLI_KEY_B: [u8; 32] = [2u8; 32];

    fn cli_b64(key: &[u8; 32]) -> String {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD.encode(key)
    }

    /// The CLI's idempotent enroll: a fresh file gains the entry; a second
    /// identical add is a no-op returning the SAME count; a different key
    /// appends alongside. These pin the behavior an operator relies on when
    /// scripting `choreographr acl-add` against a live daemon.
    #[test]
    fn acl_add_to_appends_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("authorized_clients.toml");

        // First add: file is created (with its parent dir), count = 1.
        let count = acl_add_to(&path, &cli_b64(&CLI_KEY_A)).unwrap();
        assert_eq!(count, 1);
        assert!(
            crate::server::acl::Acl::load(&path).contains(&CLI_KEY_A),
            "the enrolled key must authorize"
        );

        // Idempotent re-add: no duplicate entry.
        assert_eq!(acl_add_to(&path, &cli_b64(&CLI_KEY_A)).unwrap(), 1);
        let file = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            file.matches("pubkey").count(),
            1,
            "re-adding must not duplicate the entry"
        );

        // A second key appends alongside.
        assert_eq!(acl_add_to(&path, &cli_b64(&CLI_KEY_B)).unwrap(), 2);
        assert!(crate::server::acl::Acl::load(&path).contains(&CLI_KEY_B));
    }

    #[test]
    fn acl_add_to_rejects_bad_keys() {
        use base64::Engine as _;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("authorized_clients.toml");
        assert!(acl_add_to(&path, "not-base64!!!").is_err());
        // Valid base64, wrong decoded length.
        let short = base64::engine::general_purpose::STANDARD.encode([9u8; 16]);
        assert!(acl_add_to(&path, &short).is_err());
        // Nothing was written for the rejected keys.
        assert!(!path.exists(), "a rejected add must not create the file");
    }

    #[test]
    fn parse_max_turns_env_accepts_zero() {
        assert_eq!(parse_max_turns_env("0").unwrap(), 0);
    }

    #[test]
    fn parse_max_turns_env_rejects_non_numeric() {
        assert!(parse_max_turns_env("abc").is_err());
    }

    #[test]
    fn parse_max_turns_env_rejects_negative() {
        assert!(parse_max_turns_env("-5").is_err());
    }

    #[test]
    fn parse_max_turns_env_rejects_empty() {
        assert!(parse_max_turns_env("").is_err());
    }

    /// `--version` is handled by clap before any real arg parsing: it exits
    /// with a `DisplayVersion` error whose message is the version string.
    /// Assert both so the flag stays wired to `CARGO_PKG_VERSION` (it breaks
    /// silently if the derive attribute loses the bare `version` marker).
    #[test]
    fn version_flag_displays_package_version() {
        // clap returns the version as a `DisplayVersion` error instead of a
        // value; match it out by hand (Cli doesn't derive Debug, so
        // `unwrap_err()`'s Debug bound doesn't apply).
        let Err(err) = Cli::try_parse_from(["choreographr", "--version"]) else {
            panic!("--version should short-circuit before arg validation")
        };
        assert_eq!(err.kind(), clap::error::ErrorKind::DisplayVersion);
        assert!(err.to_string().contains(env!("CARGO_PKG_VERSION")));
        // And the release name (from choreo-shared/release-name.txt) rides along.
        let expected = choreo_shared::release_name::version_string(env!("CARGO_PKG_VERSION"));
        assert!(err.to_string().contains(&expected));
    }
}
