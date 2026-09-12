//! Mid-stream cancel force-close integration test (sockreg wiring, task 4).
//!
//! Scenario: a provider (a real local TCP server) starts an SSE response,
//! sends a couple of chunks, then STALLS — the headers promise far more body
//! than is ever written and the connection is held open. The daemon's
//! inference worker is therefore blocked in a provider `read()` — a state
//! the cooperative cancel flag alone cannot interrupt (channels cannot reach
//! into a syscall).
//!
//! The client sends `ClientMessage::Cancel`. The daemon command loop's
//! `handle_cancel_request` (the site where the cancel is DECIDED) calls
//! `SocketRegistry::shutdown_all()` on the ONE daemon-wide registry, which
//! shuts down the registered provider socket and makes the blocked read
//! return immediately. The worker then aborts the turn and the session
//! leaves `Inference` — asserted via the prompt arrival of
//! `SessionEvent::Done`, well inside the configured 30 s request timeout.
//!
//! This binds real sockets and spawns the real daemon, so per AGENTS.md it
//! lives in `tests/`, is `#[ignore = "integration"]`, and runs under `cargo test-integration`.

// AGENTS.md permits unwrap/expect/panic in tests/ files, but clippy's
// allow-*-in-tests config only recognizes #[test]-annotated functions —
// helper fns in this file need this file-level allowance.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::panic_in_result_fn,
    clippy::indexing_slicing
)]
use choreo_proto::{ClientMessage, DaemonMessage, SessionEvent, SessionStatus};
use std::fmt::Write as _;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

mod common;

/// Bounded wait for the post-cancel `Done`: generous headroom for a loaded
/// CI box, but far below the provider request timeout (30 s) — the whole
/// point is that the force-close makes the abort PROMPT rather than
/// timeout-bound.
const DONE_TIMEOUT: Duration = Duration::from_secs(15);

const READ_TIMEOUT: Duration = Duration::from_secs(30);

/// An SSE body delivering `count` small answer deltas, terminated by
/// `[DONE]` — never actually completed by the stalling server.
fn sse_prefix(chunks: usize) -> String {
    let mut sse = String::new();
    for i in 0..chunks {
        let payload =
            serde_json::json!({ "choices": [{ "delta": { "content": format!("c{i} ") } }] });
        let _ = write!(sse, "data: {payload}\n\n");
    }
    sse.push_str("data: [DONE]\n\n");
    sse
}

/// A local HTTP server that serves ONE stalling SSE response: full headers
/// promising `promised_len` body bytes, a short prefix actually written and
/// flushed, then the connection is held open forever (the server parks in a
/// read loop until the peer goes away). The bound address is available
/// before the serve thread starts, so the caller can build a client base
/// URL from it.
struct StallingSseServer {
    url_root: String,
    /// Signalled by the serve thread right after the SSE prefix is flushed —
    /// i.e. the provider connection is established, registered, and about to
    /// wedge in its body read. The test must cancel only AFTER this fires:
    /// cancelling earlier races the worker's dial (under a loaded test box
    /// the worker can be delayed by seconds after `Started`), and a cancel
    /// that lands on an empty registry closes nothing — the request then
    /// stalls to its full 30 s request timeout, past `DONE_TIMEOUT`.
    prefix_flushed_rx: std::sync::mpsc::Receiver<()>,
}

