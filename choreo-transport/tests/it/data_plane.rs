//! The data plane over a real socket: encrypted round trips of every size, the
//! fragmentation and reassembly rules, the peer-close classification, and the
//! refusals that bound memory — the per-frame cap on an unauthenticated length
//! prefix here, and the cumulative reassembly cap as a unit test of its
//! arithmetic in `noise::frame` (reaching it end to end would mean pushing
//! `MAX_FRAME_SIZE` of ciphertext through the socket).

use choreo_proto::{ClientMessage, ClientMessageType, DaemonMessage, DaemonMessageType};
use choreo_transport::error::TransportError;
use choreo_transport::handshake::{handshake_initiator, handshake_responder};
use choreo_transport::noise::NoiseStream;
use std::io::Write;
use std::net::TcpStream;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use crate::common::{
    noise_test_pair, raw_handshake_initiator, raw_handshake_responder, shrink_socket_buffers,
};

/// Test the encrypted data plane: typed messages (`ClientMessage`
/// /
/// `DaemonMessage`) round-trip over the Noise transport after the handshake.
/// A *second* round trip proves the transport is not a one-shot — each
/// message consumes a nonce on both sides, so this exercises nonce
/// advancement through the shared `TransportState`.
#[test]
#[ignore = "integration"]
fn noise_encrypted_message_round_trip() {
    let (listener, server_sk, server_pk, client_sk, client_pk) =
        noise_test_pair().expect("noise test listener bind");
    let addr = listener.local_addr().expect("local addr");

    let (tx, rx) = mpsc::channel();

    thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        // Run all server-side work through a closure returning a Result so
        // that a content mismatch or transport error is *relayed* over mpsc
        // instead of panicking — a panic would drop the stream mid-protocol
        // and could leave the client blocked on recv().
        let result = (|| -> Result<(), String> {
            let mut server = handshake_responder(stream, &server_sk, |pk| *pk == client_pk)
                .map_err(|e| format!("server handshake failed: {e:?}"))?;

            let msg = server
                .recv_client_message()
                .map_err(|e| format!("recv ping failed: {e:?}"))?;
            if msg.inner != ClientMessageType::Ping {
                return Err(format!("expected Ping, got {msg:?}"));
            }
            server
                .send_daemon_message(&DaemonMessage::broadcast(DaemonMessageType::Pong))
                .map_err(|e| format!("send pong failed: {e:?}"))?;

            let msg2 = server
                .recv_client_message()
                .map_err(|e| format!("recv list failed: {e:?}"))?;
            if msg2.inner != ClientMessageType::ListSessions {
                return Err(format!("expected ListSessions, got {msg2:?}"));
            }
            server
                .send_daemon_message(&DaemonMessage::broadcast(DaemonMessageType::Sessions {
                    sessions: vec![],
                }))
                .map_err(|e| format!("send sessions failed: {e:?}"))?;
            Ok(())
        })();
        tx.send(result).expect("send server result");
    });

    let stream = TcpStream::connect(addr).expect("connect");
    let mut client = handshake_initiator(stream, &client_sk, &server_pk).expect("client handshake");

    // First round trip: Ping -> Pong.
    client
        .send_client_message(&ClientMessage::request(0, ClientMessageType::Ping))
        .expect("client send ping");
    let reply = client.recv_daemon_message().expect("client recv pong");
    assert_eq!(reply, DaemonMessage::broadcast(DaemonMessageType::Pong));

    // Second round trip: ListSessions -> Sessions. Each encrypted message
    // advances the nonce on both sides of the shared TransportState; a
    // second successful round trip proves the transport keeps working
    // across nonce advancement rather than being a one-shot.
    client
        .send_client_message(&ClientMessage::request(0, ClientMessageType::ListSessions))
        .expect("client send list");
    let reply2 = client.recv_daemon_message().expect("client recv sessions");
    assert_eq!(
        reply2,
        DaemonMessage::broadcast(DaemonMessageType::Sessions { sessions: vec![] })
    );

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
/// knob to raise it. proto's 64 MiB `MAX_FRAME_SIZE` governs only the typed
/// codec layer above the cipher; this test pins the single-fragment wire
/// format, while `noise_fragmented_message_round_trip` covers payloads past
/// this cap (which the transport now splits and reassembles transparently).
#[test]
#[ignore = "integration"]
fn noise_large_message_round_trip() {
    let (listener, server_sk, server_pk, client_sk, client_pk) =
        noise_test_pair().expect("noise test listener bind");
    let addr = listener.local_addr().expect("local addr");

    let (tx, rx) = mpsc::channel();

    thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        let result = (|| -> Result<(), String> {
            let mut server = handshake_responder(stream, &server_sk, |pk| *pk == client_pk)
                .map_err(|e| format!("server handshake failed: {e:?}"))?;

            // The client sends raw bytes (not a typed message) — the point
            // is byte-exact framing, so compare against the expected 0xAB
            // pattern rather than decoding a proto frame.
            let expected = vec![0xABu8; 65535 - 16 - 1];
            let payload = server
                .recv_message()
                .map_err(|e| format!("recv large message failed: {e:?}"))?;
            if payload.as_slice() != expected.as_slice() {
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
    let mut client = handshake_initiator(stream, &client_sk, &server_pk).expect("client handshake");

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
/// `TransportState` keeps working across fragment boundaries — send and recv
/// nonces stay in sync after a multi-fragment exchange, and the transport
/// still carries ordinary single-frame messages afterwards.
#[test]
#[ignore = "integration"]
fn noise_fragmented_message_round_trip() {
    let (listener, server_sk, server_pk, client_sk, client_pk) =
        noise_test_pair().expect("noise test listener bind");
    let addr = listener.local_addr().expect("local addr");

    let (tx, rx) = mpsc::channel();

    thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        // Same Result-relay pattern as the other tests: all server-side
        // assertions run in a closure whose error is sent over mpsc, so a
        // mismatch surfaces in the client thread instead of panicking and
        // dropping the stream mid-protocol.
        let result = (|| -> Result<(), String> {
            let mut server = handshake_responder(stream, &server_sk, |pk| *pk == client_pk)
                .map_err(|e| format!("server handshake failed: {e:?}"))?;

            // 65519 bytes = 65518 + 1: exactly two fragments.
            let expected_two = vec![0x11u8; 65518 + 1];
            let payload_two = server
                .recv_message()
                .map_err(|e| format!("recv 2-fragment message failed: {e:?}"))?;
            if payload_two.as_slice() != expected_two.as_slice() {
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
            if payload_mib.as_slice() != expected_mib.as_slice() {
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
    let mut client = handshake_initiator(stream, &client_sk, &server_pk).expect("client handshake");

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
    assert_eq!(
        echo.as_slice(),
        tail.as_slice(),
        "server must echo the tail verbatim"
    );

    let server_result = rx.recv().expect("recv server result");
    assert!(
        server_result.is_ok(),
        "server should verify all fragmented payloads: {:?}",
        server_result.err()
    );
}

/// Test that an empty payload still produces a real wire frame: an empty
/// message must round-trip as `Ok(vec![])` — one frame carrying only the
/// cleared continuation header — because the peer's `recv_message` reads a
/// length prefix first and would otherwise block until some later message's
/// frame arrived (and then misparse it as this one). A follow-up non-empty
/// round trip proves the shared `TransportState` keeps working: send/recv
/// nonces stay in sync across the empty message.
#[test]
#[ignore = "integration"]
fn noise_empty_message_round_trip() {
    let (listener, server_sk, server_pk, client_sk, client_pk) =
        noise_test_pair().expect("noise test listener bind");
    let addr = listener.local_addr().expect("local addr");

    let (tx, rx) = mpsc::channel();

    thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        // Same Result-relay pattern as the other tests: a mismatch surfaces in
        // the client thread instead of panicking mid-protocol.
        let result = (|| -> Result<(), String> {
            let mut server = handshake_responder(stream, &server_sk, |pk| *pk == client_pk)
                .map_err(|e| format!("server handshake failed: {e:?}"))?;

            let empty = server
                .recv_message()
                .map_err(|e| format!("recv empty message failed: {e:?}"))?;
            if !empty.is_empty() {
                return Err(format!(
                    "expected an empty message, got {} bytes",
                    empty.len()
                ));
            }

            // Follow-up after the empty frame: if the empty message had not
            // consumed its nonce (or had consumed two), the counters would be
            // out of sync and this decrypt would fail.
            let after = server
                .recv_message()
                .map_err(|e| format!("recv follow-up failed: {e:?}"))?;
            if after.as_slice() != b"after" {
                return Err(format!("expected b\"after\", got {after:?}"));
            }
            Ok(())
        })();
        tx.send(result).expect("send server result");
    });

    let stream = TcpStream::connect(addr).expect("connect");
    let mut client = handshake_initiator(stream, &client_sk, &server_pk).expect("client handshake");

    // Empty message first: it must emit exactly one frame and consume exactly
    // one nonce. An empty send that emitted nothing would leave the server's
    // recv_message blocked on a length prefix until the follow-up frame
    // arrived — which it would then misparse as the empty message, failing the
    // is_empty check below.
    client.send_message(&[]).expect("client send empty");
    client
        .send_message(b"after")
        .expect("client send follow-up");

    let server_result = rx.recv().expect("recv server result");
    assert!(
        server_result.is_ok(),
        "server side should succeed: {:?}",
        server_result.err()
    );
}

/// The peer closing its end of the socket after the handshake surfaces as
/// `TransportError::ConnectionClosed` (not a raw `Io(UnexpectedEof)`): the
/// daemon's read loop branches on that variant to log a graceful disconnect
/// and run cleanup instead of reporting an error. Pins the EOF classification
/// in `NoiseStream::recv_message`.
#[test]
#[ignore = "integration"]
fn noise_peer_close_surfaces_as_connection_closed() {
    let (listener, server_sk, server_pk, client_sk, client_pk) =
        noise_test_pair().expect("noise test listener bind");
    let addr = listener.local_addr().expect("local addr");

    let (tx, rx) = mpsc::channel();

    thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        let result = (|| -> Result<(), String> {
            let mut server = handshake_responder(stream, &server_sk, |pk| *pk == client_pk)
                .map_err(|e| format!("server handshake failed: {e:?}"))?;
            // The client closes its end without sending a frame; the next
            // recv must report ConnectionClosed, not hang or surface a raw
            // I/O error.
            match server.recv_message() {
                Err(TransportError::ConnectionClosed) => Ok(()),
                other => Err(format!("expected ConnectionClosed, got {other:?}")),
            }
        })();
        tx.send(result).expect("send server result");
    });

    let stream = TcpStream::connect(addr).expect("connect");
    let client = handshake_initiator(stream, &client_sk, &server_pk).expect("client handshake");
    // Close our end: the server's next read hits EOF.
    drop(client);

    let server_result = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("server must return instead of hanging");
    assert!(
        server_result.is_ok(),
        "server side should report ConnectionClosed: {:?}",
        server_result.err()
    );
}

/// Regression test for the bidirectional large-message deadlock.
///
/// Both endpoints send 1 MiB CONCURRENTLY (each side's writer thread sends
/// before its receive completes) under tiny socket buffers, so both
/// directions' in-flight data far exceeds the kernel buffers. Each side runs
/// a reader thread via `try_clone` (the same writer-thread + reader-thread
/// shape the daemon uses per connection), so the writer's `write_all` can
/// only complete if the reader keeps draining the socket. That drain is
/// exactly what a `send_message` holding the shared `TransportState` lock
/// across the blocking `write_all`s prevents: each side's reader drains one
/// fragment (65 KiB, lock-free) and then blocks at `lock()` while its own
/// writer holds the lock and is blocked in `write_all` (the peer's receive
/// buffer full) — neither side drains, so both writers block forever. The
/// lock must therefore be held only per-chunk during encryption and never
/// across socket I/O, which is what this test pins: with the wider scope it
/// times out, with the per-chunk scope every `write_all` completes.
#[test]
#[ignore = "integration"]
fn noise_concurrent_bidirectional_large_messages() {
    let (listener, server_sk, server_pk, client_sk, client_pk) =
        noise_test_pair().expect("noise test listener bind");
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

    thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        // Shrink the kernel buffers BEFORE the handshake so the socket's
        // in-flight capacity stays far below the 1 MiB payloads, forcing the
        // writers to block on a full peer receive buffer (see
        // shrink_socket_buffers for why failures are ignored).
        shrink_socket_buffers(&stream);
        let result = (|| -> Result<(), String> {
            let mut server = handshake_responder(stream, &server_sk, |pk| *pk == client_pk)
                .map_err(|e| format!("server handshake failed: {e:?}"))?;

            // Spawn the READER on its own thread via try_clone — the same
            // shape as the daemon (one writer thread + one reader thread per
            // connection, sharing the TransportState through the Arc<Mutex>).
            // The writer thread below sends 1 MiB FIRST; the reader drains
            // the socket concurrently, and that concurrent drain is exactly
            // what a lock held across write_all would prevent (the reader
            // would block at lock() while the writer held it).
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
                    if received.as_slice() != expected.as_slice() {
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
            // above drains the other direction. Holding the TransportState
            // lock across write_all would deadlock here once both sides'
            // receive buffers filled — the reader could not acquire the lock
            // to decrypt, so this write_all would never complete. It completes
            // because the lock is taken only per-chunk, so the reader always
            // drains.
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
            let mut client = handshake_initiator(stream, &client_sk, &server_pk)
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
                    if received.as_slice() != expected.as_slice() {
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
        // Any `RecvTimeoutError` here is a timeout: the sender only sends
        // once at completion, and a dead receiver would panic on send.
        Err(e) => panic!("timed out waiting for server side ({e:?})"),
    };
    let client_result = match client_rx.recv_timeout(std::time::Duration::from_secs(30)) {
        Ok(result) => result.map_err(|e| format!("client side failed: {e}")),
        Err(e) => panic!("timed out waiting for client side ({e:?})"),
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
/// value. `recv_message` must reject a fragment longer than snow's 65535-byte
/// ciphertext cap BEFORE allocating or reading that many bytes — a receiver
/// that trusted the prefix would allocate (and then block in `read_exact`
/// waiting for) the declared `0x7FFF_FFFF` bytes, roughly 2 GiB.
///
/// (The prefix carries only the ciphertext length: the continuation decision
/// lives in the authenticated plaintext, so the raw value here is just a huge
/// length.)
#[test]
#[ignore = "integration"]
fn noise_rejects_oversized_fragment_prefix() {
    let (listener, server_sk, server_pk, client_sk, client_pk) =
        noise_test_pair().expect("noise test listener bind");
    let addr = listener.local_addr().expect("local addr");

    let (tx, rx) = mpsc::channel();

    thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        let result = (|| -> Result<(), String> {
            let mut server = handshake_responder(stream, &server_sk, |pk| *pk == client_pk)
                .map_err(|e| format!("server handshake failed: {e:?}"))?;
            match server.recv_message() {
                Err(_) => Ok(()), // rejected — the desired outcome
                Ok(_) => Err("oversized fragment prefix was NOT rejected".into()),
            }
        })();
        tx.send(result).expect("send server result");
    });

    let stream = TcpStream::connect(addr).expect("connect");
    let client = handshake_initiator(stream, &client_sk, &server_pk).expect("client handshake");

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

/// Test that a wire-level tamper with a fragment's length prefix is rejected
/// loudly and can never silently truncate a reassembled message. The 4-byte
/// prefix precedes the GCM ciphertext and is NOT authenticated, so the
/// reassembly decision must come from the authenticated continuation byte
/// INSIDE the plaintext: a flipped prefix bit changes `ct_len`, which either
/// trips the size cap or fails the GCM authentication on the wrong byte
/// count — in no case may the receiver return a truncated prefix as a
/// complete message.
///
/// The test drives a manually-derived `TransportState` (mirroring the
/// production handshake byte-for-byte) on both sides so it can encrypt raw
/// fragments and then write a tampered prefix — something the public API
/// cannot express, since `send_message` always writes an intact prefix.
#[test]
#[ignore = "integration"]
fn noise_rejects_tampered_length_prefix() {
    let (listener, server_sk, server_pk, client_sk, client_pk) =
        noise_test_pair().expect("noise test listener bind");
    let addr = listener.local_addr().expect("local addr");

    let (tx, rx) = mpsc::channel();

    thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        let result = (|| -> Result<(), String> {
            let (stream, ts) = raw_handshake_responder(stream, &server_sk, &client_pk)?;
            let mut server = NoiseStream::new(stream, ts);

            // Control: a valid two-fragment message reassembles intact.
            let payload = server
                .recv_message()
                .map_err(|e| format!("control recv failed: {e:?}"))?;
            let expected = vec![0xABu8; 65518 + 100];
            if payload.as_slice() != expected.as_slice() {
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
    let (mut stream, mut ts) =
        raw_handshake_initiator(stream, &client_sk, &server_pk).expect("client handshake");

    // Helper to write a full message as raw frames with the production framing
    // (4-byte BE ciphertext length, then the ciphertext), optionally tampering
    // with the second fragment's length prefix.
    let mut write_message_raw = |payload: &[u8], tamper_second: bool| {
        let mut ct = vec![0u8; 65535];
        let mut frag = vec![0u8; 65519]; // 1 continuation byte + max payload
        let chunks: Vec<&[u8]> = payload.chunks(65518).collect();
        for (i, chunk) in chunks.iter().enumerate() {
            let more = i + 1 < chunks.len();
            frag[0] = u8::from(more);
            frag[1..=chunk.len()].copy_from_slice(chunk);
            let n = ts
                .write_message(&frag[..=chunk.len()], &mut ct)
                .expect("encrypt");
            // Snow ciphertexts are capped at 65535 bytes; the u32 prefix
            // cannot truncate.
            let mut len = u32::try_from(n).expect("snow ciphertext length fits u32");
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

/// `send_message_by` with a generous deadline and stall round-trips a fragmented
/// message: the bounded path writes every fragment exactly like `send_message`,
/// and a follow-up send proves the shared `TransportState` nonces stay in sync
/// across the bounded frames (each write is re-armed to the tighter of the two
/// bounds, but that is a write-policy detail, not a framing one).
#[test]
#[ignore = "integration"]
fn send_message_by_round_trips_a_fragmented_message() {
    let (listener, server_sk, server_pk, client_sk, client_pk) =
        noise_test_pair().expect("noise test listener bind");
    let addr = listener.local_addr().expect("local addr");

    let (tx, rx) = mpsc::channel();

    thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        let result = (|| -> Result<(), String> {
            let mut server = handshake_responder(stream, &server_sk, |pk| *pk == client_pk)
                .map_err(|e| format!("server handshake failed: {e:?}"))?;

            // One byte past the single-fragment capacity: the sender splits this
            // into two fragments, and the receiver glues them back.
            let expected = vec![0x5Au8; 65518 + 1];
            let payload = server
                .recv_message()
                .map_err(|e| format!("recv fragmented message failed: {e:?}"))?;
            if payload.as_slice() != expected.as_slice() {
                return Err(format!(
                    "fragmented payload mismatch: got {} bytes, want {} bytes of 0x5A",
                    payload.len(),
                    expected.len()
                ));
            }

            // The follow-up proves the send/recv nonces stayed in sync across the
            // deadline-bounded fragments: a desync would fail this decrypt.
            let tail = server
                .recv_message()
                .map_err(|e| format!("recv tail failed: {e:?}"))?;
            if tail.as_slice() != b"tail" {
                return Err(format!("expected b\"tail\", got {tail:?}"));
            }
            Ok(())
        })();
        tx.send(result).expect("send server result");
    });

    let stream = TcpStream::connect(addr).expect("connect");
    let mut client = handshake_initiator(stream, &client_sk, &server_pk).expect("client handshake");

    // A budget far larger than the local loopback round trip needs, and a stall
    // that no loopback write can approach.
    let deadline = Instant::now() + Duration::from_secs(30);
    let stall = Duration::from_secs(5);
    let payload = vec![0x5Au8; 65518 + 1];
    client
        .send_message_by(&payload, deadline, stall)
        .expect("send fragmented payload within the deadline");
    client
        .send_message_by(b"tail", Instant::now() + Duration::from_secs(30), stall)
        .expect("send tail within the deadline");

    let server_result = rx.recv().expect("recv server result");
    assert!(
        server_result.is_ok(),
        "server should reassemble both deadline-bounded messages: {:?}",
        server_result.err()
    );
}

/// An already-elapsed deadline must fail immediately, not block: `write_all_with_stall`
/// returns `TimedOut` before its first syscall, which surfaces as
/// `TransportError::Io`. The receiver is left with no bytes and reports the
/// client's close as a graceful disconnect.
#[test]
#[ignore = "integration"]
fn send_message_by_fails_promptly_when_the_deadline_has_elapsed() {
    let (listener, server_sk, server_pk, client_sk, client_pk) =
        noise_test_pair().expect("noise test listener bind");
    let addr = listener.local_addr().expect("local addr");

    let (tx, rx) = mpsc::channel();

    thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        let result = (|| -> Result<(), String> {
            let mut server = handshake_responder(stream, &server_sk, |pk| *pk == client_pk)
                .map_err(|e| format!("server handshake failed: {e:?}"))?;
            // The client writes nothing (its deadline fired first), then closes;
            // the server sees that as a graceful disconnect rather than a frame.
            match server.recv_message() {
                Err(TransportError::ConnectionClosed) | Ok(_) => Ok(()),
                Err(e) => Err(format!("unexpected recv error: {e:?}")),
            }
        })();
        tx.send(result).expect("send server result");
    });

    let stream = TcpStream::connect(addr).expect("connect");
    let mut client = handshake_initiator(stream, &client_sk, &server_pk).expect("client handshake");

    match client.send_message_by(b"never sent", Instant::now(), Duration::from_secs(5)) {
        Err(TransportError::Io(e)) => assert_eq!(
            e.kind(),
            std::io::ErrorKind::TimedOut,
            "an elapsed deadline must surface as a TimedOut-shaped Io error"
        ),
        other => panic!("expected Io(TimedOut) from an elapsed deadline, got {other:?}"),
    }

    // Close our end so the server's blocking recv returns instead of hanging.
    drop(client);
    let server_result = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("server must return instead of hanging");
    assert!(
        server_result.is_ok(),
        "server side should report the client's graceful close: {:?}",
        server_result.err()
    );
}

/// A caller asking for more than the codec's message cap is a LOCAL fault, so it
/// is refused at the source as `InternalInvariant` — never `InvalidFragment`,
/// which `choreo-client-core` classifies as a handshake refusal and renders as
/// enrollment remediation. The check runs before any fragment is staged, so
/// nothing reaches the wire: the peer only sees the connection close.
#[test]
#[ignore = "integration"]
fn an_oversize_outgoing_message_is_an_internal_invariant() {
    let (listener, server_sk, server_pk, client_sk, client_pk) =
        noise_test_pair().expect("noise test listener bind");
    let addr = listener.local_addr().expect("local addr");

    let (tx, rx) = mpsc::channel();

    thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        let result = (|| -> Result<(), String> {
            let mut server = handshake_responder(stream, &server_sk, |pk| *pk == client_pk)
                .map_err(|e| format!("server handshake failed: {e:?}"))?;
            // The client wrote nothing, so this is its close.
            match server.recv_message() {
                Err(TransportError::ConnectionClosed) | Ok(_) => Ok(()),
                Err(e) => Err(format!("unexpected recv error: {e:?}")),
            }
        })();
        tx.send(result).expect("send server result");
    });

    let stream = TcpStream::connect(addr).expect("connect");
    let mut client = handshake_initiator(stream, &client_sk, &server_pk).expect("client handshake");

    let oversize = vec![0xABu8; choreo_proto::MAX_FRAME_SIZE + 1];
    let error = client
        .send_message(&oversize)
        .expect_err("one byte past the message cap must be refused");
    assert!(
        matches!(error, TransportError::InternalInvariant(_)),
        "an over-cap outgoing message is a local fault, not a peer violation: {error:?}"
    );
    assert!(
        error.to_string().contains("exceeds the"),
        "the refusal must name the cap it broke: {error}"
    );

    drop(client);
    let server_result = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("server must return instead of hanging");
    assert!(
        server_result.is_ok(),
        "server side should report the client's graceful close: {:?}",
        server_result.err()
    );
}
