//! The handshake contract over a real socket: both modes end to end, the mode
//! dispatch and refusal paths, the ACL check on each mode, and the absolute
//! deadline that bounds the whole unauthenticated phase.

use choreo_proto::{ClientMessage, ClientMessageType, DaemonMessage, DaemonMessageType};
use choreo_transport::error::TransportError;
use choreo_transport::handshake::{
    HandshakeMode, handshake_initiator, handshake_initiator_xx, handshake_responder,
    handshake_responder_with_timeout, handshake_responder_xx, handshake_responder_xx_with_timeout,
};
use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use x25519_dalek::{PublicKey, StaticSecret};

use crate::common::{dribbling_peer_is_cut_off, noise_test_pair};

/// Test full Noise IK handshake between client and server.
#[test]
#[ignore = "integration"]
fn noise_ik_handshake_round_trip() {
    let (listener, server_sk, server_pk, client_sk, client_pk) =
        noise_test_pair().expect("noise test listener bind");
    let addr = listener.local_addr().expect("local addr");

    let (tx, rx) = mpsc::channel();

    thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        let result = handshake_responder(stream, &server_sk, |pk| *pk == client_pk);
        tx.send(result.map(|_| ())).expect("send server result");
    });

    let stream = TcpStream::connect(addr).expect("connect");
    let result = handshake_initiator(stream, &client_sk, &server_pk);
    assert!(
        result.is_ok(),
        "client handshake should succeed: {:?}",
        result.err()
    );

    let server_result = rx.recv().expect("recv server result");
    assert!(
        server_result.is_ok(),
        "server handshake should succeed: {:?}",
        server_result.err()
    );
}

/// Test that ACL rejection works.
#[test]
#[ignore = "integration"]
fn noise_ik_handshake_rejects_unknown_client() {
    let (listener, server_sk, server_pk, client_sk, _client_pk) =
        noise_test_pair().expect("noise test listener bind");
    let addr = listener.local_addr().expect("local addr");
    let wrong_pk = PublicKey::from(&StaticSecret::random_from_rng(&mut rand::rng()));

    let (tx, rx) = mpsc::channel();

    thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        let result = handshake_responder(stream, &server_sk, |pk| *pk == wrong_pk.to_bytes());
        tx.send(result.map(|_| ())).expect("send");
    });

    let stream = TcpStream::connect(addr).expect("connect");
    let client_result = handshake_initiator(stream, &client_sk, &server_pk);
    assert!(
        client_result.is_err(),
        "client handshake should fail when server rejects"
    );

    let server_result = rx.recv().expect("recv");
    assert!(
        server_result.is_err(),
        "server should reject unknown client"
    );
}

/// Test that a peer sending garbage instead of a Noise handshake message 1
/// cannot wedge the responder: `handshake_responder` must error out (not
/// hang), and that failure is relayed over mpsc. The client writes a
/// plausible 2-byte big-endian length prefix followed by 5 bytes that are
/// NOT a Noise IK message (X25519 + AESGCM message 1 is 96 bytes); the
/// responder reads the prefix + body and snow's `read_message` fails.
#[test]
#[ignore = "integration"]
fn noise_garbage_handshake_message_rejected() {
    let server_sk = StaticSecret::random_from_rng(&mut rand::rng());
    let server_sk_bytes = server_sk.to_bytes();

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("local addr");

    let (tx, rx) = mpsc::channel();

    thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        let result = handshake_responder(stream, &server_sk_bytes, |_| true);
        tx.send(result.map(|_| ())).expect("send server result");
    });

    let mut stream = TcpStream::connect(addr).expect("connect");
    // 2-byte big-endian length prefix (5) + 5 bytes of non-Noise data.
    let garbage = b"\x00\x05hello".to_vec();
    stream.write_all(&garbage).expect("write garbage");

    // Drop the stream: the server already has all 7 bytes it needs to
    // reach the failing read_message call, so it must error out on its own.
    drop(stream);

    let server_result = rx.recv().expect("recv server result");
    assert!(
        server_result.is_err(),
        "responder must reject a malformed handshake message"
    );
}

