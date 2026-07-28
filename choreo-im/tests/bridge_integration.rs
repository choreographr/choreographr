use choreo_im::bridge::{BridgeEvent, DaemonBridge};
use choreo_proto::{ClientMessage, DaemonMessage, OutputStream, read_message, write_message};
use std::io::{BufReader, BufWriter};
use std::os::unix::net::UnixStream;

fn connected_bridge() -> (DaemonBridge, BufReader<UnixStream>, BufWriter<UnixStream>) {
    let (b_reader, my_writer) = UnixStream::pair().unwrap();
    let (my_reader, b_writer) = UnixStream::pair().unwrap();
    let bridge = DaemonBridge::spawn(BufReader::new(b_reader), BufWriter::new(b_writer));
    (bridge, BufReader::new(my_reader), BufWriter::new(my_writer))
}

#[ignore]
#[test]
fn bridge_ping_pong() {
    let (bridge, mut daemon_reader, mut daemon_writer) = connected_bridge();
    let (tx, rx) = bridge.into_parts();

    tx.send(ClientMessage::Ping).unwrap();

    let msg = read_message::<_, ClientMessage>(&mut daemon_reader).unwrap();
    assert!(matches!(msg, ClientMessage::Ping));

    write_message(&mut daemon_writer, &DaemonMessage::Pong).unwrap();
    use std::io::Write;
    let _ = daemon_writer.flush();

    let event = rx.recv().unwrap();
    assert!(matches!(event, BridgeEvent::Pong));
}

#[ignore]
#[test]
fn bridge_unlock_locked() {
    let (bridge, mut daemon_reader, mut daemon_writer) = connected_bridge();
    let (tx, rx) = bridge.into_parts();

    tx.send(ClientMessage::Unlock {
        private_key: vec![0u8; 32],
    })
    .unwrap();

    let msg = read_message::<_, ClientMessage>(&mut daemon_reader).unwrap();
    assert!(matches!(msg, ClientMessage::Unlock { .. }));

    write_message(&mut daemon_writer, &DaemonMessage::Unlocked).unwrap();
    use std::io::Write;
    let _ = daemon_writer.flush();

    let event = rx.recv().unwrap();
    assert!(matches!(event, BridgeEvent::Unlocked));
}

#[ignore]
#[test]
fn bridge_text_streaming() {
    let (bridge, _daemon_reader, mut daemon_writer) = connected_bridge();
    let (_tx, rx) = bridge.into_parts();

    write_message(
        &mut daemon_writer,
        &DaemonMessage::OutputChunk {
            request_id: 1,
            stream: OutputStream::Answer,
            data: b"hello ".to_vec(),
        },
    )
    .unwrap();

    write_message(
        &mut daemon_writer,
        &DaemonMessage::OutputChunk {
            request_id: 1,
            stream: OutputStream::Answer,
            data: b"world".to_vec(),
        },
    )
    .unwrap();

    write_message(
        &mut daemon_writer,
        &DaemonMessage::Done {
            request_id: 1,
            token_usage: None,
            last_prompt_tokens: None,
        },
    )
    .unwrap();
    use std::io::Write;
    let _ = daemon_writer.flush();

    let event = rx.recv().unwrap();
    assert!(matches!(&event, BridgeEvent::Text(text) if text == "hello world"));
}

#[ignore]
#[test]
fn bridge_tool_call_events() {
    let (bridge, _daemon_reader, mut daemon_writer) = connected_bridge();
    let (_tx, rx) = bridge.into_parts();

    write_message(
        &mut daemon_writer,
        &DaemonMessage::ToolCallStarted {
            request_id: 1,
            call_id: "call_1".into(),
            tool_name: "read_file".into(),
            arguments_json: r#"{"path":"/tmp"}"#.into(),
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
        &DaemonMessage::ToolResultChunk {
            request_id: 1,
            call_id: "call_1".into(),
            data: b"file contents".to_vec(),
        },
    )
    .unwrap();
    write_message(
        &mut daemon_writer,
        &DaemonMessage::ToolCallFinished {
            request_id: 1,
            call_id: "call_1".into(),
            tool_name: "read_file".into(),
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

#[ignore]
#[test]
fn bridge_tool_call_failed() {
    let (bridge, _daemon_reader, mut daemon_writer) = connected_bridge();
    let (_tx, rx) = bridge.into_parts();

    write_message(
        &mut daemon_writer,
        &DaemonMessage::ToolCallFailed {
            request_id: 1,
            call_id: "call_1".into(),
            tool_name: "read_file".into(),
            error: "permission denied".into(),
        },
    )
    .unwrap();
    use std::io::Write;
    let _ = daemon_writer.flush();

    let event = rx.recv().unwrap();
    assert!(matches!(&event, BridgeEvent::ToolCallFailed { name, error }
        if name == "read_file" && error == "permission denied"));
}

#[ignore]
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
    };

    write_message(
        &mut daemon_writer,
        &DaemonMessage::TurnAppended { turn_id: 1, turn },
    )
    .unwrap();
    use std::io::Write;
    let _ = daemon_writer.flush();

    let event = rx.recv().unwrap();
    assert!(matches!(&event, BridgeEvent::Image { _mime, data }
        if _mime == "image/png" && data == b"abcd"));
}

#[ignore]
#[test]
fn bridge_error_variants() {
    let (bridge, _daemon_reader, mut daemon_writer) = connected_bridge();
    let (_tx, rx) = bridge.into_parts();

    write_message(
        &mut daemon_writer,
        &DaemonMessage::Failed {
            request_id: 1,
            error: "something went wrong".into(),
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

#[ignore]
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
        &DaemonMessage::ModelSelected {
            model: "claude".into(),
            reasoning_capability: None,
        },
    )
    .unwrap();
    let _ = daemon_writer.flush();

    let event = rx.recv().unwrap();
    assert!(matches!(&event, BridgeEvent::ModelSelected(model) if model == "claude"));
}

#[ignore]
#[test]
fn bridge_cancelled_clears_buffer() {
    let (bridge, _daemon_reader, mut daemon_writer) = connected_bridge();
    let (_tx, rx) = bridge.into_parts();

    write_message(
        &mut daemon_writer,
        &DaemonMessage::OutputChunk {
            request_id: 42,
            stream: OutputStream::Answer,
            data: b"buffered data".to_vec(),
        },
    )
    .unwrap();

    write_message(
        &mut daemon_writer,
        &DaemonMessage::Cancelled { request_id: 42 },
    )
    .unwrap();
    use std::io::Write;
    let _ = daemon_writer.flush();

    let event = rx.recv().unwrap();
    assert!(matches!(&event, BridgeEvent::Error(msg) if msg == "cancelled"));
}
