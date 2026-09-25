use super::*;
use crate::client::handle_shell_command;
use choreo_client_core::{Command, dispatch_daemon_message};
use choreo_proto::{
    ClientMessage, DaemonMessage, DisplayedImageRecord, ImageMetadata, OutputStream, SessionEvent,
    TimestampMs, TokenUsage, Turn,
};

#[test]
fn app_state_stream_updates_history() {
    let mut state = AppState::new("/tmp/choreographr.sock");

    // Simulate a Started message to set up request-to-turn mapping.
    dispatch_daemon_message(
        DaemonMessage::Session {
            session_id: Some(1),
            event: SessionEvent::Started {
                request_id: 7,
                turn_id: 1,
                estimated_prompt_tokens: 0,
            },
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
            reasoning_artifact: None,
            reasoning_producer: None,
        },
    );

    dispatch_daemon_message(
        DaemonMessage::Session {
            session_id: Some(1),
            event: SessionEvent::OutputChunk {
                request_id: 7,
                stream: OutputStream::Reasoning,
                data: b"thinking".to_vec(),
            },
        },
        &mut state,
    );

    dispatch_daemon_message(
        DaemonMessage::Session {
            session_id: Some(1),
            event: SessionEvent::OutputChunk {
                request_id: 7,
                stream: OutputStream::Answer,
                data: b"hello".to_vec(),
            },
        },
        &mut state,
    );

    dispatch_daemon_message(
        DaemonMessage::Session {
            session_id: Some(1),
            event: SessionEvent::OutputChunk {
                request_id: 7,
                stream: OutputStream::Answer,
                data: b" world".to_vec(),
            },
        },
        &mut state,
    );

    let turn = state
        .session_view
        .turns
        .get(&1)
        .expect("turn 1 should exist");

    assert_eq!(turn.assistant_text.as_deref(), Some("hello world"));
    // Reasoning content is retained alongside the response (see stream_chunk)
    // so UIs can offer a collapsible reasoning view after the answer arrives.
}

#[test]
fn apply_daemon_turn_appended_with_image() {
    let mut state = AppState::new("/tmp/choreographr.sock");
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
            cached_tokens: 0,
        }),
        tool_results: vec![],
        displayed_images: vec![DisplayedImageRecord {
            metadata: metadata.clone(),
            data: png,
            tool_call_id: None,
        }],
        reasoning_artifact: None,
        reasoning_producer: None,
    };

    dispatch_daemon_message(
        DaemonMessage::Session {
            session_id: Some(1),
            event: SessionEvent::TurnAppended { turn_id: 1, turn },
        },
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
    let mut state = AppState::new("/tmp/choreographr.sock");
    state.attached_session_id = Some(42);
    state.next_request_id = 5;
    let (tx, rx) = crossbeam_channel::unbounded();

    handle_shell_command(&mut state, Some(tx), Command::Continue);

    assert_eq!(state.next_request_id, 6);
    let msg = rx.recv().expect("should send ContinueGeneration");
    assert_eq!(msg, ClientMessage::ContinueGeneration { request_id: 5 });
}

#[test]
fn handle_continue_when_not_attached_shows_error() {
    let mut state = AppState::new("/tmp/choreographr.sock");
    state.attached_session_id = None;

    handle_shell_command(&mut state, None, Command::Continue);

    assert!(
        state
            .status_texts
            .iter()
            .any(|t| t.contains("no session attached"))
    );
}

#[test]
fn handle_stop_when_attached_sends_cancel_all() {
    let mut state = AppState::new("/tmp/choreographr.sock");
    state.attached_session_id = Some(42);
    let (tx, rx) = crossbeam_channel::unbounded();

    handle_shell_command(&mut state, Some(tx), Command::Stop);

    let msg = rx.recv().expect("should send Cancel");
    assert_eq!(msg, ClientMessage::Cancel { request_id: 0 });
}

#[test]
fn handle_stop_when_not_attached_shows_error() {
    let mut state = AppState::new("/tmp/choreographr.sock");
    state.attached_session_id = None;

    handle_shell_command(&mut state, None, Command::Stop);

    assert!(
        state
            .status_texts
            .iter()
            .any(|t| t.contains("no session attached"))
    );
}

