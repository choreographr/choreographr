//! The read-only Terminal page's input path: `Alt+T` / `/terminal` open it,
//! `Esc` and `Alt+T` close it, `Up`/`Down`/`PgUp`/`PgDn` scroll its body, and
//! while it is open no keystroke (or mouse event) reaches the Chat page
//! underneath.

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
fn alt_t_opens_the_terminal_page() {
    let mut app = test_app();
    let (tx, _rx) = crossbeam_channel::unbounded();
    assert!(!app.show_terminal_page, "the page starts closed");

    key(&mut app, &tx, alt_char('t'));
    assert!(
        app.show_terminal_page,
        "Alt+T opens the Terminal page from the Chat page"
    );
    assert!(
        app.input.text.is_empty(),
        "a command shortcut must not type into the composer"
    );
}

#[test]
fn bare_terminal_command_opens_the_terminal_page() {
    let mut app = test_app();
    let (tx, _rx) = crossbeam_channel::unbounded();
    app.input.text = "/terminal".to_string();
    app.input.cursor = app.input.text.len();

    key(
        &mut app,
        &tx,
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
    );
    assert!(
        app.show_terminal_page,
        "the palette's `/terminal` spelling opens the same page"
    );
    assert!(app.input.is_empty(), "the command line is discarded");
}

#[test]
fn esc_closes_the_terminal_page() {
    let mut app = test_app();
    let (tx, rx) = crossbeam_channel::unbounded();
    key(&mut app, &tx, alt_char('t'));
    assert!(app.show_terminal_page);

    key(
        &mut app,
        &tx,
        Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
    );
    assert!(!app.show_terminal_page, "Esc closes the page");
    assert!(
        rx.try_recv().is_err(),
        "Esc on the page must not cancel the running request the way the Chat page's Esc does"
    );
}

#[test]
fn alt_t_toggles_the_page_closed() {
    let mut app = test_app();
    let (tx, _rx) = crossbeam_channel::unbounded();
    key(&mut app, &tx, alt_char('t'));
    assert!(app.show_terminal_page);

    key(&mut app, &tx, alt_char('t'));
    assert!(!app.show_terminal_page, "the same chord toggles it closed");
}

#[test]
fn keystrokes_never_reach_the_composer_while_the_page_is_open() {
    let mut app = test_app();
    let (tx, rx) = crossbeam_channel::unbounded();
    key(&mut app, &tx, alt_char('t'));

    // A printable key edits nothing, Enter submits nothing.
    key(&mut app, &tx, plain_char('x'));
    key(
        &mut app,
        &tx,
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
    );
    assert!(
        app.input.text.is_empty(),
        "the page swallows text input, got {:?}",
        app.input.text
    );
    assert!(
        rx.try_recv().is_err(),
        "no prompt may be sent from the page"
    );
    assert!(app.show_terminal_page, "the page stays open");
}

#[test]
fn the_mouse_is_inert_over_the_page() {
    let mut app = test_app();
    let (tx, _rx) = crossbeam_channel::unbounded();
    key(&mut app, &tx, alt_char('t'));

    // A click anywhere — including over the history pane, where it would
    // normally arm a text selection — does nothing while the page is up.
    let click = Event::Mouse(crossterm::event::MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: 1,
        row: 1,
        modifiers: KeyModifiers::NONE,
    });
    handle_terminal_event(click, &mut app, &tx).expect("handle click");
    assert!(app.text_selection.is_none(), "no row is clickable");
    assert!(app.show_terminal_page, "a click never closes the page");
}

#[test]
fn arrows_and_page_keys_scroll_the_page_body() {
    // The page is taller than a short terminal, so the overlay guard scrolls it.
    // The offset is stored raw and clamped to the content at draw time.
    let mut app = test_app();
    let (tx, _rx) = crossbeam_channel::unbounded();
    key(&mut app, &tx, alt_char('t'));
    assert_eq!(app.terminal_page_scroll, 0, "opens at the top");

    let arrow = |code| Event::Key(KeyEvent::new(code, KeyModifiers::NONE));
    key(&mut app, &tx, arrow(KeyCode::Down));
    assert_eq!(app.terminal_page_scroll, 1, "Down scrolls one row");
    key(&mut app, &tx, arrow(KeyCode::PageDown));
    assert_eq!(app.terminal_page_scroll, 11, "PageDown scrolls a page");
    key(&mut app, &tx, arrow(KeyCode::Up));
    assert_eq!(app.terminal_page_scroll, 10, "Up scrolls back one row");
    key(&mut app, &tx, arrow(KeyCode::PageUp));
    assert_eq!(app.terminal_page_scroll, 0, "PageUp scrolls back a page");
    // Already at the top: PageUp/Up saturate rather than wrap.
    key(&mut app, &tx, arrow(KeyCode::Up));
    key(&mut app, &tx, arrow(KeyCode::PageUp));
    assert_eq!(app.terminal_page_scroll, 0, "scrolling up saturates at 0");
    assert!(app.show_terminal_page, "scrolling never closes the page");
}

#[test]
fn reopening_the_page_resets_the_scroll() {
    let mut app = test_app();
    let (tx, _rx) = crossbeam_channel::unbounded();
    key(&mut app, &tx, alt_char('t'));
    key(
        &mut app,
        &tx,
        Event::Key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE)),
    );
    assert!(app.terminal_page_scroll > 0);

    key(
        &mut app,
        &tx,
        Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
    );
    assert!(!app.show_terminal_page, "Esc closes the page");

    key(&mut app, &tx, alt_char('t'));
    assert!(app.show_terminal_page, "Alt+T reopens it");
    assert_eq!(app.terminal_page_scroll, 0, "reopening resets the scroll");
}
