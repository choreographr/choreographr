//! The framed message codec: one length-prefixed message in each direction.
//!
//! A frame is a 4-byte big-endian payload length followed by that many
//! `MessagePack` bytes (see [`crate::frame`], which owns the encoding and the
//! [`crate::MAX_FRAME_SIZE`] policy). These three functions are the read and write
//! of one such frame over any `Read`/`Write` — the deadline-bounded variants a
//! long-lived connection uses live in [`super::read`] and [`super::write`], and a
//! transport that supplies its own framing (the Noise control plane) uses
//! [`crate::encode_payload`] instead.

use crate::ProtoError;
use crate::frame::{MAX_FRAME_SIZE, decode_frame, encode_frame};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};

/// Encode `message` and write the framed payload to `writer`.
///
/// # Errors
///
/// Fails with `ProtoError` when serialization fails or the underlying writer
/// rejects the frame bytes.
pub fn write_message<W, T>(writer: &mut W, message: &T) -> Result<(), ProtoError>
where
    W: Write,
    T: Serialize,
{
    let frame = encode_frame(message)?;
    writer.write_all(&frame)?;
    Ok(())
}

/// Read one length-prefixed frame from `reader` and deserialize it as `T`.
///
/// # Errors
///
/// Fails with `ProtoError` when the frame exceeds [`MAX_FRAME_SIZE`] or the
/// payload fails to deserialize into `T`.
pub fn read_message<R, T>(reader: &mut R) -> Result<T, ProtoError>
where
    R: Read,
    T: for<'de> Deserialize<'de>,
{
    let payload = read_payload(reader)?;
    decode_frame(&payload)
}

/// Read a single length-prefixed frame's raw payload bytes.
///
/// # Errors
///
/// Fails with `ProtoError` when the length header declares a payload larger
/// than [`MAX_FRAME_SIZE`] (`FrameTooLarge`), or when I/O on `reader` fails.
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
