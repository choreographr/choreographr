// AGENTS.md permits unwrap/expect/panic in tests/ files, but clippy's
// allow-*-in-tests config only recognizes #[test]-annotated functions —
// helper fns in this file need this file-level allowance.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::panic_in_result_fn,
    clippy::indexing_slicing,
    // pedantic backfill: these helpers predate the pedantic sweep and
    // were only covered by the deny-set allowance above.
    clippy::items_after_statements,
    clippy::used_underscore_binding,
    clippy::doc_markdown
)]
use choreo_im::bridge::{BridgeEvent, DaemonBridge};
use choreo_proto::{
    ClientMessage, DaemonMessage, OutputStream, SessionEvent, SessionStatus, SessionSummary,
    read_message, write_message,
};
use std::io::{BufReader, BufWriter};
use std::os::unix::net::UnixStream;

fn connected_bridge() -> (DaemonBridge, BufReader<UnixStream>, BufWriter<UnixStream>) {
    let (b_reader, my_writer) = UnixStream::pair().unwrap();
    let (my_reader, b_writer) = UnixStream::pair().unwrap();
    let bridge = DaemonBridge::spawn(BufReader::new(b_reader), BufWriter::new(b_writer));
    (bridge, BufReader::new(my_reader), BufWriter::new(my_writer))
}

/// A minimal `SessionSummary` (only the fields the attach policy consults
/// matter; the rest are filler).
fn summary(session_id: u64, parent_session_id: Option<u64>) -> SessionSummary {
    SessionSummary {
        session_id,
        title: None,
        selected_model: None,
        reasoning_effort: None,
        parent_session_id,
        working_dir: None,
        created_at: 0,
        last_modified: 0,
        turn_count: 0,
        status: SessionStatus::Inactive,
        active_tool_groups: vec![],
        account_name: None,
        token_usage: None,
        context_window: None,
        last_prompt_tokens: None,
        pinned: false,
        archived_at: None,
    }
}

/// The bridge now sends `ClientMessage::SubscribeSessionsSummary` and then
/// `ClientMessage::ListSessions` at startup (to pick a session to attach).
/// Tests that read the bridge's outgoing wire must consume that two-message
/// handshake before their own messages.
fn consume_startup_handshake(reader: &mut BufReader<UnixStream>) {
    let first = read_message::<_, ClientMessage>(reader).unwrap();
    assert!(matches!(first, ClientMessage::SubscribeSessionsSummary));
    let second = read_message::<_, ClientMessage>(reader).unwrap();
    assert!(matches!(second, ClientMessage::ListSessions));
}

#[ignore = "integration"]
#[test]
fn bridge_ping_pong() {
    let (bridge, mut daemon_reader, mut daemon_writer) = connected_bridge();
    let (tx, rx) = bridge.into_parts();

    tx.send(ClientMessage::Ping).unwrap();

    consume_startup_handshake(&mut daemon_reader);
    let msg = read_message::<_, ClientMessage>(&mut daemon_reader).unwrap();
    assert!(matches!(msg, ClientMessage::Ping));

    write_message(&mut daemon_writer, &DaemonMessage::Pong).unwrap();
    use std::io::Write;
    let _ = daemon_writer.flush();

    let event = rx.recv().unwrap();
    assert!(matches!(event, BridgeEvent::Pong));
}

#[ignore = "integration"]
#[test]
fn bridge_unlock_locked() {
    let (bridge, mut daemon_reader, mut daemon_writer) = connected_bridge();
    let (tx, rx) = bridge.into_parts();

    tx.send(ClientMessage::Unlock {
        private_key: vec![0u8; 32],
    })
    .unwrap();

    consume_startup_handshake(&mut daemon_reader);
    let msg = read_message::<_, ClientMessage>(&mut daemon_reader).unwrap();
    assert!(matches!(msg, ClientMessage::Unlock { .. }));

    write_message(&mut daemon_writer, &DaemonMessage::Unlocked).unwrap();
    use std::io::Write;
    let _ = daemon_writer.flush();

    let event = rx.recv().unwrap();
    assert!(matches!(event, BridgeEvent::Unlocked));
}

#[ignore = "integration"]
#[test]
fn bridge_text_streaming() {
    let (bridge, _daemon_reader, mut daemon_writer) = connected_bridge();
    let (_tx, rx) = bridge.into_parts();

    write_message(
        &mut daemon_writer,
        &DaemonMessage::Session {
            session_id: None,
            event: SessionEvent::OutputChunk {
                request_id: 1,
                stream: OutputStream::Answer,
                data: b"hello ".to_vec(),
            },
        },
    )
    .unwrap();

    write_message(
        &mut daemon_writer,
        &DaemonMessage::Session {
            session_id: None,
            event: SessionEvent::OutputChunk {
                request_id: 1,
                stream: OutputStream::Answer,
                data: b"world".to_vec(),
            },
        },
    )
    .unwrap();

    write_message(
        &mut daemon_writer,
        &DaemonMessage::Session {
            session_id: None,
            event: SessionEvent::Done {
                request_id: 1,
                token_usage: None,
                last_prompt_tokens: None,
            },
        },
    )
    .unwrap();
    use std::io::Write;
    let _ = daemon_writer.flush();

    let event = rx.recv().unwrap();
    assert!(matches!(&event, BridgeEvent::Text(text) if text == "hello world"));
}

