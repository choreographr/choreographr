use anyhow::{Context, bail};
use choreo_proto::{ClientMessage, DaemonMessage, read_message, socket_path, write_message};
use clap::Parser;
use std::env;
use std::io::{BufReader, BufWriter, Write};
#[cfg(unix)]
use std::os::unix::net::UnixStream;
use tracing::{error, info};
use tracing_subscriber::{EnvFilter, fmt};
// Windows: std::os::windows::net::UnixStream is unstable (E0658, feature
// `windows_unix_domain_sockets`, rust-lang/rust#150487), so uds_windows provides
// the same connect/try_clone/shutdown API over named pipes.
#[cfg(windows)]
use uds_windows::UnixStream;

pub mod bridge;
pub mod telegram;
pub mod tg_api;

/// Shared clap [`Styles`] for this crate's CLI binary.
///
/// Each CLI crate keeps its own copy (choreo-proto is the wire protocol and
/// must not host CLI styling); if this ever grows, promote it to a dedicated
/// micro-crate instead of putting it in choreo-proto.
///
/// Uses real ANSI hues (green headers/usage, cyan literals/placeholders) rather
/// than bold/underline only, so help output stays legible even in terminals whose
/// bold text isn't visually distinct (e.g. themes that don't remap the bold color).
/// `Styles::styled()` keeps clap's default error/invalid/valid coloring; the
/// overrides colorize the help elements.
fn clap_styles() -> clap::builder::Styles {
    use clap::builder::styling::{AnsiColor, Effects, Styles};
    Styles::styled()
        .header(AnsiColor::Green.on_default() | Effects::BOLD)
        .usage(AnsiColor::Green.on_default() | Effects::BOLD)
        .literal(AnsiColor::Cyan.on_default() | Effects::BOLD)
        .placeholder(AnsiColor::Cyan.on_default())
}

#[derive(Parser)]
// Bare `version` wires `--version`/`-V` to CARGO_PKG_VERSION, matching the
// rest of the suite. ColorChoice is explicitly Auto (clap's default): color
// only on a TTY, never forced into pipes.
#[command(
    name = "choreo-im",
    version,
    about = "IM platform bridge for Choreographr",
    color = clap::ColorChoice::Auto,
    styles = clap_styles()
)]
struct Cli {
    /// IM platform to bridge (e.g. telegram)
    platform: String,
}

