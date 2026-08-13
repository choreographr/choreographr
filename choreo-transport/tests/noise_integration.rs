use choreo_proto::{ClientMessage, DaemonMessage};
use choreo_transport::noise::{NoiseStream, handshake_initiator, handshake_responder};
use snow::Builder;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use x25519_dalek::{PublicKey, StaticSecret};

/// Shrink a raw TcpStream's kernel send/recv buffers (SO_SNDBUF/SO_RCVBUF)
/// before the handshake, so the socket's in-flight capacity stays far below
/// the 1 MiB payloads and the writers are forced to block on a full peer
/// receive buffer. `TcpStream` has no stable API for these options, so use
/// `nix`'s setsockopt (Unix only; the dependency is target-gated in
/// Cargo.toml). Errors are ignored on purpose: on platforms where the sets
/// fail the kernel keeps default-sized buffers, and even defaults (~64-256
/// KiB) are far below 1 MiB, so the test still reproduces the deadlock.
///
/// 64 KiB, not smaller: the kernel doubles SO_SNDBUF/SO_RCVBUF, and on the
/// loopback interface the TCP MSS is 32768 bytes. A 4096-byte request (8 KiB
/// effective) leaves the receive window smaller than one full segment, which
/// on loopback stalls the transfer with window/retransmit collapses even with
/// the per-chunk lock fix — a test-environment artifact unrelated to the
/// lock scope. 64 KiB (128 KiB effective) is well below the 1 MiB payloads,
/// so the writers still block and the lock-scope deadlock still reproduces,
/// while TCP can make normal progress.
#[cfg(unix)]
fn shrink_socket_buffers(stream: &TcpStream) {
    use nix::sys::socket::setsockopt;
    use nix::sys::socket::sockopt::{RcvBuf, SndBuf};
    let _ = setsockopt(stream, SndBuf, &65536usize);
    let _ = setsockopt(stream, RcvBuf, &65536usize);
}

/// Non-Unix fallback: no portable way to shrink the kernel buffers, but the
/// defaults (~64-256 KiB) are still far below the 1 MiB payloads, so the test
/// reproduces the deadlock against the old code regardless.
#[cfg(not(unix))]
fn shrink_socket_buffers(_stream: &TcpStream) {}

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
/// The payload is 65518 bytes (65535 − 16-byte GCM tag − 1-byte authenticated
/// continuation header), which is the *largest payload a single Noise fragment
/// can carry*: the Noise spec caps messages at 65535 bytes ciphertext, and
/// snow 0.10 enforces this via a hard `MAXMSGLEN` constant with no builder
/// knob to raise it. proto's 32 MiB `MAX_FRAME_SIZE` governs only the typed
/// codec layer above the cipher; this test pins the single-fragment wire
/// format, while `noise_fragmented_message_round_trip` covers payloads past
/// this cap (which the transport now splits and reassembles transparently).
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
            let expected = vec![0xABu8; 65535 - 16 - 1];
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

    // 65518 bytes of a single byte pattern — the largest payload a single
    // fragment can carry (65535 MAXMSGLEN minus the 16-byte GCM tag and the
    // 1-byte continuation header), well beyond the 2-byte handshake framing's
    // u16 range, so the u32 data-plane framing is what gets exercised.
    let payload = vec![0xABu8; 65535 - 16 - 1];
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

/// Test that payloads larger than a single fragment's capacity fragment on
/// the wire and reassemble transparently. 65519 bytes is exactly one past the
/// 65518-byte single-fragment maximum, so it must split into two fragments;
/// 1024 * 1024 bytes spans 17 fragments (16 full 65518-byte chunks plus a
/// 288-byte remainder). The final small echo round trip proves the shared
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

            // 65519 bytes = 65518 + 1: exactly two fragments.
            let expected_two = vec![0x11u8; 65518 + 1];
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

            // 1024 * 1024 bytes = 16 full chunks + a 288-byte remainder:
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

    // One byte past the single-fragment capacity: the sender must split this
    // into two fragments (65518 + 1), and the receiver must glue them back.
    let payload_two = vec![0x11u8; 65518 + 1];
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

