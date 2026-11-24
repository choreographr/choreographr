//! The caps, deadlines and stalls that bound a message: the refusals a hostile
//! or corrupted wire prefix earns, the deadline-bounded send path, the
//! idle-versus-stalled distinction on receive, and the post-failure poisoning that
//! refuses a stream whose byte offset is no longer known. The cumulative
//! reassembly cap is a unit test of its arithmetic in `noise::frame` instead —
//! reaching it end to end would mean pushing `MAX_FRAME_SIZE` of ciphertext
//! through the socket.

use choreo_transport::error::TransportError;
use choreo_transport::handshake::{handshake_initiator, handshake_responder};
use choreo_transport::noise::NoiseStream;
use choreo_transport::noise::wire::{LEN_PREFIX_LEN, MAX_PLAINTEXT_CHUNK};
use std::io::Write;
use std::net::TcpStream;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use crate::common::{noise_test_pair, raw_handshake_initiator, raw_handshake_responder};

/// Test that a hostile or corrupted length prefix cannot make the receiver
/// allocate a huge buffer or hang: the 4-byte data-plane prefix is NOT
/// authenticated (it precedes the GCM ciphertext), so a peer can send any
/// value. `recv_message` must reject a fragment longer than the
/// `MAX_CIPHERTEXT_LEN`-byte ciphertext cap BEFORE allocating or reading that
/// many bytes — a receiver that trusted the prefix would allocate (and then
/// block in `read_exact` waiting for) the declared `0x7FFF_FFFF` bytes, roughly
/// 2 GiB.
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
    // (far above the MAX_CIPHERTEXT_LEN-byte cap), bypassing send_message so the
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
            let expected = vec![0xABu8; MAX_PLAINTEXT_CHUNK + 100];
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

    // Control message: intact prefixes, two fragments — the same framing the
    // transport itself writes, so the receiver must reassemble it.
    let payload = vec![0xABu8; MAX_PLAINTEXT_CHUNK + 100];
    for frame in super::wire_frames(&mut ts, &payload) {
        stream.write_all(&frame).expect("write control frame");
    }

    // Attack message: the identical payload, with the second fragment's length
    // prefix decremented by one before the bytes reach the wire.
    let mut frames = super::wire_frames(&mut ts, &payload);
    let second = frames
        .get_mut(1)
        .expect("the payload spans two fragments, so a second frame exists");
    let mut prefix = [0u8; LEN_PREFIX_LEN];
    prefix.copy_from_slice(
        second
            .get(..LEN_PREFIX_LEN)
            .expect("a frame opens with its length prefix"),
    );
    // Decrement (never increment): a larger prefix would make the receiver
    // block waiting for bytes we never send, hanging the test. A smaller one
    // trips the GCM authentication instead.
    let tampered = (u32::from_be_bytes(prefix) - 1).to_be_bytes();
    second
        .get_mut(..LEN_PREFIX_LEN)
        .expect("a frame opens with its length prefix")
        .copy_from_slice(&tampered);
    for frame in frames {
        stream.write_all(&frame).expect("write tampered frame");
    }

    let server_result = rx.recv().expect("recv server result");
    assert!(
        server_result.is_ok(),
        "tampered length prefix must be rejected: {:?}",
        server_result.err()
    );
}

