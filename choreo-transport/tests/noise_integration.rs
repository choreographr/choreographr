use choreo_proto::{ClientMessage, DaemonMessage};
use choreo_transport::noise::{handshake_initiator, handshake_responder};
use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::thread;
use x25519_dalek::{PublicKey, StaticSecret};

/// Test full Noise IK handshake between client and server.
#[test]
#[ignore]
fn noise_ik_handshake_round_trip() {
    let server_sk = StaticSecret::random_from_rng(&mut rand::rng());
    let server_pk = PublicKey::from(&server_sk);
    let server_sk_bytes = server_sk.to_bytes();
    let server_pk_bytes = server_pk.to_bytes();

    let client_sk = StaticSecret::random_from_rng(&mut rand::rng());
    let client_pk = PublicKey::from(&client_sk);
    let client_sk_bytes = client_sk.to_bytes();
    let client_pk_bytes = client_pk.to_bytes();

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("local addr");

    let (tx, rx) = mpsc::channel();

    let server_sk = server_sk_bytes;
    let client_pk_ref = client_pk_bytes;
    thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        let result = handshake_responder(stream, &server_sk, |pk| *pk == client_pk_ref);
        tx.send(result.map(|_| ())).expect("send server result");
    });

    let stream = TcpStream::connect(addr).expect("connect");
    let result = handshake_initiator(stream, &client_sk_bytes, &server_pk_bytes);
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
#[ignore]
fn noise_ik_handshake_rejects_unknown_client() {
    let server_sk = StaticSecret::random_from_rng(&mut rand::rng());
    let server_pk = PublicKey::from(&server_sk);
    let server_sk_bytes = server_sk.to_bytes();
    let server_pk_bytes = server_pk.to_bytes();

    let client_sk = StaticSecret::random_from_rng(&mut rand::rng());
    let client_sk_bytes = client_sk.to_bytes();

    let wrong_pk = PublicKey::from(&StaticSecret::random_from_rng(&mut rand::rng()));

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("local addr");

    let (tx, rx) = mpsc::channel();

    thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        let result = handshake_responder(stream, &server_sk_bytes, |pk| *pk == wrong_pk.to_bytes());
        tx.send(result.map(|_| ())).expect("send");
    });

    let stream = TcpStream::connect(addr).expect("connect");
    let client_result = handshake_initiator(stream, &client_sk_bytes, &server_pk_bytes);
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

/// Test the encrypted data plane: typed messages (ClientMessage /
/// DaemonMessage) round-trip over the Noise transport after the handshake.
/// A *second* round trip proves the transport is not a one-shot — each
/// message consumes a nonce on both sides, so this exercises nonce
/// advancement through the shared TransportState.
#[test]
#[ignore]
fn noise_encrypted_message_round_trip() {
    let server_sk = StaticSecret::random_from_rng(&mut rand::rng());
    let server_pk = PublicKey::from(&server_sk);
    let server_sk_bytes = server_sk.to_bytes();
    let server_pk_bytes = server_pk.to_bytes();

    let client_sk = StaticSecret::random_from_rng(&mut rand::rng());
    let client_pk = PublicKey::from(&client_sk);
    let client_sk_bytes = client_sk.to_bytes();
    let client_pk_bytes = client_pk.to_bytes();

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("local addr");

    let (tx, rx) = mpsc::channel();

    let server_sk = server_sk_bytes;
    let client_pk_ref = client_pk_bytes;
    thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        // Run all server-side work through a closure returning a Result so
        // that a content mismatch or transport error is *relayed* over mpsc
        // instead of panicking — a panic would drop the stream mid-protocol
        // and could leave the client blocked on recv().
        let result = (|| -> Result<(), String> {
            let mut server = handshake_responder(stream, &server_sk, |pk| *pk == client_pk_ref)
                .map_err(|e| format!("server handshake failed: {e:?}"))?;

            let msg = server
                .recv_client_message()
                .map_err(|e| format!("recv ping failed: {e:?}"))?;
            if msg != ClientMessage::Ping {
                return Err(format!("expected Ping, got {msg:?}"));
            }
            server
                .send_daemon_message(&DaemonMessage::Pong)
                .map_err(|e| format!("send pong failed: {e:?}"))?;

            let msg2 = server
                .recv_client_message()
                .map_err(|e| format!("recv list failed: {e:?}"))?;
            if msg2 != ClientMessage::ListSessions {
                return Err(format!("expected ListSessions, got {msg2:?}"));
            }
            server
                .send_daemon_message(&DaemonMessage::Sessions { sessions: vec![] })
                .map_err(|e| format!("send sessions failed: {e:?}"))?;
            Ok(())
        })();
        tx.send(result).expect("send server result");
    });

    let stream = TcpStream::connect(addr).expect("connect");
    let mut client =
        handshake_initiator(stream, &client_sk_bytes, &server_pk_bytes).expect("client handshake");

    // First round trip: Ping -> Pong.
    client
        .send_client_message(&ClientMessage::Ping)
        .expect("client send ping");
    let reply = client.recv_daemon_message().expect("client recv pong");
    assert_eq!(reply, DaemonMessage::Pong);

    // Second round trip: ListSessions -> Sessions. Each encrypted message
    // advances the nonce on both sides of the shared TransportState; a
    // second successful round trip proves the transport keeps working
    // across nonce advancement rather than being a one-shot.
    client
        .send_client_message(&ClientMessage::ListSessions)
        .expect("client send list");
    let reply2 = client.recv_daemon_message().expect("client recv sessions");
    assert_eq!(reply2, DaemonMessage::Sessions { sessions: vec![] });

    let server_result = rx.recv().expect("recv server result");
    assert!(
        server_result.is_ok(),
        "server side should succeed: {:?}",
        server_result.err()
    );
}