#[ignore = "integration"]
#[test]
fn bridge_tool_call_events() {
    let (bridge, _daemon_reader, mut daemon_writer) = connected_bridge();
    let (_tx, rx) = bridge.into_parts();

    write_message(
        &mut daemon_writer,
        &DaemonMessage::Session {
            session_id: None,
            event: SessionEvent::ToolCallStarted {
                request_id: 1,
                call_id: "call_1".into(),
                tool_name: "read_file".into(),
                arguments_json: r#"{"path":"/tmp"}"#.into(),
                invocation_description: String::new(),
            },
        },
    )
    .unwrap();
    use std::io::Write;
    let _ = daemon_writer.flush();

    let event = rx.recv().unwrap();
    assert!(
        matches!(&event, BridgeEvent::ToolCallStarted { name, arguments_json }
        if name == "read_file" && arguments_json == r#"{"path":"/tmp"}"#)
    );

    write_message(
        &mut daemon_writer,
        &DaemonMessage::Session {
            session_id: None,
            event: SessionEvent::ToolResultChunk {
                request_id: 1,
                call_id: "call_1".into(),
                data: b"file contents".to_vec(),
            },
        },
    )
    .unwrap();
    write_message(
        &mut daemon_writer,
        &DaemonMessage::Session {
            session_id: None,
            event: SessionEvent::ToolCallFinished {
                request_id: 1,
                call_id: "call_1".into(),
                tool_name: "read_file".into(),
            },
        },
    )
    .unwrap();
    let _ = daemon_writer.flush();

    let event = rx.recv().unwrap();
    assert!(
        matches!(&event, BridgeEvent::ToolCallFinished { name, output }
        if name == "read_file" && output == "file contents")
    );
}

#[ignore = "integration"]
#[test]
fn bridge_tool_call_failed() {
    let (bridge, _daemon_reader, mut daemon_writer) = connected_bridge();
    let (_tx, rx) = bridge.into_parts();

    write_message(
        &mut daemon_writer,
        &DaemonMessage::Session {
            session_id: None,
            event: SessionEvent::ToolCallFailed {
                request_id: 1,
                call_id: "call_1".into(),
                tool_name: "read_file".into(),
                error: "permission denied".into(),
            },
        },
    )
    .unwrap();
    use std::io::Write;
    let _ = daemon_writer.flush();

    let event = rx.recv().unwrap();
    assert!(matches!(&event, BridgeEvent::ToolCallFailed { name, error }
        if name == "read_file" && error == "permission denied"));
}

#[ignore = "integration"]
#[test]
fn bridge_turn_images() {
    let (bridge, _daemon_reader, mut daemon_writer) = connected_bridge();
    let (_tx, rx) = bridge.into_parts();

    let turn = choreo_proto::Turn {
        created_at: choreo_proto::TimestampMs::now(),
        undone: false,
        error: None,
        user_text: Some("generate an image".into()),
        assistant_text: None,
        assistant_reasoning: None,
        tool_calls: vec![],
        token_usage: None,
        tool_results: vec![],
        displayed_images: vec![choreo_proto::DisplayedImageRecord {
            metadata: choreo_proto::ImageMetadata {
                mime_type: "image/png".into(),
                width: 100,
                height: 100,
                byte_len: 4,
                alt: None,
            },
            data: b"abcd".to_vec(),
            tool_call_id: None,
        }],
        reasoning_artifact: None,
        reasoning_producer: None,
    };

    write_message(
        &mut daemon_writer,
        &DaemonMessage::Session {
            session_id: None,
            event: SessionEvent::TurnAppended { turn_id: 1, turn },
        },
    )
    .unwrap();
    use std::io::Write;
    let _ = daemon_writer.flush();

    let event = rx.recv().unwrap();
    assert!(matches!(&event, BridgeEvent::Image { _mime, data }
        if _mime == "image/png" && data == b"abcd"));
}

#[ignore = "integration"]
#[test]
fn bridge_attaches_on_sessions_reply() {
    // The bridge picks a session from the `ListSessions` reply and attaches
    // exactly once — the pre-image-fetch handshake that makes `GetImage`
    // (and live turns) work at all.
    let (bridge, mut daemon_reader, mut daemon_writer) = connected_bridge();
    let (_tx, _rx) = bridge.into_parts();

    consume_startup_handshake(&mut daemon_reader);

    write_message(
        &mut daemon_writer,
        &DaemonMessage::Sessions {
            sessions: vec![summary(5, Some(9)), summary(42, None)],
        },
    )
    .unwrap();
    use std::io::Write;
    let _ = daemon_writer.flush();

    // The bridge must attach to the first TOP-LEVEL session (42), not the
    // sub-session listed first.
    let msg = read_message::<_, ClientMessage>(&mut daemon_reader).unwrap();
    assert_eq!(msg, ClientMessage::AttachSession { session_id: 42 });
}

#[ignore = "integration"]
#[test]
fn bridge_attaches_on_session_created_after_empty_list() {
    // If the bridge starts before any session exists (empty `ListSessions`
    // reply), a later top-level `SessionCreated` push must still trigger the
    // attach — the retry path.
    let (bridge, mut daemon_reader, mut daemon_writer) = connected_bridge();
    let (_tx, _rx) = bridge.into_parts();

    consume_startup_handshake(&mut daemon_reader);

    // Empty list: nothing to attach to yet.
    write_message(
        &mut daemon_writer,
        &DaemonMessage::Sessions { sessions: vec![] },
    )
    .unwrap();
    use std::io::Write;
    let _ = daemon_writer.flush();

    // A top-level session is created afterwards.
    write_message(
        &mut daemon_writer,
        &DaemonMessage::Session {
            session_id: Some(7),
            event: SessionEvent::SessionCreated {
                title: None,
                parent_session_id: None,
                working_dir: None,
                account_name: None,
                selected_model: None,
                reasoning_effort: None,
            },
        },
    )
    .unwrap();
    let _ = daemon_writer.flush();

    let msg = read_message::<_, ClientMessage>(&mut daemon_reader).unwrap();
    assert_eq!(msg, ClientMessage::AttachSession { session_id: 7 });
}

#[ignore = "integration"]
#[test]
fn bridge_error_variants() {
    let (bridge, _daemon_reader, mut daemon_writer) = connected_bridge();
    let (_tx, rx) = bridge.into_parts();

    write_message(
        &mut daemon_writer,
        &DaemonMessage::Session {
            session_id: None,
            event: SessionEvent::Failed {
                request_id: 1,
                error: "something went wrong".into(),
            },
        },
    )
    .unwrap();
    use std::io::Write;
    let _ = daemon_writer.flush();

    let event = rx.recv().unwrap();
    assert!(matches!(&event, BridgeEvent::Error(msg) if msg == "something went wrong"));

    write_message(
        &mut daemon_writer,
        &DaemonMessage::LockedError {
            error: "already locked".into(),
        },
    )
    .unwrap();
    let _ = daemon_writer.flush();

    let event = rx.recv().unwrap();
    assert!(matches!(&event, BridgeEvent::Error(msg) if msg == "already locked"));
}

#[ignore = "integration"]
#[test]
fn bridge_models() {
    let (bridge, _daemon_reader, mut daemon_writer) = connected_bridge();
    let (_tx, rx) = bridge.into_parts();

    write_message(
        &mut daemon_writer,
        &DaemonMessage::Models {
            models: vec!["gpt-4".into()],
            selected_model: Some("gpt-4".into()),
        },
    )
    .unwrap();
    use std::io::Write;
    let _ = daemon_writer.flush();

    let event = rx.recv().unwrap();
    match &event {
        BridgeEvent::Models { models, selected } => {
            assert_eq!(models.as_slice(), &["gpt-4".to_string()]);
            assert_eq!(*selected, Some("gpt-4".to_string()));
        }
        other => panic!("expected Models event, got {other:?}"),
    }

    write_message(
        &mut daemon_writer,
        &DaemonMessage::Session {
            session_id: None,
            event: SessionEvent::ModelSelected {
                model: "claude".into(),
                reasoning_capability: None,
            },
        },
    )
    .unwrap();
    let _ = daemon_writer.flush();

    let event = rx.recv().unwrap();
    assert!(matches!(&event, BridgeEvent::ModelSelected(model) if model == "claude"));
}

#[ignore = "integration"]
#[test]
fn bridge_cancelled_clears_buffer() {
    let (bridge, _daemon_reader, mut daemon_writer) = connected_bridge();
    let (_tx, rx) = bridge.into_parts();

    write_message(
        &mut daemon_writer,
        &DaemonMessage::Session {
            session_id: None,
            event: SessionEvent::OutputChunk {
                request_id: 42,
                stream: OutputStream::Answer,
                data: b"buffered data".to_vec(),
            },
        },
    )
    .unwrap();

    write_message(
        &mut daemon_writer,
        &DaemonMessage::Session {
            session_id: None,
            event: SessionEvent::Cancelled { request_id: 42 },
        },
    )
    .unwrap();
    use std::io::Write;
    let _ = daemon_writer.flush();

    let event = rx.recv().unwrap();
    assert!(matches!(&event, BridgeEvent::Error(msg) if msg == "cancelled"));
}
