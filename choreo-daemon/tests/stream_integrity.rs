//! Lossless-streaming integrity integration tests (Phase 3 of the
//! lossless-streaming redesign).
//!
//! These tests pin the two halves of the new delivery contract:
//!
//! * **Lossless delivery** — a client attached to a streaming session
//!   receives every `OutputChunk` / `ToolResultChunk` in order, and the
//!   byte-for-byte concatenation equals the finalized turn. The daemon never
//!   drops a broadcast message, so the live view and the record cannot
//!   diverge.
//! * **Lag-based eviction** — a client whose queue crosses the per-client
//!   byte cap is evicted (disconnected) while the daemon stays healthy.
//!
//! Tests 1 and 2 drive the SESSION-LEVEL contract (the task brief's
//! sanctioned fallback): a real `session_main` loop + `run_request_worker`
//! agent loop, fed by a scripted OpenAI-compatible SSE provider
//! (`choreo_ai_protocols::test_utils::MockProvider` — the same mock the
//! reasoning round-trip wire tests use), with two `SubscriberSink`s standing
//! in for connected clients. This exercises the lossless broadcast fan-out,
//! the agent loop's streaming, and the final-turn snapshot without the
//! socket/writer-thread machinery.
//!
//! Test 3 spawns the REAL daemon (`common::SpawnedDaemon`) with tiny
//! injectable [`LagLimits`] and a mock provider pre-registered in the daemon
//! state, connects a raw `UnixStream` client it can deliberately stop
//! reading, streams past the cap, and asserts the connection is reaped
//! (EOF) while a second client still gets a Pong.
//!
//! These tests bind real sockets and spawn real subprocesses, so per
//! AGENTS.md they live in `tests/`, are marked `#[ignore]`, and run under
//! `cargo test-integration`. Time-based waits are bounded (recv_timeout /
//! wait-for-EOF deadlines), never unbounded.

use choreo_ai_protocols::openai::{MaxTokensField, OpenAiClient, ServiceConfig};
use choreo_ai_protocols::test_utils::MockProvider;
use choreo_daemon::broadcast::{LagLimits, SubscriberSink};
use choreo_daemon::providers::InferenceProvider;
use choreo_daemon::{RequestContext, SessionCommand, session_main};
use choreo_proto::{ClientMessage, DaemonMessage, OutputStream, Turn};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::mpsc;
use std::time::{Duration, Instant};

mod common;

/// Bounded wait for a session-stream message: long enough that a wedged
/// session fails loudly instead of hanging the suite, with plenty of headroom
/// for a loaded CI box.
const TIMEOUT: Duration = Duration::from_secs(30);

/// Read timeout for the raw eviction-test client socket. Kept SHORT so the
/// wait-for-EOF loop below polls at ~5 s granularity and fails near its
/// deadline instead of overshooting it by a full timeout.
const READ_TIMEOUT: Duration = Duration::from_secs(5);

// ── Mock-provider helpers ────────────────────────────────────────────────

/// Build the SSE body of a chat-completions streaming response made of the
/// given content deltas, terminated by `[DONE]`. Each delta becomes exactly
/// one `StreamEvent::Answer` on the client side, i.e. one daemon
/// `OutputChunk`.
fn sse_text_stream(chunks: &[&str]) -> String {
    let mut sse = String::new();
    for chunk in chunks {
        let payload = serde_json::json!({ "choices": [{ "delta": { "content": chunk } }] });
        sse.push_str(&format!("data: {payload}\n\n"));
    }
    sse.push_str("data: [DONE]\n\n");
    sse
}

/// SSE body that emits a single `sh` tool call (arguments in `command`),
/// then `[DONE]` — enough for the agent loop to execute the tool.
fn sse_tool_use(command: &str) -> String {
    let args = serde_json::json!({ "command": command, "shell": "bash" });
    let payload = serde_json::json!({
        "choices": [{
            "delta": {
                "tool_calls": [{
                    "index": 0,
                    "id": "call_1",
                    "type": "function",
                    "function": { "name": "sh", "arguments": args.to_string() }
                }]
            }
        }]
    });
    format!("data: {payload}\n\ndata: [DONE]\n\n")
}