/// Regression test for the bidirectional large-message deadlock.
///
/// Both endpoints send 1 MiB CONCURRENTLY (each side's writer thread sends
/// before its receive completes) under tiny socket buffers, so both
/// directions' in-flight data far exceeds the kernel buffers. Each side runs
/// a reader thread via `try_clone` (the same writer-thread + reader-thread
/// shape the daemon uses per connection), so the writer's `write_all` can
/// only complete if the reader keeps draining the socket. With the old code,
/// `send_message` held the shared `TransportState` lock across the blocking
/// `write_all`s: each side's reader drained one fragment (65 KiB, lock-free)
/// and then blocked at `lock()` while its own writer held the lock and was
/// blocked in `write_all` (the peer's receive buffer full) — neither side
/// drained, so both writers blocked forever. With the fix the lock is held
/// only per-chunk during encryption and never across socket I/O, so the
/// readers always acquire the lock (briefly) and drain, and every
/// `write_all` completes. This test pins that fix: it deadlocks (and times
/// out) against the old code, passes against the new one.
#[test]
#[ignore]
fn noise_concurrent_bidirectional_large_messages() {
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

    // Both sides send 1 MiB before either's receive completes; distinct byte
    // patterns let each side verify it got the peer's message, not its own.
    let client_payload = vec![0xABu8; 1024 * 1024];
    let server_payload = vec![0xCDu8; 1024 * 1024];

    // Relay each side's result (Ok or a String error) over its own channel;
    // a panic in either thread would drop its stream mid-protocol and could
    // wedge the peer, so all failures are returned as Err instead.
    let (server_tx, server_rx) = mpsc::channel();
    let (client_tx, client_rx) = mpsc::channel();

    let server_sk = server_sk_bytes;
    let client_pk_ref = client_pk_bytes;
    thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        // Shrink the kernel buffers BEFORE the handshake so the socket's
        // in-flight capacity stays far below the 1 MiB payloads, forcing the
        // writers to block on a full peer receive buffer (see
        // shrink_socket_buffers for why failures are ignored).
        shrink_socket_buffers(&stream);
        let result = (|| -> Result<(), String> {
            let mut server = handshake_responder(stream, &server_sk, |pk| *pk == client_pk_ref)
                .map_err(|e| format!("server handshake failed: {e:?}"))?;

            // Spawn the READER on its own thread via try_clone — the same
            // shape as the daemon (one writer thread + one reader thread per
            // connection, sharing the TransportState through the Arc<Mutex>).
            // The writer thread below sends 1 MiB FIRST; the reader drains
            // the socket concurrently, and that concurrent drain is exactly
            // what the old lock-across-write_all code prevented (the reader
            // blocked at lock() while the writer held the lock and blocked in
            // write_all).
            let mut reader = server
                .try_clone()
                .map_err(|e| format!("server try_clone failed: {e:?}"))?;
            let (recv_tx, recv_rx) = mpsc::channel();
            thread::spawn(move || {
                let recv_result = (|| -> Result<(), String> {
                    let received = reader
                        .recv_message()
                        .map_err(|e| format!("server recv 1 MiB failed: {e:?}"))?;
                    let expected = vec![0xABu8; 1024 * 1024];
                    if received != expected {
                        return Err(format!(
                            "server payload mismatch: got {} bytes, want 1 MiB of 0xAB",
                            received.len()
                        ));
                    }
                    Ok(())
                })();
                recv_tx.send(recv_result).expect("send server recv result");
            });

            // SEND BEFORE RECEIVING: the send starts while the reader thread
            // above drains the other direction. Under the old code this
            // deadlocked once both sides' receive buffers filled — the reader
            // could not acquire the TransportState lock to decrypt, so the
            // writer's write_all never completed. With the fix the send
            // completes because the reader always drains.
            server
                .send_message(&server_payload)
                .map_err(|e| format!("server send 1 MiB failed: {e:?}"))?;

            // The send completed, so the reader must have drained everything;
            // join it to surface any receive-side failure.
            recv_rx
                .recv()
                .map_err(|e| format!("server recv join failed: {e}"))?
        })();
        server_tx.send(result).expect("send server result");
    });

    let stream = TcpStream::connect(addr).expect("connect");
    // Same tiny-buffer treatment on the client side, before the handshake.
    shrink_socket_buffers(&stream);
    thread::spawn(move || {
        let result = (|| -> Result<(), String> {
            let mut client = handshake_initiator(stream, &client_sk_bytes, &server_pk_bytes)
                .map_err(|e| format!("client handshake failed: {e:?}"))?;

            // Same reader-thread shape as the server side (see above).
            let mut reader = client
                .try_clone()
                .map_err(|e| format!("client try_clone failed: {e:?}"))?;
            let (recv_tx, recv_rx) = mpsc::channel();
            thread::spawn(move || {
                let recv_result = (|| -> Result<(), String> {
                    let received = reader
                        .recv_message()
                        .map_err(|e| format!("client recv 1 MiB failed: {e:?}"))?;
                    let expected = vec![0xCDu8; 1024 * 1024];
                    if received != expected {
                        return Err(format!(
                            "client payload mismatch: got {} bytes, want 1 MiB of 0xCD",
                            received.len()
                        ));
                    }
                    Ok(())
                })();
                recv_tx.send(recv_result).expect("send client recv result");
            });

            // SEND BEFORE RECEIVING (see server side above).
            client
                .send_message(&client_payload)
                .map_err(|e| format!("client send 1 MiB failed: {e:?}"))?;

            recv_rx
                .recv()
                .map_err(|e| format!("client recv join failed: {e}"))?
        })();
        client_tx.send(result).expect("send client result");
    });

    // Wait on BOTH sides with a bounded timeout: a deadlock fails the test
    // with a clear message instead of hanging the suite. Nextest runs each
    // test in its own process and tears it down when the test function
    // returns, so the deadlocked threads do not leak into other tests.
    let server_result = match server_rx.recv_timeout(std::time::Duration::from_secs(30)) {
        Ok(result) => result.map_err(|e| format!("server side failed: {e}")),
        Err(_) => panic!("timed out waiting for server side (deadlock?)"),
    };
    let client_result = match client_rx.recv_timeout(std::time::Duration::from_secs(30)) {
        Ok(result) => result.map_err(|e| format!("client side failed: {e}")),
        Err(_) => panic!("timed out waiting for client side (deadlock?)"),
    };
    assert!(
        server_result.is_ok(),
        "server side should succeed: {:?}",
        server_result.err()
    );
    assert!(
        client_result.is_ok(),
        "client side should succeed: {:?}",
        client_result.err()
    );
}