/// Test that a peer connecting but sending nothing cannot hold the
/// responder's handshake open forever: the handshake runs under an ABSOLUTE
/// deadline, so a silent peer gets an error instead of a thread + FD held
/// indefinitely. The handshake runs before the ACL check, so this is what
/// stops unauthenticated peers from exhausting daemon resources.
///
/// Uses `handshake_responder_with_timeout` with a short budget so the
/// timeout path runs in milliseconds instead of waiting out the 10 s
/// production default.
#[test]
#[ignore = "integration"]
fn noise_handshake_times_out_when_peer_silent() {
    let server_sk = StaticSecret::random_from_rng(&mut rand::rng());
    let server_sk_bytes = server_sk.to_bytes();

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("local addr");

    let (tx, rx) = mpsc::channel();

    thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        let result = handshake_responder_with_timeout(
            stream,
            &server_sk_bytes,
            |_| true,
            Duration::from_millis(100),
        );
        tx.send(result.map(|_| ())).expect("send server result");
    });

    // Connect and then send NOTHING. The responder blocks reading the 2-byte
    // handshake length prefix; the deadline must fire and return an error
    // — NOT hang (bounded by recv_timeout below), and NOT a clean EOF (the
    // socket is still open, so the timeout is what ends the handshake).
    let _silent = TcpStream::connect(addr).expect("connect");

    let server_result = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("responder must return instead of hanging");
    match server_result {
        Err(TransportError::HandshakeTimeout) => {}
        other => panic!("a silent peer must be cut off with HandshakeTimeout, got {other:?}"),
    }
}

/// Test that a peer which keeps the handshake alive by DRIBBLING bytes — each
/// byte arriving well within any single read's window, so a per-recv socket
/// timeout never fires — is still cut off by the ABSOLUTE handshake deadline.
/// Every handshake read is bounded by the time remaining until the deadline,
/// so the total duration cannot exceed it no matter how the peer paces its
/// bytes: the slow-loris case a per-read timeout alone does not cover. (A
/// per-recv-only timeout would accept these dribbles past this test's wait
/// window; only the absolute deadline cuts them off.) An IK msg1 (-> e, es, s,
/// ss) is ~96 bytes on the wire, so the peer claims 96 and then paces single
/// bytes — see `common::dribbling_peer_is_cut_off`, which the XX variant
/// shares.
#[test]
#[ignore = "integration"]
fn noise_handshake_times_out_against_dribbling_peer() {
    dribbling_peer_is_cut_off(HandshakeMode::Ik, 96)
        .expect("a dribbling IK peer must be cut off with HandshakeTimeout");
}

/// Test the full Noise XX handshake between client and server (first-contact
/// mode), including the data plane over the resulting transport.
///
/// XX differs from IK in one contract-critical way: the client does NOT know
/// the server's static key in advance — it LEARNS it from handshake message
/// 2. The returned key must be exactly the server's real static (that is
/// what the caller later verifies out-of-band / base64-compares), and
/// the derived transport must carry encrypted traffic (Ping -> Pong) exactly
/// like the IK transport.
#[test]
#[ignore = "integration"]
fn noise_xx_handshake_round_trip() {
    let (listener, server_sk, server_pk, client_sk, client_pk) =
        noise_test_pair().expect("noise test listener bind");
    let addr = listener.local_addr().expect("local addr");

    let (tx, rx) = mpsc::channel();

    thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        // Same ACL closure shape as the IK responder: XX clients are
        // authorized identically (the check runs after msg3, where XX
        // delivers the client's static).
        let result = handshake_responder_xx(stream, &server_sk, |pk| *pk == client_pk);
        tx.send(result).expect("send server result");
    });

    let stream = TcpStream::connect(addr).expect("connect");
    let (mut client, learned_server_pk) =
        handshake_initiator_xx(stream, &client_sk).expect("client XX handshake");

    // THE XX contract: the learned key IS the server's static. A MITM
    // substituting its own key would surface exactly here (as a different
    // learned value for the human to reject) — this assert is the pin.
    assert_eq!(
        learned_server_pk, server_pk,
        "learned key must be the server's static"
    );

    let mut server = rx
        .recv()
        .expect("recv server result")
        .expect("server handshake");
    // Prove the data plane over the XX transport: one encrypted round trip.
    client
        .send_client_message(&ClientMessage::request(0, ClientMessageType::Ping))
        .expect("send Ping over XX transport");
    assert_eq!(
        server
            .recv_client_message()
            .expect("recv from XX client")
            .inner,
        ClientMessageType::Ping
    );
    server
        .send_daemon_message(&DaemonMessage::broadcast(DaemonMessageType::Pong))
        .expect("send Pong over XX transport");
    assert_eq!(
        client.recv_daemon_message().expect("recv Pong"),
        DaemonMessage::broadcast(DaemonMessageType::Pong)
    );
}

