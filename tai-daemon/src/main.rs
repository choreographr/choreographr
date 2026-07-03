use anyhow::Context;
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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .init();

    let max_turns = resolve_max_turns();
    info!(max_turns, "tool loop iteration limit");

    info!("tai-daemon starting (locked) — send /unlock <passphrase> to unlock");
    let config_path = tai_daemon::openai::config_path()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|e| format!("<error: {e}>"));
    let keystore_path = tai_keystore::keystore_path()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|e| format!("<error: {e}>"));
    let db_path = tai_daemon::db::db_path()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|e| format!("<error: {e}>"));
    info!(%config_path, %keystore_path, %db_path, "daemon paths");
    let db = tai_daemon::db::open_db().context("failed to open database")?;
    let state = tai_daemon::new_daemon_state(db, max_turns).await;
    let socket_path = socket_path();
    tai_daemon::run_server(&socket_path, state)
        .await
        .context("failed to run server")
}
