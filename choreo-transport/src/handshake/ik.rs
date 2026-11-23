//! The Noise IK handshake flows — initiator and responder.
//!
//! IK is the authenticated mode: the initiator already knows the responder's
//! static key, which is what lets the whole exchange fit in two messages.
//! Both flows hand every encoded message to the shared frame plumbing in the
//! sibling `frame` module, so the length prefix, the bounded read and the
//! coalesced bounded write have a single implementation.
//!
//! The public entry points own the mode's 1-byte preamble: the initiator writes
//! [`HandshakeMode::Ik`]'s byte before its first message, and the responder reads
//! it back and rejects a peer that asked for the other mode. The
//! `initiator`/`responder` functions below are the frame-level flows the entry
//! points and [`super::handshake_responder_auto`] share.

use std::net::TcpStream;
use std::time::{Duration, Instant};
use tracing::{debug, info};

use super::frame::{
    HANDSHAKE_FRAME_BUF_LEN, LEN_PREFIX_LEN, MAX_HANDSHAKE_FRAME, read_handshake_message,
    slice_mut, write_handshake_frame, write_preamble,
};
use super::{HANDSHAKE_TIMEOUT, HandshakeMode, read_preamble, require_mode};
use crate::error::TransportError;
use crate::noise::NoiseStream;

/// The stock snow pattern string for both IK flows. Shared by the initiator
/// and the responder so the two halves of one handshake can never drift onto
/// different patterns.
const PATTERN: &str = "Noise_IK_25519_AESGCM_SHA256";

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
/// budget is absolute, not per-read). Exposed so integration tests can exercise
/// the timeout path in milliseconds instead of waiting out the 10 s default.
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
    initiator(stream, static_sk, server_pk, Instant::now() + timeout)
}

/// The IK initiator under an absolute deadline: preamble write, then the two
/// handshake messages.
pub(super) fn initiator(
    mut stream: TcpStream,
    static_sk: &[u8; 32],
    server_pk: &[u8; 32],
    deadline: Instant,
) -> Result<NoiseStream, TransportError> {
    use snow::Builder;

    // The prologue binds the domain-separation tag and this flow's mode byte
    // into the handshake hash; the IK responder derives the identical buffer
    // from the same HandshakeMode::Ik, so a peer that ran a different mode (or an
    // intermediary that rewrote the preamble byte) cannot complete this
    // handshake.
    let prologue = HandshakeMode::Ik.prologue();

    let mut handshake = Builder::new(PATTERN.parse()?)
        .local_private_key(static_sk)?
        .remote_public_key(server_pk)?
        .prologue(&prologue)?
        .build_initiator()?;

    // Declare the mode before any handshake message: the responder dispatches on
    // this byte, and both ends bind it into the prologue above.
    write_preamble(&mut stream, deadline, HandshakeMode::Ik)?;

    // Message 1 (-> e, es, s, ss): the ephemeral key plus the client's static,
    // encrypted to the server. It leaves through the shared frame writer, so
    // the prefix and body go out as ONE bounded write — a server that stops
    // reading mid-handshake cannot hold this writer past the deadline. The
    // same buffer then carries the read side's decryption output.
    let mut buf = vec![0u8; HANDSHAKE_FRAME_BUF_LEN];
    let n = handshake.write_message(
        &[],
        slice_mut(&mut buf, LEN_PREFIX_LEN.., "handshake frame body")?,
    )?;
    write_handshake_frame(&mut stream, deadline, &mut buf, n)?;

    // Message 2 (<- e, ee, se): read through the shared frame reader, whose
    // every read is bounded by the time remaining until `deadline`, so a
    // stalled server cannot hold the client forever.
    read_handshake_message(
        &mut handshake,
        &mut stream,
        deadline,
        slice_mut(&mut buf, LEN_PREFIX_LEN.., "handshake frame body")?,
    )?;

    // Handshake complete — restore the unbounded data-plane I/O. Clear BOTH
    // socket timeouts: the deadline helpers armed SO_RCVTIMEO and SO_SNDTIMEO
    // to the remaining budget, and neither belongs in the data plane (readers
    // block until a message or EOF by design).
    stream.set_read_timeout(None)?;
    stream.set_write_timeout(None)?;

    let transport = handshake.into_transport_mode()?;
    debug!("Noise IK handshake complete (initiator)");
    Ok(NoiseStream::new(stream, transport))
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
/// handshake, preamble read included (see `HANDSHAKE_TIMEOUT`). Exposed so
/// integration tests can exercise the timeout path in milliseconds instead of
/// waiting out the 10 s default.
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
    responder_with_mode_check(stream, static_sk, check_client, Instant::now() + timeout)
}

/// Read the preamble under `deadline`, require IK, then run the flow.
fn responder_with_mode_check<F>(
    mut stream: TcpStream,
    static_sk: &[u8; 32],
    check_client: F,
    deadline: Instant,
) -> Result<NoiseStream, TransportError>
where
    F: FnOnce(&[u8; 32]) -> bool,
{
    let mode = read_preamble(&mut stream, deadline)?;
    require_mode(mode, HandshakeMode::Ik)?;
    responder(stream, static_sk, check_client, deadline)
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
    use snow::Builder;

    // Same prologue as the IK initiator above — the two halves of the IK flow
    // must derive an identical handshake hash, so both take HandshakeMode::Ik.
    let prologue = HandshakeMode::Ik.prologue();

    let mut handshake = Builder::new(PATTERN.parse()?)
        .local_private_key(static_sk)?
        .prologue(&prologue)?
        .build_responder()?;

    // Snow's decrypted handshake payloads land here — empty for these
    // patterns, but the buffer is still the read call's output. It is sized to
    // the frame cap, which is the largest body a peer may declare.
    let mut out_buf = vec![0u8; MAX_HANDSHAKE_FRAME];

    // Message 1 (<- e, es, s, ss): the ACL check has not happened yet, so a
    // peer that connects and stalls (or dribbles) must not be able to hold
    // this thread and FD forever — every read is bounded by the time
    // remaining until `deadline`.
    read_handshake_message(&mut handshake, &mut stream, deadline, &mut out_buf)?;

    // Extract client's static public key for the ACL check.
    if let Some(client_pk) = handshake.get_remote_static() {
        let mut pk = [0u8; 32];
        pk.copy_from_slice(client_pk);
        if !check_client(&pk) {
            info!("Noise IK handshake rejected: client not in ACL");
            return Err(TransportError::AuthFailed);
        }
    } else {
        info!("Noise IK handshake rejected: client did not send static key");
        return Err(TransportError::AuthFailed);
    }

    // Message 2 (-> e, ee, se): one bounded write of the prefix and body, so a
    // client that stops reading after msg1 cannot hold the responder's writer
    // past `deadline`.
    let mut buf = vec![0u8; HANDSHAKE_FRAME_BUF_LEN];
    let n = handshake.write_message(
        &[],
        slice_mut(&mut buf, LEN_PREFIX_LEN.., "handshake frame body")?,
    )?;
    write_handshake_frame(&mut stream, deadline, &mut buf, n)?;

    // Handshake complete — restore the unbounded data-plane I/O: clear BOTH
    // timeouts (see `initiator`).
    stream.set_read_timeout(None)?;
    stream.set_write_timeout(None)?;

    let transport = handshake.into_transport_mode()?;
    debug!("Noise IK handshake complete (responder)");
    Ok(NoiseStream::new(stream, transport))
}
