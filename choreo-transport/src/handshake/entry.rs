//! The responder entry points: the server-side "run whatever mode the peer
//! asked for" facade.
//!
//! A responder does not know in advance which mode a client will request, so
//! [`handshake_responder_auto`] reads the 1-byte mode preamble and dispatches on
//! it. This module owns that dispatch and the mode-locked wrapper the per-mode
//! entry points use; the shared building blocks it composes — the state-machine
//! builder, the ACL check, the hand-off to the data plane, the preamble read and
//! the deadline — live in [`super`].
//!
//! One deadline covers the whole unauthenticated phase: the preamble read AND
//! the handshake, so a peer that stalls before the mode byte buys no extra time.

use std::net::TcpStream;
use std::time::{Duration, Instant};

use super::support::{HANDSHAKE_TIMEOUT, deadline_from};
use super::{HandshakeMode, ik, read_preamble, require_mode, xx};
use crate::error::TransportError;
use crate::noise::NoiseStream;

/// Read the mode preamble and run the responder flow it selects, optionally
/// requiring a specific mode.
///
/// The one dispatch site for the responder side: `expected: Some(mode)` is a
/// mode-locked responder (which must refuse a peer that asked for the other
/// flow), `None` is the server entry point that accepts either. The SAME
/// deadline covers the preamble read and the handshake in both cases.
fn responder_dispatch<F>(
    mut stream: TcpStream,
    static_sk: &[u8; 32],
    check_client: F,
    deadline: Instant,
    expected: Option<HandshakeMode>,
) -> Result<(NoiseStream, HandshakeMode), TransportError>
where
    F: FnOnce(&[u8; 32]) -> bool,
{
    let mode = read_preamble(&mut stream, deadline)?;
    if let Some(expected) = expected {
        require_mode(mode, expected)?;
    }
    // Both arms take the stream by value and the identical ACL closure, so XX
    // is authorized exactly like IK, and both inherit the remaining budget.
    let noise = match mode {
        HandshakeMode::Ik => ik::responder(stream, static_sk, check_client, deadline)?,
        HandshakeMode::Xx => xx::responder(stream, static_sk, check_client, deadline)?,
    };
    Ok((noise, mode))
}

/// The responder behind a mode-locked entry point
/// (`handshake_responder` / `handshake_responder_xx`): read the preamble,
/// require `mode`, run that flow.
pub(super) fn responder_requiring<F>(
    stream: TcpStream,
    static_sk: &[u8; 32],
    check_client: F,
    deadline: Instant,
    mode: HandshakeMode,
) -> Result<NoiseStream, TransportError>
where
    F: FnOnce(&[u8; 32]) -> bool,
{
    responder_dispatch(stream, static_sk, check_client, deadline, Some(mode))
        .map(|(noise, _mode)| noise)
}

/// Perform the Noise handshake the peer asked for as the **responder** (server
/// side), using the default `HANDSHAKE_TIMEOUT` budget.
///
/// Reads the 1-byte mode preamble, then runs the matching IK or XX responder —
/// one deadline covering both, so a peer that stalls before the mode byte gets no
/// extra time before the handshake's own budget starts. Returns the transport and
/// the mode that was run, so a caller can log or account for the choice.
///
/// This is the server-side entry point: a responder does not know in advance
/// which mode a client will request, and this dispatches on the byte rather than
/// making every caller re-implement the read-then-match.
///
/// * `stream` — the accepted TCP stream from the listener.
/// * `static_sk` — the server's transport secret (32-byte X25519 secret key).
/// * `check_client` — receives the client's 32-byte static public key and
///   returns `true` if the client is authorized.
///
/// # Errors
///
/// [`TransportError::HandshakeModeUnknown`] if the preamble names no mode,
/// [`TransportError::AuthFailed`] if the client is not authorized or never sent
/// its static key, [`TransportError::HandshakeTimeout`] if the preamble plus
/// handshake exceed the `HANDSHAKE_TIMEOUT` budget, or the snow/`TransportError`
/// I/O and protocol errors raised by the exchange itself.
pub fn handshake_responder_auto<F>(
    stream: TcpStream,
    static_sk: &[u8; 32],
    check_client: F,
) -> Result<(NoiseStream, HandshakeMode), TransportError>
where
    F: FnOnce(&[u8; 32]) -> bool,
{
    handshake_responder_auto_with_timeout(stream, static_sk, check_client, HANDSHAKE_TIMEOUT)
}

/// [`handshake_responder_auto`] with an explicit total-duration budget for the
/// whole unauthenticated phase (preamble read plus handshake). A caller that
/// needs a shorter (or longer) bound than the default passes it here; the budget
/// is a hard ceiling on the whole exchange, never a hint, and one beyond one day
/// is clamped.
///
/// # Errors
///
/// As [`handshake_responder_auto`], with the budget given by `timeout`.
pub fn handshake_responder_auto_with_timeout<F>(
    stream: TcpStream,
    static_sk: &[u8; 32],
    check_client: F,
    timeout: Duration,
) -> Result<(NoiseStream, HandshakeMode), TransportError>
where
    F: FnOnce(&[u8; 32]) -> bool,
{
    responder_dispatch(
        stream,
        static_sk,
        check_client,
        deadline_from(timeout),
        None,
    )
}
