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
//! `support::HANDSHAKE_TIMEOUT` by default, or an explicit budget via the
//! `_with_timeout` variants — through the one pair of bounded helpers in
//! `deadline`.
//!
//! A flow is a sequence of message steps, not a copy of the framing: the two
//! message-order differences live in the `ik` and `xx` submodules, while the
//! mode type and its wire/prologue encoding live in `mode`, the framing in
//! `frame`, the deadline-bound socket I/O in `deadline`, and the spine every flow
//! shares — the budget, the state-machine builder, the ACL check, the hand-off to
//! the data plane — in `support`. What stays here is the module's public facade
//! and the two wire-level helpers a responder entry point composes: the preamble
//! read and the mode requirement.

mod deadline;
mod entry;
mod frame;
mod ik;
mod mode;
mod support;
mod xx;

use std::net::TcpStream;
use std::time::Instant;

use crate::error::TransportError;

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

// Re-exported for the crate's own data-plane tests, which build a handshake state
// directly to encrypt raw fragments the public API cannot express. A non-test
// build has no caller, so the alias is `cfg(test)`-only and the real items stay
// reachable inside `support`.
#[cfg(test)]
pub(crate) use support::{Role, build};

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