/// `send_message_by` with a generous deadline round-trips a fragmented message:
/// the caller-overridden budget writes every fragment exactly like `send_message`,
/// and a follow-up send proves the shared `TransportState` nonces stay in sync
/// across the bounded frames (each write is re-armed to the tighter of the
/// remaining budget and the connection's stall, but that is a write-policy detail,
/// not a framing one).
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
            let expected = vec![0x5Au8; MAX_PLAINTEXT_CHUNK + 1];
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

    // A budget far larger than the local loopback round trip needs, and a
    // connection stall that no loopback write can approach — one value on the
    // connection's shared record, so it covers every fragment of every message in
    // both directions rather than one call.
    let deadline = Instant::now() + Duration::from_secs(30);
    client.set_stall(Duration::from_secs(5));
    let payload = vec![0x5Au8; MAX_PLAINTEXT_CHUNK + 1];
    client
        .send_message_by(&payload, deadline)
        .expect("send fragmented payload within the deadline");
    client
        .send_message_by(b"tail", Instant::now() + Duration::from_secs(30))
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
/// `TransportError::Io`. That failure also poisons the connection — a failed
/// transfer may have left part of a frame on the wire — so the next send is
/// refused with `StreamUnusable` before it touches the socket. The receiver is left
/// with no bytes and reports the client's close as a graceful disconnect.
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

    match client.send_message_by(b"never sent", Instant::now()) {
        Err(TransportError::Io(e)) => assert_eq!(
            e.kind(),
            std::io::ErrorKind::TimedOut,
            "an elapsed deadline must surface as a TimedOut-shaped Io error"
        ),
        other => panic!("expected Io(TimedOut) from an elapsed deadline, got {other:?}"),
    }

    // The connection is refused from here on rather than allowed to desync the
    // peer. The refusal happens at the entry point, before the socket is touched,
    // so it is immediate and writes nothing — which is also why the server still
    // sees a graceful close rather than a partial frame.
    match client.send_message(b"after the failure") {
        Err(TransportError::StreamUnusable) => {}
        other => panic!("a failed transfer must poison the stream, got {other:?}"),
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
/// nothing reaches the wire — which also means it must NOT poison the connection:
/// a normal message sent afterwards is received intact.
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
            // The refused oversize never reached the wire, so the FIRST thing the
            // server reads is the normal message that followed it.
            let message = server
                .recv_message()
                .map_err(|e| format!("a normal message must follow a refused oversize: {e:?}"))?;
            if message.as_slice() != b"still usable" {
                return Err(format!(
                    "expected b\"still usable\", got {:?}",
                    String::from_utf8_lossy(&message)
                ));
            }
            Ok(())
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

    // A local refusal never touched the wire, so the stream stays usable.
    client
        .send_message(b"still usable")
        .expect("a caller-side refusal must leave the connection usable");

    let server_result = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("server must return instead of hanging");
    assert!(
        server_result.is_ok(),
        "the message after the refusal must arrive intact: {:?}",
        server_result.err()
    );
    drop(client);
}

/// A failed receive leaves the encrypted byte stream at an unknown offset, so the
/// stream must refuse every later read rather than reassemble whatever happens to
/// be there. An over-cap length prefix is the cheapest way to fail a read after the
/// handshake: the first `recv_message` returns `InvalidFragment`, and the second
/// must return `StreamUnusable` WITHOUT touching the socket — the peer sends
/// nothing further, so a receiver that tried to read would block until this test's
/// own timeout fired instead.
#[test]
#[ignore = "integration"]
fn a_failed_receive_poisons_the_stream() {
    let (listener, server_sk, server_pk, client_sk, client_pk) =
        noise_test_pair().expect("noise test listener bind");
    let addr = listener.local_addr().expect("local addr");

    let (tx, rx) = mpsc::channel();

    thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        let result = (|| -> Result<(), String> {
            let (stream, ts) = raw_handshake_responder(stream, &server_sk, &client_pk)?;
            let mut server = NoiseStream::new(stream, ts);

            match server.recv_message() {
                Err(TransportError::InvalidFragment(_)) => {}
                other => {
                    return Err(format!(
                        "an over-cap fragment prefix must be refused as InvalidFragment, got {other:?}"
                    ));
                }
            }

            // The peer writes nothing more, so this can only return because the
            // entry point refuses a poisoned stream before the socket is touched.
            match server.recv_message() {
                Err(TransportError::StreamUnusable) => Ok(()),
                other => Err(format!(
                    "a failed receive must poison the stream, got {other:?}"
                )),
            }
        })();
        tx.send(result).expect("send server result");
    });

    let stream = TcpStream::connect(addr).expect("connect");
    let (mut stream, _ts) =
        raw_handshake_initiator(stream, &client_sk, &server_pk).expect("client handshake");

    // Raw write of a length prefix claiming 0x7FFF_FFFF ciphertext bytes (far
    // above the MAX_CIPHERTEXT_LEN-byte cap), bypassing send_message so the wire
    // carries a genuinely invalid frame.
    stream
        .write_all(&0x7FFF_FFFFu32.to_be_bytes())
        .expect("write bogus prefix");

    let server_result = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("server must return instead of hanging");
    assert!(
        server_result.is_ok(),
        "a failed receive must poison the stream: {:?}",
        server_result.err()
    );
    // Closed only after the assertion: while the socket stays open the server's
    // second read can be refused by the poisoning alone, not by our EOF.
    drop(stream);
}

