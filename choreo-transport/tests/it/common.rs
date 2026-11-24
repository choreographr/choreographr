//! The shared harness for `choreo-transport`'s integration tests.
//!
//! Keypairs plus an ephemeral listener for every test, the socket-buffer
//! shrinker the deadlock tests need, and the raw IK handshake halves that let a
//! test put bytes on the wire the public API cannot express (a tampered length
//! prefix). Declared once at the crate root and reached from each test module as
//! `crate::common`.
//!
//! Every helper reports a failure as a `Result` instead of panicking. Clippy's
//! test allowances are attribute-based and do not reach a `tests/` module's plain
//! helper fns (see AGENTS.md → Test Discipline → "Lint allowances in test code"),
//! and a `Result` both satisfies that without a file-level `#![expect]` and keeps
//! the failure message with the `#[test]` caller, which is the only place that
//! knows what the helper was being used for.

use choreo_transport::error::TransportError;
use choreo_transport::handshake::{HandshakeMode, MAX_HANDSHAKE_FRAME};
use snow::Builder;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use x25519_dalek::{PublicKey, StaticSecret};

#[cfg(unix)]
pub(crate) fn shrink_socket_buffers(stream: &TcpStream) {
    use nix::sys::socket::setsockopt;
    use nix::sys::socket::sockopt::{RcvBuf, SndBuf};
    let _ = setsockopt(stream, SndBuf, &65536usize);
    let _ = setsockopt(stream, RcvBuf, &65536usize);
}

/// Non-Unix fallback: no portable way to shrink the kernel buffers, but the
/// defaults (~64-256 KiB) are still far below the 1 MiB payloads, so the test
/// forces the writers to block on a full peer buffer either way.
#[cfg(not(unix))]
pub(crate) fn shrink_socket_buffers(_stream: &TcpStream) {}

/// Keypairs + listener bundle returned by [`noise_test_pair`]: the ephemeral
/// listener plus the server and client static keys (secret, then public, in
/// construction order), destructured flat by every test. Factored into a type
/// alias to keep the shared test helper's signature readable.
pub(crate) type NoiseTestPair = (TcpListener, [u8; 32], [u8; 32], [u8; 32], [u8; 32]);

/// Fresh X25519 keypairs for both handshake sides plus an ephemeral
/// listener, shared by every test (cuts the per-test keygen boilerplate).
pub(crate) fn noise_test_pair() -> std::result::Result<NoiseTestPair, String> {
    let server_sk = StaticSecret::random_from_rng(&mut rand::rng());
    let server_pk = PublicKey::from(&server_sk);
    let client_sk = StaticSecret::random_from_rng(&mut rand::rng());
    let client_pk = PublicKey::from(&client_sk);
    let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
    Ok((
        listener,
        server_sk.to_bytes(),
        server_pk.to_bytes(),
        client_sk.to_bytes(),
        client_pk.to_bytes(),
    ))
}

/// Read one `MAX_HANDSHAKE_FRAME`-bounded handshake frame body: the 2-byte
/// big-endian length prefix, a refusal above the cap BEFORE any allocation, then
/// exactly that many body bytes. Production refuses an over-cap declaration the
/// same way; mirroring it here keeps the harness from being handed an arbitrary
/// allocation by a peer that declares a huge length.
fn read_framed(stream: &mut TcpStream, what: &str) -> Result<Vec<u8>, String> {
    let mut len_buf = [0u8; 2];
    stream
        .read_exact(&mut len_buf)
        .map_err(|e| format!("read {what} len failed: {e}"))?;
    let msg_len = u16::from_be_bytes(len_buf) as usize;
    if msg_len > MAX_HANDSHAKE_FRAME {
        return Err(format!(
            "{what} length {msg_len} exceeds the {MAX_HANDSHAKE_FRAME}-byte cap"
        ));
    }
    let mut rbuf = vec![0u8; msg_len];
    stream
        .read_exact(&mut rbuf)
        .map_err(|e| format!("read {what} failed: {e}"))?;
    Ok(rbuf)
}

