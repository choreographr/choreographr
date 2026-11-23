//! The Noise XX handshake flows — initiator and responder.
//!
//! XX is the first-contact mode: the initiator does NOT know the responder's
//! static key in advance, so the key exchange takes three messages (XX's extra
//! round trip is exactly what transmits that static). Both flows keep only the
//! message ORDER; the state machine, the ACL check and the hand-off to the data
//! plane live in `super`, and the framing they call into lives in `super::frame`.
//!
//! The public entry points own the mode's 1-byte preamble: the initiator writes
//! [`HandshakeMode::Xx`]'s byte before its first message, and the responder reads
//! it back and rejects a peer that asked for the other mode.

use std::net::TcpStream;
use std::time::{Duration, Instant};

use super::entry::responder_requiring;
use super::frame::{Wire, write_preamble};
use super::{
    HANDSHAKE_TIMEOUT, HandshakeMode, Role, authorize_client, build, deadline_from, static_key_32,
};
use crate::error::TransportError;
use crate::noise::NoiseStream;

/// The flow this module implements (see the IK module's counterpart).
const MODE: HandshakeMode = HandshakeMode::Xx;

/// Perform the Noise XX handshake as the **initiator** (client side, first
/// contact), using the default `HANDSHAKE_TIMEOUT` budget.
///
/// Unlike [`super::handshake_initiator`] (Noise IK), the XX initiator does NOT
/// know the server's static key in advance — that is the point of
/// first-contact mode. The server's static public key is transmitted
/// (encrypted) in handshake message 2 and is returned alongside the transport
/// so the caller can verify it out-of-band (base64 confirmation) BEFORE any
/// encrypted protocol traffic other than the handshake itself flows.
///
/// * `stream` — the already-connected TCP stream.
/// * `static_sk` — the client's transport.sec (32-byte X25519 secret key).
///
/// Writes the XX mode preamble first, so the caller only has to dial and call
/// this. On success returns `(NoiseStream, [u8; 32])` — the encrypted transport
/// and the server's static public key learned from the handshake. Fails if
/// the server never transmitted its static key (an XX protocol-contract
/// violation, surfaced as `InvalidFragment`).
///
/// # Errors
///
/// Returns [`TransportError::HandshakeTimeout`] if the exchange exceeds the
/// `HANDSHAKE_TIMEOUT` budget, [`TransportError::InvalidFragment`] if the
/// server omits its static key, or the snow/`TransportError` errors raised
/// by the exchange itself.
pub fn handshake_initiator_xx(
    stream: TcpStream,
    static_sk: &[u8; 32],
) -> Result<(NoiseStream, [u8; 32]), TransportError> {
    handshake_initiator_xx_with_timeout(stream, static_sk, HANDSHAKE_TIMEOUT)
}

/// [`handshake_initiator_xx`] with an explicit total-duration budget for the
/// WHOLE handshake, preamble write included (see `HANDSHAKE_TIMEOUT`; a budget
/// beyond one day is clamped). Reuses the exact same absolute-deadline plumbing
/// as the IK variants — the shared frame reader and writer — so the XX timeout
/// semantics are identical: a silent or dribbling peer is cut off at the
/// deadline with `HandshakeTimeout`, and both socket timeouts are cleared
/// before the data plane.
///
/// # Errors
///
/// Returns [`TransportError::HandshakeTimeout`] if the exchange exceeds
/// `timeout`, [`TransportError::InvalidFragment`] if the server omits its
/// static key, or the snow/`TransportError` errors raised by the exchange
/// itself.
pub fn handshake_initiator_xx_with_timeout(
    stream: TcpStream,
    static_sk: &[u8; 32],
    timeout: Duration,
) -> Result<(NoiseStream, [u8; 32]), TransportError> {
    initiator(stream, static_sk, deadline_from(timeout))
}

