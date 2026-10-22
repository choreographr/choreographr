use crate::connection::handle_terminal_event;
use crate::state::*;
use crate::tests::support::test_app;
use choreo_proto::{ClientMessage, ClientMessageType, SessionStatus};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

use super::press_enter;

// ── Prompts are only accepted while the session is idle ──
//
// A plain prompt (RunInput) begins a new inference turn, which the daemon can
// only start when the session is idle.  Submitting one while the session is
// busy must be rejected client-side with a clear status message — and, just as
// importantly, must NOT clear the input buffer or forget the per-session draft
// (otherwise the user's text would be silently lost).  Slash-commands stay
// available so the user can still e.g. `/cancel`.

#[test]
fn submitting_prompt_while_busy_is_rejected_and_preserves_input() {
    let mut app = test_app();
    app.attached_session_id = Some(42);
    // The attached session is mid-inference.
    app.attached_status = Some(SessionStatus::Responding);
    app.input.text = "hello".to_string();
    app.input.cursor = 5;
    let (tx, rx) = crossbeam_channel::unbounded();

    press_enter(&mut app, &tx);

    assert_eq!(
        app.status(),
        Some("Session is not idle, please wait before prompting.")
    );
    assert!(
        rx.try_recv().is_err(),
        "no RunInput may be sent while the session is busy"
    );
    // The user's text must survive the rejection — a rejected prompt is still
    // an unsent prompt, so it stays in the input bar ready to resubmit.
    assert_eq!(app.input.text, "hello", "input buffer must be preserved");
    assert_eq!(app.input.cursor, 5, "cursor position must be preserved");
}

#[test]
fn submitting_prompt_while_sleeping_is_rejected() {
    // Only `Inactive` is idle: a `Sleeping` session cannot accept a prompt
    // either, so it is rejected the same way.
    let mut app = test_app();
    app.attached_session_id = Some(42);
    app.attached_status = Some(SessionStatus::Sleeping);
    app.input.text = "hello".to_string();
    let (tx, rx) = crossbeam_channel::unbounded();

    press_enter(&mut app, &tx);

    assert_eq!(
        app.status(),
        Some("Session is not idle, please wait before prompting.")
    );
    assert!(rx.try_recv().is_err());
}

#[test]
fn submitting_prompt_while_idle_is_sent() {
    let mut app = test_app();
    app.attached_session_id = Some(42);
    app.attached_status = Some(SessionStatus::Inactive);
    app.input.text = "hello".to_string();
    let (tx, rx) = crossbeam_channel::unbounded();

    press_enter(&mut app, &tx);

    assert!(app.input.is_empty());
    assert_eq!(
        rx.recv().expect("sent message").inner,
        ClientMessageType::RunInput {
            input: b"hello".to_vec(),
        }
    );
}

#[test]
fn submitting_prompt_with_unknown_status_fails_open() {
    // No status known yet (fresh client / pre-report window): the daemon stays
    // the authority, so the prompt is sent rather than blocked.
    let mut app = test_app();
    app.attached_session_id = Some(42);
    app.attached_status = None;
    app.input.text = "hello".to_string();
    let (tx, rx) = crossbeam_channel::unbounded();

    press_enter(&mut app, &tx);

    assert!(
        rx.try_recv().is_ok(),
        "unknown status must fail open and send the prompt"
    );
}

#[test]
fn slash_command_is_accepted_while_busy() {
    // The guard only applies to plain prompts: a shell command such as
    // `/ping` must still reach the daemon while the session is busy.
    // (`/models` no longer exists — `/model` opens the selector and
    // `ListModels` is sent from there, so `/ping` is the minimal
    // daemon-reaching command for this check.)
    let mut app = test_app();
    app.attached_session_id = Some(42);
    app.attached_status = Some(SessionStatus::Responding);
    app.input.text = "/ping ".to_string();
    let (tx, rx) = crossbeam_channel::unbounded();

    press_enter(&mut app, &tx);

    assert_eq!(
        rx.recv().expect("sent message").inner,
        ClientMessageType::Ping,
        "slash-commands must bypass the idle guard"
    );
}

#[test]
fn empty_submission_while_busy_is_a_noop() {
    // An empty line is `Command::Empty`, not a prompt — it must not trip
    // the guard and must not set an error status.
    let mut app = test_app();
    app.attached_session_id = Some(42);
    app.attached_status = Some(SessionStatus::Responding);
    app.input.text = String::new();
    let (tx, rx) = crossbeam_channel::unbounded();

    press_enter(&mut app, &tx);

    assert_eq!(app.status(), None);
    assert!(rx.try_recv().is_err());
}

/// Drive an Alt+Enter keypress through the full terminal-event pipeline.
fn press_alt_enter(app: &mut App, tx: &crossbeam_channel::Sender<ClientMessage>) {
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT)),
        app,
        tx,
    )
    .expect("handle alt+enter");
}