/// The responder entry point a server actually uses —
/// [`handshake_responder_auto`] — dispatches on the peer's mode byte and reports
/// which flow it ran: IK clients get IK, XX clients get XX, and neither has to
/// tell the server which one it wanted out of band.
#[test]
#[ignore = "integration"]
fn noise_responder_auto_dispatches_on_the_mode_byte() {
    for expected in [HandshakeMode::Ik, HandshakeMode::Xx] {
        let (listener, server_sk, server_pk, client_sk, client_pk) =
            noise_test_pair().expect("noise test listener bind");
        let addr = listener.local_addr().expect("local addr");

        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            let result =
                choreo_transport::handshake::handshake_responder_auto(stream, &server_sk, |pk| {
                    *pk == client_pk
                });
            tx.send(result.map(|(_, mode)| mode)).expect("send result");
        });

        let stream = TcpStream::connect(addr).expect("connect");
        let client = match expected {
            HandshakeMode::Ik => handshake_initiator(stream, &client_sk, &server_pk).map(|_| ()),
            HandshakeMode::Xx => handshake_initiator_xx(stream, &client_sk).map(|_| ()),
        };
        client.expect("client handshake");

        assert_eq!(
            rx.recv()
                .expect("recv server result")
                .expect("server handshake"),
            expected,
            "the responder must run (and report) the mode the client declared"
        );
    }
}

/// A preamble byte that names no mode is refused BEFORE any handshake runs, and
/// a mode-specific responder refuses a peer that asked for the other mode. Both
/// are protocol refusals, not authentication failures — and both fail fast, which
/// is what stops a garbage first byte from costing a handshake's worth of work.
#[test]
#[ignore = "integration"]
fn noise_responder_rejects_unknown_and_mismatched_modes() {
    let server_sk = StaticSecret::random_from_rng(&mut rand::rng());
    let server_sk_bytes = server_sk.to_bytes();

    // Unknown byte: 0x00 is reserved precisely so a NUL fill can never look
    // like a mode. The responder must error without waiting for more bytes.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        let result =
            choreo_transport::handshake::handshake_responder_auto(stream, &server_sk_bytes, |_| {
                true
            });
        tx.send(result.map(|_| ())).expect("send server result");
    });
    let mut stream = TcpStream::connect(addr).expect("connect");
    stream.write_all(&[0x00]).expect("write reserved byte");
    match rx
        .recv_timeout(Duration::from_secs(5))
        .expect("responder must return instead of waiting for a handshake")
    {
        Err(TransportError::HandshakeModeUnknown { byte: 0x00 }) => {}
        other => panic!("a reserved preamble must be refused as unknown, got {other:?}"),
    }

    // Mismatched: the IK responder is asked for XX. The mode byte and the flow
    // must agree, so this is a refusal rather than a handshake attempt.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        let result = handshake_responder(stream, &server_sk_bytes, |_| true);
        tx.send(result.map(|_| ())).expect("send server result");
    });
    let mut stream = TcpStream::connect(addr).expect("connect");
    stream
        .write_all(&[HandshakeMode::Xx.byte()])
        .expect("write XX preamble");
    match rx
        .recv_timeout(Duration::from_secs(5))
        .expect("responder must return")
    {
        Err(TransportError::HandshakeModeMismatch { expected, got }) => {
            assert_eq!(expected, HandshakeMode::Ik.byte());
            assert_eq!(got, HandshakeMode::Xx.byte());
        }
        other => panic!("a cross-mode request must be refused, got {other:?}"),
    }
}

/// The responder's budget covers the preamble read AND the handshake: a peer
/// that sends the mode byte immediately and then stalls is cut off at ONE
/// `timeout`, not two. With a separate budget per phase the elapsed time would be
/// roughly double, so the bound below distinguishes the two designs.
#[test]
#[ignore = "integration"]
fn noise_handshake_budget_covers_preamble_and_handshake() {
    let server_sk = StaticSecret::random_from_rng(&mut rand::rng());
    let server_sk_bytes = server_sk.to_bytes();

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        let started = std::time::Instant::now();
        let result = choreo_transport::handshake::handshake_responder_auto_with_timeout(
            stream,
            &server_sk_bytes,
            |_| true,
            Duration::from_millis(300),
        );
        tx.send((result.map(|_| ()), started.elapsed()))
            .expect("send server result");
    });

    // Send the mode byte (so the preamble read succeeds) and then nothing: the
    // stall lands inside the handshake, on the remaining part of the same budget.
    let mut stream = TcpStream::connect(addr).expect("connect");
    stream
        .write_all(&[HandshakeMode::Ik.byte()])
        .expect("write preamble");

    let (result, elapsed) = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("responder must return");
    match result {
        Err(TransportError::HandshakeTimeout) => {}
        other => panic!("a stalled handshake must time out, got {other:?}"),
    }
    // Two independent 300 ms budgets would take >= 600 ms; one shared budget
    // finishes near 300 ms. A generous ceiling keeps this immune to scheduler
    // slop while still failing the double-budget behaviour.
    assert!(
        elapsed < Duration::from_millis(550),
        "one budget must cover both phases, took {elapsed:?}"
    );
}