/// The XX initiator under an absolute deadline: preamble write, then the three
/// handshake messages.
pub(super) fn initiator(
    mut stream: TcpStream,
    static_sk: &[u8; 32],
    deadline: Instant,
) -> Result<(NoiseStream, [u8; 32]), TransportError> {
    // XX's initiator deliberately passes NO remote static key: the server's is
    // unknown until the handshake reveals it, which is the whole point of this
    // mode (IK passes Some here, which is what shortens it to two messages).
    let mut handshake = build(MODE, static_sk, None, Role::Initiator)?;

    // Declare the mode before any handshake message (see the IK initiator).
    write_preamble(&mut stream, deadline, MODE)?;

    let mut wire = Wire::new();
    // Message 1 (-> e): the client's ephemeral key only.
    wire.write(&mut handshake, &mut stream, deadline)?;
    // Message 2 (<- e, ee, s, es): the server's ephemeral AND static keys, with
    // the static authenticated by the DH operations. After reading this the
    // initiator can extract the server's static.
    wire.read(&mut handshake, &mut stream, deadline)?;
    // Message 3 (-> s, es): the client's static key, authenticating us to the
    // server (whose responder-side ACL check consumes it).
    wire.write(&mut handshake, &mut stream, deadline)?;

    // Learn the server's static key — the entire reason this mode exists, and
    // it has to be read BEFORE the state machine becomes a transport. XX always
    // transmits `s` in message 2, so an absent value is a protocol contract
    // violation, not an authentication failure; InvalidFragment is the closest
    // honest variant (the responder side DOES use AuthFailed for the analogous
    // check, because there it gates the ACL decision).
    let server_pk = match handshake.get_remote_static() {
        Some(bytes) => static_key_32(bytes, "XX server static key")?,
        None => {
            return Err(TransportError::HandshakeStaticMissing);
        }
    };

    let noise = super::finish(stream, handshake, MODE, Role::Initiator)?;
    Ok((noise, server_pk))
}

/// Perform the Noise XX handshake as the **responder** (server side,
/// first-contact), using the default `HANDSHAKE_TIMEOUT` budget.
///
/// Mirrors [`super::handshake_responder`] (Noise IK) except for the message
/// flow: XX is 3 messages instead of IK's 2, and the client's static key
/// arrives with message 3 (not message 1), so the `check_client` ACL closure
/// runs AFTER the full key exchange — see the comment at the check below for why
/// that is still safe. Reads the XX mode preamble first and rejects a peer that
/// asked for another mode; [`super::handshake_responder_auto`] is the entry point
/// that dispatches on the byte instead.
///
/// # Errors
///
/// Returns [`TransportError::HandshakeModeMismatch`] if the peer requested a
/// different mode, [`TransportError::AuthFailed`] if the client is not
/// authorized or never sent its static key, [`TransportError::HandshakeTimeout`]
/// if the exchange exceeds the `HANDSHAKE_TIMEOUT` budget, or the snow/
/// `TransportError` I/O and protocol errors raised by the exchange itself.
pub fn handshake_responder_xx<F>(
    stream: TcpStream,
    static_sk: &[u8; 32],
    check_client: F,
) -> Result<NoiseStream, TransportError>
where
    F: FnOnce(&[u8; 32]) -> bool,
{
    handshake_responder_xx_with_timeout(stream, static_sk, check_client, HANDSHAKE_TIMEOUT)
}

/// [`handshake_responder_xx`] with an explicit total-duration budget for the
/// WHOLE handshake, preamble read included (see `HANDSHAKE_TIMEOUT`; a budget
/// beyond one day is clamped). Same absolute-deadline plumbing as the IK
/// responder — every read AND write is bounded by the time remaining until the
/// deadline. A caller that needs a shorter (or longer) bound than the default
/// passes it here; the budget is a hard ceiling, never a hint.
///
/// # Errors
///
/// As [`handshake_responder_xx`], with the budget given by `timeout`.
pub fn handshake_responder_xx_with_timeout<F>(
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

/// The XX responder's frames, with the preamble already consumed and the mode
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
    // Message 1 (<- e): read the client's ephemeral key. Every read is bounded
    // by the remaining budget — pre-authentication, exactly like IK.
    wire.read(&mut handshake, &mut stream, deadline)?;

    // Message 2 (-> e, ee, s, es): our ephemeral + static keys. Unlike IK, this
    // MUST be written before the ACL check — the client's static only arrives
    // with message 3, and message 2 is what carries the server's static to the
    // client. This is inherent to XX (the server cannot know who it is talking
    // to until the client finishes authenticating), and it is not a disclosure
    // risk: msg2's static is encrypted to the client's ephemeral key, so a
    // passive observer learns nothing, and an active attacker would have to
    // complete the key exchange to decrypt it — at which point its own static is
    // exposed to the ACL check.
    wire.write(&mut handshake, &mut stream, deadline)?;

    // Message 3 (-> s, es): the client's static key arrives here — this is the
    // point where the responder finally learns who it is talking to, and the
    // shared ACL check runs BEFORE the transport is handed over, so a rejected
    // client gets a closed socket and can never send or receive a data-plane
    // byte.
    wire.read(&mut handshake, &mut stream, deadline)?;
    authorize_client(&handshake, check_client, MODE)?;

    super::finish(stream, handshake, MODE, Role::Responder)
}