#[test]
fn alt_enter_while_busy_is_rejected_by_the_same_guard() {
    // Alt+Enter sends a `ContinueGeneration`, which the daemon turns into a
    // fresh turn — so it must be refused while the session is not idle, exactly
    // like a plain prompt.  Without the shared guard the daemon would answer
    // with a transient "session already has an active request".
    let mut app = test_app();
    app.attached_session_id = Some(42);
    app.attached_status = Some(SessionStatus::ToolCall("shell".into()));
    let (tx, rx) = crossbeam_channel::unbounded();

    press_alt_enter(&mut app, &tx);

    assert_eq!(
        app.status(),
        Some("Session is not idle, please wait before prompting.")
    );
    assert!(
        rx.try_recv().is_err(),
        "no ContinueGeneration may be sent while the session is busy"
    );
}

#[test]
fn alt_enter_while_locked_is_rejected_by_the_same_guard() {
    // The locked half of the guard applies to ContinueGeneration too — it needs
    // the same credentials a prompt does.
    let mut app = test_app();
    app.attached_session_id = Some(42);
    app.attached_status = Some(SessionStatus::Inactive);
    app.keystore_locked = true;
    let (tx, rx) = crossbeam_channel::unbounded();

    press_alt_enter(&mut app, &tx);

    let status = app.status().expect("lock feedback status");
    assert!(status.contains("locked"), "got: {status}");
    assert!(rx.try_recv().is_err());
}

#[test]
fn alt_enter_while_idle_is_sent() {
    let mut app = test_app();
    app.attached_session_id = Some(42);
    app.attached_status = Some(SessionStatus::Inactive);
    let (tx, rx) = crossbeam_channel::unbounded();

    press_alt_enter(&mut app, &tx);

    assert!(matches!(
        rx.recv().expect("sent message").inner,
        ClientMessageType::ContinueGeneration
    ));
    assert!(app.status().is_none(), "no status message on success");
}

#[test]
fn continue_command_while_busy_is_rejected_by_the_same_guard() {
    // `/continue` is the shell-command spelling of Alt+Enter, so it must trip
    // the same new-turn guard while the session is not idle.
    let mut app = test_app();
    app.attached_session_id = Some(42);
    app.attached_status = Some(SessionStatus::Responding);
    app.input.text = "/continue ".to_string();
    let (tx, rx) = crossbeam_channel::unbounded();

    press_enter(&mut app, &tx);

    assert_eq!(
        app.status(),
        Some("Session is not idle, please wait before prompting.")
    );
    assert!(
        rx.try_recv().is_err(),
        "no ContinueGeneration may be sent while the session is busy"
    );
}

#[test]
fn continue_command_while_locked_is_rejected_by_the_same_guard() {
    let mut app = test_app();
    app.attached_session_id = Some(42);
    app.attached_status = Some(SessionStatus::Inactive);
    app.keystore_locked = true;
    app.input.text = "/continue ".to_string();
    let (tx, rx) = crossbeam_channel::unbounded();

    press_enter(&mut app, &tx);

    let status = app.status().expect("lock feedback status");
    assert!(status.contains("locked"), "got: {status}");
    assert!(rx.try_recv().is_err());
}

#[test]
fn terminal_event_esc_noop_on_chat() {
    let (tx, _rx) = crossbeam_channel::unbounded();

    let mut app = test_app();
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("handle esc");

    assert_eq!(app.page, Page::Chat);
    assert!(!app.should_quit);
}

#[test]
fn terminal_event_ctrl_c_noop_on_chat() {
    let (tx, _rx) = crossbeam_channel::unbounded();
    let mut app = test_app();

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        &mut app,
        &tx,
    )
    .expect("handle ctrl+c");

    assert_eq!(app.page, Page::Chat);
    assert!(!app.should_quit);
    assert!(
        app.input.text.is_empty(),
        "Ctrl+C should not insert literal 'c'"
    );
    assert_eq!(app.input.cursor, 0, "cursor should remain at 0");
}

#[test]
fn global_alt_q_quits_from_chat() {
    let (tx, _rx) = crossbeam_channel::unbounded();
    let mut app = test_app();

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::ALT)),
        &mut app,
        &tx,
    )
    .expect("handle alt+q");

    assert!(app.should_quit);
}

#[test]
fn global_alt_q_quits_from_session_manager() {
    let (tx, _rx) = crossbeam_channel::unbounded();
    let mut app = test_app();
    app.page = Page::SessionManager;

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::ALT)),
        &mut app,
        &tx,
    )
    .expect("handle alt+q from session manager");

    assert!(app.should_quit);
}

#[test]
fn global_alt_q_quits_from_ai_providers() {
    let (tx, _rx) = crossbeam_channel::unbounded();
    let mut app = test_app();
    app.page = Page::AIProviders;

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::ALT)),
        &mut app,
        &tx,
    )
    .expect("handle alt+q from ai providers");

    assert!(app.should_quit);
}

