//! The Noise IK and XX handshake state machines over a `TcpStream`.
//!
//! The state machines themselves are the stock `snow` patterns
//! `Noise_IK_25519_AESGCM_SHA256` ([`handshake_initiator`] /
//! [`handshake_responder`]) and `Noise_XX_25519_AESGCM_SHA256`
//! ([`handshake_initiator_xx`] / [`handshake_responder_xx`]). Nothing about
//! those patterns, their key schedule, or the cipher is modified. What this
//! module adds is application-level framing around the handshake: the 1-byte
//! unauthenticated mode preamble ([`PREAMBLE_IK`] / [`PREAMBLE_XX`]), the
//! 2-byte big-endian handshake length prefix (see the `frame` submodule), and
//! — in [`crate::noise`] — the 4-byte data-plane prefix carrying an
//! authenticated continuation byte.
//!
//! The preamble is safe precisely because it authorizes nothing: it only
//! selects which pattern to run. Both patterns authenticate both static keys,
//! so a peer that claims one mode and speaks another simply fails its
//! handshake — there is no mode confusion to exploit.
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
