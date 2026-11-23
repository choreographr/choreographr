//! The Noise IK handshake flows — initiator and responder.
//!
//! IK is the authenticated mode: the initiator already knows the responder's
//! static key, which is what lets the whole exchange fit in two messages. Both
//! flows keep only the message ORDER; the state machine, the ACL check and the
//! hand-off to the data plane live in `super`, and the framing they call into
//! lives in `super::frame`.
//!
//! The public entry points own the mode's 1-byte preamble: the initiator writes
//! [`HandshakeMode::Ik`]'s byte before its first message, and the responder reads
//! it back and rejects a peer that asked for the other mode. The
//! `initiator`/`responder` functions below are the frame-level flows the entry
//! points and `super::handshake_responder_auto` share.

use std::net::TcpStream;
use std::time::{Duration, Instant};

use super::frame::{Wire, write_preamble};
use super::{
    HANDSHAKE_TIMEOUT, HandshakeMode, Role, authorize_client, build, deadline_from,
    responder_requiring,
};
use crate::error::TransportError;
use crate::noise::NoiseStream;

/// The flow this module implements. Every entry point and both halves of the
/// exchange take it from here, so the mode the wire declares and the mode the
/// state machine runs are one value.
const MODE: HandshakeMode = HandshakeMode::Ik;

/// Perform the Noise IK handshake as the **initiator** (client side), using
/// the default `HANDSHAKE_TIMEOUT` budget.
///
/// * `stream` — the already-connected TCP stream.
/// * `static_sk` — the client's transport.sec (32-byte X25519 secret key).
/// * `server_pk` — the server's transport public key (32-byte X25519 key, as
///   printed by `choreographr pubkey` on the server).
///
/// Writes the IK mode preamble first, so the caller only has to dial and call
/// this. On success returns a `NoiseStream` ready for encrypted message I/O.
///
/// # Errors
///
/// Returns [`TransportError::HandshakeTimeout`] if the exchange exceeds the
/// `HANDSHAKE_TIMEOUT` budget, or the snow/`TransportError` I/O and
/// protocol errors raised by the exchange itself.
pub fn handshake_initiator(
    stream: TcpStream,
    static_sk: &[u8; 32],
    server_pk: &[u8; 32],
) -> Result<NoiseStream, TransportError> {
    handshake_initiator_with_timeout(stream, static_sk, server_pk, HANDSHAKE_TIMEOUT)
}

/// [`handshake_initiator`] with an explicit total-duration budget for the
/// WHOLE handshake, preamble write included (see `HANDSHAKE_TIMEOUT` for why the
/// budget is absolute, not per-read; a budget beyond one day is clamped).
/// A caller that needs a shorter (or longer) bound than the default passes it
/// here; the budget is a hard ceiling on the whole exchange, never a hint.
///
/// # Errors
///
/// Returns [`TransportError::HandshakeTimeout`] if the exchange exceeds
/// `timeout`, or the snow/`TransportError` I/O and protocol errors raised
/// by the exchange itself.
pub fn handshake_initiator_with_timeout(
    stream: TcpStream,
    static_sk: &[u8; 32],
    server_pk: &[u8; 32],
    timeout: Duration,
) -> Result<NoiseStream, TransportError> {
    initiator(stream, static_sk, server_pk, deadline_from(timeout))
}

/// The IK initiator under an absolute deadline: preamble write, then the two
/// handshake messages.
pub(super) fn initiator(
    mut stream: TcpStream,
    static_sk: &[u8; 32],
    server_pk: &[u8; 32],
    deadline: Instant,
) -> Result<NoiseStream, TransportError> {
    // IK's initiator knows the server's static key up front, which is exactly
    // what makes the two-message exchange possible (XX passes None here).
    let mut handshake = build(MODE, static_sk, Some(server_pk), Role::Initiator)?;

    // Declare the mode before any handshake message: the responder dispatches on
    // this byte, and both ends bind it into the prologue the state machine just
    // took.
    write_preamble(&mut stream, deadline, MODE)?;

    let mut wire = Wire::new();
    // Message 1 (-> e, es, s, ss): the ephemeral key plus the client's static,
    // encrypted to the server.
    wire.write(&mut handshake, &mut stream, deadline)?;
    // Message 2 (<- e, ee, se).
    wire.read(&mut handshake, &mut stream, deadline)?;

    super::finish(stream, handshake, MODE, Role::Initiator)
}

