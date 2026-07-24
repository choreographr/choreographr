use super::*;
use crate::client::handle_shell_command;
use tai_client_core::{ShellCommand, dispatch_daemon_message};
use tai_proto::{
    ClientMessage, DaemonMessage, DisplayedImageRecord, ImageMetadata, OutputStream, TimestampMs,
    TokenUsage, Turn,
};

#[test]
fn app_state_stream_updates_history() {
    let mut state = AppState::new("/tmp/tai.sock".to_string());

    // Simulate a Started message to set up request-to-turn mapping.
    dispatch_daemon_message(
        &DaemonMessage::Started {
            request_id: 7,
            turn_id: 1,
            estimated_prompt_tokens: 0,
        },
        &mut state,
    );

    // The turn should have been created by the TurnAppended message (sent
    // before Started in practice). We insert a stub turn manually.
    state.session_view.turns.insert(
        1,
        Turn {
            created_at: TimestampMs::now(),
            undone: false,
            error: None,
            user_text: Some("hello".into()),
            assistant_text: None,
            assistant_reasoning: None,
            tool_calls: vec![],
            token_usage: None,
            tool_results: vec![],
            displayed_images: vec![],
        },
    );

    dispatch_daemon_message(
        &DaemonMessage::OutputChunk {
            request_id: 7,
            stream: OutputStream::Reasoning,
            data: b"thinking".to_vec(),
        },
        &mut state,
    );

    dispatch_daemon_message(
        &DaemonMessage::OutputChunk {
            request_id: 7,
            stream: OutputStream::Answer,
            data: b"hello".to_vec(),
        },
        &mut state,
    );

    dispatch_daemon_message(
        &DaemonMessage::OutputChunk {
            request_id: 7,
            stream: OutputStream::Answer,
            data: b" world".to_vec(),
        },
        &mut state,
    );

    let turn = state
        .session_view
        .turns
        .get(&1)
        .expect("turn 1 should exist");

    assert_eq!(turn.assistant_reasoning.as_deref(), Some("thinking"));
    assert_eq!(turn.assistant_text.as_deref(), Some("hello world"));
}

#[test]
fn apply_daemon_turn_appended_with_image() {
    let mut state = AppState::new("/tmp/tai.sock".to_string());
    let metadata = ImageMetadata {
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

    let turn = Turn {
        created_at: TimestampMs::now(),
        undone: false,
        error: None,
        user_text: Some("generate an image".into()),
        assistant_text: None,
        assistant_reasoning: None,
        tool_calls: vec![],
        token_usage: Some(TokenUsage {
            input_tokens: 10,
            output_tokens: 5,
            total_tokens: 15,
        }),
        tool_results: vec![],
        displayed_images: vec![DisplayedImageRecord {
            metadata: metadata.clone(),
            data: png,
            tool_call_id: None,
        }],
    };

    dispatch_daemon_message(
        &DaemonMessage::TurnFinalized { turn_id: 1, turn },
        &mut state,
    );

    let stored = state
        .session_view
        .turns
        .get(&1)
        .expect("turn 1 should exist");

    assert_eq!(stored.displayed_images.len(), 1);
    assert_eq!(stored.displayed_images[0].metadata, metadata);
}

// ── Shell command dispatch ────────────────────────────────────────────

#[test]
fn handle_continue_when_attached_sends_continue_generation() {
    let mut state = AppState::new("/tmp/tai.sock".to_string());
    state.attached_session_id = Some(42);
    state.next_request_id = 5;
    let (tx, rx) = std::sync::mpsc::channel();

    handle_shell_command(&mut state, Some(tx), ShellCommand::Continue);

    assert_eq!(state.next_request_id, 6);
    let msg = rx.recv().expect("should send ContinueGeneration");
    assert_eq!(msg, ClientMessage::ContinueGeneration { request_id: 5 });
}

#[test]
fn handle_continue_when_not_attached_shows_error() {
    let mut state = AppState::new("/tmp/tai.sock".to_string());
    state.attached_session_id = None;

    handle_shell_command(&mut state, None, ShellCommand::Continue);

    assert!(
        state
            .status_texts
            .iter()
            .any(|t| t.contains("no session attached"))
    );
}

#[test]
fn handle_stop_when_attached_sends_cancel_all() {
    let mut state = AppState::new("/tmp/tai.sock".to_string());
    state.attached_session_id = Some(42);
    let (tx, rx) = std::sync::mpsc::channel();

    handle_shell_command(&mut state, Some(tx), ShellCommand::Stop);

    let msg = rx.recv().expect("should send Cancel");
    assert_eq!(msg, ClientMessage::Cancel { request_id: 0 });
}

#[test]
fn handle_stop_when_not_attached_shows_error() {
    let mut state = AppState::new("/tmp/tai.sock".to_string());
    state.attached_session_id = None;

    handle_shell_command(&mut state, None, ShellCommand::Stop);

    assert!(
        state
            .status_texts
            .iter()
            .any(|t| t.contains("no session attached"))
    );
}

#[test]
fn handle_undo_sends_undo_message() {
    let mut state = AppState::new("/tmp/tai.sock".to_string());
    let (tx, rx) = std::sync::mpsc::channel();

    handle_shell_command(&mut state, Some(tx), ShellCommand::Undo);

    let msg = rx.recv().expect("should send Undo");
    assert_eq!(msg, ClientMessage::Undo);
}

#[test]
fn handle_redo_sends_redo_message() {
    let mut state = AppState::new("/tmp/tai.sock".to_string());
    let (tx, rx) = std::sync::mpsc::channel();

    handle_shell_command(&mut state, Some(tx), ShellCommand::Redo);

    let msg = rx.recv().expect("should send Redo");
    assert_eq!(msg, ClientMessage::Redo);
}
