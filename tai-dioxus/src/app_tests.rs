use super::*;
use crate::state::HistoryItem;
use tai_client_core::{ShellCommand, parse_input_line};
use tai_proto::{DaemonMessage, ImageMetadata, OutputStream};

#[test]
fn parses_empty_line() {
    let mut next = 1;
    assert_eq!(parse_input_line("   ", &mut next), ShellCommand::Empty);
    assert_eq!(next, 1);
}

#[test]
fn parses_ping() {
    let mut next = 3;
    assert_eq!(
        parse_input_line("/ping", &mut next),
        ShellCommand::Send(ClientMessage::Ping)
    );
    assert_eq!(next, 3);
}

#[test]
fn parses_cancel() {
    let mut next = 3;
    assert_eq!(
        parse_input_line("/cancel 42", &mut next),
        ShellCommand::Send(ClientMessage::Cancel { request_id: 42 })
    );
    assert_eq!(next, 3);
}

#[test]
fn rejects_invalid_cancel() {
    let mut next = 3;
    assert_eq!(
        parse_input_line("/cancel nope", &mut next),
        ShellCommand::InvalidCancel("nope".to_string())
    );
    assert_eq!(next, 3);
}

#[test]
fn parses_test_image_command() {
    let mut next = 10;
    assert_eq!(
        parse_input_line("/image", &mut next),
        ShellCommand::Send(ClientMessage::TestImage { request_id: 10 })
    );
    assert_eq!(next, 11);
}

#[test]
fn parses_models_command() {
    let mut next = 10;
    assert_eq!(
        parse_input_line("/models", &mut next),
        ShellCommand::Send(ClientMessage::ListModels)
    );
    assert_eq!(next, 10);
}

#[test]
fn parses_set_model_command() {
    let mut next = 10;
    assert_eq!(
        parse_input_line("/models gpt-5.4-nano", &mut next),
        ShellCommand::Send(ClientMessage::SetModel {
            model: "gpt-5.4-nano".to_string(),
        })
    );
    assert_eq!(next, 10);
}

#[test]
fn parses_run_input_and_increments_request_id() {
    let mut next = 10;
    assert_eq!(
        parse_input_line("hello world", &mut next),
        ShellCommand::Send(ClientMessage::RunInput {
            request_id: 10,
            input: b"hello world".to_vec(),
        })
    );
    assert_eq!(next, 11);
}

#[test]
fn app_state_stream_updates_history() {
    let mut state = AppState::new("/tmp/tai.sock".to_string());
    state.begin_stream(7);
    state.append_stream(7, OutputStream::Reasoning, "thinking");
    state.append_stream(7, OutputStream::Answer, "hello");
    state.append_stream(7, OutputStream::Answer, " world");

    let index = state.client.in_progress[&7];
    match &state.client.history[index] {
        HistoryItem::Streaming(entry) => {
            assert_eq!(entry.request_id, 7);
            assert_eq!(entry.reasoning, "thinking");
            assert_eq!(entry.answer, "hello world");
        }
        _ => panic!("expected streaming entry"),
    }
}

#[test]
fn apply_daemon_image_messages_pushes_renderable_image() {
    let mut state = AppState::new("/tmp/tai.sock".to_string());
    let metadata = ImageMetadata {
        image_id: 5,
        mime_type: "image/png".to_string(),
        width: 1,
        height: 1,
        byte_len: 68,
        alt: Some("tiny".to_string()),
    };
    let png = vec![
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x04, 0x00, 0x00, 0x00, 0xB5,
        0x1C, 0x0C, 0x02, 0x00, 0x00, 0x00, 0x0B, 0x49, 0x44, 0x41, 0x54, 0x78, 0xDA, 0x63, 0xFC,
        0xFF, 0x1F, 0x00, 0x03, 0x03, 0x01, 0xFF, 0xA5, 0xC2, 0xB9, 0x81, 0x00, 0x00, 0x00, 0x00,
        0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];

    apply_daemon_message(
        &mut state,
        DaemonMessage::ImageStart {
            request_id: 7,
            metadata: metadata.clone(),
        },
        None,
    )
    .expect("start");
    apply_daemon_message(
        &mut state,
        DaemonMessage::ImageChunk {
            request_id: 7,
            image_id: 5,
            data: png,
        },
        None,
    )
    .expect("chunk");
    apply_daemon_message(
        &mut state,
        DaemonMessage::ImageEnd {
            request_id: 7,
            image_id: 5,
        },
        None,
    )
    .expect("end");

    match state.client.history.last().expect("image history item") {
        HistoryItem::Image(image) => {
            assert_eq!(image.metadata, metadata);
            assert!(image.data_url.starts_with("data:image/png;base64,"));
        }
        other => panic!("expected image item, got {other:?}"),
    }
}
