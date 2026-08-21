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
/// 1 = postcard era; 2 = MessagePack (named mode, rmp-serde >= 1.3);
/// 3 = removed `TurnFinalized` (the final-turn snapshot now rides
/// `TurnAppended`), added `Evicted` (best-effort lag-eviction advisory);
/// 4 = session-scoped messages are now wrapped in
/// `DaemonMessage::Session { session_id, event }` (the `SessionEvent`
/// envelope). Mixed-version peers still fail fast at the version gate,
/// unchanged.
pub const PROTOCOL_VERSION: u8 = 4;
/// Max serialised *payload* size, enforced identically on encode (before the
/// 4-byte length prefix is added — [`encode_inner`]) and on decode
/// (`read_payload`, which checks the length prefix before reading the body).
/// A payload at the limit therefore produces a frame of
/// `MAX_FRAME_SIZE + 4` bytes.
///
/// Bumped from 1 MiB to 32 MiB to accommodate `SessionState` responses that
/// carry full image binary data inside `DisplayedImage` records.  The old
/// limit was tight enough that a single large tool-generated image could
/// overflow the frame when the client re-attaches to a session.
pub const MAX_FRAME_SIZE: usize = 32 * 1024 * 1024;

/// Encode `(PROTOCOL_VERSION, message)` as named MessagePack, enforcing
/// [`MAX_FRAME_SIZE`]. Shared by [`encode_payload`] (transport-provided
/// framing) and [`encode_frame`] (4-byte length prefix added by the caller),
/// so the codec and the size policy live in exactly one place.
fn encode_inner<T: Serialize>(message: &T) -> Result<Vec<u8>, ProtoError> {
    let payload = rmp_serde::to_vec_named(&(PROTOCOL_VERSION, message))
        .map_err(|e| ProtoError::Codec(e.to_string()))?;

    if payload.len() > MAX_FRAME_SIZE {
        return Err(ProtoError::FrameTooLarge);
    }
    Ok(payload)
}

/// Encode a message without the 4-byte length prefix.
///
/// This is used when the transport layer already provides its own
/// framing (e.g. NoiseStream), so only the raw MessagePack payload is needed.
pub fn encode_payload<T: Serialize>(message: &T) -> Result<Vec<u8>, ProtoError> {
    encode_inner(message)
}

pub fn encode_frame<T: Serialize>(message: &T) -> Result<Vec<u8>, ProtoError> {
    let payload = encode_inner(message)?;

    let mut frame = Vec::with_capacity(4 + payload.len());
    // `encode_inner` already bounds `payload.len()` to MAX_FRAME_SIZE (32 MiB),
    // far below u32::MAX, but go through `try_from` so the length prefix can
    // never silently truncate if the limit is ever raised past 4 GiB.
    let len = u32::try_from(payload.len()).map_err(|_| ProtoError::FrameTooLarge)?;
    frame.extend_from_slice(&len.to_be_bytes());
    frame.extend_from_slice(&payload);
    Ok(frame)
}

pub fn decode_frame<T>(payload: &[u8]) -> Result<T, ProtoError>
where
    T: for<'de> Deserialize<'de>,
{
    // Version gate BEFORE the full decode: the envelope is always a
    // 2-element MessagePack array (`0x92`, fixarray of 2) whose first
    // element is the version, encoded as a single-byte positive fixint for
    // any plausible protocol version — so `payload[1]` is the version byte.
    // Rejecting a mismatched peer here means its message body is never
    // deserialized at all (a peer on a different protocol may send bytes
    // this binary cannot meaningfully parse), and the documented "reject any
    // frame whose version field does not match" contract holds at the
    // earliest possible point. The tuple decode below re-checks the version
    // as defense-in-depth in case the envelope shape ever changes.
    match payload {
        [0x92, version, ..] if *version != PROTOCOL_VERSION => {
            return Err(ProtoError::UnsupportedVersion { version: *version });
        }
        _ => {}
    }
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
