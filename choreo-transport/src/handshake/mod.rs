//! The Noise IK and XX handshake state machines over a `TcpStream`.
//!
//! The state machines themselves are the stock `snow` patterns
//! `Noise_IK_25519_AESGCM_SHA256` ([`handshake_initiator`] /
//! [`handshake_responder`]) and `Noise_XX_25519_AESGCM_SHA256`
//! ([`handshake_initiator_xx`] / [`handshake_responder_xx`]). Nothing about
//! those patterns, their key schedule, or the cipher is modified. What this
//! module adds is application-level framing around the handshake: the 1-byte
//! mode preamble ([`PREAMBLE_IK`] / [`PREAMBLE_XX`]), the 2-byte big-endian
//! handshake length prefix (see the `frame` submodule), and — in
//! [`crate::noise`] — the 4-byte data-plane prefix carrying an authenticated
//! continuation byte.
//!
//! The preamble byte authorizes nothing on its own: it only selects which
//! pattern to run. Both patterns authenticate both static keys, so a peer that
//! claims one mode and speaks another simply fails its handshake — there is no
//! mode confusion to exploit. The selection is additionally part of the
//! handshake transcript: every flow mixes [`PROLOGUE_TAG`] plus its own mode
//! byte into the Noise prologue ([`noise_prologue`]), which snow hashes into
//! the handshake hash before any message is processed. The mode is therefore
//! authenticated by the same exchange it selects — an intermediary that
//! rewrites the unauthenticated preamble byte on the path produces a prologue
//! mismatch and a failed handshake, and a cross-pattern handshake fails
//! structurally rather than incidentally.
//!
//! Every handshake read and write is bounded by an absolute deadline —
//! `HANDSHAKE_TIMEOUT` by default, or an explicit budget via the
//! `_with_timeout` variants — so a silent or slow-drip unauthenticated peer
//! cannot hold a connection thread and socket FD open.
//!
//! The flows live in the `ik` and `xx` submodules, one pair of functions each,
//! and both share the single framing path in `frame` so the length prefix and
//! the bounded I/O have exactly one implementation.

mod frame;
mod ik;
mod xx;

use std::net::TcpStream;
use std::time::{Duration, Instant};

use crate::error::TransportError;

use frame::read_handshake_exact;

pub use ik::{
    handshake_initiator, handshake_initiator_with_timeout, handshake_responder,
    handshake_responder_with_timeout,
};
pub use xx::{
    handshake_initiator_xx, handshake_initiator_xx_with_timeout, handshake_responder_xx,
    handshake_responder_xx_with_timeout,
};

// The bounds-checked slice helpers are shared with the data plane in
// `crate::noise`, which reaches them through this module's path.
pub(crate) use frame::{at, at_mut, slice, slice_mut};

