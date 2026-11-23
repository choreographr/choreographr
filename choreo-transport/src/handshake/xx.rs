//! The Noise XX handshake flows — initiator and responder.
//!
//! XX is the first-contact mode: the initiator does NOT know the responder's
//! static key in advance, so the key exchange takes three messages (XX's
//! extra round trip is exactly what transmits that static). Both flows hand
//! every encoded message to the shared frame plumbing in the sibling `frame`
//! module, so the length prefix, the bounded read and the coalesced bounded
//! write have a single implementation shared with the IK flows.
//!
//! The public entry points own the mode's 1-byte preamble: the initiator writes
//! [`HandshakeMode::Xx`]'s byte before its first message, and the responder reads
//! it back and rejects a peer that asked for the other mode.

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

/// The stock snow pattern string for both XX flows. Shared by the initiator
/// and the responder so the two halves of one handshake can never drift onto
/// different patterns.
const PATTERN: &str = "Noise_XX_25519_AESGCM_SHA256";

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
/// WHOLE handshake, preamble write included (see `HANDSHAKE_TIMEOUT`). Reuses
/// the exact same absolute-deadline plumbing as the IK variants — the shared
/// frame reader and writer — so the XX timeout semantics are identical: a silent
/// or dribbling peer is cut off at the deadline with `HandshakeTimeout`, and both
/// socket timeouts are cleared before the data plane.
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
    initiator(stream, static_sk, Instant::now() + timeout)
}

/// The XX initiator under an absolute deadline: preamble write, then the three
/// handshake messages.
pub(super) fn initiator(
    mut stream: TcpStream,
    static_sk: &[u8; 32],
    deadline: Instant,
) -> Result<(NoiseStream, [u8; 32]), TransportError> {
    use snow::Builder;

    // The prologue binds the domain-separation tag and this flow's mode byte
    // into the handshake hash; the XX responder derives the identical buffer
    // from the same HandshakeMode::Xx, so the mode is part of the authenticated
    // transcript rather than a claim the wire makes unchecked.
    let prologue = HandshakeMode::Xx.prologue();

    // XX initiator: NO `.remote_public_key()` — the server's static key is
    // unknown until the handshake reveals it (IK hard-codes it here, which
    // is what makes IK a 2-message pattern instead of XX's 3).
    let mut handshake = Builder::new(PATTERN.parse()?)
        .local_private_key(static_sk)?
        .prologue(&prologue)?
        .build_initiator()?;

    // Declare the mode before any handshake message (see the IK initiator).
    write_preamble(&mut stream, deadline, HandshakeMode::Xx)?;

    // One buffer carries every outbound frame and every decryption output, so
    // the three messages allocate nothing extra.
    let mut buf = vec![0u8; HANDSHAKE_FRAME_BUF_LEN];

    // Message 1 (-> e): the client's ephemeral key only. The write side is
    // bounded exactly like IK — prefix and body leave as ONE bounded write.
    let n = handshake.write_message(
        &[],
        slice_mut(&mut buf, LEN_PREFIX_LEN.., "handshake frame body")?,
    )?;
    write_handshake_frame(&mut stream, deadline, &mut buf, n)?;

    // Message 2 (<- e, ee, s, es): the server's ephemeral AND static keys,
    // with the static authenticated by the DH operations. After reading this
    // the initiator can extract the server's static via get_remote_static().
    read_handshake_message(
        &mut handshake,
        &mut stream,
        deadline,
        slice_mut(&mut buf, LEN_PREFIX_LEN.., "handshake frame body")?,
    )?;

    // Message 3 (-> s, es): the client's static key, authenticating us to
    // the server (the server's responder-side ACL check consumes it).
    let n = handshake.write_message(
        &[],
        slice_mut(&mut buf, LEN_PREFIX_LEN.., "handshake frame body")?,
    )?;
    write_handshake_frame(&mut stream, deadline, &mut buf, n)?;

    // Handshake complete — restore the unbounded data-plane I/O (see
    // `super::ik::initiator`: BOTH timeouts must be cleared, the deadline
    // helpers armed SO_RCVTIMEO and SO_SNDTIMEO).
    stream.set_read_timeout(None)?;
    stream.set_write_timeout(None)?;

    // Learn the server's static key — the entire reason this mode exists.
    // XX always transmits `s` in message 2, so an absent value is a protocol
    // contract violation, not an authentication failure; InvalidFragment is
    // the closest honest variant (the responder side DOES use AuthFailed for
    // the analogous check, because there it gates the ACL decision).
    let server_pk_bytes = handshake
        .get_remote_static()
        .ok_or_else(|| {
            TransportError::InvalidFragment(
                "XX handshake: server did not transmit its static key".to_string(),
            )
        })?
        .to_owned();
    let mut server_pk = [0u8; 32];
    server_pk.copy_from_slice(&server_pk_bytes);

    let transport = handshake.into_transport_mode()?;
    debug!("Noise XX handshake complete (initiator)");
    Ok((NoiseStream::new(stream, transport), server_pk))
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
/// WHOLE handshake, preamble read included (see `HANDSHAKE_TIMEOUT`). Same
/// absolute-deadline plumbing as the IK responder — every read AND write is
/// bounded by the time remaining until the deadline.
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
    responder_with_mode_check(stream, static_sk, check_client, Instant::now() + timeout)
}