/// Test that the 4-byte length-prefixed data-plane framing survives a
/// max-size message. The handshake framing is limited to u16 lengths, but
/// the data plane uses u32, so a multi-KiB payload exercises a size range
/// the handshake path can never carry and proves the length prefix +
/// ciphertext are reassembled exactly by the peer's `read_exact`.
///
/// The payload is 65519 bytes (65535 − 16-byte GCM tag), which is the
/// *largest single message the Noise layer permits*: the Noise spec caps
/// messages at 65535 bytes ciphertext, and snow 0.10 enforces this via a
/// hard `MAXMSGLEN` constant with no builder knob to raise it. proto's
/// 32 MiB `MAX_FRAME_SIZE` governs only the typed codec layer above the
/// cipher; this test pins the single-fragment wire format, while
/// `noise_fragmented_message_round_trip` covers payloads past this cap
/// (which the transport now splits and reassembles transparently).
#[test]
#[ignore]
fn noise_large_message_round_trip() {
    let server_sk = StaticSecret::random_from_rng(&mut rand::rng());
    let server_pk = PublicKey::from(&server_sk);
    let server_sk_bytes = server_sk.to_bytes();
    let server_pk_bytes = server_pk.to_bytes();

    let client_sk = StaticSecret::random_from_rng(&mut rand::rng());
    let client_pk = PublicKey::from(&client_sk);
    let client_sk_bytes = client_sk.to_bytes();
    let client_pk_bytes = client_pk.to_bytes();

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("local addr");

    let (tx, rx) = mpsc::channel();

    let server_sk = server_sk_bytes;
    let client_pk_ref = client_pk_bytes;
    thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        let result = (|| -> Result<(), String> {
            let mut server = handshake_responder(stream, &server_sk, |pk| *pk == client_pk_ref)
                .map_err(|e| format!("server handshake failed: {e:?}"))?;

            // The client sends raw bytes (not a typed message) — the point
            // is byte-exact framing, so compare against the expected 0xAB
            // pattern rather than decoding a proto frame.
            let expected = vec![0xABu8; 65535 - 16];
            let payload = server
                .recv_message()
                .map_err(|e| format!("recv large message failed: {e:?}"))?;
            if payload != expected {
                return Err(format!(
                    "payload mismatch: got {} bytes, want {} bytes of 0xAB",
                    payload.len(),
                    expected.len()
                ));
            }
            Ok(())
        })();
        tx.send(result).expect("send server result");
    });

    let stream = TcpStream::connect(addr).expect("connect");
    let mut client =
        handshake_initiator(stream, &client_sk_bytes, &server_pk_bytes).expect("client handshake");

    // 65519 bytes of a single byte pattern — the largest payload snow will
    // accept in one `write_message` (65535 MAXMSGLEN minus the 16-byte GCM
    // tag), well beyond the 2-byte handshake framing's u16 range, so the
    // u32 data-plane framing is what gets exercised.
    let payload = vec![0xABu8; 65535 - 16];
    client
        .send_message(&payload)
        .expect("client send large payload");

    let server_result = rx.recv().expect("recv server result");
    assert!(
        server_result.is_ok(),
        "server should verify the payload byte-for-byte: {:?}",
        server_result.err()
    );
}