/// Test that a hostile or corrupted length prefix cannot make the receiver
/// allocate a huge buffer or hang: the 4-byte data-plane prefix is NOT
/// authenticated (it precedes the GCM ciphertext), so a peer can send any
/// value. recv_message must reject a fragment longer than snow's 65535-byte
/// ciphertext cap BEFORE allocating or reading that many bytes — without the
/// validation the old code would `vec![0u8; ct_len]` for a 0x7FFF_FFFF
/// prefix (~2 GiB) and block in read_exact waiting for the bytes.
///
/// (The prefix no longer carries a continuation flag — that decision moved
/// into the authenticated plaintext — so the raw value is just a huge length.)
#[test]
#[ignore]
fn noise_rejects_oversized_fragment_prefix() {
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
            match server.recv_message() {
                Err(_) => Ok(()), // rejected — the desired outcome
                Ok(_) => Err("oversized fragment prefix was NOT rejected".into()),
            }
        })();
        tx.send(result).expect("send server result");
    });

    let stream = TcpStream::connect(addr).expect("connect");
    let client =
        handshake_initiator(stream, &client_sk_bytes, &server_pk_bytes).expect("client handshake");

    // Raw write of a length prefix claiming 0x7FFF_FFFF ciphertext bytes
    // (far above the 65535-byte Noise cap), bypassing send_message so the
    // wire carries a genuinely invalid frame.
    client
        .get_ref()
        .write_all(&0x7FFF_FFFFu32.to_be_bytes())
        .expect("write bogus prefix");

    let server_result = rx.recv().expect("recv server result");
    assert!(
        server_result.is_ok(),
        "oversized fragment prefix must be rejected: {:?}",
        server_result.err()
    );
}

