use crate::ProtoError;
use serde::{Deserialize, Serialize};

/// Wire protocol version.
///
/// The encoder always writes the current version — there is no
/// version-negotiation handshake.  The decoder rejects any frame whose
/// version field does not match, ensuring that mixed-version peers are
/// caught at deserialisation time.
pub const PROTOCOL_VERSION: u8 = 1;
/// Max serialised frame size (4 bytes length prefix + payload).
///
/// Bumped from 1 MiB to 32 MiB to accommodate `SessionState` responses that
/// carry full image binary data inside `DisplayedImage` records.  The old
/// limit was tight enough that a single large tool-generated image could
/// overflow the frame when the client re-attaches to a session.
pub const MAX_FRAME_SIZE: usize = 32 * 1024 * 1024;

pub fn encode_frame<T: Serialize>(message: &T) -> Result<Vec<u8>, ProtoError> {
    let payload =
        bincode::serde::encode_to_vec((PROTOCOL_VERSION, message), bincode::config::standard())
            .map_err(|e| ProtoError::Bincode(e.to_string()))?;

    if payload.len() > MAX_FRAME_SIZE {
        return Err(ProtoError::FrameTooLarge);
    }

    let mut frame = Vec::with_capacity(4 + payload.len());
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    frame.extend_from_slice(&payload);
    Ok(frame)
}

pub fn decode_frame<T>(payload: &[u8]) -> Result<T, ProtoError>
where
    T: for<'de> Deserialize<'de>,
{
    let ((version, message), consumed): ((u8, T), usize) =
        bincode::serde::decode_from_slice(payload, bincode::config::standard())
            .map_err(|e| ProtoError::Bincode(e.to_string()))?;

    if consumed != payload.len() {
        return Err(ProtoError::TrailingBytes);
    }

    if version != PROTOCOL_VERSION {
        return Err(ProtoError::UnsupportedVersion { version });
    }

    Ok(message)
}
