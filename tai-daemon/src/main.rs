use std::io;
use tai_proto::socket_path;
use tracing::info;
use tracing_subscriber::{EnvFilter, fmt};

#[tokio::main]
async fn main() -> io::Result<()> {
    fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .init();

    info!("tai-daemon starting (locked) — send /unlock <passphrase> to unlock");
    let config_path =
        tai_daemon::openai::config_path().map_or_else(|_| "?".into(), |p| p.display().to_string());
    let keystore_path =
        tai_keystore::keystore_path().map_or_else(|_| "?".into(), |p| p.display().to_string());
    info!(%config_path, %keystore_path, "daemon paths");
    let state = tai_daemon::new_daemon_state();
    let socket_path = socket_path();
    tai_daemon::run_server(&socket_path, state).await
}
