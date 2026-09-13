use crate::ProtoError;
use crate::frame::{MAX_FRAME_SIZE, decode_frame, encode_frame};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};

pub const SOCKET_PATH_ENV: &str = "CHOREOGRAPHR_SOCKET_PATH";

/// The default Unix-socket path when `CHOREOGRAPHR_SOCKET_PATH` is unset:
/// `choreographr.sock` under the PLATFORM temp dir (`std::env::temp_dir()`).
///
/// That is `/tmp/choreographr.sock` on a desktop Linux (TMPDIR unset), so
/// behavior there is unchanged — but on Android/Termux `TMPDIR` points at
/// the app's writable prefix tmp dir, which is the difference between the
/// daemon and TUI working at all and dying with a context-free
/// "Permission denied (os error 13)" on a hardcoded `/tmp`.
pub fn default_socket_path() -> String {
    std::env::temp_dir()
        .join("choreographr.sock")
        .to_string_lossy()
        .into_owned()
}

pub fn socket_path() -> String {
    socket_path_impl(|| std::env::var(SOCKET_PATH_ENV).ok())
}

pub(crate) fn socket_path_impl(get_env: impl Fn() -> Option<String>) -> String {
    get_env().unwrap_or_else(default_socket_path)
}

/// Whether a unix-socket DIAL failure means "nothing is listening at the
/// path" — the only condition a daemon autostart (or a stale-socket cleanup)
/// is allowed to act on:
///
/// - `NotFound` — the socket file does not exist at all (no daemon has ever
///   run, or the socket file was removed).
/// - `ConnectionRefused` — the socket file exists but nothing is bound to it
///   (a stale leftover from a crashed daemon, which the daemon unlinks at
///   startup).
///
/// Every OTHER dial failure (`PermissionDenied`, a wedged listener timing out
/// on the backlog, …) means a live-or-unknown daemon state that autostart
/// cannot fix, so the error must be surfaced verbatim instead of spawning a
/// second daemon.
///
/// This predicate lives in `choreo-proto` (not in the client or the daemon)
/// because BOTH sides classify dials and they must never disagree: the TUI
/// connection path (`choreo-client-core`'s
/// `run_daemon_connection_with_autostart`) uses it to decide when to fire the
/// autostart hook, and the daemon's `remove_stale_socket` uses the same
/// connect-probe shape to decide whether an existing socket file is safe to
/// unlink. A platform whose dial error for "no listener" ever changes (e.g.
/// Windows returning an unexpected kind through `uds_windows`) then needs a
/// one-line fix here, not two divergent `matches!` on opposite sides of the
/// wire.
pub fn dial_error_means_no_listener(kind: std::io::ErrorKind) -> bool {
    matches!(
        kind,
        std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
    )
}

pub fn write_message<W, T>(writer: &mut W, message: &T) -> Result<(), ProtoError>
where
    W: Write,
    T: Serialize,
{
    let frame = encode_frame(message)?;
    writer.write_all(&frame)?;
    Ok(())
}

pub fn read_message<R, T>(reader: &mut R) -> Result<T, ProtoError>
where
    R: Read,
    T: for<'de> Deserialize<'de>,
{
    let payload = read_payload(reader)?;
    decode_frame(&payload)
}

pub fn read_payload<R>(reader: &mut R) -> Result<Vec<u8>, ProtoError>
where
    R: Read,
{
    let mut len_buf = [0_u8; 4];
    reader.read_exact(&mut len_buf)?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_FRAME_SIZE {
        return Err(ProtoError::FrameTooLarge);
    }

    let mut payload = vec![0_u8; len];
    reader.read_exact(&mut payload)?;
    Ok(payload)
}
