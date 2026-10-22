use crate::connection::handle_terminal_event;
use crate::tests::support::test_app;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

// ── IME text input (Vietnamese, CJK, …) ──
//
// crossterm 0.29 has no KeyEvent.text field, so kitty-protocol "text events"
// (CSI 0;;<codepoints>u — how IME-composed text is delivered when
// REPORT_ALL_KEYS_AS_ESCAPE_CODES is enabled) are parsed as Char('\0') with
// the composed text silently dropped.  We therefore never request
// REPORT_ALL_KEYS (see KITTY_KEYBOARD_FLAGS in connection.rs); these tests
// pin the defence-in-depth behaviours that keep NUL garbage out of the input.

#[test]
fn ime_text_event_must_not_insert_nul_into_chat_input() {
    let (tx, _rx) = crossbeam_channel::unbounded();
    let mut app = test_app();
    app.input.text = "xin chào".to_string();
    app.input.cursor = app.input.text.len();

    // This is exactly the Event crossterm 0.29 yields for the IME text event
    // `CSI 0;;7871u` ('ế') — key code 0, associated text dropped.
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('\0'), KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("handle ime text event");

    assert_eq!(
        app.input.text, "xin chào",
        "a NUL from a mangled IME text event must never enter the buffer"
    );
}
