//! The spine every handshake flow shares: the budget, the state machine, the ACL
//! check, and the hand-off to the data plane.
//!
//! `ik` and `xx` own nothing but their message ORDER; everything around that
//! order lives here, so the two modes cannot drift apart in how they are built,
//! authorized, bounded, or handed over. One budget covers the whole
//! unauthenticated phase — a responder spends a single absolute deadline on the
//! preamble read and then on the handshake — and every read and write is bounded
//! by the time remaining until it (see [`super::deadline`]).
//!
//! The state machine is constructed in exactly one place ([`build`]), so the
//! pattern string, the local static key and the prologue (the domain-separation
//! tag plus this mode's byte) cannot differ between a flow and its peer: both
//! halves of a mode call it with the same [`HandshakeMode`], and only the role and
//! the peer's static key differ.

use std::net::TcpStream;
use std::time::{Duration, Instant};

use snow::{Builder, HandshakeState};

use super::HandshakeMode;
use crate::error::TransportError;
use crate::noise::NoiseStream;

/// Default total-duration budget for a Noise handshake. The
/// `handshake_*_with_timeout` variants take their own budget; the plain
/// functions delegate to them with this constant.
///
/// The budget is an ABSOLUTE deadline, not a per-read socket timeout: every
/// handshake read is bounded by the time remaining until it (see
/// [`choreo_proto::read_exact_by`]), so a peer that dribbles bytes to keep
/// resetting a per-recv timeout is still cut off at the deadline. The responder
/// side spends ONE budget on the preamble read plus the handshake (see
/// [`super::handshake_responder_auto`]), so stalling before the mode byte buys no
/// extra time. It is cleared before the `TransportState` is handed over (see
/// [`finish`]); the data plane has no timeout by design — readers block until a
/// message or EOF, and the daemon's shutdown path closes sockets to unblock them.
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
/// completion log line, so one shared [`finish`] can report both sides.
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
    // A pattern string this build cannot parse is a compile-time constant that
    // went wrong, not anything a peer did — so it is reported as the local
    // invariant break it is, NOT as a snow protocol error. `TransportError::Noise`
    // is a handshake refusal (see `choreo-client-core`'s `is_handshake_refusal`),
    // so mapping this here would turn a defect in this build into enrollment
    // remediation for the user.
    let params = mode.pattern().parse().map_err(|e| {
        TransportError::InternalInvariant(format!(
            "the {mode} noise pattern {} failed to parse: {e}",
            mode.pattern()
        ))
    })?;
    let mut builder = Builder::new(params)
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
/// connection thread.
///
/// The mismatch is a [`TransportError::InternalInvariant`], not an
/// [`TransportError::InvalidFragment`]: only a defect in this build (a pattern
/// whose DH is not 25519, or a `snow` change) can produce a different length, so
/// it must never reach a caller as a peer's framing violation — which
/// `choreo-client-core` classifies as a handshake refusal and renders as
/// enrollment remediation.
pub(super) fn static_key_32(bytes: &[u8], what: &str) -> Result<[u8; 32], TransportError> {
    <[u8; 32]>::try_from(bytes).map_err(|_| {
        TransportError::InternalInvariant(format!(
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
/// Restores the unbounded I/O the deadline machinery replaced — BOTH socket
/// timeouts, since the handshake armed `SO_RCVTIMEO` and `SO_SNDTIMEO` — and
/// derives the `TransportState`. The data plane has no timeout by design:
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

#[cfg(test)]
mod tests {
    use super::static_key_32;
    use crate::error::TransportError;

    /// A static key that is not the 32 bytes this crate's `*_25519_*` patterns
    /// guarantee can only come from a defect here, so it is the crate's
    /// `InternalInvariant` — never `InvalidFragment`, which
    /// `choreo-client-core` classifies as a handshake refusal and renders as
    /// enrollment remediation. Pinned because that classification is what decides
    /// whether a user is told to re-enroll a key nobody judged.
    #[test]
    fn a_wrong_length_static_key_is_an_internal_invariant() {
        for len in [0usize, 1, 16, 31, 33, 64] {
            let bytes = vec![0xABu8; len];
            let err = static_key_32(&bytes, "peer static key").expect_err("wrong length");
            assert!(
                matches!(err, TransportError::InternalInvariant(_)),
                "a {len}-byte static key must be the local invariant break, got {err:?}"
            );
            assert!(
                err.to_string().contains("32-byte X25519 key"),
                "the refusal must name the expected length: {err}"
            );
        }
        // Exactly 32 bytes is the only length that yields a key.
        assert_eq!(
            static_key_32(&[0x5Au8; 32], "peer static key").expect("32 bytes is a key"),
            [0x5Au8; 32]
        );
    }
}
