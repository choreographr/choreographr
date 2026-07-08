use anyhow::{Context, bail};
use std::env;
use std::io::{BufReader, BufWriter, Write};
use std::os::unix::net::UnixStream;
use tai_proto::{ClientMessage, DaemonMessage, read_message_sync, socket_path, write_message_sync};
use tracing::{error, info};
use tracing_subscriber::{EnvFilter, fmt};

use tai_im::bridge;
use tai_im::telegram;

fn main() -> anyhow::Result<()> {
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
    let stream = UnixStream::connect(&path).context("failed to connect to daemon")?;

    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = BufWriter::new(stream);

    if let Some(ref passphrase) = unlock_passphrase {
        info!("unlocking daemon keystore");
        write_message_sync(
            &mut writer,
            &ClientMessage::Unlock {
                passphrase: passphrase.clone(),
            },
        )
        .context("failed to send unlock message")?;
        writer.flush().context("failed to flush unlock message")?;
        match read_message_sync::<_, DaemonMessage>(&mut reader) {
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
    write_message_sync(
        &mut writer,
        &ClientMessage::GetCredential {
            service: platform.clone(),
        },
    )
    .context("failed to send credential request")?;
    writer
        .flush()
        .context("failed to flush credential request")?;
    match read_message_sync::<_, DaemonMessage>(&mut reader) {
        Ok(DaemonMessage::Credential {
            key: Some(bot_token),
            ..
        }) => {
            info!(%platform, "got credential, starting platform bridge");
            run_platform(&platform, bot_token, reader, writer).context("platform bridge failed")?;
        }
        Ok(DaemonMessage::Credential { key: None, .. }) => {
            if unlock_passphrase.is_none() {
                bail!(
                    "daemon is locked. unlock the daemon via TUI first, or set TAI_KEYSTORE_PASSPHRASE env var"
                );
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

fn run_platform(
    platform: &str,
    bot_token: String,
    reader: BufReader<UnixStream>,
    writer: BufWriter<UnixStream>,
) -> anyhow::Result<()> {
    match platform {
        "telegram" => {
            let admin_ids_str = env::var("TAI_TELEGRAM_USER_IDS").unwrap_or_default();
            let admin_ids: Vec<i64> = admin_ids_str
                .split(',')
                .filter_map(|s| s.trim().parse().ok())
                .collect();

            if admin_ids.is_empty() {
                bail!(
                    "TAI_TELEGRAM_USER_IDS must be set to a comma-separated list of Telegram user IDs"
                );
            }

            let admin_count = admin_ids.len();
            info!(admin_count, "starting telegram bridge");

            let bridge = bridge::DaemonBridge::spawn(reader, writer);
            let (tx, rx) = bridge.into_parts();

            telegram::run(bot_token, admin_ids, tx, rx);
            Ok(())
        }
        other => {
            bail!("unknown platform: {other}");
        }
    }
}
