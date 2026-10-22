use crate::connection::{handle_daemon_message, handle_terminal_event};
use crate::state::*;
use crate::tests::support::test_app;
use choreo_proto::{ClientMessageType, DaemonMessage, DaemonMessageType, SessionEvent};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

// ── First-run follow-through (key saved → account selected → model picker) ──

/// Every request the handler sent, by payload.
fn sent(rx: &crossbeam_channel::Receiver<choreo_proto::ClientMessage>) -> Vec<ClientMessageType> {
    rx.try_iter().map(|m| m.inner).collect()
}

#[test]
fn confirmed_key_selects_the_account_for_an_accountless_session() {
    let mut app = test_app();
    let (tx, rx) = crossbeam_channel::unbounded();
    app.attached_session_id = Some(0);
    app.page = Page::AIProviders;
    app.arm_setup_follow("anthropic");

    handle_daemon_message(
        DaemonMessage::broadcast(DaemonMessageType::CredentialAdded {
            service: "anthropic".to_string(),
        }),
        &mut app,
        &tx,
    )
    .expect("handle CredentialAdded");

    assert!(sent(&rx).iter().any(|m| matches!(
        m,
        ClientMessageType::SetSessionAccount { name } if name == "anthropic"
    )));
    assert_eq!(
        app.setup_follow,
        Some(SetupFollow::AwaitAccountSet("anthropic".to_string()))
    );
    assert_eq!(app.page, Page::Chat, "the user is returned to the chat");
}

#[test]
fn a_key_for_another_account_does_not_advance_the_follow_through() {
    let mut app = test_app();
    let (tx, rx) = crossbeam_channel::unbounded();
    app.attached_session_id = Some(0);
    app.arm_setup_follow("anthropic");

    handle_daemon_message(
        DaemonMessage::broadcast(DaemonMessageType::CredentialAdded {
            service: "other".to_string(),
        }),
        &mut app,
        &tx,
    )
    .expect("handle CredentialAdded");

    assert!(
        !sent(&rx)
            .iter()
            .any(|m| matches!(m, ClientMessageType::SetSessionAccount { .. }))
    );
    assert_eq!(
        app.setup_follow,
        Some(SetupFollow::AwaitCredential("anthropic".to_string()))
    );
}

#[test]
fn a_rejected_key_ends_the_follow_through() {
    let mut app = test_app();
    let (tx, _rx) = crossbeam_channel::unbounded();
    app.attached_session_id = Some(0);
    app.arm_setup_follow("anthropic");

    handle_daemon_message(
        DaemonMessage::broadcast(DaemonMessageType::CredentialAddFailed {
            service: "anthropic".to_string(),
            error: "invalid key".to_string(),
        }),
        &mut app,
        &tx,
    )
    .expect("handle CredentialAddFailed");

    assert_eq!(app.setup_follow, None);
}

#[test]
fn a_failed_account_selection_ends_the_follow_through() {
    let mut app = test_app();
    let (tx, _rx) = crossbeam_channel::unbounded();
    app.attached_session_id = Some(0);
    app.setup_follow = Some(SetupFollow::AwaitAccountSet("anthropic".to_string()));

    handle_daemon_message(
        DaemonMessage::broadcast(DaemonMessageType::Session {
            session_id: Some(0),
            event: SessionEvent::SessionFailed {
                operation: "set_account".to_string(),
                error: "account 'anthropic' not found".to_string(),
            },
        }),
        &mut app,
        &tx,
    )
    .expect("handle SessionFailed");

    // The selection the follow-through was waiting on was rejected, so the
    // chain must not stay armed for a later, unrelated `SessionAccountSet`.
    assert_eq!(app.setup_follow, None);
}

#[test]
fn an_unrelated_session_failure_keeps_the_follow_through() {
    let mut app = test_app();
    let (tx, _rx) = crossbeam_channel::unbounded();
    app.attached_session_id = Some(0);
    app.setup_follow = Some(SetupFollow::AwaitAccountSet("anthropic".to_string()));

    handle_daemon_message(
        DaemonMessage::broadcast(DaemonMessageType::Session {
            session_id: Some(0),
            event: SessionEvent::SessionFailed {
                operation: "set_session_pinned".to_string(),
                error: "boom".to_string(),
            },
        }),
        &mut app,
        &tx,
    )
    .expect("handle SessionFailed");

    assert_eq!(
        app.setup_follow,
        Some(SetupFollow::AwaitAccountSet("anthropic".to_string()))
    );
}

#[test]
fn selected_account_opens_the_model_picker() {
    let mut app = test_app();
    let (tx, rx) = crossbeam_channel::unbounded();
    app.attached_session_id = Some(42);
    app.active_session_id = Some(42);
    app.setup_follow = Some(SetupFollow::AwaitAccountSet("anthropic".to_string()));

    handle_daemon_message(
        DaemonMessage::broadcast(DaemonMessageType::Session {
            session_id: Some(42),
            event: SessionEvent::SessionAccountSet {
                account: "anthropic".to_string(),
            },
        }),
        &mut app,
        &tx,
    )
    .expect("handle SessionAccountSet");

    assert!(app.model_selector.is_open());
    assert!(
        sent(&rx)
            .iter()
            .any(|m| matches!(m, ClientMessageType::ListModels))
    );
    assert_eq!(app.setup_follow, None);
}

#[test]
fn model_picker_without_an_account_explains_what_is_missing() {
    let mut app = test_app();
    let (tx, rx) = crossbeam_channel::unbounded();
    app.attached_session_id = Some(0);
    app.attached_account_slug = None;

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('m'), KeyModifiers::ALT)),
        &mut app,
        &tx,
    )
    .expect("handle Alt+M");

    assert!(!app.model_selector.is_open());
    assert!(app.error().is_some_and(|e| e.contains("alt+a")));
    assert!(
        !sent(&rx)
            .iter()
            .any(|m| matches!(m, ClientMessageType::ListModels))
    );
}