impl StallingSseServer {
    fn start(prefix: String) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind stalling server");
        let addr = listener.local_addr().expect("local addr");
        // The serve thread signals `prefix_flushed_tx` after the SSE prefix
        // is on the wire (see the field doc on the struct).
        let (prefix_flushed_tx, prefix_flushed_rx) = std::sync::mpsc::channel();
        // The serve thread is detached: it parks until the daemon-side
        // connection disappears (the registry force-close under test), and
        // test-process exit reaps it either way.
        let _ = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            // Read (and ignore) the request head — the client blocks until
            // the head arrives, which is all it needs to start the body.
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);
            // Headers promise a huge body that is never fully delivered —
            // the client must block in `read` waiting for the rest.
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n",
                4 * 1024 * 1024
            );
            stream.write_all(head.as_bytes()).expect("write head");
            stream
                .write_all(prefix.as_bytes())
                .expect("write sse prefix");
            stream.flush().expect("flush");
            // Signal NOW: the provider socket is dialed, registered, and the
            // worker is moments from wedging in its body read.
            let _ = prefix_flushed_tx.send(());
            // Hold the connection open until the peer goes away (the
            // registry force-close). Timeouts here are the poll tick — the
            // ONLY exits are a clean EOF (Ok(0)) or a real connection
            // error; breaking on a read timeout would close the socket and
            // defeat the stall.
            let _ = stream.set_read_timeout(Some(Duration::from_millis(100)));
            loop {
                match stream.read(&mut buf) {
                    Ok(0) => break,
                    Ok(_) => {}
                    Err(e)
                        if matches!(
                            e.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        ) => {}
                    Err(_) => break,
                }
            }
        });
        Self {
            url_root: format!("http://{addr}"),
            prefix_flushed_rx,
        }
    }

    /// Block (bounded) until the fake provider has actually delivered the
    /// SSE prefix, which is the earliest point the daemon's worker socket is
    /// guaranteed to be registered in the TARGET SESSION's registry.
    fn wait_until_wedged(&self, timeout: std::time::Duration) {
        self.prefix_flushed_rx
            .recv_timeout(timeout)
            .expect("stalling server never delivered its SSE prefix");
    }
}

fn write_message<W: Write, T: serde::Serialize>(writer: &mut W, msg: &T) {
    choreo_proto::write_message(writer, msg).expect("write protocol message");
}

fn read_message<R: Read, T: serde::de::DeserializeOwned>(reader: &mut R) -> T {
    choreo_proto::read_message(reader).expect("read protocol message")
}