/// A connection with nothing in flight is **idle, not broken**: the receive path
/// bounds only a message that is already arriving, and clears the socket's (sticky)
/// read timeout before waiting for the next one. This is the test the naive
/// implementation fails — arming a timeout across the whole receive would reap this
/// connection, which is exactly what a front-end left open overnight does.
///
/// The peer sends nothing for several stalls, then sends a message: that message
/// must arrive. Only a *stalled message* is a fault, never a silent connection.
#[test]
#[ignore = "integration"]
fn an_idle_connection_between_messages_survives() {
    let (listener, server_sk, server_pk, client_sk, client_pk) =
        noise_test_pair().expect("noise test listener bind");
    let addr = listener.local_addr().expect("local addr");

    // Short enough that an "idle counts as stalled" implementation fails fast, long
    // enough that the write below is not racing the assertion.
    let stall = Duration::from_millis(250);

    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        let result = (|| -> Result<String, String> {
            let mut server = handshake_responder(stream, &server_sk, |pk| *pk == client_pk)
                .map_err(|e| format!("server handshake failed: {e:?}"))?;
            server.set_stall(stall);
            // The peer is silent here for several stalls: this read must block
            // rather than fail.
            let message = server
                .recv_message()
                .map_err(|e| format!("an idle connection must not be reaped: {e:?}"))?;
            Ok(String::from_utf8_lossy(&message).into_owned())
        })();
        tx.send(result).expect("send server result");
    });

    let stream = TcpStream::connect(addr).expect("connect");
    let mut client = handshake_initiator(stream, &client_sk, &server_pk).expect("client handshake");

    // Stay quiet well past the stall, then speak. No sleeps: the client simply does
    // its work late, which the server's blocked read cannot distinguish from an idle
    // session.
    thread::sleep(stall * 4);
    client
        .send_message(b"still here")
        .expect("send after idling");

    let received = rx
        .recv_timeout(Duration::from_secs(10))
        .expect("server must answer instead of hanging")
        .expect("the message sent after the idle period must be received");
    assert_eq!(received, "still here");
}

/// A peer that starts a message and then stops is a fault, and is reaped after the
/// stall rather than holding the connection forever. This is the case the
/// per-message bound exists for: it pins a connection slot, a thread and up to the
/// reassembly cap of memory, and unlike a vanished peer it is not caught by
/// keepalive (the socket is alive; the sender has just gone quiet mid-frame).
#[test]
#[ignore = "integration"]
fn a_stalled_message_is_reaped_after_the_stall() {
    let (listener, server_sk, server_pk, client_sk, client_pk) =
        noise_test_pair().expect("noise test listener bind");
    let addr = listener.local_addr().expect("local addr");

    let stall = Duration::from_millis(250);

    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        let result = (|| -> Result<(), String> {
            let mut server = handshake_responder(stream, &server_sk, |pk| *pk == client_pk)
                .map_err(|e| format!("server handshake failed: {e:?}"))?;
            server.set_stall(stall);
            match server.recv_message() {
                Err(TransportError::Io(e)) if e.kind() == std::io::ErrorKind::TimedOut => Ok(()),
                other => Err(format!(
                    "a peer that stops mid-message must be reaped with a TimedOut, got {other:?}"
                )),
            }
        })();
        tx.send(result).expect("send server result");
    });

    let stream = TcpStream::connect(addr).expect("connect");
    let client = handshake_initiator(stream, &client_sk, &server_pk).expect("client handshake");

    // The start of a length prefix and one payload byte, then silence: the message
    // is in flight and will never complete. Written through `get_ref` because the
    // tampered wire needs raw bytes `send_message` would never produce.
    {
        let mut wire = std::io::BufWriter::new(client.get_ref());
        wire.write_all(&[0, 0, 0xFF, 0xFF]).expect("write prefix");
        wire.write_all(&[0xAB]).expect("write partial body");
        wire.flush().expect("flush partial frame");
    }

    let outcome = rx
        .recv_timeout(Duration::from_secs(10))
        .expect("server must return instead of hanging");
    assert!(
        outcome.is_ok(),
        "a stalled message must be reaped: {:?}",
        outcome.err()
    );
    drop(client);
}

