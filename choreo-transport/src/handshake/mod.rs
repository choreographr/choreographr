//! The Noise IK and XX handshake state machines over a `TcpStream`.
//!
//! The state machines themselves are the stock `snow` patterns named by
//! [`HandshakeMode::pattern`]: `Noise_IK_25519_AESGCM_SHA256`
//! ([`handshake_initiator`] / [`handshake_responder`]) and
//! `Noise_XX_25519_AESGCM_SHA256` ([`handshake_initiator_xx`] /
//! [`handshake_responder_xx`]). Nothing about those patterns, their key
//! schedule, or the cipher is modified. What this module adds is
//! application-level framing around the handshake: the 1-byte mode preamble
//! that names the pattern ([`HandshakeMode`] / [`PREAMBLE_IK`] /
//! [`PREAMBLE_XX`]), the 2-byte big-endian handshake length prefix (see the
//! `frame` submodule), and — in [`crate::noise`] — a 4-byte data-plane prefix
//! whose ciphertext carries an authenticated continuation byte.
//!
//! The mode byte is written and read by the handshake entry points themselves,
//! not by their callers: an initiator writes the byte for the flow it is about
//! to run, and a responder reads it before running anything. A peer therefore
//! cannot announce one mode and then speak another, because there is no second
//! place for the two to disagree.
//!
//! The byte authorizes nothing on its own: it only selects which pattern to
//! run. Both patterns authenticate both static keys, so a peer that claims one
//! mode and speaks another simply fails its handshake — there is no mode
//! confusion to exploit. The selection is additionally part of the handshake
//! transcript: every flow mixes [`PROLOGUE_TAG`] plus its own mode byte into the
//! Noise prologue, which snow hashes into the handshake hash before any message
//! is processed. The mode is therefore authenticated by the same exchange it
//! selects — an intermediary that rewrites the cleartext preamble byte on the
//! path produces a prologue mismatch and a failed handshake, and a
//! cross-pattern handshake fails structurally rather than incidentally.
//!
//! One budget covers a connection's whole unauthenticated phase. A responder
//! takes a single absolute deadline from [`handshake_responder_auto`] and spends
//! it on the preamble read and then on the handshake, so a peer cannot hold a
//! connection thread and socket FD for two budgets by stalling before the mode
//! byte and then again inside the handshake. Every read and write is bounded by
//! the time remaining until that deadline —
//! `HANDSHAKE_TIMEOUT` by default, or an explicit budget via the `_with_timeout`
//! variants — through the one pair of bounded helpers in `deadline`.
//!
//! A flow is a sequence of message steps, not a copy of the framing: the two
//! message-order differences live in the `ik` and `xx` submodules, while the
//! mode type and its wire/prologue encoding live in `mode`, the framing in
//! `frame`, the deadline-bound socket I/O in `deadline`, and the parts every
//! flow shares — building the state machine, the ACL check, handing the socket
//! to the data plane — here.

mod deadline;
mod entry;
mod frame;
mod ik;
mod mode;
mod xx;

use std::net::TcpStream;
use std::time::{Duration, Instant};

use snow::{Builder, HandshakeState};

use crate::error::TransportError;
use crate::noise::NoiseStream;

pub use entry::{handshake_responder_auto, handshake_responder_auto_with_timeout};
pub use frame::MAX_HANDSHAKE_FRAME;
pub use ik::{
    handshake_initiator, handshake_initiator_with_timeout, handshake_responder,
    handshake_responder_with_timeout,
};
pub use mode::{HandshakeMode, PREAMBLE_IK, PREAMBLE_XX, PROLOGUE_LEN, PROLOGUE_TAG};
pub use xx::{
    handshake_initiator_xx, handshake_initiator_xx_with_timeout, handshake_responder_xx,
    handshake_responder_xx_with_timeout,
};

