use crate::connection::{handle_daemon_message, handle_terminal_event};
use crate::test_util::test_app;
use choreo_proto::{ClientMessage, DaemonMessage};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

// ── Model selector (Ctrl+M) ──

#[test]
fn chat_ctrl_m_opens_selector_and_requests_models() {
    let (tx, rx) = std::sync::mpsc::channel();
    let mut app = test_app();

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('m'), KeyModifiers::CONTROL)),
        &mut app,
        &tx,
    )
    .expect("handle ctrl+m");

    assert!(app.model_selector.is_open(), "ctrl+m opens the selector");
    assert!(app.model_selector.loading, "selector waits for the reply");
    let msg = rx.recv().expect("sent message");
    assert_eq!(msg, ClientMessage::ListModels);
}

#[test]
fn model_selector_populates_from_models_reply() {
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = test_app();
    app.model_selector.open();

    handle_daemon_message(
        DaemonMessage::Models {
            models: vec![
                "gpt-4o".to_string(),
                "gpt-4o-mini".to_string(),
                "claude-3".to_string(),
            ],
            selected_model: Some("gpt-4o-mini".to_string()),
        },
        &mut app,
        &tx,
    )
    .expect("handle Models");

    assert!(!app.model_selector.loading);
    assert_eq!(app.model_selector.all_models.len(), 3);
    assert_eq!(
        app.model_selector.highlighted().as_deref(),
        Some("gpt-4o-mini"),
        "the active model is preselected"
    );
    // The reply must not leak into the chat status line.
    assert!(app.status.is_none());
}

#[test]
fn model_selector_enter_sends_set_model_and_closes() {
    let (tx, rx) = std::sync::mpsc::channel();
    let mut app = test_app();
    app.model_selector.open();
    app.model_selector.apply_models(
        vec!["gpt-4o".to_string(), "claude-3".to_string()],
        Some("gpt-4o".to_string()),
    );

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("handle enter");

    assert!(!app.model_selector.is_open(), "enter closes the selector");
    let msg = rx.recv().expect("sent message");
    assert_eq!(
        msg,
        ClientMessage::SetModel {
            model: "gpt-4o".to_string()
        }
    );
}

#[test]
fn model_selector_esc_closes_without_sending() {
    let (tx, rx) = std::sync::mpsc::channel();
    let mut app = test_app();
    app.model_selector.open();

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("handle esc");

    assert!(!app.model_selector.is_open(), "esc closes the selector");
    assert!(
        rx.try_recv().is_err(),
        "no message should be sent on dismiss"
    );
}

#[test]
fn model_selector_filter_narrows_and_submits_highlighted() {
    let (tx, rx) = std::sync::mpsc::channel();
    let mut app = test_app();
    app.model_selector.open();
    app.model_selector.apply_models(
        vec![
            "gpt-4o".to_string(),
            "gpt-4o-mini".to_string(),
            "claude-3".to_string(),
        ],
        Some("claude-3".to_string()),
    );

    // Type "mini" into the filter.
    for c in "mini".chars() {
        handle_terminal_event(
            Event::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)),
            &mut app,
            &tx,
        )
        .expect("handle filter char");
    }

    assert_eq!(app.model_selector.filtered(), vec!["gpt-4o-mini"]);
    assert_eq!(
        app.model_selector.highlighted().as_deref(),
        Some("gpt-4o-mini")
    );

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("handle enter");

    assert!(!app.model_selector.is_open());
    let msg = rx.recv().expect("sent message");
    assert_eq!(
        msg,
        ClientMessage::SetModel {
            model: "gpt-4o-mini".to_string()
        }
    );
}

#[test]
fn model_selector_down_moves_highlight() {
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = test_app();
    app.model_selector.open();
    app.model_selector.apply_models(
        vec!["a".to_string(), "b".to_string(), "c".to_string()],
        None,
    );
    assert_eq!(app.model_selector.highlighted().as_deref(), Some("a"));

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("handle down");

    assert_eq!(app.model_selector.highlighted().as_deref(), Some("b"));
}

#[test]
fn model_selector_models_reply_falls_through_when_closed() {
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = test_app();

    // Selector closed: `/model` behavior — the reply prints into the status.
    handle_daemon_message(
        DaemonMessage::Models {
            models: vec!["gpt-4o".to_string()],
            selected_model: Some("gpt-4o".to_string()),
        },
        &mut app,
        &tx,
    )
    .expect("handle Models");

    assert!(
        app.status
            .as_deref()
            .is_some_and(|s| s.contains("supported models")),
        "closed selector keeps the chat-history behavior"
    );
}

#[test]
fn model_selector_failed_reply_shows_error_when_open() {
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = test_app();
    app.model_selector.open();

    handle_daemon_message(
        DaemonMessage::ModelsFailed {
            error: "no credential".to_string(),
        },
        &mut app,
        &tx,
    )
    .expect("handle ModelsFailed");

    assert!(!app.model_selector.loading);
    assert_eq!(app.model_selector.error.as_deref(), Some("no credential"));
    assert!(app.status.is_none(), "error stays scoped to the popup");
}

#[test]
fn model_selector_paste_goes_to_filter() {
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = test_app();
    app.model_selector.open();
    app.model_selector
        .apply_models(vec!["gpt-4o-mini".to_string()], None);

    handle_terminal_event(Event::Paste("mini".to_string()), &mut app, &tx).expect("handle paste");

    assert_eq!(app.model_selector.filter.text, "mini");
    assert!(
        app.input.text.is_empty(),
        "paste must not hit the main input"
    );
}