/// SSE body repeating the same content delta `count` times — a stream big
/// enough to cross a tiny lag cap even while the writer drains.
fn sse_repeat_chunks(chunk: &str, count: usize) -> String {
    let mut sse = String::with_capacity(chunk.len() * count + 64);
    for _ in 0..count {
        let payload = serde_json::json!({ "choices": [{ "delta": { "content": chunk } }] });
        sse.push_str(&format!("data: {payload}\n\n"));
    }
    sse.push_str("data: [DONE]\n\n");
    sse
}

/// An OpenAI-compatible `InferenceProvider` pointed at a mock base URL, with
/// streaming enabled. The unknown test model falls back to the Chat
/// Completions request format (the default), so the mock speaks plain
/// chat-completions SSE.
fn mock_openai_provider(base_url: String) -> InferenceProvider {
    let client = OpenAiClient::new(
        ServiceConfig {
            base_url,
            provider_slug: "openai".to_string(),
            streaming: true,
            retry_max_attempts: 1,
            connect_timeout_secs: 5,
            request_timeout_secs: 30,
            total_timeout_secs: 60,
            chat_completions_max_tokens_field: MaxTokensField::MaxCompletionTokens,
            ..Default::default()
        },
        "test-key".to_string(),
    )
    .expect("openai client");
    InferenceProvider::from_openai(client)
}

// ── Session-level harness (tests 1 & 2) ─────────────────────────────────

/// Spawn a real `session_main` loop with the given provider and return the
/// `SessionCommand` sender + thread handle. The daemon command channel has a
/// DROPPED receiver (same as `session_integration.rs`): session→daemon
/// messages fail silently, and `SetModel` validation falls back to "allow"
/// instead of blocking forever on a reply that never arrives.
fn spawn_session_with_provider(
    provider: InferenceProvider,
) -> (mpsc::Sender<SessionCommand>, std::thread::JoinHandle<()>) {
    let db = Arc::new(common::test_db());
    let (daemon_tx, _daemon_rx) = mpsc::channel();
    let (session_tx, session_rx) = mpsc::channel();
    let tool_registry = choreo_daemon::tools::ToolRegistry::new().build();
    let cmd_tx = session_tx.clone();
    let handle = std::thread::spawn(move || {
        session_main(
            session_rx,
            Some(provider),
            None,
            None,
            RequestContext {
                cmd_tx,
                session_id: 1,
                db,
                tool_registry,
                daemon_tx,
                max_turns: 0,
                lag_limits: LagLimits::default(),
                global_lag: Arc::new(AtomicUsize::new(0)),
            },
        );
    });
    (session_tx, handle)
}

/// Attach a `SubscriberSink` to the session and return its receiver.
fn attach(
    session_tx: &mpsc::Sender<SessionCommand>,
    client_id: u64,
) -> crossbeam_channel::Receiver<DaemonMessage> {
    let (tx, rx) = crossbeam_channel::unbounded::<DaemonMessage>();
    session_tx
        .send(SessionCommand::Attach {
            client_id,
            tx: SubscriberSink::new(tx),
        })
        .expect("send attach");
    rx
}

/// Receive the next daemon message on a subscriber, with a bounded timeout.
fn recv_msg(rx: &crossbeam_channel::Receiver<DaemonMessage>) -> DaemonMessage {
    rx.recv_timeout(TIMEOUT)
        .unwrap_or_else(|e| panic!("timed out waiting for daemon message: {e:?}"))
}

/// Collect every `OutputStream::Answer` chunk until `Done`, plus the last
/// finalized `TurnAppended` (the final-turn snapshot carrying
/// `assistant_text`). Returns `(concatenated_chunk_bytes, final_turn)`.
///
/// The finalized turn is broadcast BEFORE `Done` (the worker's
/// `finalize_and_broadcast_turn` runs inside the agent loop, then
/// `run_request_worker` broadcasts `Done` — both through the same FIFO
/// command channel), so stopping at `Done` never misses it.
fn collect_answer_until_done(rx: &crossbeam_channel::Receiver<DaemonMessage>) -> (Vec<u8>, Turn) {
    let mut bytes = Vec::new();
    let mut final_turn = None;
    loop {
        match recv_msg(rx) {
            DaemonMessage::OutputChunk {
                stream: OutputStream::Answer,
                data,
                ..
            } => bytes.extend_from_slice(&data),
            DaemonMessage::OutputChunk { .. } => {
                // Reasoning deltas are not part of assistant_text; skip them.
            }
            DaemonMessage::TurnAppended { turn, .. } if turn.assistant_text.is_some() => {
                final_turn = Some(turn);
            }
            DaemonMessage::Done { .. } => break,
            _ => {}
        }
    }
    let final_turn = final_turn.expect("final TurnAppended must arrive before Done");
    (bytes, final_turn)
}