/// Responder half of the Noise IK handshake with the production wire format
/// (1-byte mode preamble, then 2-byte BE length-prefixed handshake messages),
/// returning the raw `TcpStream` and the derived `TransportState`. Byte-for-byte
/// what [`handshake_responder`] does — the IK preamble included — but keeps the
/// raw pieces so a test can write frames the public API cannot express (e.g. a
/// tampered length prefix).
pub(crate) fn raw_handshake_responder(
    mut stream: TcpStream,
    server_sk: &[u8; 32],
    expected_client_pk: &[u8; 32],
) -> Result<(TcpStream, snow::TransportState), String> {
    // The mode byte the production responder reads and validates first.
    let mut preamble = [0u8; 1];
    stream
        .read_exact(&mut preamble)
        .map_err(|e| format!("read preamble failed: {e}"))?;
    if preamble[0] != HandshakeMode::Ik.byte() {
        return Err(format!(
            "expected the IK preamble {:#04x}, got {:#04x}",
            HandshakeMode::Ik.byte(),
            preamble[0]
        ));
    }

    let prologue = HandshakeMode::Ik.prologue();
    // The pattern name comes from the mode, never a copy: a hand-typed string
    // here would keep validating a pattern production no longer negotiates.
    let mut handshake = Builder::new(
        HandshakeMode::Ik
            .pattern()
            .parse()
            .map_err(|e| format!("pattern parse failed: {e:?}"))?,
    )
    .local_private_key(server_sk)
    .map_err(|e| format!("server key failed: {e:?}"))?
    .prologue(&prologue)
    .map_err(|e| format!("prologue failed: {e:?}"))?
    .build_responder()
    .map_err(|e| format!("build responder failed: {e:?}"))?;

    let rbuf = read_framed(&mut stream, "msg1")?;
    let mut out_buf = vec![0u8; MAX_HANDSHAKE_FRAME];
    handshake
        .read_message(&rbuf, &mut out_buf)
        .map_err(|e| format!("server read msg1 failed: {e:?}"))?;

    let pk = handshake
        .get_remote_static()
        .ok_or_else(|| "no client static key".to_string())?;
    if *pk != *expected_client_pk {
        return Err("unexpected client public key".into());
    }

    let n = handshake
        .write_message(&[], &mut out_buf)
        .map_err(|e| format!("server write msg2 failed: {e:?}"))?;
    // Handshake messages are tiny (few hundred bytes); the u16 length
    // prefix cannot truncate.
    let n = u16::try_from(n).map_err(|e| format!("handshake message length overflows u16: {e}"))?;
    stream
        .write_all(&n.to_be_bytes())
        .map_err(|e| format!("write msg2 len failed: {e}"))?;
    stream
        .write_all(
            out_buf
                .get(..usize::from(n))
                .ok_or("out_buf slice out of bounds")?,
        )
        .map_err(|e| format!("write msg2 failed: {e}"))?;

    let ts = handshake
        .into_transport_mode()
        .map_err(|e| format!("server transport failed: {e:?}"))?;
    Ok((stream, ts))
}

/// Initiator half of the Noise IK handshake with the production wire format,
/// returning the raw `TcpStream` and the derived `TransportState` (see
/// [`raw_handshake_responder`]).
pub(crate) fn raw_handshake_initiator(
    mut stream: TcpStream,
    client_sk: &[u8; 32],
    server_pk: &[u8; 32],
) -> Result<(TcpStream, snow::TransportState), String> {
    // The mode byte the production initiator writes first.
    stream
        .write_all(&[HandshakeMode::Ik.byte()])
        .map_err(|e| format!("write preamble failed: {e}"))?;

    let prologue = HandshakeMode::Ik.prologue();
    // The pattern name comes from the mode, never a copy: a hand-typed string
    // here would keep validating a pattern production no longer negotiates.
    let mut handshake = Builder::new(
        HandshakeMode::Ik
            .pattern()
            .parse()
            .map_err(|e| format!("pattern parse failed: {e:?}"))?,
    )
    .local_private_key(client_sk)
    .map_err(|e| format!("client key failed: {e:?}"))?
    .remote_public_key(server_pk)
    .map_err(|e| format!("server pk failed: {e:?}"))?
    .prologue(&prologue)
    .map_err(|e| format!("prologue failed: {e:?}"))?
    .build_initiator()
    .map_err(|e| format!("build initiator failed: {e:?}"))?;

    let mut buf = vec![0u8; MAX_HANDSHAKE_FRAME];
    let n = handshake
        .write_message(&[], &mut buf)
        .map_err(|e| format!("client write msg1 failed: {e:?}"))?;
    // Handshake messages are tiny (few hundred bytes); the u16 length
    // prefix cannot truncate.
    let n = u16::try_from(n).map_err(|e| format!("handshake message length overflows u16: {e}"))?;
    stream
        .write_all(&n.to_be_bytes())
        .map_err(|e| format!("write msg1 len failed: {e}"))?;
    stream
        .write_all(buf.get(..usize::from(n)).ok_or("buf slice out of bounds")?)
        .map_err(|e| format!("write msg1 failed: {e}"))?;

    let rbuf = read_framed(&mut stream, "msg2")?;
    handshake
        .read_message(&rbuf, &mut buf)
        .map_err(|e| format!("client read msg2 failed: {e:?}"))?;

    let ts = handshake
        .into_transport_mode()
        .map_err(|e| format!("client transport failed: {e:?}"))?;
    Ok((stream, ts))
}

