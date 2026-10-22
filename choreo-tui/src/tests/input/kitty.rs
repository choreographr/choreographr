use crate::connection::handle_terminal_event;
use crate::tests::support::test_app;
use choreo_proto::ClientMessageType;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

// ── Kitty keyboard protocol shift normalisation (end-to-end) ──
//
// With REPORT_ALL_KEYS_AS_ESCAPE_CODES enabled, kitty-protocol terminals
// report shifted text keys as unshifted codepoints + SHIFT modifier.  The
// normalisation in handle_terminal_event must restore legacy-equivalent
// behaviour everywhere.

#[test]
fn alt_shift_m_opens_selector_like_alt_m() {
    let (tx, rx) = crossbeam_channel::unbounded();
    let mut app = test_app();

    // Alt+Shift+M arrives as Char('M') + ALT under the kitty protocol (the
    // normaliser maps the shifted glyph and keeps ALT); the shortcut table
    // folds the letter to lowercase, so it opens the selector like Alt+M.
    handle_terminal_event(
        Event::Key(KeyEvent::new(
            KeyCode::Char('m'),
            KeyModifiers::ALT | KeyModifiers::SHIFT,
        )),
        &mut app,
        &tx,
    )
    .expect("handle alt+shift+m");

    assert!(
        app.model_selector.is_open(),
        "alt+shift+m opens the selector"
    );
    let msg = rx.recv().expect("sent message");
    assert_eq!(msg.inner, ClientMessageType::ListModels);
}

#[test]
fn shift_letter_inserts_uppercase_into_chat_input() {
    let (tx, _rx) = crossbeam_channel::unbounded();
    let mut app = test_app();

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::SHIFT)),
        &mut app,
        &tx,
    )
    .expect("handle shift+h");

    assert_eq!(
        app.input.text, "H",
        "shift+letter must insert the uppercase glyph"
    );
}

#[test]
fn shift_digit_inserts_symbol_into_chat_input() {
    let (tx, _rx) = crossbeam_channel::unbounded();
    let mut app = test_app();

    // Shift+1 (kitty: Char('1') + SHIFT) must produce '!' like a legacy
    // terminal would.
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('1'), KeyModifiers::SHIFT)),
        &mut app,
        &tx,
    )
    .expect("handle shift+1");

    assert_eq!(
        app.input.text, "!",
        "shift+1 must insert the shifted symbol"
    );
}

#[test]
fn shift_enter_still_inserts_newline() {
    let (tx, _rx) = crossbeam_channel::unbounded();
    let mut app = test_app();

    // Shift+Enter is not a Char key, so normalisation must leave it alone and
    // the chat page must keep inserting a literal newline.
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT)),
        &mut app,
        &tx,
    )
    .expect("handle shift+enter");

    assert_eq!(app.input.text, "\n", "shift+enter inserts a newline");
}

#[test]
fn model_selector_filter_receives_shifted_chars() {
    let (tx, _rx) = crossbeam_channel::unbounded();
    let mut app = test_app();
    app.model_selector.open();
    app.model_selector
        .apply_models(vec!["GPT-4O".to_string()], None, false);

    // While the popup is open, a shifted letter must go to the filter box
    // (uppercased, matching what a legacy terminal would have sent).
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::SHIFT)),
        &mut app,
        &tx,
    )
    .expect("handle shift+g");

    assert_eq!(app.model_selector.filter.text, "G");
}
