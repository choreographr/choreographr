use crate::ProtoError;
use serde::{Deserialize, Serialize};
use std::io::Cursor;

/// Wire protocol version.
///
/// The encoder always writes the current version — there is no
/// version-negotiation handshake.  The decoder rejects any frame whose
/// version field does not match, ensuring that mixed-version peers are
/// caught at deserialisation time.
///
/// 1 = postcard era; 2 = MessagePack (named mode, rmp-serde >= 1.3).
pub const PROTOCOL_VERSION: u8 = 2;
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
/// framing (e.g. NoiseStream), so only the raw MessagePack payload is needed.
pub fn encode_payload<T: Serialize>(message: &T) -> Result<Vec<u8>, ProtoError> {
    let payload = rmp_serde::to_vec_named(&(PROTOCOL_VERSION, message))
        .map_err(|e| ProtoError::Codec(e.to_string()))?;

    if payload.len() > MAX_FRAME_SIZE {
        return Err(ProtoError::FrameTooLarge);
    }
    Ok(payload)
}

pub fn encode_frame<T: Serialize>(message: &T) -> Result<Vec<u8>, ProtoError> {
    let payload = rmp_serde::to_vec_named(&(PROTOCOL_VERSION, message))
        .map_err(|e| ProtoError::Codec(e.to_string()))?;

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
    // rmp-serde 1.3.1 has no `from_slice_ref` (unlike postcard's
    // `take_from_bytes`), so decode through an explicit `Deserializer` over a
    // `Cursor` and use `position()` as the remainder probe: the cursor is not
    // read ahead, so its position after deserialization is exactly the number
    // of bytes consumed. Any leftover bytes are a trailing-bytes violation.
    let mut de = rmp_serde::Deserializer::new(Cursor::new(payload));
    let (version, message): (u8, T) =
        serde::Deserialize::deserialize(&mut de).map_err(|e| ProtoError::Codec(e.to_string()))?;
    let used = de.position() as usize;

    if used != payload.len() {
        return Err(ProtoError::TrailingBytes);
    }

    if version != PROTOCOL_VERSION {
        return Err(ProtoError::UnsupportedVersion { version });
    }

    Ok(message)
}