/// Default total-duration budget for a Noise handshake. The
/// `handshake_*_with_timeout` variants take their own budget; the plain
/// functions delegate to them with this constant.
///
/// The budget is an ABSOLUTE deadline, not a per-read socket timeout: every
/// handshake read is bounded by the time remaining until it (see the `frame`
/// submodule's read helper), so a peer that dribbles bytes to keep resetting
/// a per-recv timeout is still cut off at the deadline. The responder side
/// spends ONE budget on the preamble read plus the handshake (see
/// [`handshake_responder_auto`]), so stalling before the mode byte buys no
/// extra time. It is cleared before the `TransportState` is handed over; the
/// data plane has no timeout by design — readers block until a message or EOF,
/// and the daemon's shutdown path closes sockets to unblock them.
///
/// The handshake runs BEFORE any authentication (the responder's ACL check
/// happens mid-handshake), so without a bound an unauthenticated peer could
/// hold a connection thread + socket FD open forever by connecting and
/// sending nothing — a resource-exhaustion vector on the daemon's TCP
/// listener. 10 s is far beyond the single round trip a healthy handshake
/// needs (sub-millisecond on loopback, a few ms on a real network).
pub(crate) const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Upper bound on any budget a caller may pass to a `_with_timeout` variant
/// (one day).
///
/// The unauthenticated phase must never be settable to effectively-infinite,
/// and an unbounded `Duration` would also overflow the `Instant` arithmetic
/// that turns a budget into a deadline (`Instant + Duration` panics on
/// overflow). Clamping here keeps both properties without a panic path.
const MAX_HANDSHAKE_BUDGET: Duration = Duration::from_hours(24);

/// Which half of a handshake a flow is running. Carried only for the
/// completion log line, so one shared `finish` can report both sides.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Role {
    /// The dialing side, which writes the mode preamble first.
    Initiator,
    /// The accepting side, which reads the mode preamble first.
    Responder,
}

impl std::fmt::Display for Role {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Initiator => "initiator",
            Self::Responder => "responder",
        })
    }
}

/// Turn a caller's total-duration budget into an absolute deadline, clamped to
/// [`MAX_HANDSHAKE_BUDGET`] so the addition can never overflow.
pub(super) fn deadline_from(timeout: Duration) -> Instant {
    Instant::now() + timeout.min(MAX_HANDSHAKE_BUDGET)
}

/// Build the `snow` handshake state for `mode`.
///
/// The one place a handshake state machine is constructed, so the pattern
/// string, the local static key and the prologue (the domain-separation tag
/// plus this mode's byte) cannot differ between a flow and its peer: both
/// halves of a mode call this with the same [`HandshakeMode`], and only the
/// role and the peer's static key differ.
///
/// * `remote_pk` — the peer's static key, for the flows that know it up front
///   (IK's initiator). `None` for XX (whose point is learning it) and for every
///   responder.
pub(crate) fn build(
    mode: HandshakeMode,
    static_sk: &[u8; 32],
    remote_pk: Option<&[u8; 32]>,
    role: Role,
) -> Result<HandshakeState, TransportError> {
    // The prologue binds the domain-separation tag and this flow's mode byte
    // into the handshake hash; the peer's half derives the identical buffer
    // from the same HandshakeMode, so a peer that ran a different mode (or an
    // intermediary that rewrote the preamble byte) cannot complete this
    // handshake. It lives for the builder's borrow below.
    let prologue = mode.prologue();
    let mut builder = Builder::new(mode.pattern().parse()?)
        .local_private_key(static_sk)?
        .prologue(&prologue)?;
    if let Some(remote_pk) = remote_pk {
        builder = builder.remote_public_key(remote_pk)?;
    }
    let handshake = match role {
        Role::Initiator => builder.build_initiator()?,
        Role::Responder => builder.build_responder()?,
    };
    Ok(handshake)
}

