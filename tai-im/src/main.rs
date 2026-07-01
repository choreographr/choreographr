use std::env;
use std::io;
use std::process;
use tai_proto::{ClientMessage, DaemonMessage, read_message, socket_path, write_message};
use tokio::net::UnixStream;

mod bridge;
mod telegram;

#[tokio::main]
async fn main() -> io::Result<()> {
    let mut args = env::args().skip(1);

    let platform = match args.next().as_deref() {
        Some(p) => p.to_string(),
        None => {
            eprintln!("usage: tai-im telegram");
            process::exit(1);
        }
    };

    let unlock_passphrase = env::var("TAI_KEYSTORE_PASSPHRASE").ok();

    let path = socket_path();
    let mut stream = UnixStream::connect(&path).await?;

    if let Some(ref passphrase) = unlock_passphrase {
        write_message(
            &mut stream,
            &ClientMessage::Unlock {
                passphrase: passphrase.clone(),
            },
        )
        .await?;
        match read_message::<_, DaemonMessage>(&mut stream).await {
            Ok(DaemonMessage::Unlocked) => {}
            Ok(DaemonMessage::LockedError { error }) => {
                eprintln!("unlock failed: {error}");
                process::exit(1);
            }
            Ok(other) => {
                eprintln!("unexpected response to unlock: {other:?}");
                process::exit(1);
            }
            Err(e) => {
                eprintln!("failed to read unlock response: {e}");
                process::exit(1);
            }
        }
    }

    write_message(
        &mut stream,
        &ClientMessage::GetCredential {
            service: platform.clone(),
        },
    )
    .await?;
    match read_message::<_, DaemonMessage>(&mut stream).await {
        Ok(DaemonMessage::Credential {
            key: Some(bot_token),
            ..
        }) => {
            run_platform(&platform, bot_token, stream).await;
        }
        Ok(DaemonMessage::Credential { key: None, .. }) => {
            if unlock_passphrase.is_none() {
                eprintln!("daemon is locked. unlock the daemon via TUI first, or set TAI_KEYSTORE_PASSPHRASE env var");
            } else {
                eprintln!("no '{}' credential found in keystore", platform);
            }
            process::exit(1);
        }
        Ok(other) => {
            eprintln!("unexpected response to GetCredential: {other:?}");
            process::exit(1);
        }
        Err(e) => {
            eprintln!("failed to read credential response: {e}");
            process::exit(1);
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
                eprintln!(
                    "TAI_TELEGRAM_USER_IDS must be set to a comma-separated list of Telegram user IDs"
                );
                process::exit(1);
            }

            let (reader, writer) = stream.into_split();
            let bridge = bridge::DaemonBridge::spawn(reader, writer);
            let (tx, rx) = bridge.into_parts();

            telegram::run(bot_token, admin_ids, tx, rx).await;
        }
        other => {
            eprintln!("unknown platform: {other}");
            process::exit(1);
        }
    }
}
