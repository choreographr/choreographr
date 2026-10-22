use crate::connection::handle_terminal_event;
use crate::tests::support::test_app;
use choreo_proto::ClientMessageType;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

#[test]
fn terminal_event_appends_characters() {
    let mut app = test_app();
    let (tx, rx) = crossbeam_channel::unbounded();

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("handle key");
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("handle key");

    assert_eq!(app.input.text, "hi");
    assert_eq!(app.input.cursor, 2);
    assert!(rx.try_recv().is_err());
}

#[test]
fn terminal_event_submits_run_input() {
    let mut app = test_app();
    app.input.text = "hello".to_string();
    let (tx, rx) = crossbeam_channel::unbounded();

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("handle enter");

    assert!(app.input.is_empty());
    assert_eq!(app.input.cursor, 0);
    let message = rx.recv().expect("sent message");
    assert_eq!(
        message.inner,
        ClientMessageType::RunInput {
            input: b"hello".to_vec(),
        }
    );
}

#[test]
fn submitting_prompt_while_locked_is_rejected_with_feedback() {
    // Submitting a prompt (RunInput) to a locked daemon must produce clear
    // feedback instead of silence — and must NOT send the RunInput (the
    // daemon cannot run inference with no credentials in memory, so a sent
    // message would only come back as a transient "no credential stored"
    // failure that a keypress clears).
    let mut app = test_app();
    app.attached_session_id = Some(42);
    app.keystore_locked = true;
    app.input.text = "hello".to_string();
    let (tx, rx) = crossbeam_channel::unbounded();

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("handle enter");

    let status = app.status().expect("lock feedback status");
    assert!(
        status.contains("locked"),
        "must tell the user the daemon is locked, got: {status}"
    );
    assert!(
        rx.try_recv().is_err(),
        "no RunInput may be sent to a locked daemon"
    );
    assert!(app.keystore_locked, "the lock flag stays latched");
}