#[test]
#[ignore = "integration"]
fn mid_stream_cancel_finishes_promptly_via_registry_force_close() {
    // The stalling server lives OUTSIDE the daemon-state closure so its join
    // handle stays with the test (SpawnedDaemon's keepalive box takes a
    // detached token instead — see `keepalive_token`).
    let staller = StallingSseServer::start(sse_prefix(3));
    // The daemon-state closure is `move` but Fn (called once per start
    // retry), so it can only BORROW its captures — it must construct its own
    // keepalive token each call from the captured URL string.
    let staller_url = staller.url_root.clone();

    let mut daemon = common::SpawnedDaemon::start_with_state(
        move || {
            let mut state = common::test_daemon_state();
            // Seed the mock account pointed at the stalling server so the
            // session resolves its provider LAZILY against its own socket
            // registry: the session's client is what dials the staller, so
            // the wedged socket lands in the session registry that the
            // cancel force-closes. The warm model cache suppresses the
            // create-session background prefetch (which would otherwise
            // race the single-connection stalling server) and lets the
            // session's model selection validate locally.
            common::seed_mock_account(
                &mut state,
                "mock-account",
                format!("{staller_url}/v1"),
                &["mock-4o"],
            );
            // Detached keepalive token, built per call (same content as
            // `StallingSseServer::keepalive_token` — a URL the box holds
            // alive so test-process exit reaps the serve thread).
            let keepalive: Box<dyn std::any::Any + Send> = Box::new(staller_url.clone());
            (state, vec![keepalive])
        },
        &[],
    );

    let mut stream = UnixStream::connect(daemon.socket_str()).expect("connect");
    stream
        .set_read_timeout(Some(READ_TIMEOUT))
        .expect("read timeout");

    write_message(
        &mut stream,
        &ClientMessage::CreateSession {
            title: None,
            parent_session_id: None,
            working_dir: None,
            context_config: None,
            account_name: Some("mock-account".to_string()),
            selected_model: Some("mock-4o".to_string()),
            reasoning_effort: None,
        },
    );
    let session_id = match read_message::<_, DaemonMessage>(&mut stream) {
        DaemonMessage::Session {
            session_id: Some(session_id),
            event: SessionEvent::SessionCreated { .. },
        } => session_id,
        other => panic!("expected SessionCreated, got {other:?}"),
    };
    write_message(&mut stream, &ClientMessage::AttachSession { session_id });
    // Drain the attach acks (SessionAttached + SessionState).
    for _ in 0..2 {
        read_message::<_, DaemonMessage>(&mut stream);
    }

    // Kick off the streaming request. The stalling provider delivers only
    // the SSE prefix and then goes quiet, so the daemon's inference worker
    // parks in a provider `read()` that will never complete on its own
    // (the promised body is 4 MiB; ~150 bytes ever arrive). We do not rely
    // on seeing OutputChunks — the client may buffer them — the in-flight
    // `Started` event plus the stalling server's contract are the wedge.
    write_message(
        &mut stream,
        &ClientMessage::RunInput {
            request_id: 1,
            input: b"hello".to_vec(),
        },
    );
    let t0 = Instant::now();
    let start_deadline = t0 + Duration::from_secs(10);
    let mut in_flight = 0u32;
    while in_flight < 2 {
        assert!(
            Instant::now() < start_deadline,
            "timed out waiting for the request to start"
        );
        match read_message::<_, DaemonMessage>(&mut stream) {
            DaemonMessage::Session {
                event: SessionEvent::Started { .. },
                ..
            } => in_flight += 1,
            // Status/usage/seed noise on the way to the first chunk.
            DaemonMessage::Session { .. } => {}
            other => panic!("unexpected message during stream start: {other:?}"),
        }
    }

    // Cancel only after the stalling server has confirmed the provider
    // connection is established and registered — see `prefix_flushed_rx`.
    // (Racing the dial would make shutdown_all a no-op and this test would
    // measure the 30 s request timeout instead of the force-close.)
    staller.wait_until_wedged(Duration::from_secs(10));
    // The cancel: decided in the daemon's handle_cancel_request, which must
    // force-close the registry's provider sockets so the wedged reader
    // unblocks NOW. Assert the turn aborts promptly: a `Done` (and the
    // session leaving `Inference`) arrives within DONE_TIMEOUT — not after
    // the 30 s request timeout.
    write_message(&mut stream, &ClientMessage::Cancel { request_id: 1 });
    let deadline = t0 + Duration::from_secs(10) + DONE_TIMEOUT;
    // Drop the read timeout for this phase so a message slightly later
    // than expected surfaces as a deadline assert, not a 30 s protocol
    // read hang. 5 s (not 2 s): the whole suite runs many daemons and
    // servers in parallel, and a too-tight per-read timeout turns a
    // scheduling hiccup into a protocol-read panic instead of letting
    // the outer DONE_TIMEOUT assert measure promptness.
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("read timeout");
    let mut finished = false;
    let mut left_inference = false;
    while !(finished && left_inference) {
        assert!(
            Instant::now() < deadline,
            "cancel did not finish the wedged request promptly — \
             the registry force-close did not reach the provider socket"
        );
        // Deadline-driven read: a socket-read timeout under a loaded test
        // box is NOT a failure — only exceeding the outer DONE_TIMEOUT is.
        // Poll in short slices so the Instant assert stays in charge.
        stream
            .set_read_timeout(Some(Duration::from_millis(250)))
            .expect("read timeout");
        let msg = loop {
            match choreo_proto::read_message::<_, DaemonMessage>(&mut stream) {
                Ok(msg) => break msg,
                Err(choreo_proto::ProtoError::Io(e))
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    assert!(
                        Instant::now() < deadline,
                        "cancel did not finish the wedged request promptly — \
                         the registry force-close did not reach the provider socket"
                    );
                }
                Err(e) => panic!("read protocol message: {e}"),
            }
        };
        match msg {
            DaemonMessage::Session {
                event: SessionEvent::SessionStatusChanged { status, .. },
                ..
            } => {
                if !matches!(status, SessionStatus::Inference) {
                    left_inference = true;
                }
            }
            DaemonMessage::Session {
                event: SessionEvent::Done { .. },
                ..
            }
            | DaemonMessage::Session {
                event: SessionEvent::Failed { .. },
                ..
            } => finished = true,
            // A cancel that beats the provider error finalizes via the
            // cancelled-turn path: the worker observes the cancel signal
            // before (or instead of) surfacing the force-closed socket's IO
            // error, so the daemon emits `Cancelled` + the cancelled turn —
            // and deliberately NO `Failed`. That is an equally prompt
            // completion, so accept it as `finished` too.
            DaemonMessage::Session {
                event: SessionEvent::Cancelled { request_id: 1 },
                ..
            } => finished = true,
            // The aborted turn's TurnAppended / Error events are expected.
            DaemonMessage::Session { .. } => {}
            other => panic!("unexpected message while cancelling: {other:?}"),
        }
    }
    assert!(
        left_inference,
        "session status must leave Inference after the cancel"
    );

    daemon.shutdown();
    // The daemon shutdown closed the client connection, releasing the
    // stalling server's read loop. `staller` was moved into the state-
    // builder closure, so its join handle is unreachable here — the serve
    // thread exits on its own once the peer disappears.
}
