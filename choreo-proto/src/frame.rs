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

/// Encode a message without the 4-byte length prefix.
///
/// This is used when the transport layer already provides its own
/// framing (e.g. NoiseStream), so only the raw postcard payload is needed.
pub fn encode_payload<T: Serialize>(message: &T) -> Result<Vec<u8>, ProtoError> {
    let payload = postcard::to_allocvec(&(PROTOCOL_VERSION, message))
        .map_err(|e| ProtoError::Postcard(e.to_string()))?;

    if payload.len() > MAX_FRAME_SIZE {
        return Err(ProtoError::FrameTooLarge);
    }
    Ok(payload)
}

pub fn encode_frame<T: Serialize>(message: &T) -> Result<Vec<u8>, ProtoError> {
    let payload = postcard::to_allocvec(&(PROTOCOL_VERSION, message))
        .map_err(|e| ProtoError::Postcard(e.to_string()))?;

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
    let ((version, message), remainder): ((u8, T), &[u8]) =
        postcard::take_from_bytes(payload).map_err(|e| ProtoError::Postcard(e.to_string()))?;

    if !remainder.is_empty() {
        return Err(ProtoError::TrailingBytes);
    }

    if version != PROTOCOL_VERSION {
        return Err(ProtoError::UnsupportedVersion { version });
    }

    Ok(message)
}