/// Collect every `ToolResultChunk` (for the scripted `call_1`) until `Done`,
/// plus the last finalized `TurnAppended` carrying the tool results (the tool
/// turn's final snapshot — the model's follow-up text turn has none).
fn collect_tool_chunks_until_done(
    rx: &crossbeam_channel::Receiver<DaemonMessage>,
) -> (Vec<u8>, Turn) {
    let mut bytes = Vec::new();
    let mut tool_turn = None;
    loop {
        match recv_msg(rx) {
            DaemonMessage::ToolResultChunk { call_id, data, .. } => {
                assert_eq!(call_id, "call_1", "only the scripted sh call streams");
                bytes.extend_from_slice(&data);
            }
            DaemonMessage::TurnAppended { turn, .. } if !turn.tool_results.is_empty() => {
                tool_turn = Some(turn);
            }
            DaemonMessage::Done { .. } => break,
            _ => {}
        }
    }
    let tool_turn = tool_turn.expect("the tool turn's final TurnAppended must arrive before Done");
    (bytes, tool_turn)
}

// ── Tests ───────────────────────────────────────────────────────────────

/// The lossless proof: a many-chunk answer streamed through the real agent
/// loop + session broadcast must arrive complete (every chunk, in order) and
/// its concatenation must equal the finalized turn's `assistant_text`
/// exactly — the daemon never drops a broadcast message, so the live view
/// and the record cannot diverge.
#[test]
#[ignore]
fn streamed_answer_matches_final_turn_byte_for_byte() {
    let chunks: Vec<String> = (0..64)
        .map(|i| format!("chunk {i:02} — {}\n", "x".repeat(80)))
        .collect();
    let answer: String = chunks.concat();
    let chunk_refs: Vec<&str> = chunks.iter().map(String::as_str).collect();

    let mock = MockProvider::start(vec![(
        200,
        "text/event-stream",
        sse_text_stream(&chunk_refs),
    )]);
    let provider = mock_openai_provider(mock.base_url("v1"));
    let (session_tx, session_handle) = spawn_session_with_provider(provider);

    // Two clients attached to the same session: the broadcast fan-out must
    // deliver the identical, complete stream to both.
    let rx_a = attach(&session_tx, 10);
    let rx_b = attach(&session_tx, 20);

    session_tx
        .send(SessionCommand::SetModel {
            model: "mock-4o".to_string(),
        })
        .expect("set model");
    session_tx
        .send(SessionCommand::RunInput {
            request_id: 1,
            input: b"hello".to_vec(),
        })
        .expect("run input");

    let (streamed_a, turn_a) = collect_answer_until_done(&rx_a);
    let (streamed_b, turn_b) = collect_answer_until_done(&rx_b);

    // The stream is genuinely multi-chunk (otherwise "every chunk" is trivial).
    assert!(streamed_a.len() > 1024, "expected a substantial stream");
    assert_eq!(
        streamed_a,
        answer.as_bytes(),
        "client A lost or reordered chunks"
    );
    assert_eq!(
        streamed_b,
        answer.as_bytes(),
        "client B lost or reordered chunks"
    );

    // The lossless contract: the live stream equals the finalized turn.
    let recorded_a = turn_a
        .assistant_text
        .as_deref()
        .expect("final assistant_text");
    assert_eq!(
        streamed_a,
        recorded_a.as_bytes(),
        "the streamed view must equal the final turn byte-for-byte"
    );
    assert_eq!(
        turn_a.assistant_text, turn_b.assistant_text,
        "both clients must see the same final turn"
    );
    assert_eq!(recorded_a, answer);

    session_tx.send(SessionCommand::Shutdown).expect("shutdown");
    drop(session_tx);
    session_handle.join().expect("session thread panicked");
}

