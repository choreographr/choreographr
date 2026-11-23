//! The `choreographr` CLI: argument parsing and the process entry point.
//!
//! [`main`] is the binary's entry point — it parses the command line, installs
//! logging, opens the daemon state, and then either runs the server (the
//! default, no-subcommand case) or dispatches one of the offline utility
//! subcommands (`acl-add`, `pubkey`, `migrate`, `mcp`). The serve flags
//! (base-dir, listeners, log destination, auto-exit) live on the parent command
//! so `choreographr --tcp-addr …` keeps working without a subcommand.
//!
//! Every utility subcommand either prints ONE machine-readable line on stdout
//! (the value) or nothing at all; diagnostics go to stderr through `tracing`, so
//! a shell script can consume stdout directly.
//!
//! The utility implementations live in sibling modules by concern — `acl` for
//! `acl-add`, `mcp` for the `mcp` group, and `util` for `pubkey` and
//! `migrate` — so this module stays the argument surface and the serve path.

mod acl;
mod mcp;
mod util;

use crate::config::{DaemonConfig, load_daemon_config};
use crate::daemon::DaemonState;
use anyhow::Context;
use choreo_proto::socket_path;
use choreo_shared::clap_styles;
use choreo_shared::logging::{ConsoleSink, LogOptions, Verbosity};
use choreo_transport::key::{ensure_transport_keypair, pubkey_base64};
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
        /// Base64 of the client's 32-byte transport public key — the client
        /// machine prints it with `choreographr pubkey`.
        pubkey: String,
    },
    /// Print this machine's transport public key as base64: one line on stdout,
    /// nothing else. Add it to a daemon's `authorized_clients.toml` (via
    /// `acl-add`) to authorize this client, or compare it against the entry a
    /// `known_servers.toml` pin holds. Read-only: with no identity in the config
    /// dir it reports that instead of generating one.
    Pubkey,
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

const DEFAULT_MAX_TURNS: u32 = 0;

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
            let count = acl::acl_add_to(&path, pubkey)?;
            println!("client key authorized ({count} client(s) now trusted)");
            return Ok(());
        }
        Some(Command::Pubkey) => {
            // Exactly one line on stdout — the value, nothing else — so a shell
            // script can capture it without filtering. Diagnostics travel on
            // stderr through tracing.
            println!("{}", util::pubkey_value()?);
            return Ok(());
        }
        Some(Command::Migrate {
            move_,
            dry_run,
            force,
        }) => return util::run_migrate(*move_, *dry_run, *force),
        Some(Command::Mcp { command }) => {
            return mcp::run_mcp_cli(command);
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
    let (transport_sk, transport_pk) =
        ensure_transport_keypair().context("failed to load/generate transport keypair")?;
    // The public half is not secret, and operators need it to enroll a client
    // (`acl-add`) and to check a client's `known_servers.toml` pin — so it is
    // logged ONCE here, at startup, rather than per connection. The secret never
    // reaches a log line.
    info!(
        pubkey = %pubkey_base64(&transport_pk),
        "transport public key"
    );

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
        // The CLI binds a fixed, operator-chosen port, so it never needs the
        // bound address reported back.
        None,
    )
    .context("failed to run server")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_max_turns_env_accepts_positive() {
        assert_eq!(parse_max_turns_env("42").unwrap(), 42);
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