/// Test that the XX responder's ACL check rejects a client whose static key
/// is not authorized — the same policy as IK.
///
/// XX ordering note: the rejection happens AFTER msg2 (the server cannot
/// check the ACL before msg3, because XX delivers the client's static only
/// with msg3 — and msg2 must go out first since it carries the server's
/// static). So the CLIENT's handshake completes successfully and it learns
/// the server's key; the rejection surfaces as the server closing the
/// socket before any data-plane traffic flows, which the client's reader
/// observes as a clean `ConnectionClosed`. This asymmetry is inherent to XX
/// and is safe: the rejected client holds no channel — it never sends or
/// receives a single data-plane byte.
#[test]
#[ignore = "integration"]
fn noise_xx_handshake_rejects_unknown_client() {
    let (listener, server_sk, _server_pk, client_sk, _client_pk) =
        noise_test_pair().expect("noise test listener bind");
    let addr = listener.local_addr().expect("local addr");

    let (tx, rx) = mpsc::channel();

    thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        // Nobody is authorized: the ACL closure rejects everyone.
        let result = handshake_responder_xx(stream, &server_sk, |_| false);
        tx.send(result).expect("send server result");
    });

    let stream = TcpStream::connect(addr).expect("connect");
    // The client's XX handshake itself succeeds — msg2 (with the server's
    // static) was already written when the server ran the ACL check.
    let (mut client, learned) =
        handshake_initiator_xx(stream, &client_sk).expect("client handshake");
    let _ = learned; // public key, not secret — learning it is harmless

    // The server must have rejected the unknown client. (Match, not Debug
    // format: NoiseStream deliberately has no Debug impl — a Debug render of
    // a live transport would risk leaking keying state into logs.)
    let server_result = rx.recv().expect("recv server result");
    assert!(
        matches!(server_result, Err(TransportError::AuthFailed)),
        "server must reject the unknown client (AuthFailed)"
    );

    // And the rejected client must not be able to push any data-plane byte
    // through: the server closed its end, so the client's next read hits a
    // clean ConnectionClosed.
    match client.recv_daemon_message() {
        Err(TransportError::ConnectionClosed) => {}
        other => panic!("rejected client's reader must see ConnectionClosed, got {other:?}"),
    }
}

/// XX variant of `noise_handshake_times_out_when_peer_silent`: a peer that
/// connects and sends nothing must be cut off by the ABSOLUTE deadline, not
/// hold a thread + FD forever. The XX responder's first read (msg1 -> e) is
/// pre-authentication, exactly like IK's, so the same resource-exhaustion
/// argument applies to the first-contact path.
#[test]
#[ignore = "integration"]
fn noise_xx_handshake_times_out_when_peer_silent() {
    let server_sk = StaticSecret::random_from_rng(&mut rand::rng());
    let server_sk_bytes = server_sk.to_bytes();

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("local addr");

    let (tx, rx) = mpsc::channel();

    thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        let result = handshake_responder_xx_with_timeout(
            stream,
            &server_sk_bytes,
            |_| true,
            Duration::from_millis(100),
        );
        tx.send(result.map(|_| ())).expect("send server result");
    });

    let _silent = TcpStream::connect(addr).expect("connect");

    let server_result = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("responder must return instead of hanging");
    match server_result {
        Err(TransportError::HandshakeTimeout) => {}
        other => panic!("a silent peer must be cut off with HandshakeTimeout, got {other:?}"),
    }
}

/// XX variant of `noise_handshake_times_out_against_dribbling_peer`: a peer
/// that dribbles bytes to keep every individual read alive must still be cut
/// off by the absolute deadline. An XX msg1 (-> e) is 48 bytes on the wire
/// (32-byte ephemeral + 16-byte GCM tag), so the peer claims 48 and then paces
/// single bytes — each within the read window, the total far past it.
#[test]
#[ignore = "integration"]
fn noise_xx_handshake_times_out_against_dribbling_peer() {
    dribbling_peer_is_cut_off(HandshakeMode::Xx, 48)
        .expect("a dribbling XX peer must be cut off with HandshakeTimeout");
}
