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
    let state = tai_daemon::new_daemon_state();
    let socket_path = socket_path();
    tai_daemon::run_server(&socket_path, state).await
}
