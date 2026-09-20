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
/// 1 = postcard era; 2 = `MessagePack` (named mode, `rmp-serde` >= 1.3);
/// 3 = removed `TurnFinalized` (the final-turn snapshot now rides
/// `TurnAppended`), added `Evicted` (best-effort lag-eviction advisory);
/// 4 = session-scoped messages are now wrapped in
/// `DaemonMessage::Session { session_id: Option<u64>, event }` (the
/// `SessionEvent` envelope). Mixed-version peers still fail fast at the
/// version gate, unchanged. The v4 shape was amended in place — the
/// envelope's `session_id` became `Option<u64>` before first release, so it
/// stays version 4 with no bump (mixed-version peers do not exist yet).
/// 5 = replaced the `Locked`/`Unlocked` status *broadcasts* with a dedicated
/// `DaemonMessage::Keystore { state: KeystoreState }` carrying the
/// authoritative three-state keystore status (`Unbound`/`Locked`/`Unlocked`),
/// so a first-run client learns the keystore is unbound and auto-binds.
/// `Locked`/`Unlocked` remain as targeted operation replies.
/// 6 = displayed-image bytes are no longer shipped in session-scoped
/// snapshots (`SessionState`, `TurnAppended`, `TurnsRedone`) — those now
/// carry only `ImageMetadata`. Clients fetch an image's bytes on demand via
/// the new `ClientMessage::GetImage` ⇄ `DaemonMessage::Image` pair, so opening
/// a long session no longer transfers its entire image history.
/// 7 = the create-session reply is split from the create-session broadcast:
/// `SessionEvent::SessionCreatedForRequester` is the direct reply to the
/// creating connection (frontends may attach to it), while
/// `SessionEvent::SessionCreated` is now notification-only and must never move
/// a client's view. Fixes a client hijacking its own view when ANOTHER client
/// created a session.
/// 8 = per-session `pinned`/`archived` flags: the new
/// `ClientMessage::SetSessionPinned`/`SetSessionArchived` requests and the
/// broadcast `SessionEvent::SessionFlagsChanged`, plus the
/// `SessionSummary::pinned`/`archived_at` fields they surface.
pub const PROTOCOL_VERSION: u8 = 8;
/// Max serialised *payload* size, enforced identically on encode (before the
/// 4-byte length prefix is added — [`encode_inner`]) and on decode
/// (`read_payload`, which checks the length prefix before reading the body).
/// A payload at the limit therefore produces a frame of
/// `MAX_FRAME_SIZE + 4` bytes.
///
/// Bumped from 1 MiB to 32 MiB to accommodate `SessionState` responses that
/// carried full image binary data inside `DisplayedImage` records, then to
/// 64 MiB because a long session's full `SessionState` snapshot is
/// re-serialized *uncompressed* for the wire (the on-disk turns are
/// zstd-compressed), so accumulated tool output plus displayed images can
/// exceed 32 MiB even when the database is far smaller.  A payload over the
/// limit fails to encode (`FrameTooLarge`).
///
/// As of protocol v6 the displayed-image bytes no longer ride the snapshot
/// (clients fetch them on demand via `ClientMessage::GetImage`), so the
/// image-driven pressure that motivated the 32 MiB bump is gone; the limit
/// stays at 64 MiB for the accumulated tool-output case.
pub const MAX_FRAME_SIZE: usize = 64 * 1024 * 1024;

/// Encode `(PROTOCOL_VERSION, message)` as named `MessagePack`, enforcing
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
/// framing (e.g. `NoiseStream`), so only the raw `MessagePack` payload is needed.
///
/// # Errors
///
/// Returns [`ProtoError::Codec`] when serialization fails and
/// [`ProtoError::FrameTooLarge`] when the payload exceeds [`MAX_FRAME_SIZE`].
pub fn encode_payload<T: Serialize>(message: &T) -> Result<Vec<u8>, ProtoError> {
    encode_inner(message)
}

/// Encode a message with the 4-byte big-endian length prefix.
///
/// # Errors
///
/// Returns [`ProtoError::Codec`] when serialization fails,
/// [`ProtoError::FrameTooLarge`] when the payload exceeds [`MAX_FRAME_SIZE`],
/// and the same error if the length prefix itself would overflow `u32`.
pub fn encode_frame<T: Serialize>(message: &T) -> Result<Vec<u8>, ProtoError> {
    let payload = encode_inner(message)?;

    let mut frame = Vec::with_capacity(4 + payload.len());
    // `encode_inner` already bounds `payload.len()` to MAX_FRAME_SIZE (64 MiB),
    // far below u32::MAX, but go through `try_from` so the length prefix can
    // never silently truncate if the limit is ever raised past 4 GiB.
    let len = u32::try_from(payload.len()).map_err(|_| ProtoError::FrameTooLarge)?;
    frame.extend_from_slice(&len.to_be_bytes());
    frame.extend_from_slice(&payload);
    Ok(frame)
}

/// Decode a payload produced by [`encode_payload`] into `T`, enforcing the
/// protocol-version gate and the trailing-bytes contract.
///
/// # Errors
///
/// Returns [`ProtoError::UnsupportedVersion`] when the frame's version byte
/// does not match [`PROTOCOL_VERSION`], [`ProtoError::Codec`] when
/// deserialization fails, and [`ProtoError::TrailingBytes`] when the payload
/// carries bytes beyond the decoded message.
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
    let used = usize::try_from(de.position()).map_err(|_| ProtoError::TrailingBytes)?;

    if used != payload.len() {
        return Err(ProtoError::TrailingBytes);
    }

    if version != PROTOCOL_VERSION {
        return Err(ProtoError::UnsupportedVersion { version });
    }

    Ok(message)
}
