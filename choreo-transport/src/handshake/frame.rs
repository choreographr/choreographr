//! The handshake's wire framing: the lengths, the 2-byte prefix and the
//! scratch buffers.
//!
//! One read path and one write path serve all four Noise flows in the sibling
//! `ik` and `xx` modules, so the framed read and the coalesced framed write
//! exist exactly once. The 2-byte big-endian length prefix is produced and
//! validated through the shared [`crate::len_prefix`] codec, so the width/cap
//! arithmetic is not restated here. [`Wire`] packages the scratch buffers those
//! paths need, so a flow reads as a sequence of `wire.read(..)` /
//! `wire.write(..)` steps and allocates nothing.
//!
//! The deadline those reads and writes are bounded by lives one module over, in
//! [`super::deadline`]: framing decides WHAT bytes go on the wire, the deadline
//! helpers decide how long any one syscall may take to move them.

use std::net::TcpStream;
use std::time::Instant;

use snow::HandshakeState;

use crate::buf::slice_mut;
use crate::error::TransportError;

use super::deadline::{read_handshake_exact, write_handshake_all};

/// Largest handshake frame body accepted from (or sent to) a peer, in bytes.
///
/// The 2-byte length prefix is **unauthenticated** and arrives before any key
/// material exists, so without a cap a peer could declare 65535 bytes and make
/// the responder allocate — and then block reading — that much before it has
/// authenticated anything. 1 KiB is generous: with the empty payloads these
/// patterns use, the largest handshake message is on the order of 96 bytes
/// (32-byte key material plus the 16-byte GCM tag), so no legitimate peer
/// comes close. A larger declaration is rejected by `declared_frame_len`
/// before any read happens, and a larger encoded message is refused by
/// `encode_frame_len`.
///
/// Public (re-exported as [`crate::handshake::MAX_HANDSHAKE_FRAME`]) because it
/// is a wire contract value: a test harness driving `snow` directly must size
/// its buffers by THIS bound rather than a private copy that could drift.
pub const MAX_HANDSHAKE_FRAME: usize = 1024;

/// Length of the big-endian handshake length prefix that precedes every
/// handshake frame body.
pub(super) const LEN_PREFIX_LEN: usize = 2;

/// Size of an outbound handshake frame buffer: [`LEN_PREFIX_LEN`] bytes of
/// length-prefix headroom followed by room for the largest body a peer may
/// declare ([`MAX_HANDSHAKE_FRAME`]). Snow encodes the body into the headroom
/// tail and the prefix is laid into the headroom, so one buffer carries the
/// whole frame.
pub(super) const HANDSHAKE_FRAME_BUF_LEN: usize = LEN_PREFIX_LEN + MAX_HANDSHAKE_FRAME;

/// Decode a length prefix and validate it against [`MAX_HANDSHAKE_FRAME`].
///
/// This is the single place a peer-declared handshake body length is trusted,
/// and it runs before anything is read for the body: an unauthenticated peer
/// that declares a frame above the cap is rejected with
/// [`TransportError::InvalidFragment`] instead of making the responder read
/// and hand an arbitrary buffer to snow. The 2-byte big-endian arithmetic lives
/// in the shared [`crate::len_prefix`] codec, so this path and the data plane's
/// 4-byte prefix are one implementation parameterised by width and cap.
fn declared_frame_len(len_buf: &[u8]) -> Result<usize, TransportError> {
    crate::len_prefix::decode::<LEN_PREFIX_LEN>(
        len_buf,
        MAX_HANDSHAKE_FRAME,
        "handshake frame length",
    )
}

/// Encode a body length as its 2-byte big-endian prefix, refusing a body this
/// crate's frame buffer cannot hold.
///
/// The read side validates a peer's declaration against the same cap, so the
/// two bounds are one policy stated twice — and the explicit check (rather
/// than a cast plus a later slice failure) means an over-long message is
/// reported as what it is instead of as a broken internal invariant.
fn encode_frame_len(n: usize) -> Result<[u8; LEN_PREFIX_LEN], TransportError> {
    crate::len_prefix::encode(n, MAX_HANDSHAKE_FRAME, "handshake frame length")
}