/// The responder budget the dribble helpers arm: far shorter than the total a
/// dribbling peer would need, so what the test proves is that the ABSOLUTE
/// deadline cuts the peer off (a per-recv timeout would let it continue).
const DRIBBLE_BUDGET: Duration = Duration::from_millis(500);

/// The dribbler's pace: one byte per gap, each well inside any single read's
/// window, with the total stretching far past [`DRIBBLE_BUDGET`].
const DRIBBLE_GAP: Duration = Duration::from_millis(150);

/// How many dribbled bytes to attempt. Enough for the total to pass the budget
/// several times over; later writes may fail once the responder has errored and
/// closed the socket, which is the outcome under test.
const DRIBBLE_BYTES: usize = 8;

/// Drive the `mode` responder from a peer that claims a `frame_len`-byte
/// handshake message and then dribbles it one byte per [`DRIBBLE_GAP`], and
/// report whether the responder gave up with `HandshakeTimeout`.
///
/// This is the slow-loris shape every handshake read is bounded against: no
/// individual read ever waits longer than one gap, so only the absolute deadline
/// ends the exchange. Both flows take the same preamble and length prefix from a
/// peer, so they need the same behaviour here and differ only in which responder
/// the accepted stream is handed to — which is exactly what this one driver
/// pins, so the IK and XX timeout contracts cannot drift apart.
///
/// Err describes what happened instead of the expected timeout; the caller (a
/// `#[test]` fn) turns that into a failure.
pub(crate) fn dribbling_peer_is_cut_off(mode: HandshakeMode, frame_len: u16) -> Result<(), String> {
    let server_sk_bytes = StaticSecret::random_from_rng(&mut rand::rng()).to_bytes();

    let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
    let addr = listener.local_addr().map_err(|e| e.to_string())?;
    let (tx, rx) = mpsc::channel();

    thread::spawn(move || {
        let Ok((stream, _)) = listener.accept() else {
            return;
        };
        let result = match mode {
            HandshakeMode::Ik => choreo_transport::handshake::handshake_responder_with_timeout(
                stream,
                &server_sk_bytes,
                |_| true,
                DRIBBLE_BUDGET,
            ),
            HandshakeMode::Xx => choreo_transport::handshake::handshake_responder_xx_with_timeout(
                stream,
                &server_sk_bytes,
                |_| true,
                DRIBBLE_BUDGET,
            ),
        };
        let _ = tx.send(result.map(|_| ()));
    });

    let mut stream = TcpStream::connect(addr).map_err(|e| e.to_string())?;
    // The mode preamble goes first (the responder reads it before the frame
    // stream), so the dribble happens where the attack targets: the handshake's
    // length prefix and body.
    stream
        .write_all(&[mode.byte()])
        .map_err(|e| format!("write preamble: {e}"))?;
    stream
        .write_all(&frame_len.to_be_bytes())
        .map_err(|e| format!("write length prefix: {e}"))?;
    for _ in 0..DRIBBLE_BYTES {
        thread::sleep(DRIBBLE_GAP);
        let _ = stream.write_all(&[0xAB]);
    }

    // The expected outcome is a timeout; every other outcome is a failure this
    // helper reports to its caller rather than panicking on.
    match rx
        .recv_timeout(Duration::from_secs(10))
        .map_err(|e| format!("responder did not return: {e}"))?
    {
        Err(TransportError::HandshakeTimeout) => Ok(()),
        Err(e) => Err(format!(
            "responder returned {e} instead of HandshakeTimeout"
        )),
        Ok(()) => Err("responder accepted the dribbled handshake".to_string()),
    }
}