/// Responder half of the Noise IK handshake with the production wire format
/// (2-byte BE length-prefixed handshake messages), returning the raw
/// `TcpStream` and the derived `TransportState`. Byte-for-byte what
/// [`handshake_responder`] does, but keeps the raw pieces so a test can write
/// frames the public API cannot express (e.g. a tampered length prefix).
fn raw_handshake_responder(
    mut stream: TcpStream,
    server_sk: &[u8; 32],
    expected_client_pk: &[u8; 32],
) -> Result<(TcpStream, snow::TransportState), String> {
    let mut handshake = Builder::new(
        "Noise_IK_25519_AESGCM_SHA256"
            .parse()
            .map_err(|e| format!("pattern parse failed: {e:?}"))?,
    )
    .local_private_key(server_sk)
    .map_err(|e| format!("server key failed: {e:?}"))?
    .build_responder()
    .map_err(|e| format!("build responder failed: {e:?}"))?;

    let mut len_buf = [0u8; 2];
    stream
        .read_exact(&mut len_buf)
        .map_err(|e| format!("read msg1 len failed: {e}"))?;
    let msg_len = u16::from_be_bytes(len_buf) as usize;
    let mut rbuf = vec![0u8; msg_len];
    stream
        .read_exact(&mut rbuf)
        .map_err(|e| format!("read msg1 failed: {e}"))?;
    let mut out_buf = vec![0u8; 1024];
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
    stream
        .write_all(&(n as u16).to_be_bytes())
        .map_err(|e| format!("write msg2 len failed: {e}"))?;
    stream
        .write_all(&out_buf[..n])
        .map_err(|e| format!("write msg2 failed: {e}"))?;

    let ts = handshake
        .into_transport_mode()
        .map_err(|e| format!("server transport failed: {e:?}"))?;
    Ok((stream, ts))
}

/// Initiator half of the Noise IK handshake with the production wire format,
/// returning the raw `TcpStream` and the derived `TransportState` (see
/// [`raw_handshake_responder`]).
fn raw_handshake_initiator(
    mut stream: TcpStream,
    client_sk: &[u8; 32],
    server_pk: &[u8; 32],
) -> Result<(TcpStream, snow::TransportState), String> {
    let mut handshake = Builder::new(
        "Noise_IK_25519_AESGCM_SHA256"
            .parse()
            .map_err(|e| format!("pattern parse failed: {e:?}"))?,
    )
    .local_private_key(client_sk)
    .map_err(|e| format!("client key failed: {e:?}"))?
    .remote_public_key(server_pk)
    .map_err(|e| format!("server pk failed: {e:?}"))?
    .build_initiator()
    .map_err(|e| format!("build initiator failed: {e:?}"))?;

    let mut buf = vec![0u8; 1024];
    let n = handshake
        .write_message(&[], &mut buf)
        .map_err(|e| format!("client write msg1 failed: {e:?}"))?;
    stream
        .write_all(&(n as u16).to_be_bytes())
        .map_err(|e| format!("write msg1 len failed: {e}"))?;
    stream
        .write_all(&buf[..n])
        .map_err(|e| format!("write msg1 failed: {e}"))?;

    let mut len_buf = [0u8; 2];
    stream
        .read_exact(&mut len_buf)
        .map_err(|e| format!("read msg2 len failed: {e}"))?;
    let msg_len = u16::from_be_bytes(len_buf) as usize;
    let mut rbuf = vec![0u8; msg_len];
    stream
        .read_exact(&mut rbuf)
        .map_err(|e| format!("read msg2 failed: {e}"))?;
    handshake
        .read_message(&rbuf, &mut buf)
        .map_err(|e| format!("client read msg2 failed: {e:?}"))?;

    let ts = handshake
        .into_transport_mode()
        .map_err(|e| format!("client transport failed: {e:?}"))?;
    Ok((stream, ts))
}