/// Write the 1-byte handshake-mode preamble, bounded by `deadline`.
///
/// The initiators call this as their first step, so the byte that selects a
/// flow and the flow itself come from one [`super::HandshakeMode`] value — a
/// client cannot announce one mode and then run the other. The write goes
/// through the same bounded writer as the handshake frames, so a peer that stops
/// reading cannot hold the writer: a single byte fits any socket buffer in
/// practice, making this a backstop rather than the normal path.
pub(super) fn write_preamble(
    stream: &mut TcpStream,
    deadline: Instant,
    mode: super::HandshakeMode,
) -> Result<(), TransportError> {
    write_handshake_all(stream, deadline, &[mode.byte()])
}

/// Read one complete handshake frame body into `body`: the 2-byte big-endian
/// length prefix, its validation against [`MAX_HANDSHAKE_FRAME`], then exactly
/// that many body bytes — every read bounded by `deadline`.
///
/// The prefix is decoded and validated before the body is read, so a hostile
/// declaration costs nothing. Returns the populated prefix of `body`.
fn read_handshake_frame<'a>(
    stream: &mut TcpStream,
    deadline: Instant,
    body: &'a mut [u8],
) -> Result<&'a mut [u8], TransportError> {
    let mut len_buf = [0u8; LEN_PREFIX_LEN];
    read_handshake_exact(stream, deadline, &mut len_buf)?;
    let body_len = declared_frame_len(&len_buf)?;
    // Reborrow for the read so `body` itself stays live for the return: the
    // caller gets the populated prefix, not just a success flag.
    read_handshake_exact(
        stream,
        deadline,
        slice_mut(&mut *body, ..body_len, "handshake frame body")?,
    )?;
    slice_mut(body, ..body_len, "handshake frame body")
}

/// Write an already-encoded handshake message as a complete wire frame: lay its
/// 2-byte big-endian length prefix into `buf`'s headroom, then send the prefix
/// and the `n`-byte body as ONE bounded write.
///
/// `buf` is an outbound frame buffer ([`HANDSHAKE_FRAME_BUF_LEN`]): two bytes of
/// length-prefix headroom followed by the body snow encoded at
/// `buf[LEN_PREFIX_LEN..]`. The single write means a peer that stops reading
/// cannot hold the writer past `deadline` between a prefix write and a body
/// write.
fn write_handshake_frame(
    stream: &mut TcpStream,
    deadline: Instant,
    buf: &mut [u8],
    n: usize,
) -> Result<(), TransportError> {
    let prefix = encode_frame_len(n)?;
    slice_mut(buf, ..LEN_PREFIX_LEN, "length prefix")?.copy_from_slice(&prefix);
    write_handshake_all(
        stream,
        deadline,
        crate::buf::slice(buf, ..LEN_PREFIX_LEN + n, "handshake frame")?,
    )
}

/// The three scratch buffers one handshake needs, sized at compile time.
///
/// A handshake is a strict alternation of "encode a message and send it" and
/// "read a frame and decrypt it", so the same three buffers serve every step
/// and the whole exchange allocates nothing:
///
/// * `frame` — the inbound frame body, before snow sees it;
/// * `payload` — snow's decrypted handshake payload (empty for these patterns,
///   but it is still each `read_message` call's output, and snow requires room
///   for whatever a peer sent);
/// * `outbound` — the length-prefix headroom plus the encoded outgoing frame.
pub(super) struct Wire {
    frame: [u8; MAX_HANDSHAKE_FRAME],
    payload: [u8; MAX_HANDSHAKE_FRAME],
    outbound: [u8; HANDSHAKE_FRAME_BUF_LEN],
}