/// Default total-duration budget for a Noise handshake. The
/// `handshake_*_with_timeout` variants take their own budget; the plain
/// functions delegate to them with this constant.
///
/// The budget is an ABSOLUTE deadline, not a per-read socket timeout: every
/// handshake read is bounded by the time remaining until it (see the `frame`
/// submodule's read helper), so a peer that dribbles bytes to keep resetting
/// a per-recv timeout is still cut off at the deadline. It is cleared before
/// the `TransportState` is handed over; the data plane has no timeout by
/// design — readers block until a message or EOF, and the daemon's shutdown
/// path closes sockets to unblock them.
///
/// The handshake runs BEFORE any authentication (the responder's ACL check
/// happens mid-handshake), so without a bound an unauthenticated peer could
/// hold a connection thread + socket FD open forever by connecting and
/// sending nothing — a resource-exhaustion vector on the daemon's TCP
/// listener. 10 s is far beyond the single round trip a healthy handshake
/// needs (sub-millisecond on loopback, a few ms on a real network).
pub(crate) const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Domain-separation tag mixed into every handshake prologue (see
/// [`noise_prologue`]).
///
/// The prologue is hashed into the Noise handshake hash before any handshake
/// message is processed, so its content is covered by exactly the same
/// authentication as the key exchange. This tag pins the transcript to THIS
/// protocol: another use of the same stock pattern and the same static keys
/// derives a different handshake hash, so the two exchanges can never be
/// confused for one another. The `-1` suffix leaves room for a future
/// incompatible prologue without colliding with this one.
pub const PROLOGUE_TAG: &[u8] = b"choreographr-noise-1";

/// Build the Noise prologue for `mode` (one of [`PREAMBLE_IK`] or
/// [`PREAMBLE_XX`]): [`PROLOGUE_TAG`] followed by the mode byte.
///
/// Both ends of one handshake pass the SAME mode — the IK initiator and the IK
/// responder both use [`PREAMBLE_IK`], the XX pair both [`PREAMBLE_XX`] — which
/// is what makes the binding meaningful: the prologue mismatch that any other
/// choice would produce is a handshake failure, so the mode a connection uses
/// is the mode both peers committed to inside the transcript. There is no
/// negotiation and no version byte, so there is nothing for an intermediary to
/// steer the selection with.
///
/// Returns the tag and the byte concatenated; the caller keeps the buffer
/// alive for as long as the `snow` builder it hands it to, which borrows it.
#[must_use]
pub fn noise_prologue(mode: u8) -> Vec<u8> {
    // One allocation of exactly the final size: the tag plus the single mode
    // byte. The buffer is tiny (20 bytes) and lives only for one builder.
    let mut prologue = Vec::with_capacity(PROLOGUE_TAG.len() + 1);
    prologue.extend_from_slice(PROLOGUE_TAG);
    prologue.push(mode);
    prologue
}

/// Handshake-mode preamble byte: Noise IK (the existing authenticated
/// mode — the client knows the server's static key in advance).
///
/// TCP wire protocol v5: every TCP connection now starts with exactly one
/// unauthenticated mode byte before any handshake message. 0x00 is
/// deliberately reserved (never assigned) so an all-zero garbage fill or a
/// trailing-NUL bug can never be mistaken for a valid mode.
pub const PREAMBLE_IK: u8 = 0x01;

/// Handshake-mode preamble byte: Noise XX (first-contact mode — the client
/// does NOT know the server's static key in advance and learns it from the
/// handshake; the caller then verifies the key out-of-band before any
/// protocol traffic flows).
pub const PREAMBLE_XX: u8 = 0x02;

/// Read the 1-byte handshake-mode preamble from `stream`, using the default
/// `HANDSHAKE_TIMEOUT` budget (see [`read_handshake_preamble_with_timeout`]).
///
/// The preamble runs BEFORE any authentication, so it must be bounded by the
/// same absolute-deadline machinery as the handshake itself: an
/// unauthenticated peer that connects and sends nothing must not be able to
/// hold a connection thread + FD open. The preamble read gets its own full
/// budget, and the handshake that follows gets its own — the two are
/// independent deadlines, which only widens the worst case by one budget and
/// keeps the deadline plumbing in exactly one place.
///
/// # Errors
///
/// Returns [`TransportError::HandshakeTimeout`] if the peer does not send
/// the preamble byte within the `HANDSHAKE_TIMEOUT` budget, or any
/// propagated socket I/O error (EOF before the byte reads as
/// `UnexpectedEof`).
pub fn read_handshake_preamble(stream: &mut TcpStream) -> Result<u8, TransportError> {
    read_handshake_preamble_with_timeout(stream, HANDSHAKE_TIMEOUT)
}

/// [`read_handshake_preamble`] with an explicit total-duration budget.
/// Reuses the frame submodule's bounded read (1 byte), so the deadline
/// semantics are byte-for-byte the handshake's: a silent peer is cut off with
/// `HandshakeTimeout`, and a dribbling peer cannot stretch the read past the
/// deadline by keeping per-read timers alive.
///
/// # Errors
///
/// Returns [`TransportError::HandshakeTimeout`] if the peer does not send
/// the preamble byte within `timeout`, or any propagated socket I/O error.
pub fn read_handshake_preamble_with_timeout(
    stream: &mut TcpStream,
    timeout: Duration,
) -> Result<u8, TransportError> {
    let deadline = Instant::now() + timeout;
    let byte = read_handshake_exact(stream, deadline, 1)?;
    // The bounded read above returned exactly 1 byte, so the index is in
    // bounds by construction; the access is bounds-checked anyway (see the
    // slice helpers' comment) rather than panicking.
    at(&byte, 0, "preamble byte").copied()
}

#[cfg(test)]
mod tests {
    use snow::Builder;

    use super::{PREAMBLE_IK, PREAMBLE_XX, noise_prologue};
    use crate::error::TransportError;

    /// The prologue helper lays down the tag and then the mode byte, so the two
    /// modes produce prologues of the same length that differ only in that last
    /// byte — the difference that has to reach the handshake hash.
    #[test]
    fn prologue_binds_the_tag_and_the_mode() {
        let ik = noise_prologue(PREAMBLE_IK);
        let xx = noise_prologue(PREAMBLE_XX);

        assert_eq!(ik.len(), super::PROLOGUE_TAG.len() + 1);
        assert!(ik.starts_with(super::PROLOGUE_TAG));
        assert_eq!(ik.last().copied(), Some(PREAMBLE_IK));
        assert_eq!(xx.last().copied(), Some(PREAMBLE_XX));
        assert_ne!(ik, xx, "the mode byte must change the prologue");
    }

    /// Run one complete XX exchange in memory and report whether it succeeds.
    /// Pure crypto: no sockets, no timers, no sleeps. `snow` fails the
    /// exchange when the two prologues differ, because the prologue is mixed
    /// into the handshake hash that every AEAD tag in the exchange covers.
    fn run_xx_exchange(
        initiator_prologue: &[u8],
        responder_prologue: &[u8],
    ) -> Result<(), TransportError> {
        const PATTERN: &str = "Noise_XX_25519_AESGCM_SHA256";

        let initiator_sk = [7u8; 32];
        let responder_sk = [9u8; 32];

        let mut initiator = Builder::new(PATTERN.parse()?)
            .local_private_key(&initiator_sk)?
            .prologue(initiator_prologue)?
            .build_initiator()?;
        let mut responder = Builder::new(PATTERN.parse()?)
            .local_private_key(&responder_sk)?
            .prologue(responder_prologue)?
            .build_responder()?;

        // The handshake messages for an empty payload are well under a
        // kilobyte; snow caps every write at 65535, so these buffers are the
        // same bound the production frame path uses.
        let mut frame = vec![0u8; 65_535];
        let mut scratch = vec![0u8; 65_535];

        // Message 1 (-> e): unencrypted, so it is accepted either way.
        let n = initiator.write_message(&[], &mut frame)?;
        responder.read_message(&frame[..n], &mut scratch)?;

        // Message 2 (<- e, ee, s, es): the first encrypted message. Its tags
        // are keyed from the handshake hash, which includes the prologue — a
        // mismatched prologue fails right here.
        let n = responder.write_message(&[], &mut frame)?;
        initiator.read_message(&frame[..n], &mut scratch)?;

        // Message 3 (-> s, es): completes the exchange, proving the two sides
        // derived the same transport keys.
        let n = initiator.write_message(&[], &mut frame)?;
        responder.read_message(&frame[..n], &mut scratch)?;

        Ok(())
    }

    /// Same mode on both ends: the exchange completes, so the prologue the
    /// production flows use for one mode is agreed on by both of its halves.
    #[test]
    fn matching_prologues_complete_the_handshake() {
        let prologue = noise_prologue(PREAMBLE_XX);
        assert!(
            run_xx_exchange(&prologue, &prologue).is_ok(),
            "both sides using the same prologue must complete the handshake"
        );
    }

    /// Different modes on the two ends: the exchange fails, which is the whole
    /// point of binding the mode — an intermediary that rewrites the preamble
    /// byte cannot make the peers run different modes undetected.
    #[test]
    fn mismatched_prologues_fail_the_handshake() {
        assert!(
            run_xx_exchange(&noise_prologue(PREAMBLE_IK), &noise_prologue(PREAMBLE_XX)).is_err(),
            "a prologue mismatch must fail the handshake"
        );
    }
}
