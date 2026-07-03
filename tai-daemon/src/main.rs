use anyhow::Context;
use tai_proto::socket_path;
use tracing::info;
use tracing_subscriber::{EnvFilter, fmt};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .init();

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
    let state = tai_daemon::new_daemon_state(db);
    let socket_path = socket_path();
    tai_daemon::run_server(&socket_path, state)
        .await
        .context("failed to run server")
}