impl Wire {
    /// Fresh zeroed buffers. `const` so a flow can create one without a
    /// constructor call in the hot path's way.
    pub(super) const fn new() -> Self {
        Self {
            frame: [0u8; MAX_HANDSHAKE_FRAME],
            payload: [0u8; MAX_HANDSHAKE_FRAME],
            outbound: [0u8; HANDSHAKE_FRAME_BUF_LEN],
        }
    }

    /// Read one handshake frame and feed its body to the Noise state machine,
    /// decrypting into the payload buffer.
    pub(super) fn read(
        &mut self,
        handshake: &mut HandshakeState,
        stream: &mut TcpStream,
        deadline: Instant,
    ) -> Result<(), TransportError> {
        // Disjoint field borrows: the frame is the ciphertext source and the
        // payload is snow's output, so the two never alias.
        let frame = read_handshake_frame(stream, deadline, &mut self.frame)?;
        handshake.read_message(frame, &mut self.payload)?;
        Ok(())
    }

    /// Encode one handshake message (empty payload — these patterns carry none)
    /// and send it as a single framed, deadline-bounded write.
    pub(super) fn write(
        &mut self,
        handshake: &mut HandshakeState,
        stream: &mut TcpStream,
        deadline: Instant,
    ) -> Result<(), TransportError> {
        let n = handshake.write_message(
            &[],
            slice_mut(&mut self.outbound, LEN_PREFIX_LEN.., "handshake frame body")?,
        )?;
        write_handshake_frame(stream, deadline, &mut self.outbound, n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The 2-byte length prefix round-trips through the shared codec: a length
    /// encodes to big-endian bytes and decodes back unchanged, and the high byte
    /// comes first. Deterministic, no sockets.
    #[test]
    fn length_prefix_round_trips() {
        for len in [0usize, 1, 48, 96, MAX_HANDSHAKE_FRAME] {
            let encoded = encode_frame_len(len).expect("length encodes");
            assert_eq!(
                declared_frame_len(&encoded).expect("prefix decodes"),
                len,
                "length prefix must round-trip"
            );
        }
        // Big-endian by construction: the high byte comes first.
        assert_eq!(encode_frame_len(0x01_02).expect("encodes"), [0x01, 0x02]);
    }

    /// A declared body length inside the cap is accepted and one above it is
    /// rejected with the crate's protocol error — the pre-authentication bound.
    /// Calls the validation path directly, so no socket and no allocation is
    /// involved.
    #[test]
    fn declared_length_above_cap_is_rejected() {
        let at_cap = encode_frame_len(MAX_HANDSHAKE_FRAME).expect("at the cap encodes");
        assert_eq!(
            declared_frame_len(&at_cap).expect("at the cap is allowed"),
            MAX_HANDSHAKE_FRAME
        );
        let empty = encode_frame_len(0).expect("an empty body is a legal length");
        assert_eq!(declared_frame_len(&empty).expect("empty allowed"), 0);

        let over = encode_frame_len(MAX_HANDSHAKE_FRAME + 1);
        assert!(
            matches!(over, Err(TransportError::InvalidFragment(_))),
            "a length one past the cap must be refused, got {over:?}"
        );
        // The whole 16-bit range a hostile peer can declare is rejected above
        // the cap, including the maximum.
        let max = declared_frame_len(&u16::MAX.to_be_bytes());
        assert!(
            matches!(max, Err(TransportError::InvalidFragment(_))),
            "the maximum 16-bit length must be rejected, got {max:?}"
        );
    }

    /// The outbound side enforces the same cap as the inbound side, and reports
    /// it as an over-long frame rather than as a broken buffer invariant.
    #[test]
    fn encoding_above_the_cap_is_refused() {
        let over = encode_frame_len(MAX_HANDSHAKE_FRAME + 1).expect_err("over the cap");
        assert!(
            over.to_string().contains("exceeds the"),
            "an over-long message must name the cap: {over}"
        );
        // Larger than the 16-bit prefix can express at all: same verdict, not
        // a silent truncation.
        assert!(encode_frame_len(usize::from(u16::MAX) + 1).is_err());
    }
}