/// Read the preamble under `deadline`, require XX, then run the flow.
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
    require_mode(mode, HandshakeMode::Xx)?;
    responder(stream, static_sk, check_client, deadline)
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
    use snow::Builder;

    // Same prologue as the XX initiator above — the two halves of the XX flow
    // must derive an identical handshake hash, so both take HandshakeMode::Xx.
    let prologue = HandshakeMode::Xx.prologue();

    let mut handshake = Builder::new(PATTERN.parse()?)
        .local_private_key(static_sk)?
        .prologue(&prologue)?
        .build_responder()?;

    // Snow's decrypted handshake payloads land here — empty for these
    // patterns, but the buffer is still each read call's output, sized to the
    // frame cap (the largest body a peer may declare).
    let mut out_buf = vec![0u8; MAX_HANDSHAKE_FRAME];

    // Message 1 (<- e): read the client's ephemeral key. Every read is
    // bounded by the remaining budget — pre-authentication, exactly like IK.
    read_handshake_message(&mut handshake, &mut stream, deadline, &mut out_buf)?;

    // Message 2 (-> e, ee, s, es): our ephemeral + static keys. Unlike IK,
    // this MUST be written before the ACL check — the client's static only
    // arrives with message 3, and message 2 is what carries the server's
    // static to the client. This is inherent to XX (the server cannot know
    // who it is talking to until the client finishes authenticating), and it
    // is not a disclosure risk: msg2's static is encrypted to the client's
    // ephemeral key, so a passive observer learns nothing, and an active
    // attacker would have to complete the key exchange to decrypt it — at
    // which point its own static is exposed to the ACL check.
    let mut buf = vec![0u8; HANDSHAKE_FRAME_BUF_LEN];
    let n = handshake.write_message(
        &[],
        slice_mut(&mut buf, LEN_PREFIX_LEN.., "handshake frame body")?,
    )?;
    write_handshake_frame(&mut stream, deadline, &mut buf, n)?;

    // Message 3 (-> s, es): the client's static key arrives here — this is
    // the point where the responder finally learns who it is talking to.
    read_handshake_message(&mut handshake, &mut stream, deadline, &mut out_buf)?;

    // ACL check — the same closure pattern as the IK responder, so XX
    // connections are authorized identically. The check runs after the key
    // exchange (see the msg2 comment) but BEFORE the transport is handed
    // over: a rejected client gets a closed socket and can never send or
    // receive a single data-plane byte.
    if let Some(client_pk) = handshake.get_remote_static() {
        let mut pk = [0u8; 32];
        pk.copy_from_slice(client_pk);
        if !check_client(&pk) {
            info!("Noise XX handshake rejected: client not in ACL");
            return Err(TransportError::AuthFailed);
        }
    } else {
        info!("Noise XX handshake rejected: client did not send static key");
        return Err(TransportError::AuthFailed);
    }

    // Handshake complete — restore the unbounded data-plane I/O (see
    // `super::ik::responder`).
    stream.set_read_timeout(None)?;
    stream.set_write_timeout(None)?;

    let transport = handshake.into_transport_mode()?;
    debug!("Noise XX handshake complete (responder)");
    Ok(NoiseStream::new(stream, transport))
}