#[test]
fn handle_undo_sends_undo_message() {
    let mut state = AppState::new("/tmp/choreographr.sock");
    let (tx, rx) = crossbeam_channel::unbounded();

    handle_shell_command(&mut state, Some(tx), Command::Undo);

    let msg = rx.recv().expect("should send Undo");
    assert_eq!(msg, ClientMessage::Undo);
}

#[test]
fn handle_redo_sends_redo_message() {
    let mut state = AppState::new("/tmp/choreographr.sock");
    let (tx, rx) = crossbeam_channel::unbounded();

    handle_shell_command(&mut state, Some(tx), Command::Redo);

    let msg = rx.recv().expect("should send Redo");
    assert_eq!(msg, ClientMessage::Redo);
}

// ── Keystore bind/unlock flow ─────────────────────────────────────

#[test]
fn bound_message_records_the_pending_key() {
    let (_dir, _guard) = choreo_client_core::test_support::isolate_config();
    let mut state = AppState::new("/tmp/choreographr.sock");
    state.pending_unlock_key = Some(vec![3u8; 32]);
    let (tx, rx) = crossbeam_channel::unbounded();

    apply_daemon_message(&mut state, DaemonMessage::Bound, Some(tx));

    assert!(state.pending_unlock_key.is_none(), "pending key consumed");
    let store = choreo_client_core::KnownServers::load().unwrap();
    let addr = crate::client::connection_addr();
    assert_eq!(
        store.unlock_key(&addr).unwrap(),
        Some([3u8; 32]),
        "the confirmed key is recorded per-daemon"
    );
    assert!(rx.try_recv().is_err(), "Bound sends no client messages");
}

#[test]
fn keystore_unbound_auto_binds_once() {
    let (_dir, _guard) = choreo_client_core::test_support::isolate_config();
    let mut state = AppState::new("/tmp/choreographr.sock");
    // A stale verify-only pending key must be discarded by the unbound arm.
    state.pending_unlock_key = Some(vec![5u8; 32]);
    let (tx, rx) = crossbeam_channel::unbounded();

    apply_daemon_message(
        &mut state,
        DaemonMessage::KeystoreUnbound {
            error: "no binding".into(),
        },
        Some(tx.clone()),
    );
    assert!(state.keystore_auto_bind.attempted(), "bind attempt latched");
    assert!(
        state.pending_unlock_key.is_some(),
        "minted key held pending"
    );
    let ClientMessage::BindKeystore { key } = rx.recv().expect("bind sent") else {
        panic!("auto-bind must send BindKeystore");
    };
    let store = choreo_client_core::KnownServers::load().unwrap();
    let addr = crate::client::connection_addr();
    assert_eq!(
        store.unlock_key(&addr).unwrap(),
        Some(key.as_slice().try_into().unwrap()),
        "the minted key is recorded pre-send"
    );

    // A second KeystoreUnbound must NOT re-bind.
    apply_daemon_message(
        &mut state,
        DaemonMessage::KeystoreUnbound {
            error: "still unbound".into(),
        },
        Some(tx),
    );
    assert!(rx.try_recv().is_err(), "no second bind attempt");
    assert!(
        state
            .status_texts
            .iter()
            .any(|t| t.contains("still unbound")),
        "the repeat unbound report is surfaced"
    );
}

#[test]
fn keystore_unbound_status_push_auto_binds() {
    // The server-authoritative `Keystore { Unbound }` push triggers the same
    // auto-bind as the `KeystoreUnbound` reply.
    let (_dir, _guard) = choreo_client_core::test_support::isolate_config();
    let mut state = AppState::new("/tmp/choreographr.sock");
    let (tx, rx) = crossbeam_channel::unbounded();

    apply_daemon_message(
        &mut state,
        DaemonMessage::Keystore {
            state: choreo_proto::KeystoreState::Unbound,
        },
        Some(tx.clone()),
    );
    assert!(state.keystore_auto_bind.attempted());
    assert!(state.pending_unlock_key.is_some());
    assert!(matches!(
        rx.recv().expect("bind sent"),
        ClientMessage::BindKeystore { .. }
    ));

    // A duplicate unbound push is inert: no re-bind.
    apply_daemon_message(
        &mut state,
        DaemonMessage::Keystore {
            state: choreo_proto::KeystoreState::Unbound,
        },
        Some(tx),
    );
    assert!(rx.try_recv().is_err(), "no second bind attempt");
}
