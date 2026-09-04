use crate::accounts::{AccountManager, accounts_config_path};
use crate::config::load_daemon_config;
use crate::daemon::DaemonState;
use crate::db::read_all_sessions;
use anyhow::Context;
use choreo_proto::socket_path;
use choreo_transport::key::ensure_transport_keypair;
use clap::Parser;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::mpsc;
use tracing::{info, warn};
use tracing_subscriber::{EnvFilter, fmt};

/// Shared clap [`Styles`] for this crate's CLI binary.
///
/// Each CLI crate keeps its own copy (choreo-proto is the wire protocol and
/// must not host CLI styling); if this ever grows, promote it to a dedicated
/// micro-crate instead of putting it in choreo-proto.
///
/// Uses real ANSI hues (green headers/usage, cyan literals/placeholders) rather
/// than bold/underline only, so help output stays legible even in terminals whose
/// bold text isn't visually distinct (e.g. themes that don't remap the bold color).
/// `Styles::styled()` keeps clap's default error/invalid/valid coloring; the
/// overrides colorize the help elements.
fn clap_styles() -> clap::builder::Styles {
    use clap::builder::styling::{AnsiColor, Effects, Styles};
    Styles::styled()
        .header(AnsiColor::Green.on_default() | Effects::BOLD)
        .usage(AnsiColor::Green.on_default() | Effects::BOLD)
        .literal(AnsiColor::Cyan.on_default() | Effects::BOLD)
        .placeholder(AnsiColor::Cyan.on_default())
}

#[derive(Parser)]
// Bare `version` wires `--version`/`-V` to CARGO_PKG_VERSION (the crate
// version), which the Homebrew formula test, installer, and smoke tests rely on.
// `color` is explicitly `Auto` (clap's default) to document the intent that
// help/error output is colored only when stdout/stderr is a TTY.
#[command(
    name = "choreographr",
    version,
    about = "Choreographr AI daemon",
    color = clap::ColorChoice::Auto,
    styles = clap_styles()
)]
struct Cli {
    /// Increase logging verbosity (-v debug, -vv trace)
    #[arg(short = 'v', long = "verbose", action = clap::ArgAction::Count)]
    verbose: u8,

    /// Decrease logging verbosity (only errors and warnings)
    #[arg(short = 'q', long = "quiet", action = clap::ArgAction::Count)]
    quiet: u8,

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
    let fp = match path {
        Some(p) => fingerprint_of_file(std::path::Path::new(p))
            .with_context(|| format!("failed to fingerprint key file {p}"))?,
        None => {
            let pk = read_server_pk(None).context(
                "failed to read this machine's transport public key (has the daemon ever run here?)",
            )?;
            fingerprint(&pk)
        }
    };
    println!("{fp}");
    Ok(())
}

const DEFAULT_MAX_TURNS: u32 = 0;

/// Resolve the tool-loop iteration limit.
///
/// Resolution chain: `CHOREOGRAPHR_MAX_TURNS` env var → `config.toml` → default 0 (unlimited).
/// A value of `0` means *unlimited* — the agent loop will run until the
/// model produces a final answer, is cancelled, or hits an error.
///
/// A `CHOREOGRAPHR_MAX_TURNS` that is set but not a valid `u32` is a
/// configuration error: failing startup beats silently running unbounded.
fn resolve_max_turns() -> anyhow::Result<u32> {
    match std::env::var("CHOREOGRAPHR_MAX_TURNS") {
        Ok(val) => return parse_max_turns_env(&val),
        Err(std::env::VarError::NotPresent) => {}
        Err(e) => {
            return Err(anyhow::anyhow!(
                "failed to read CHOREOGRAPHR_MAX_TURNS: {e}"
            ));
        }
    }
    if let Ok(config) = load_daemon_config()
        && let Some(n) = config.max_turns
    {
        return Ok(n);
    }
    Ok(DEFAULT_MAX_TURNS)
}

/// Parse the `CHOREOGRAPHR_MAX_TURNS` value. Kept as a pure function so the
/// parsing behavior is unit-testable without touching process-global env.
fn parse_max_turns_env(val: &str) -> anyhow::Result<u32> {
    val.parse::<u32>()
        .map_err(|e| anyhow::anyhow!("CHOREOGRAPHR_MAX_TURNS={val:?} is not a valid u32: {e}"))
}

