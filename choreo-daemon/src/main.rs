use anyhow::Context;
use choreo_proto::socket_path;
use choreo_transport::key::ensure_transport_keypair;
use choreographr::accounts::{AccountManager, accounts_config_path};
use choreographr::daemon::DaemonState;
use choreographr::db::read_all_sessions;
use choreographr::openai::load_daemon_config;
use clap::Parser;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::mpsc;
use tracing::{info, warn};
use tracing_subscriber::{EnvFilter, fmt};

#[derive(Parser)]
#[command(name = "choreographr", about = "Choreographr AI daemon")]
struct Cli {
    /// Increase logging verbosity (-v debug, -vv trace)
    #[arg(short = 'v', long = "verbose", action = clap::ArgAction::Count)]
    verbose: u8,

    /// Decrease logging verbosity (only errors and warnings)
    #[arg(short = 'q', long = "quiet", action = clap::ArgAction::Count)]
    quiet: u8,

    /// Enable Prometheus metrics HTTP server on this socket address
    /// (e.g. 127.0.0.1:9464).  When absent no metrics server is started.
    #[arg(long = "metrics-addr")]
    metrics_addr: Option<String>,

    /// Enable TCP Noise IK listener on this socket address
    /// (e.g. 0.0.0.0:9443).  When absent no TCP listener is started.
    #[arg(long = "tcp-addr")]
    tcp_addr: Option<String>,
}

const DEFAULT_MAX_TURNS: u32 = 0;

/// Resolve the tool-loop iteration limit.
///
/// Resolution chain: `CHOREOGRAPHR_MAX_TURNS` env var → `config.toml` → default 0 (unlimited).
/// A value of `0` means *unlimited* — the agent loop will run until the
/// model produces a final answer, is cancelled, or hits an error.
fn resolve_max_turns() -> u32 {
    if let Ok(val) = std::env::var("CHOREOGRAPHR_MAX_TURNS")
        && let Ok(n) = val.parse::<u32>()
    {
        return n;
    }
    if let Ok(config) = load_daemon_config()
        && let Some(n) = config.max_turns
    {
        return n;
    }
    DEFAULT_MAX_TURNS
}

fn main() -> anyhow::Result<()> {
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

    let max_turns = resolve_max_turns();
    info!(max_turns, "tool loop iteration limit");
    info!("choreographr starting (locked)");

    let db = Arc::new(choreographr::db::open_db().context("failed to open database")?);

    let (daemon_tx, _daemon_rx) = mpsc::channel::<choreographr::daemon::DaemonCommand>();

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
    let mut tool_registry = choreographr::tools::ToolRegistry::new();
    let mcp_manager = choreographr::mcp::McpManager::from_config(&mut tool_registry);
    let tool_registry = tool_registry.build();

    let state = DaemonState {
        daemon_tx,
        // Compute the next session ID from actual session records so we
        // never collide with an existing session.  The DB's stored counter
        // is only updated at startup and goes stale once the in-memory
        // counter diverges — if the daemon crashes the counter resets and
        // IDs get reused, overwriting old sessions.
        next_session_id: session_metadata
            .keys()
            .max()
            .copied()
            .map(|m| m + 1)
            .unwrap_or(1),
        max_turns,
        active_sessions: std::collections::HashMap::new(),
        session_metadata,
        children: std::collections::HashMap::new(),
        accounts,
        providers: HashMap::new(),
        credentials: std::collections::HashMap::new(),
        x_credentials: None,
        db,
        tool_registry,
        client_streams: Vec::new(),
        summary_subscribers: std::collections::HashMap::new(),
        activity_subscribers: std::collections::HashMap::new(),
        model_cache: HashMap::new(),
        mcp_manager,
    };

    // Load or generate the transport keypair for Noise IK.
    let (transport_sk, _transport_pk) =
        ensure_transport_keypair().context("failed to load/generate transport keypair")?;

    // Load the ACL of authorized client public keys.
    let acl_path = choreo_keystore::paths::authorized_clients_path()
        .context("failed to resolve authorized_clients path")?;
    let acl = std::sync::Arc::new(choreographr::server::acl::Acl::load(&acl_path));

    let socket_path = socket_path();
    choreographr::run_server(
        &socket_path,
        state,
        cli.metrics_addr,
        cli.tcp_addr,
        transport_sk,
        acl,
    )
    .context("failed to run server")
}
