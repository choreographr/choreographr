use anyhow::Context;
use std::sync::Arc;
use std::sync::mpsc;
use tai_daemon::daemon::DaemonState;
use tai_daemon::openai::load_service_config;
use tai_proto::socket_path;
use tracing::info;
use tracing_subscriber::{EnvFilter, fmt};

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
    fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .init();

    let max_turns = resolve_max_turns();
    info!(max_turns, "tool loop iteration limit");
    info!("tai-daemon starting (locked)");

    let db = Arc::new(tai_daemon::db::open_db().context("failed to open database")?);

    let (daemon_tx, _daemon_rx) = mpsc::channel::<tai_daemon::daemon::DaemonCommand>();
    let state = DaemonState {
        daemon_tx,
        next_session_id: tai_daemon::db::next_session_id(&db).unwrap_or(1),
        max_turns,
        active_sessions: std::collections::HashMap::new(),
        session_metadata: std::collections::HashMap::new(),
        openai_client: None,
        keystore: None,
        x_credentials: None,
        db,
        tool_registry: Arc::new(tai_daemon::tools::ToolRegistry::new()),
        client_streams: Vec::new(),
        summary_subscribers: std::collections::HashMap::new(),
    };

    let socket_path = socket_path();
    tai_daemon::run_server(&socket_path, state)
        .context("failed to run server")
}
