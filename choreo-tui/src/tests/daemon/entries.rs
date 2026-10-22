use crate::connection::handle_terminal_event;
use crate::state::*;
use crate::tests::support::{add_user_text, test_app};
use choreo_proto::{ClientMessageType, SessionStatus};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

// ── Entry handling for Continue / Stop / Undo / Redo ──────────────────

#[test]
fn enter_continue_when_attached_sends_continue_generation() {
    let mut app = test_app();
    app.attached_session_id = Some(1);
    app.input.text = "/continue ".to_string();
    let (tx, rx) = crossbeam_channel::unbounded();

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("handle enter");

    assert_eq!(app.status(), Some("> continue"));
    let msg = rx.recv().expect("should send ContinueGeneration");
    // The daemon assigns the stream id (reported on `Started`); the client
    // sends the bare request.
    assert_eq!(msg.inner, ClientMessageType::ContinueGeneration);
}

#[test]
fn enter_continue_when_not_attached_shows_error() {
    let mut app = test_app();
    app.attached_session_id = None;
    app.input.text = "/continue ".to_string();
    let (tx, _rx) = crossbeam_channel::unbounded();

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("handle enter");

    assert_eq!(app.status(), Some("no session attached"));
}

#[test]
fn enter_continue_scrolls_to_bottom() {
    let mut app = test_app();
    app.attached_session_id = Some(1);
    app.history_viewport = HistoryViewport {
        width: 80,
        height: 5,
    };
    // Add enough content to be scrollable.
    add_user_text(&mut app, "a");
    add_user_text(&mut app, "b");
    add_user_text(&mut app, "c");
    // Scroll up so we're not at the bottom.
    app.scroll_up(2);
    let scrolled = app.effective_scroll();
    assert!(scrolled > 0, "should be scrolled up");
    app.input.text = "/continue ".to_string();
    let (tx, _rx) = crossbeam_channel::unbounded();

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("handle enter");

    assert_eq!(app.effective_scroll(), 0, "should scroll to bottom");
}

#[test]
fn enter_stop_when_attached_sends_cancel_all() {
    let mut app = test_app();
    app.attached_session_id = Some(1);
    app.input.text = "/stop ".to_string();
    let (tx, rx) = crossbeam_channel::unbounded();

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("handle enter");

    assert_eq!(app.status(), Some("> stop"));
    let msg = rx.recv().expect("should send Cancel");
    assert_eq!(msg.inner, ClientMessageType::Cancel { stream_id: 0 });
}

#[test]
fn enter_stop_when_not_attached_shows_error() {
    let mut app = test_app();
    app.attached_session_id = None;
    app.input.text = "/stop ".to_string();
    let (tx, _rx) = crossbeam_channel::unbounded();

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("handle enter");

    assert_eq!(app.status(), Some("no session attached"));
}

#[test]
fn enter_undo_sends_undo() {
    let mut app = test_app();
    app.input.text = "/undo ".to_string();
    let (tx, rx) = crossbeam_channel::unbounded();

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("handle enter");

    assert_eq!(app.status(), Some("> undo"));
    let msg = rx.recv().expect("should send Undo");
    assert_eq!(msg.inner, ClientMessageType::Undo);
}

#[test]
fn enter_redo_sends_redo() {
    let mut app = test_app();
    app.input.text = "/redo ".to_string();
    let (tx, rx) = crossbeam_channel::unbounded();

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("handle enter");

    assert_eq!(app.status(), Some("> redo"));
    let msg = rx.recv().expect("should send Redo");
    assert_eq!(msg.inner, ClientMessageType::Redo);
}

#[test]
fn enter_undo_while_busy_is_rejected_client_side() {
    // Undo mutates the turn history, so it is only sent when the attached
    // session is idle. A busy session must be refused locally (no round-trip)
    // with a status line, and must NOT echo `> undo` or send anything.
    let mut app = test_app();
    app.attached_status = Some(SessionStatus::Responding);
    app.input.text = "/undo ".to_string();
    let (tx, rx) = crossbeam_channel::unbounded();

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("handle enter");

    assert_eq!(
        app.status(),
        Some("Session is not idle, please wait before undoing or redoing.")
    );
    assert!(
        rx.try_recv().is_err(),
        "no Undo may be sent while the session is busy"
    );
}

#[test]
fn enter_redo_while_busy_is_rejected_client_side() {
    let mut app = test_app();
    app.attached_status = Some(SessionStatus::ToolCall("shell".into()));
    app.input.text = "/redo ".to_string();
    let (tx, rx) = crossbeam_channel::unbounded();

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("handle enter");

    assert_eq!(
        app.status(),
        Some("Session is not idle, please wait before undoing or redoing.")
    );
    assert!(
        rx.try_recv().is_err(),
        "no Redo may be sent while the session is busy"
    );
}

#[test]
fn alt_up_undo_while_busy_is_rejected_client_side() {
    // The Alt+Up keybinding reaches the same `Command::Undo` arm as `/undo`,
    // so it must share the idle guard.
    let mut app = test_app();
    app.attached_status = Some(SessionStatus::Waiting);
    let (tx, rx) = crossbeam_channel::unbounded();

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::ALT)),
        &mut app,
        &tx,
    )
    .expect("handle alt+up");

    assert_eq!(
        app.status(),
        Some("Session is not idle, please wait before undoing or redoing.")
    );
    assert!(
        rx.try_recv().is_err(),
        "no Undo may be sent while the session is busy"
    );
}

#[test]
fn enter_stop_does_not_scroll() {
    let mut app = test_app();
    app.attached_session_id = Some(1);
    app.history_viewport = HistoryViewport {
        width: 80,
        height: 5,
    };
    add_user_text(&mut app, "a");
    add_user_text(&mut app, "b");
    app.scroll_up(1);
    let scrolled = app.effective_scroll();
    assert!(scrolled > 0, "should be scrolled up");
    app.input.text = "/stop ".to_string();
    let (tx, _rx) = crossbeam_channel::unbounded();

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("handle enter");

    // Stop should NOT scroll to bottom (unlike Continue).
    assert!(
        app.effective_scroll() > 0,
        "should preserve scroll position"
    );
}
