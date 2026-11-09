use crate::connection::handle_terminal_event;
use crate::tests::support::{add_reasoning_turn, test_app};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

// ── Kitty keyboard protocol shift normalisation (end-to-end) ──
//
// A kitty-protocol terminal reports shifted text keys as unshifted codepoints
// + SHIFT, and a legacy terminal sends the shifted glyph.  Both must reach the
// same command, and shift must stay a real part of the chord.

#[test]
fn alt_shift_m_is_its_own_chord_not_alt_m() {
    let (tx, rx) = crossbeam_channel::unbounded();
    let mut app = test_app();

    // Alt+Shift+M arrives as Char('m') + ALT + SHIFT under the kitty protocol;
    // shift is part of the chord, so this is NOT the model-selector binding.
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
        !app.model_selector.is_open(),
        "alt+shift+m must not run the alt+m binding"
    );
    assert!(
        rx.try_recv().is_err(),
        "an unbound shifted chord sends nothing"
    );
}

#[test]
fn alt_shift_r_toggles_all_reasoning_like_its_legacy_encoding() {
    // The same press reaches the command from both encodings: a kitty terminal
    // sends SHIFT in the modifier field, a legacy one sends the shifted glyph.
    // A reasoning-only turn derives "expanded", so the press collapses it.
    for key in [
        KeyEvent::new(KeyCode::Char('r'), KeyModifiers::ALT | KeyModifiers::SHIFT),
        KeyEvent::new(KeyCode::Char('R'), KeyModifiers::ALT),
    ] {
        let (tx, _rx) = crossbeam_channel::unbounded();
        let mut app = test_app();
        add_reasoning_turn(&mut app, "thinking...");
        assert!(app.display_for(0).effective_reasoning_expanded(0, true));

        handle_terminal_event(Event::Key(key), &mut app, &tx).expect("handle alt+shift+r");

        assert_eq!(
            app.display_for(0).reasoning_mode,
            crate::state::ReasoningViewMode::Collapsed,
            "alt+shift+r collapses every reasoning section"
        );
        assert!(
            !app.display_for(0).effective_reasoning_expanded(0, true),
            "the reasoning-only turn is now collapsed"
        );
    }
}

#[test]
fn alt_r_still_cycles_reasoning_effort_not_the_visibility_mode() {
    let (tx, _rx) = crossbeam_channel::unbounded();
    let mut app = test_app();
    app.display_for(0).reasoning_mode = crate::state::ReasoningViewMode::Collapsed;

    // Alt+R alone is the effort cycle: it must not touch the bulk mode.
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::ALT)),
        &mut app,
        &tx,
    )
    .expect("handle alt+r");

    assert_eq!(
        app.display_for(0).reasoning_mode,
        crate::state::ReasoningViewMode::Collapsed,
        "alt+r leaves the bulk reasoning mode alone"
    );
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