#[test]
fn ctrl_p_does_not_insert_char_on_chat() {
    let (tx, _rx) = crossbeam_channel::unbounded();
    let mut app = test_app();
    app.input.text = "hello".to_string();
    app.input.cursor = 5;

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL)),
        &mut app,
        &tx,
    )
    .expect("handle ctrl+p");

    assert_eq!(app.input.text, "hello", "Ctrl+P must not insert 'p'");
    // Ctrl+P is readline previous-history: with a non-empty draft and empty
    // history it falls back to the line-start move (as Up does on the first
    // visual line).
    assert_eq!(app.input.cursor, 0);
    assert!(!app.should_quit);
}

#[test]
fn alt_x_does_not_insert_char_on_chat() {
    let (tx, _rx) = crossbeam_channel::unbounded();
    let mut app = test_app();
    app.input.text = "hello".to_string();
    app.input.cursor = 5;

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::ALT)),
        &mut app,
        &tx,
    )
    .expect("handle alt+x");

    assert_eq!(app.input.text, "hello", "Alt+X should not insert 'x'");
    assert_eq!(app.input.cursor, 5);
    assert!(!app.should_quit);
}

#[test]
fn chat_alt_h_toggles_help() {
    let (tx, _rx) = crossbeam_channel::unbounded();
    let mut app = test_app();
    app.show_help_overlay = false;

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::ALT)),
        &mut app,
        &tx,
    )
    .expect("handle alt+h");

    assert!(app.show_help_overlay, "first press should enable help");
}

#[test]
fn chat_alt_h_double_toggle_returns_to_off() {
    let (tx, _rx) = crossbeam_channel::unbounded();
    let mut app = test_app();
    app.show_help_overlay = false;

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::ALT)),
        &mut app,
        &tx,
    )
    .expect("handle alt+h (first)");
    assert!(app.show_help_overlay, "first press should enable help");

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::ALT)),
        &mut app,
        &tx,
    )
    .expect("handle alt+h (second)");
    assert!(!app.show_help_overlay, "second press should disable help");
}

#[test]
fn chat_alt_a_enters_ai_providers() {
    let (tx, rx) = crossbeam_channel::unbounded();
    let mut app = test_app();

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::ALT)),
        &mut app,
        &tx,
    )
    .expect("handle alt+a");

    assert_eq!(app.page, Page::AIProviders);
    let msg = rx.recv().expect("sent message");
    assert_eq!(msg.inner, ClientMessageType::ListAccounts);
}

#[test]
fn chat_alt_up_sends_undo() {
    let (tx, rx) = crossbeam_channel::unbounded();
    let mut app = test_app();

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::ALT)),
        &mut app,
        &tx,
    )
    .expect("handle alt+up");

    let msg = rx.recv().expect("sent message");
    assert_eq!(msg.inner, ClientMessageType::Undo);
}

#[test]
fn chat_alt_down_sends_redo() {
    let (tx, rx) = crossbeam_channel::unbounded();
    let mut app = test_app();

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::ALT)),
        &mut app,
        &tx,
    )
    .expect("handle alt+down");

    let msg = rx.recv().expect("sent message");
    assert_eq!(msg.inner, ClientMessageType::Redo);
}

#[test]
fn chat_esc_stops_active_session() {
    let (tx, rx) = crossbeam_channel::unbounded();
    let mut app = test_app();
    app.attached_session_id = Some(42);

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("handle esc");

    let msg = rx.recv().expect("sent message");
    assert_eq!(msg.inner, ClientMessageType::Cancel { stream_id: 0 });
    assert!(app.status().is_none(), "no status message on success");
}

#[test]
fn chat_esc_no_session_shows_status() {
    let (tx, _rx) = crossbeam_channel::unbounded();
    let mut app = test_app();
    app.attached_session_id = None;

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("handle esc");

    assert_eq!(app.status(), Some("no session attached"));
}

#[test]
fn chat_alt_enter_continues_generation() {
    let (tx, rx) = crossbeam_channel::unbounded();
    let mut app = test_app();
    app.attached_session_id = Some(42);

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT)),
        &mut app,
        &tx,
    )
    .expect("handle alt+enter");

    // The daemon assigns the stream id and reports it on `Started`; the client
    // sends only the request (its reply id is allocated by `pending`).
    let msg = rx.recv().expect("sent message");
    assert_eq!(msg.inner, ClientMessageType::ContinueGeneration);
    assert!(app.status().is_none(), "no status message on success");
}

#[test]
fn chat_alt_enter_no_session_shows_status() {
    let (tx, _rx) = crossbeam_channel::unbounded();
    let mut app = test_app();
    app.attached_session_id = None;

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT)),
        &mut app,
        &tx,
    )
    .expect("handle alt+enter");

    assert_eq!(app.status(), Some("no session attached"));
}