/// Test that payloads larger than snow's single-message cap fragment on the
/// wire and reassemble transparently. 65520 bytes is exactly one past the
/// 65519-byte single-message maximum, so it must split into two fragments;
/// 1024 * 1024 bytes spans 17 fragments (16 full 65519-byte chunks plus a
/// 272-byte remainder). The final small echo round trip proves the shared
/// TransportState keeps working across fragment boundaries — send and recv
/// nonces stay in sync after a multi-fragment exchange, and the transport
/// still carries ordinary single-frame messages afterwards.
#[test]
#[ignore]
fn noise_fragmented_message_round_trip() {
    let server_sk = StaticSecret::random_from_rng(&mut rand::rng());
    let server_pk = PublicKey::from(&server_sk);
    let server_sk_bytes = server_sk.to_bytes();
    let server_pk_bytes = server_pk.to_bytes();

    let client_sk = StaticSecret::random_from_rng(&mut rand::rng());
    let client_pk = PublicKey::from(&client_sk);
    let client_sk_bytes = client_sk.to_bytes();
    let client_pk_bytes = client_pk.to_bytes();

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("local addr");

    let (tx, rx) = mpsc::channel();

    let server_sk = server_sk_bytes;
    let client_pk_ref = client_pk_bytes;
    thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        // Same Result-relay pattern as the other tests: all server-side
        // assertions run in a closure whose error is sent over mpsc, so a
        // mismatch surfaces in the client thread instead of panicking and
        // dropping the stream mid-protocol.
        let result = (|| -> Result<(), String> {
            let mut server = handshake_responder(stream, &server_sk, |pk| *pk == client_pk_ref)
                .map_err(|e| format!("server handshake failed: {e:?}"))?;

            // 65520 bytes = 65519 + 1: exactly two fragments.
            let expected_two = vec![0x11u8; 65519 + 1];
            let payload_two = server
                .recv_message()
                .map_err(|e| format!("recv 2-fragment message failed: {e:?}"))?;
            if payload_two != expected_two {
                return Err(format!(
                    "2-fragment payload mismatch: got {} bytes, want {} bytes of 0x11",
                    payload_two.len(),
                    expected_two.len()
                ));
            }

            // 1024 * 1024 bytes = 16 full chunks + a 272-byte remainder:
            // 17 fragments in total, exercising sustained reassembly.
            let expected_mib = vec![0x22u8; 1024 * 1024];
            let payload_mib = server
                .recv_message()
                .map_err(|e| format!("recv 17-fragment message failed: {e:?}"))?;
            if payload_mib != expected_mib {
                return Err(format!(
                    "17-fragment payload mismatch: got {} bytes, want {} bytes of 0x22",
                    payload_mib.len(),
                    expected_mib.len()
                ));
            }

            // Echo the small tail message back verbatim. This is a *send*
            // after two multi-fragment *receives*: if the shared
            // TransportState's send nonce had drifted out of sync with the
            // client's recv nonce across the fragment boundaries, the client
            // would fail to decrypt this reply.
            let tail = server
                .recv_message()
                .map_err(|e| format!("recv tail failed: {e:?}"))?;
            server
                .send_message(&tail)
                .map_err(|e| format!("echo tail failed: {e:?}"))?;
            Ok(())
        })();
        tx.send(result).expect("send server result");
    });

    let stream = TcpStream::connect(addr).expect("connect");
    let mut client =
        handshake_initiator(stream, &client_sk_bytes, &server_pk_bytes).expect("client handshake");

    // One byte past the single-message cap: the sender must split this into
    // two fragments (65519 + 1), and the receiver must glue them back.
    let payload_two = vec![0x11u8; 65519 + 1];
    client
        .send_message(&payload_two)
        .expect("client send 2-fragment payload");

    // 1 MiB — 17 fragments — proves the reassembly loop sustains many
    // fragments, not just a single split.
    let payload_mib = vec![0x22u8; 1024 * 1024];
    client
        .send_message(&payload_mib)
        .expect("client send 1 MiB payload");

    // A small message after the fragments: the transport must still carry
    // ordinary single-frame messages once the multi-fragment traffic is done.
    let tail = b"tail".to_vec();
    client.send_message(&tail).expect("client send tail");
    let echo = client.recv_message().expect("client recv tail echo");
    assert_eq!(echo, tail, "server must echo the tail verbatim");

    let server_result = rx.recv().expect("recv server result");
    assert!(
        server_result.is_ok(),
        "server should verify all fragmented payloads: {:?}",
        server_result.err()
    );
}

/// Test that a peer sending garbage instead of a Noise handshake message 1
/// cannot wedge the responder: handshake_responder must error out (not
/// hang), and that failure is relayed over mpsc. The client writes a
/// plausible 2-byte big-endian length prefix followed by 5 bytes that are
/// NOT a Noise IK message (X25519 + AESGCM message 1 is 96 bytes); the
/// responder reads the prefix + body and snow's read_message fails.
#[test]
#[ignore]
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