pub fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // Determine log level: RUST_LOG env var takes precedence, otherwise use CLI flags
    let log_level = if std::env::var("RUST_LOG").is_ok() {
        if cli.verbose > 0 || cli.quiet > 0 {
            warn!("RUST_LOG is set; -v/-q CLI flags are ignored");
        }
        None // Use RUST_LOG as-is
    } else {
        let level = match (cli.verbose, cli.quiet) {
            (0, 0) => "info",
            (_, q) if q > 0 => "warn",
            (1, 0) => "debug",
            _ => "trace",
        };
        Some(level)
    };

    let env_filter = match log_level {
        Some(level) => EnvFilter::new(level),
        None => EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
    };

    fmt().with_env_filter(env_filter).init();

    info!(effective_level = ?log_level.unwrap_or("from RUST_LOG"), "logging initialized");

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
        None => {}
    }

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
            warn!(
                error = %e,
                "failed to initialize the coordination platform tokio runtime; \
                 content write tools will be unavailable"
            );
        }
    }

    let max_turns = resolve_max_turns().context("failed to resolve tool-loop iteration limit")?;
    info!(max_turns, "tool loop iteration limit");
    info!("choreographr starting (locked)");

    let db = Arc::new(crate::db::open_db().context("failed to open database")?);

    // Bring the database up to the current schema version before any table
    // access: open_db already stamped a fresh database at creation (0 → 1
    // initialization); run_migrations applies any pending migrations from
    // there up to SCHEMA_VERSION (a no-op today), and refuses a
    // newer-version database before any code can read or write it.
    crate::db::run_migrations(&db).context("failed to migrate database")?;

    // Purge any sessions that were deleted while their still-shutting-down
    // thread was alive and re-created the record before the daemon crashed:
    // without this, a deleted session could reappear after a restart.  Runs
    // before the session index is loaded so the record never surfaces.
    match crate::db::purge_tombstoned_sessions(&db) {
        Ok(n) if n > 0 => warn!(
            purged = n,
            "purged records left behind by interrupted session deletions"
        ),
        Ok(_) => {}
        Err(e) => warn!(error = %e, "failed to purge tombstoned sessions; continuing"),
    }

    let (daemon_tx, _daemon_rx) = mpsc::channel::<crate::daemon::DaemonCommand>();

    let mut session_metadata = std::collections::HashMap::new();
    match read_all_sessions(&db) {
        Ok(sessions) => {
            for (id, record) in sessions {
                session_metadata.insert(id, record.into());
            }
        }
        Err(e) => {
            warn!("failed to load sessions from database: {e}");
        }
    }

    info!(
        count = session_metadata.len(),
        "loaded sessions from database"
    );

    // Load accounts (may be empty — unlock will reload them)
    // If the config path or file is unavailable, start with an empty manager.
    let accounts = match accounts_config_path() {
        Ok(path) => AccountManager::load(&path).unwrap_or_else(|e| {
            warn!("failed to load accounts: {e}");
            AccountManager::empty()
        }),
        Err(e) => {
            warn!("no accounts config path: {e}");
            AccountManager::empty()
        }
    };

    // Build the tool registry and initialize MCP servers before wrapping
    // in Arc (McpManager needs &mut ToolRegistry to register dynamic tools).
    let mut tool_registry = crate::tools::ToolRegistry::new();
    let mcp_manager = crate::mcp::McpManager::from_config(&mut tool_registry);
    let tool_registry = tool_registry.build();

    let state = DaemonState {
        daemon_tx,
        // Derive the next session ID from the highest existing record so a
        // fresh daemon never collides with a persisted session.
        next_session_id: session_metadata
            .keys()
            .max()
            .copied()
            .map(|m| m + 1)
            .unwrap_or(1),
        max_turns,
        active_sessions: std::collections::HashMap::new(),
        session_metadata,
        deleted_sessions: std::collections::HashSet::new(),
        children: std::collections::HashMap::new(),
        accounts,
        providers: HashMap::new(),
        credentials: std::collections::HashMap::new(),
        x_credentials: None,
        // The daemon starts locked: credentials are only decrypted into memory
        // once a client presents the valid unlock key.
        locked: true,
        db,
        tool_registry,
        summary_subscribers: std::collections::HashMap::new(),
        client_writers: HashMap::new(),
        activity_subscribers: std::collections::HashMap::new(),
        client_subscribed_sessions: std::collections::HashMap::new(),
        // One daemon-wide lag counter shared by every connection's sink and
        // every session thread (see `broadcast::SubscriberSink`). The accept
        // path clones it so `register_client_writer` can hand it to the
        // connection threads' writers.
        global_lag: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        lag_limits: crate::broadcast::LagLimits::default(),
        model_cache: HashMap::new(),
        model_prefetch_in_flight: HashSet::new(),
        mcp_manager,
        // Populated by `run_server`, which spawns the maintenance thread
        // (it needs the real command-loop channel, created there).
        maintenance_tx: None,
        // Installed by `run_server` from the `acl` parameter (the command
        // loop needs the same Arc the accept paths read).
        acl: None,
        catalog_paths: crate::catalog::CatalogPaths::from_dirs(),
    };

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
        cli.metrics_addr,
        cli.tcp_addr,
        transport_sk,
        acl,
    )
    .context("failed to run server")
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("authorized_clients.toml");
        assert!(acl_add_to(&path, "not-base64!!!").is_err());
        // Valid base64, wrong decoded length.
        use base64::Engine as _;
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
    fn parse_max_turns_env_accepts_positive() {
        assert_eq!(parse_max_turns_env("42").unwrap(), 42);
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
    /// Assert both so the flag stays wired to CARGO_PKG_VERSION (it breaks
    /// silently if the derive attribute loses the bare `version` marker).
    #[test]
    fn version_flag_displays_package_version() {
        // clap returns the version as a `DisplayVersion` error instead of a
        // value; match it out by hand (Cli doesn't derive Debug, so
        // `unwrap_err()`'s Debug bound doesn't apply).
        let err = match Cli::try_parse_from(["choreographr", "--version"]) {
            Err(e) => e,
            Ok(_) => panic!("--version should short-circuit before arg validation"),
        };
        assert_eq!(err.kind(), clap::error::ErrorKind::DisplayVersion);
        assert!(err.to_string().contains(env!("CARGO_PKG_VERSION")));
    }
}