/// Entry point for the `choreo-im` platform bridge binary.
///
/// The workspace root declares this crate's binary as a thin wrapper that
/// simply calls this function, so the actual logic lives here in the lib.
pub fn main() -> anyhow::Result<()> {
    fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .init();

    let cli = Cli::parse();
    let platform = cli.platform;

    let path = socket_path();
    let stream = UnixStream::connect(&path).context("failed to connect to daemon")?;

    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = BufWriter::new(stream);

    // Establish the keystore (auto-unlock / auto-bind / probe-bind) before
    // asking for a credential; extracted verbatim from this function so the
    // flow is testable over any socket pair.
    establish_keystore(&path, &mut reader, &mut writer)?;

    info!(%platform, "requesting credential from daemon");
    write_message(
        &mut writer,
        &ClientMessage::GetCredential {
            service: platform.clone(),
        },
    )
    .context("failed to send credential request")?;
    writer
        .flush()
        .context("failed to flush credential request")?;
    match read_message::<_, DaemonMessage>(&mut reader) {
        Ok(DaemonMessage::Credential {
            key: Some(bot_token),
            ..
        }) => {
            info!(%platform, "got credential, starting platform bridge");
            run_platform(&platform, bot_token, reader, writer).context("platform bridge failed")?;
        }
        Ok(DaemonMessage::Credential { key: None, .. }) => {
            bail!(
                "daemon keystore is locked (or unbound with no client key), or no '{platform}' \
                 credential found — unlock it via the TUI (/unlock); a fresh (unbound) daemon \
                 binds automatically on connect"
            );
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

/// Connect-time keystore establishment: auto-unlock with the stored/legacy
/// key (verify-only), auto-bind on `KeystoreUnbound`, probe-bind when no key
/// resolves. Returns once the daemon is unlocked or the probe fell through.
///
/// Generic over the socket's read/write halves so both std and `uds_windows`
/// Unix streams fit (they implement the same std traits); this keeps the code
/// cfg-independent instead of forking on `#[cfg(windows)]`.
pub fn establish_keystore<R: std::io::Read, W: std::io::Write>(
    addr: &str,
    mut reader: &mut BufReader<R>,
    mut writer: &mut BufWriter<W>,
) -> anyhow::Result<()> {
    // Auto-unlock with the key ALREADY associated with this daemon: the
    // stored known_servers unlock_key (falling back to the legacy raw
    // identity.pk file, which is copied into the store). There is no
    // passphrase unlock here — identity.pk.enc decryption was removed.
    //
    // Unlock is VERIFY-ONLY: on an unbound daemon it answers
    // `KeystoreUnbound` (a supplied key can never create the binding), and
    // this bridge then AUTO-BINDS — `bind_fresh_daemon` mints a fresh key,
    // records it into known_servers PRE-SEND, and the daemon replies `Bound`
    // once it adopted the key and unlocked.
    //
    // With NO key available we still PROBE with a fresh bind: `BindKeystore`
    // on an already-bound keystore is verify-only (a mismatch is rejected
    // with `LockedError`, the binding is never overwritten), so this is safe
    // — it only ever creates a binding that did not exist, and pre-send
    // recording cannot clobber a key that does not resolve anyway.
    if let Some(private_key) = choreo_client_core::try_auto_unlock_key(addr) {
        info!("unlocking daemon with stored unlock key");
        write_message(&mut writer, &ClientMessage::Unlock { private_key })
            .context("failed to send unlock message")?;
        writer.flush().context("failed to flush unlock message")?;
        match read_message::<_, DaemonMessage>(&mut reader) {
            Ok(DaemonMessage::Unlocked) => {
                info!("daemon unlocked");
            }
            Ok(DaemonMessage::KeystoreUnbound { error }) => {
                // Unbound daemon: the stored key was a verify attempt that
                // cannot succeed. Mint a fresh binding key and send it.
                info!(%error, "daemon keystore unbound — auto-binding with a fresh key");
                let (_key, bind_msg) = choreo_client_core::bind_fresh_daemon(addr)
                    .context("failed to mint and record a fresh bind key")?;
                write_message(&mut writer, &bind_msg).context("failed to send bind message")?;
                writer.flush().context("failed to flush bind message")?;
                match read_message::<_, DaemonMessage>(&mut reader) {
                    // `Bound` is the unlock confirmation for a bind (the
                    // daemon ran the shared unlock tail after adopting the
                    // key) — accept it exactly like `Unlocked`.
                    Ok(DaemonMessage::Bound) => {
                        info!("daemon keystore bound and unlocked");
                    }
                    Ok(DaemonMessage::KeystoreUnbound { error: bind_err }) => {
                        error!(%bind_err, "bind failed: keystore still unbound");
                        bail!("bind failed: {bind_err}");
                    }
                    Ok(DaemonMessage::LockedError { error: bind_err }) => {
                        error!(%bind_err, "bind failed");
                        bail!("bind failed: {bind_err}");
                    }
                    Ok(other) => {
                        error!(?other, "unexpected response to bind");
                        bail!("unexpected response to bind: {other:?}");
                    }
                    Err(e) => {
                        error!(%e, "failed to read bind response");
                        bail!("failed to read bind response: {e}");
                    }
                }
            }
            Ok(DaemonMessage::LockedError { error: unlock_err }) => {
                error!(%unlock_err, "unlock failed");
                bail!(
                    "unlock failed: {unlock_err} — the daemon is bound to a key this client \
                 does not hold; re-pair it via the TUI"
                );
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
    } else {
        // No stored/legacy key: probe the daemon with a fresh bind. An
        // UNBOUND daemon adopts it and replies `Bound` (now unlocked); a
        // BOUND daemon rejects the mismatch and stays locked — we fall
        // through to GetCredential, which fails with the unlock guidance.
        let (_key, bind_msg) = choreo_client_core::bind_fresh_daemon(addr)
            .context("failed to mint and record a fresh bind key")?;
        info!("no stored unlock key — probing daemon with a fresh bind");
        write_message(&mut writer, &bind_msg).context("failed to send bind message")?;
        writer.flush().context("failed to flush bind message")?;
        match read_message::<_, DaemonMessage>(&mut reader) {
            Ok(DaemonMessage::Bound) => {
                info!("daemon keystore bound and unlocked");
            }
            Ok(DaemonMessage::LockedError { error }) => {
                info!(%error, "daemon is bound to a key this client does not hold");
            }
            Ok(DaemonMessage::KeystoreUnbound { error }) => {
                error!(%error, "bind rejected against an unbound keystore");
                bail!("bind failed: {error}");
            }
            Ok(other) => {
                error!(?other, "unexpected response to bind probe");
                bail!("unexpected response to bind probe: {other:?}");
            }
            Err(e) => {
                error!(%e, "failed to read bind probe response");
                bail!("failed to read bind probe response: {e}");
            }
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
            let admin_ids_str = env::var("CHOREOGRAPHR_TELEGRAM_USER_IDS").unwrap_or_default();
            let admin_ids: Vec<i64> = admin_ids_str
                .split(',')
                .filter_map(|s| s.trim().parse().ok())
                .collect();

            if admin_ids.is_empty() {
                bail!(
                    "CHOREOGRAPHR_TELEGRAM_USER_IDS must be set to a comma-separated list of Telegram user IDs"
                );
            }

            let admin_count = admin_ids.len();
            info!(admin_count, "starting telegram bridge");

            let bridge = crate::bridge::DaemonBridge::spawn(reader, writer);
            let (tx, rx) = bridge.into_parts();

            crate::telegram::run(bot_token, admin_ids, tx, rx);
            Ok(())
        }
        other => {
            bail!("unknown platform: {other}");
        }
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    /// `--version` is handled by clap before any real arg parsing: it exits
    /// with a `DisplayVersion` error whose message is the version string.
    /// Assert both so the flag stays wired to CARGO_PKG_VERSION (it breaks
    /// silently if the derive attribute loses the bare `version` marker).
    #[test]
    fn version_flag_displays_package_version() {
        // clap returns the version as a `DisplayVersion` error instead of a
        // value; match it out by hand (Cli doesn't derive Debug, so
        // `unwrap_err()`'s Debug bound doesn't apply).
        let err = match super::Cli::try_parse_from(["choreo-im", "--version"]) {
            Err(e) => e,
            Ok(_) => panic!("--version should short-circuit before arg validation"),
        };
        assert_eq!(err.kind(), clap::error::ErrorKind::DisplayVersion);
        assert!(err.to_string().contains(env!("CARGO_PKG_VERSION")));
    }

    /// The platform is a required positional; a normal invocation parses it.
    #[test]
    fn parses_platform_positional() {
        let cli = super::Cli::try_parse_from(["choreo-im", "telegram"])
            .expect("telegram is a valid platform arg");
        assert_eq!(cli.platform, "telegram");
    }

    /// clap enforces the required positional: missing args are a parse error
    /// (replacing the old hand-rolled usage message).
    #[test]
    fn missing_platform_is_a_parse_error() {
        assert!(super::Cli::try_parse_from(["choreo-im"]).is_err());
    }
}