/// Copy a Noise static public key (32 bytes for X25519) out of the handshake
/// state, as a plain array.
///
/// snow hands back the peer's static key as a slice whose length follows the DH
/// algorithm the pattern names. This crate only ever builds `*_25519_*`
/// patterns, so 32 bytes is guaranteed — but the copy is checked rather than
/// `copy_from_slice`'d, because a length mismatch there would panic in a
/// connection thread while this returns the crate's protocol error.
pub(super) fn static_key_32(bytes: &[u8], what: &str) -> Result<[u8; 32], TransportError> {
    <[u8; 32]>::try_from(bytes).map_err(|_| {
        TransportError::InvalidFragment(format!(
            "{what} is {} bytes, expected a 32-byte X25519 key",
            bytes.len()
        ))
    })
}

/// Run the responder's ACL check against the client static key the handshake
/// produced — the single implementation both modes share, so IK and XX
/// authorize identically.
///
/// A peer that never sent a static key is refused the same way as one the ACL
/// rejects: either way there is no client to authorize, and the connection is
/// closed before a single data-plane byte moves.
pub(super) fn authorize_client<F>(
    handshake: &HandshakeState,
    check_client: F,
    mode: HandshakeMode,
) -> Result<(), TransportError>
where
    F: FnOnce(&[u8; 32]) -> bool,
{
    let Some(client_pk) = handshake.get_remote_static() else {
        tracing::info!(%mode, "noise handshake rejected: client did not send a static key");
        return Err(TransportError::AuthFailed);
    };
    let pk = static_key_32(client_pk, "client static key")?;
    if !check_client(&pk) {
        tracing::info!(%mode, "noise handshake rejected: client not in ACL");
        return Err(TransportError::AuthFailed);
    }
    Ok(())
}

/// Hand a completed handshake's socket to the data plane.
///
/// Restores the unbounded I/O the `frame` helpers' deadline machinery replaced
/// — BOTH socket timeouts, since they armed `SO_RCVTIMEO` and `SO_SNDTIMEO` —
/// and derives the `TransportState`. The data plane has no timeout by design:
/// readers block until a message or EOF, and shutdown closes the socket to
/// unblock them.
///
/// The socket is also handed to the data plane with Nagle disabled
/// (`TCP_NODELAY`): the encrypted stream carries small, interactive messages
/// (streaming deltas, control traffic), so Nagle would stall each small write on
/// the peer's delayed ACK. This is the intrinsic guarantee — every IK and XX flow
/// funnels through here, so a held `NoiseStream` always runs `NODELAY`, whether or
/// not the caller tuned the raw socket before the handshake.
pub(super) fn finish(
    stream: TcpStream,
    handshake: HandshakeState,
    mode: HandshakeMode,
    role: Role,
) -> Result<NoiseStream, TransportError> {
    // Idempotent with any pre-handshake tuning the caller applied; see the doc
    // comment above for why it is set here too.
    crate::socket::tune_control_socket(&stream);
    stream.set_read_timeout(None)?;
    stream.set_write_timeout(None)?;
    let transport = handshake.into_transport_mode()?;
    tracing::debug!(%mode, %role, "noise handshake complete");
    Ok(NoiseStream::new(stream, transport))
}

/// Read the 1-byte mode preamble from `stream`, bounded by `deadline`.
///
/// The single read behind every responder entry point, so the byte's position,
/// its bound and its validation cannot differ between them.
pub(super) fn read_preamble(
    stream: &mut TcpStream,
    deadline: Instant,
) -> Result<HandshakeMode, TransportError> {
    let mut byte = [0u8; 1];
    deadline::read_handshake_exact(stream, deadline, &mut byte)?;
    // The bounded read above filled exactly one byte, so the index is in bounds
    // by construction; the access is bounds-checked anyway (see the slice
    // helpers) rather than panicking.
    let byte = *crate::buf::at(&byte, 0, "preamble byte")?;
    HandshakeMode::from_byte(byte).ok_or(TransportError::HandshakeModeUnknown { byte })
}

/// Reject a peer that asked for a mode other than `expected`.
pub(super) fn require_mode(
    got: HandshakeMode,
    expected: HandshakeMode,
) -> Result<(), TransportError> {
    if got == expected {
        Ok(())
    } else {
        Err(TransportError::HandshakeModeMismatch {
            expected: expected.byte(),
            got: got.byte(),
        })
    }
}