/// Perform the Noise IK handshake as the **responder** (server side), using
/// the default `HANDSHAKE_TIMEOUT` budget.
///
/// Reads the IK mode preamble first and rejects a peer that asked for another
/// mode; a caller that does not know which mode the client will request wants
/// [`super::handshake_responder_auto`] instead, which dispatches on the byte.
///
/// * `stream` — the accepted TCP stream from the listener.
/// * `static_sk` — the server's transport.sec (32-byte X25519 secret key).
/// * `check_client` — a closure that receives the client's 32-byte static
///   public key (extracted from the handshake) and returns `true` if the
///   client is authorized.
///
/// On success returns a `NoiseStream` ready for encrypted message I/O.
/// On authentication failure the connection is closed and `Err(AuthFailed)` is returned.
///
/// # Errors
///
/// Returns [`TransportError::HandshakeModeMismatch`] if the peer requested a
/// different mode, [`TransportError::AuthFailed`] if the client is not
/// authorized or never sent its static key, [`TransportError::HandshakeTimeout`]
/// if the exchange exceeds the `HANDSHAKE_TIMEOUT` budget, or the snow/
/// `TransportError` I/O and protocol errors raised by the exchange itself.
pub fn handshake_responder<F>(
    stream: TcpStream,
    static_sk: &[u8; 32],
    check_client: F,
) -> Result<NoiseStream, TransportError>
where
    F: FnOnce(&[u8; 32]) -> bool,
{
    handshake_responder_with_timeout(stream, static_sk, check_client, HANDSHAKE_TIMEOUT)
}

/// [`handshake_responder`] with an explicit total-duration budget for the WHOLE
/// handshake, preamble read included (see `HANDSHAKE_TIMEOUT`; a budget beyond
/// one day is clamped). A caller that needs a shorter (or longer) bound than the
/// default passes it here; the budget is a hard ceiling on the whole exchange,
/// never a hint.
///
/// # Errors
///
/// As [`handshake_responder`], with the budget given by `timeout`.
pub fn handshake_responder_with_timeout<F>(
    stream: TcpStream,
    static_sk: &[u8; 32],
    check_client: F,
    timeout: Duration,
) -> Result<NoiseStream, TransportError>
where
    F: FnOnce(&[u8; 32]) -> bool,
{
    responder_requiring(
        stream,
        static_sk,
        check_client,
        deadline_from(timeout),
        MODE,
    )
}

/// The IK responder's frames, with the preamble already consumed and the mode
/// already validated by the caller.
pub(super) fn responder<F>(
    mut stream: TcpStream,
    static_sk: &[u8; 32],
    check_client: F,
    deadline: Instant,
) -> Result<NoiseStream, TransportError>
where
    F: FnOnce(&[u8; 32]) -> bool,
{
    let mut handshake = build(MODE, static_sk, None, Role::Responder)?;

    let mut wire = Wire::new();
    // Message 1 (<- e, es, s, ss): the ACL check has not happened yet, so a
    // peer that connects and stalls (or dribbles) must not be able to hold this
    // thread and FD forever — every read is bounded by the time remaining until
    // `deadline`.
    wire.read(&mut handshake, &mut stream, deadline)?;

    // IK carries the client's static key in message 1, so the ACL decision
    // happens here — before the responder has sent anything it would have to
    // take back.
    authorize_client(&handshake, check_client, MODE)?;

    // Message 2 (-> e, ee, se): one bounded write of the prefix and body, so a
    // client that stops reading after msg1 cannot hold the responder's writer
    // past `deadline`.
    wire.write(&mut handshake, &mut stream, deadline)?;

    super::finish(stream, handshake, MODE, Role::Responder)
}