/// The receive path reads the 4-byte length prefix in TWO steps — an unbounded
/// read for whatever arrives first, then a stall-bounded read for the rest — so a
/// prefix that lands in pieces must still be reassembled. This is the case the
/// two-phase split introduces and nothing else covers: every other test writes the
/// whole prefix in one call, which the kernel delivers in one segment, so the
/// `prefix_filled` carry-over between the phases is never exercised.
///
/// The client waits for the server to be blocked in `recv_message` before it
/// writes anything, sends one byte, pauses well past the time a blocked reader
/// needs to be woken by it, and only then sends the remaining three prefix bytes
/// plus the ciphertext. A run where the two writes nonetheless coalesced would
/// still pass — what is asserted is that the frame reassembles, not how it was
/// split — but on any machine that schedules the reader at all, the first read
/// returns one byte and the tail comes from the bounded phase.
#[test]
#[ignore = "integration"]
fn a_length_prefix_split_across_the_two_read_phases_reassembles() {
    let (listener, server_sk, server_pk, client_sk, client_pk) =
        noise_test_pair().expect("noise test listener bind");
    let addr = listener.local_addr().expect("local addr");

    let payload = b"prefix split across the idle boundary".to_vec();

    let (tx, rx) = mpsc::channel();
    // Signalled once the server is about to block in its unbounded first read, so
    // the client's pause below happens with the reader already waiting.
    let (blocked_tx, blocked_rx) = mpsc::channel();
    let expected = payload.clone();
    thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        let result = (|| -> Result<(), String> {
            let (stream, ts) = raw_handshake_responder(stream, &server_sk, &client_pk)?;
            let mut server = NoiseStream::new(stream, ts);
            // A bound long enough that a correctly split prefix completes well
            // inside it, and short enough that a genuinely stuck read fails the
            // test rather than hanging the suite.
            server.set_stall(Duration::from_secs(5));
            blocked_tx.send(()).map_err(|e| e.to_string())?;
            let message = server
                .recv_message()
                .map_err(|e| format!("a split prefix must reassemble: {e:?}"))?;
            if message.as_slice() != expected.as_slice() {
                return Err(format!(
                    "reassembled {} bytes, want the {} sent",
                    message.len(),
                    expected.len()
                ));
            }
            Ok(())
        })();
        tx.send(result).expect("send server result");
    });

    let stream = TcpStream::connect(addr).expect("connect");
    let (mut stream, mut ts) =
        raw_handshake_initiator(stream, &client_sk, &server_pk).expect("client handshake");
    blocked_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("server must reach its first read");

    let frames = super::wire_frames(&mut ts, &payload);
    let frame = frames.first().expect("a small payload is one frame");
    let (head, tail) = frame.split_at(1);
    stream.write_all(head).expect("write the first prefix byte");
    stream.flush().expect("flush the partial prefix");
    // The gap is what makes the first read return a SHORT prefix: the server is
    // already blocked in that read, so the single byte wakes it immediately and
    // the remaining three bytes arrive after it has returned.
    thread::sleep(Duration::from_millis(100));
    stream.write_all(tail).expect("write the rest of the frame");
    stream.flush().expect("flush the rest of the frame");

    let outcome = rx
        .recv_timeout(Duration::from_secs(10))
        .expect("server must return instead of hanging");
    assert!(
        outcome.is_ok(),
        "a prefix split across the two read phases must reassemble: {:?}",
        outcome.err()
    );
}
