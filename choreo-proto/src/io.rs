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
