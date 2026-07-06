use anyhow::Context;
use clap::Parser;
use std::sync::Arc;
use std::sync::mpsc;
use tai_daemon::daemon::DaemonState;
use tai_daemon::db::read_all_sessions;
use tai_daemon::openai::load_service_config;
use tai_proto::socket_path;
use tracing::{info, warn};
use tracing_subscriber::{EnvFilter, fmt};

#[derive(Parser)]
#[command(name = "tai-daemon", about = "Tai AI daemon")]
struct Cli {
    /// Increase logging verbosity (-v debug, -vv trace)
    #[arg(short = 'v', long = "verbose", action = clap::ArgAction::Count)]
    verbose: u8,

    /// Decrease logging verbosity (only errors and warnings)
    #[arg(short = 'q', long = "quiet", action = clap::ArgAction::Count)]
    quiet: u8,
}

const DEFAULT_MAX_TURNS: u32 = 25;

fn resolve_max_turns() -> u32 {
    if let Ok(val) = std::env::var("TAI_MAX_TURNS") {
        if let Ok(n) = val.parse::<u32>() {
            if n > 0 {
                return n;
            }
        }
    }
    if let Ok(config) = load_service_config() {
        if let Some(n) = config.max_turns {
            if n > 0 {
                return n;
            }
        }
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
            (v, 0) if v == 0 => "info",
            (_, q) if q > 0 => "warn",
            (1, 0) => "debug",
            _ => "trace",
        };
        Some(level)
    };

    let env_filter = match log_level {
        Some(level) => EnvFilter::new(level),
        None => EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| EnvFilter::new("info")),
    };

    fmt()
        .with_env_filter(env_filter)
        .with_target(false)
        .init();

    info!(effective_level = ?log_level.unwrap_or("from RUST_LOG"), "logging initialized");

    let max_turns = resolve_max_turns();
    info!(max_turns, "tool loop iteration limit");
    info!("tai-daemon starting (locked)");

    let db = Arc::new(tai_daemon::db::open_db().context("failed to open database")?);

    let (daemon_tx, _daemon_rx) = mpsc::channel::<tai_daemon::daemon::DaemonCommand>();

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

    info!(count = session_metadata.len(), "loaded sessions from database");

    let state = DaemonState {
        daemon_tx,
        next_session_id: tai_daemon::db::next_session_id(&db).unwrap_or(1),
        max_turns,
        active_sessions: std::collections::HashMap::new(),
        session_metadata,
        openai_client: None,
        keystore: None,
        x_credentials: None,
        db,
        tool_registry: tai_daemon::tools::ToolRegistry::new().build(),
        client_streams: Vec::new(),
        summary_subscribers: std::collections::HashMap::new(),
        model_cache: None,
    };

    let socket_path = socket_path();
    tai_daemon::run_server(&socket_path, state)
        .context("failed to run server")
}