/// A streaming shell tool through the real agent loop: every
/// `ToolResultChunk` must arrive in order and their concatenation must
/// reconstruct the tool's stdout exactly — equal to the final tool result
/// body recorded on the turn (the same `$ {cmd}\n<body>\n\nExit code: N`
/// framing the shell-streaming unit tests pin).
#[test]
#[ignore]
fn tool_streaming_delivers_every_chunk_in_order() {
    const COMMAND: &str = "seq 1 10000";

    let mock = MockProvider::start(vec![
        (200, "text/event-stream", sse_tool_use(COMMAND)),
        (200, "text/event-stream", sse_text_stream(&["done"])),
    ]);
    let provider = mock_openai_provider(mock.base_url("v1"));
    let (session_tx, session_handle) = spawn_session_with_provider(provider);
    let rx = attach(&session_tx, 10);

    session_tx
        .send(SessionCommand::SetModel {
            model: "mock-4o".to_string(),
        })
        .expect("set model");
    session_tx
        .send(SessionCommand::RunInput {
            request_id: 1,
            input: b"run the tool".to_vec(),
        })
        .expect("run input");

    let (streamed, final_turn) = collect_tool_chunks_until_done(&rx);
    assert!(
        streamed.len() > 4096,
        "seq 1 10000 should stream substantially more than 4 KiB (got {})",
        streamed.len()
    );

    let result = final_turn
        .tool_results
        .iter()
        .find(|r| r.name == "sh")
        .unwrap_or_else(|| panic!("final turn must carry the sh tool result"));
    assert!(!result.is_error, "the tool must succeed");

    // The recorded body is `$ {command}\n` + the streamed body +
    // `\n\nExit code: 0` — strip the framing and require byte equality.
    let body = result
        .content
        .strip_prefix(&format!("$ {COMMAND}\n"))
        .expect("shell header prefix")
        .strip_suffix("\n\nExit code: 0")
        .expect("shell footer suffix");
    assert_eq!(
        streamed,
        body.as_bytes(),
        "the streamed tool output must equal the recorded body"
    );

    session_tx.send(SessionCommand::Shutdown).expect("shutdown");
    drop(session_tx);
    session_handle.join().expect("session thread panicked");
}

