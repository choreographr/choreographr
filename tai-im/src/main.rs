use anyhow::{Context, bail};
use std::env;
use tai_proto::{ClientMessage, DaemonMessage, read_message, socket_path, write_message};
use tokio::net::UnixStream;
use tracing::{error, info};
use tracing_subscriber::{EnvFilter, fmt};

mod bridge;
mod telegram;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .init();

    let mut args = env::args().skip(1);

    let platform = match args.next().as_deref() {
        Some(p) => p.to_string(),
        None => {
            error!("usage: tai-im telegram");
            bail!("usage: tai-im telegram");
        }
    };

    let unlock_passphrase = env::var("TAI_KEYSTORE_PASSPHRASE").ok();

    let path = socket_path();
    let mut stream = UnixStream::connect(&path)
        .await
        .context("failed to connect to daemon")?;

    if let Some(ref passphrase) = unlock_passphrase {
        info!("unlocking daemon keystore");
        write_message(
            &mut stream,
            &ClientMessage::Unlock {
                passphrase: passphrase.clone(),
            },
        )
        .await
        .context("failed to send unlock message")?;
        match read_message::<_, DaemonMessage>(&mut stream).await {
            Ok(DaemonMessage::Unlocked) => {
                info!("daemon keystore unlocked");
            }
            Ok(DaemonMessage::LockedError { error: unlock_err }) => {
                error!(%unlock_err, "unlock failed");
                bail!("unlock failed: {unlock_err}");
            }
            Ok(other) => {
                error!(?other, "unexpected response to unlock");
                bail!("unexpected response to unlock: {other:?}");
            }
            Err(e) => {
                error!(%e, "failed to read unlock response");
                bail!("failed to read unlock response: {e}");
            }
        }
    }

    info!(%platform, "requesting credential from daemon");
    write_message(
        &mut stream,
        &ClientMessage::GetCredential {
            service: platform.clone(),
        },
    )
    .await
    .context("failed to send credential request")?;
    match read_message::<_, DaemonMessage>(&mut stream).await {
        Ok(DaemonMessage::Credential {
            key: Some(bot_token),
            ..
        }) => {
            info!(%platform, "got credential, starting platform bridge");
            run_platform(&platform, bot_token, stream).await;
        }
        Ok(DaemonMessage::Credential { key: None, .. }) => {
            if unlock_passphrase.is_none() {
                bail!("daemon is locked. unlock the daemon via TUI first, or set TAI_KEYSTORE_PASSPHRASE env var");
            } else {
                bail!("no '{platform}' credential found in keystore");
            }
        }
        Ok(other) => {
            error!(?other, "unexpected response to GetCredential");
            bail!("unexpected response to GetCredential: {other:?}");
        }
        Err(e) => {
            error!(%e, "failed to read credential response");
            bail!("failed to read credential response: {e}");
        }
    }

    Ok(())
}

async fn run_platform(platform: &str, bot_token: String, stream: UnixStream) {
    match platform {
        "telegram" => {
            let admin_ids_str = env::var("TAI_TELEGRAM_USER_IDS").unwrap_or_default();
            let admin_ids: Vec<i64> = admin_ids_str
                .split(',')
                .filter_map(|s| s.trim().parse().ok())
                .collect();

            if admin_ids.is_empty() {
                error!("TAI_TELEGRAM_USER_IDS must be set to a comma-separated list of Telegram user IDs");
                return;
            }

            let admin_count = admin_ids.len();
            info!(admin_count, "starting telegram bridge");

            let (reader, writer) = stream.into_split();
            let bridge = bridge::DaemonBridge::spawn(reader, writer);
            let (tx, rx) = bridge.into_parts();

            telegram::run(bot_token, admin_ids, tx, rx).await;
        }
        other => {
            error!(platform = %other, "unknown platform");
            std::process::exit(1);
        }
    }
}