/// Test that a wire-level tamper with a fragment's length prefix is rejected
/// loudly and can never silently truncate a reassembled message. The 4-byte
/// prefix precedes the GCM ciphertext and is NOT authenticated, so the
/// reassembly decision must come from the authenticated continuation byte
/// INSIDE the plaintext: a flipped prefix bit changes ct_len, which either
/// trips the size cap or fails the GCM authentication on the wrong byte
/// count — in no case may the receiver return a truncated prefix as a
/// complete message.
///
/// The test drives a manually-derived TransportState (mirroring the
/// production handshake byte-for-byte) on both sides so it can encrypt raw
/// fragments and then write a tampered prefix — something the public API
/// cannot express, since `send_message` always writes an intact prefix.
#[test]
#[ignore]
fn noise_rejects_tampered_length_prefix() {
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
            let (stream, ts) = raw_handshake_responder(stream, &server_sk, &client_pk_ref)?;
            let mut server = NoiseStream::new(stream, ts);

            // Control: a valid two-fragment message reassembles intact.
            let payload = server
                .recv_message()
                .map_err(|e| format!("control recv failed: {e:?}"))?;
            let expected = vec![0xABu8; 65518 + 100];
            if payload != expected {
                return Err(format!(
                    "control payload mismatch: got {} bytes, want {} bytes of 0xAB",
                    payload.len(),
                    expected.len()
                ));
            }

            // Attack: the client writes a second message whose second-fragment
            // length prefix is decremented by one. The receiver must reject it
            // (the wrong byte count never decrypts), NOT return the first
            // fragment as a complete message.
            match server.recv_message() {
                Err(_) => Ok(()), // rejected — the desired outcome
                Ok(p) => Err(format!(
                    "tampered length prefix was NOT rejected ({} bytes returned)",
                    p.len()
                )),
            }
        })();
        tx.send(result).expect("send server result");
    });

    // Client side: raw initiator handshake so the test can encrypt fragments
    // and tamper with the wire itself.
    let stream = TcpStream::connect(addr).expect("connect");
    let (mut stream, mut ts) = raw_handshake_initiator(stream, &client_sk_bytes, &server_pk_bytes)
        .expect("client handshake");

    // Helper to write a full message as raw frames with the production framing
    // (4-byte BE ciphertext length, then the ciphertext), optionally tampering
    // with the second fragment's length prefix.
    let mut write_message_raw = |payload: &[u8], tamper_second: bool| {
        let mut ct = vec![0u8; 65535];
        let mut frag = vec![0u8; 65519]; // 1 continuation byte + max payload
        let chunks: Vec<&[u8]> = payload.chunks(65518).collect();
        for (i, chunk) in chunks.iter().enumerate() {
            let more = i + 1 < chunks.len();
            frag[0] = if more { 0x01 } else { 0x00 };
            frag[1..1 + chunk.len()].copy_from_slice(chunk);
            let n = ts
                .write_message(&frag[..1 + chunk.len()], &mut ct)
                .expect("encrypt");
            let mut len = n as u32;
            if tamper_second && i == 1 {
                // Decrement (never increment): a larger prefix would make the
                // receiver block waiting for bytes we never send, hanging the
                // test. A smaller one trips the GCM authentication instead.
                len -= 1;
            }
            stream.write_all(&len.to_be_bytes()).expect("write len");
            stream.write_all(&ct[..n]).expect("write ct");
        }
    };

    // Control message: intact prefixes, two fragments.
    write_message_raw(&vec![0xABu8; 65518 + 100], false);
    // Attack message: second fragment's length prefix decremented by one.
    write_message_raw(&vec![0xABu8; 65518 + 100], true);

    let server_result = rx.recv().expect("recv server result");
    assert!(
        server_result.is_ok(),
        "tampered length prefix must be rejected: {:?}",
        server_result.err()
    );
}

/// Test that a peer connecting but sending nothing cannot hold the
/// responder's handshake open forever: `handshake_responder` applies a read
/// timeout to the handshake socket, so a silent peer gets an error instead of
/// a thread + FD held indefinitely. The handshake runs before the ACL check,
/// so this is what stops unauthenticated peers from exhausting daemon
/// resources.
#[test]
#[ignore]
fn noise_handshake_times_out_when_peer_silent() {
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

    // Connect and then send NOTHING. The responder blocks reading the 2-byte
    // handshake length prefix; the read timeout must fire and return an error
    // — NOT hang (bounded by recv_timeout below), and NOT a clean EOF (the
    // socket is still open, so the timeout is what ends the handshake).
    let _silent = TcpStream::connect(addr).expect("connect");

    let server_result = rx
        .recv_timeout(Duration::from_secs(30))
        .expect("responder must return instead of hanging");
    assert!(
        server_result.is_err(),
        "a silent peer must be timed out, not served"
    );
}
