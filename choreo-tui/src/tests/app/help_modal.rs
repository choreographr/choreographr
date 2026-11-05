//! The read-only Help modal's input path: `Alt+H` / `/help` open it, `Esc` and
//! `Alt+H` close it, and while it is open no keystroke (or mouse event) reaches
//! the Chat page underneath.
//!
//! Mirrors `terminal_page` — the two overlays share the same open/close guard in
//! `handle_terminal_event`, so their tests read the same way.

use crate::connection::handle_terminal_event;
use crate::tests::support::test_app;
use choreo_proto::ClientMessage;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEventKind};

/// Drive one key event through the full terminal-event pipeline.
fn key(app: &mut crate::state::App, tx: &crossbeam_channel::Sender<ClientMessage>, event: Event) {
    handle_terminal_event(event, app, tx).expect("handle key event");
}

fn alt_char(c: char) -> Event {
    Event::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::ALT))
}

fn plain_char(c: char) -> Event {
    Event::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
}

#[test]
fn alt_h_opens_the_help_modal() {
    let mut app = test_app();
    let (tx, _rx) = crossbeam_channel::unbounded();
    assert!(!app.show_help_modal, "the modal starts closed");

    key(&mut app, &tx, alt_char('h'));
    assert!(
        app.show_help_modal,
        "Alt+H opens the Help modal from the Chat page"
    );
    assert!(
        app.input.text.is_empty(),
        "a command shortcut must not type into the composer"
    );
}

#[test]
fn bare_help_command_opens_the_help_modal() {
    let mut app = test_app();
    let (tx, _rx) = crossbeam_channel::unbounded();
    app.input.text = "/help".to_string();
    app.input.cursor = app.input.text.len();

    key(
        &mut app,
        &tx,
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
    );
    assert!(
        app.show_help_modal,
        "the palette's `/help` spelling opens the same modal"
    );
    assert!(app.input.is_empty(), "the command line is discarded");
}

#[test]
fn esc_closes_the_help_modal() {
    let mut app = test_app();
    let (tx, rx) = crossbeam_channel::unbounded();
    key(&mut app, &tx, alt_char('h'));
    assert!(app.show_help_modal);

    key(
        &mut app,
        &tx,
        Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
    );
    assert!(!app.show_help_modal, "Esc closes the modal");
    assert!(
        rx.try_recv().is_err(),
        "Esc on the modal must not cancel the running request the way the Chat page's Esc does"
    );
}

#[test]
fn alt_h_toggles_the_modal_closed() {
    let mut app = test_app();
    let (tx, _rx) = crossbeam_channel::unbounded();
    key(&mut app, &tx, alt_char('h'));
    assert!(app.show_help_modal);

    key(&mut app, &tx, alt_char('h'));
    assert!(!app.show_help_modal, "the same chord toggles it closed");
}

#[test]
fn keystrokes_never_reach_the_composer_while_the_modal_is_open() {
    let mut app = test_app();
    let (tx, rx) = crossbeam_channel::unbounded();
    key(&mut app, &tx, alt_char('h'));

    // A printable key edits nothing, Enter submits nothing.
    key(&mut app, &tx, plain_char('x'));
    key(
        &mut app,
        &tx,
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
    );
    assert!(
        app.input.text.is_empty(),
        "the modal swallows text input, got {:?}",
        app.input.text
    );
    assert!(
        rx.try_recv().is_err(),
        "no prompt may be sent from the modal"
    );
    assert!(app.show_help_modal, "the modal stays open");
}

#[test]
fn the_mouse_is_inert_over_the_modal() {
    let mut app = test_app();
    let (tx, _rx) = crossbeam_channel::unbounded();
    key(&mut app, &tx, alt_char('h'));

    // A click anywhere — including over the history pane, where it would
    // normally arm a text selection — does nothing while the modal is up.
    let click = Event::Mouse(crossterm::event::MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: 1,
        row: 1,
        modifiers: KeyModifiers::NONE,
    });
    handle_terminal_event(click, &mut app, &tx).expect("handle click");
    assert!(app.text_selection.is_none(), "no row is clickable");
    assert!(app.show_help_modal, "a click never closes the modal");
}