/// The full-daemon eviction path: with tiny lag caps, a client whose queue
/// crosses the per-client byte cap while it stops reading its socket is
/// reaped — the connection reports EOF within a bounded window — and the
/// daemon keeps serving other clients.
#[test]
#[ignore]
fn evicts_client_that_stops_reading() {
    // Tiny lag caps: the per-client cap is a few KiB, so the first big
    // OutputChunk crosses it and the daemon must evict the client (removing
    // it from every subscriber map and tearing the connection down via the
    // writer — notify-before-EOF when the writer is healthy, the 5 s socket
    // write timeout when it is wedged, exactly what this test exercises).
    let limits = LagLimits {
        per_client_cap: 8 * 1024,
        global_budget: 64 * 1024,
    };
    // ~2 MiB of answer text in ~2 KiB chunks — far past the cap, and more
    // than the socket buffers can absorb once the client stops reading.
    let chunk = "y".repeat(2048);
    let sse = sse_repeat_chunks(&chunk, 1024);

    let mut daemon = common::SpawnedDaemon::start_with_state(
        move || {
            let mock = MockProvider::start(vec![(200, "text/event-stream", sse.clone())]);
            let provider = mock_openai_provider(mock.base_url("v1"));
            let mut state = common::test_daemon_state_with_limits(limits);
            // Pre-register the provider under a fake account so a session
            // created with that account can resolve it at spawn (the real
            // daemon's normal resolution path is account-config + credential
            // based, which a test cannot drive without hitting a real API).
            state.providers.insert("mock-account".to_string(), provider);
            // Keep the mock's serve thread alive for the daemon's lifetime.
            (state, vec![Box::new(mock)])
        },
        &[],
    );

    // Client 1 is a RAW Unix socket driven by hand — the `Client` helper's
    // reader thread never stops reading, so the wedge has to be scripted at
    // the stream level.
    let mut stream = UnixStream::connect(daemon.socket_str()).expect("connect client 1");
    stream
        .set_read_timeout(Some(READ_TIMEOUT))
        .expect("read timeout");

    // Create a session bound to the mock account, attach, and kick off a
    // streaming request.
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
        DaemonMessage::SessionCreated { session_id, .. } => session_id,
        other => panic!("expected SessionCreated, got {other:?}"),
    };

    write_message(&mut stream, &ClientMessage::AttachSession { session_id });
    match read_message::<_, DaemonMessage>(&mut stream) {
        DaemonMessage::SessionAttached { session_id: sid } => assert_eq!(sid, session_id),
        other => panic!("expected SessionAttached, got {other:?}"),
    }
    match read_message::<_, DaemonMessage>(&mut stream) {
        DaemonMessage::SessionState { .. } => {}
        other => panic!("expected SessionState, got {other:?}"),
    }

    // Kick off the stream, read just enough to prove it started (the Started
    // broadcast + a couple of OutputChunks), then STOP reading entirely.
    write_message(
        &mut stream,
        &ClientMessage::RunInput {
            request_id: 1,
            input: b"stream a lot".to_vec(),
        },
    );
    let mut chunks_seen = 0u32;
    let start_deadline = Instant::now() + TIMEOUT;
    while chunks_seen < 2 {
        assert!(
            Instant::now() < start_deadline,
            "timed out waiting for the stream to start"
        );
        match read_message::<_, DaemonMessage>(&mut stream) {
            DaemonMessage::Started { .. } => {}
            DaemonMessage::OutputChunk { .. } => chunks_seen += 1,
            DaemonMessage::Evicted => {
                panic!("client was evicted before it deliberately stopped reading")
            }
            // The seed/status/token messages that precede the first answer
            // chunk are expected noise on the wire — only Started,
            // OutputChunk, and Evicted are load-bearing here.
            DaemonMessage::TurnAppended { .. }
            | DaemonMessage::SessionStatusChanged { .. }
            | DaemonMessage::TokenUsageUpdate { .. }
            | DaemonMessage::LiveOutputTokenCount { .. }
            | DaemonMessage::ToolCallStarted { .. }
            | DaemonMessage::ToolCallFinished { .. }
            | DaemonMessage::ToolResultChunk { .. } => {}
            other => panic!("unexpected message while the stream started: {other:?}"),
        }
    }

    // Wedged. The writer drains its lossless backlog into the socket until
    // the kernel buffers fill, then blocks; the 5 s write timeout trips and
    // the writer shuts the socket down, so the client eventually sees EOF.
    // (A healthy writer flushes the Evicted advisory and closes cleanly —
    // either way EOF arrives within the bounded deadline.)
    wait_for_eof(&mut stream, Duration::from_secs(15));

    // The daemon must stay healthy after evicting the laggard.
    let mut client2 = UnixStream::connect(daemon.socket_str()).expect("connect client 2");
    client2
        .set_read_timeout(Some(READ_TIMEOUT))
        .expect("read timeout");
    write_message(&mut client2, &ClientMessage::Ping);
    match read_message::<_, DaemonMessage>(&mut client2) {
        DaemonMessage::Pong => {}
        other => panic!("expected Pong from the healthy daemon, got {other:?}"),
    }

    daemon.shutdown();
}

// ── Raw-socket helpers (test 3) ─────────────────────────────────────────

/// Drain the stream (bounded by `deadline`) until the daemon closes the
/// connection: `read` returning 0 (EOF). Buffered data is consumed before the
/// EOF, so a notify-before-EOF `Evicted` advisory (if the writer flushed one)
/// is read here and discarded — the assertion is that the connection is
/// reaped at all.
fn wait_for_eof(stream: &mut UnixStream, deadline: Duration) {
    let deadline = Instant::now() + deadline;
    let mut buf = [0u8; 1];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => return, // EOF — the connection was reaped
            Ok(_) => {}      // buffered data still draining
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                assert!(
                    Instant::now() < deadline,
                    "timed out waiting for the evicted client's EOF"
                );
            }
            Err(e) => panic!("error waiting for the evicted client's EOF: {e}"),
        }
    }
}

/// Write a protocol message to a raw socket (4-byte BE length prefix +
/// MessagePack envelope).
fn write_message<W: Write, T: serde::Serialize>(writer: &mut W, msg: &T) {
    choreo_proto::write_message(writer, msg).expect("write protocol message");
}

/// Read a protocol message from a raw socket.
fn read_message<R: Read, T: serde::de::DeserializeOwned>(reader: &mut R) -> T {
    choreo_proto::read_message(reader).expect("read protocol message")
}
