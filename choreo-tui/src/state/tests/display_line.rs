//! Ingest normalization: the display-only message lines are made inert at the
//! point they are stored, so no reader — a widget, a status-bar measurement, or
//! a future non-ratatui consumer — can ever see a live escape.
//!
//! These tests deliberately go through the setters (`App::set_error` /
//! `App::set_status` / `App::set_quit_message`) rather than the private fields,
//! because the setters *are* the choke point under test.

use crate::tests::support::test_app;

/// One hostile fixture carrying every construct the normalizer has to
/// neutralize: a BEL, a tab, a right-to-left override, and a CSI sequence.
const HOSTILE: &str = "bad\u{7}\ttext \u{202e}reversed\u{1b}[31mred";

/// The properties a stored display line must have: no control character, no
/// bidi override, and no tab (the normalizer expands tabs to spaces).
fn assert_inert(stored: &str) {
    assert!(
        !stored.chars().any(char::is_control),
        "no control char may be stored: {stored:?}"
    );
    assert!(
        !stored.contains('\u{202e}'),
        "no bidi override may be stored: {stored:?}"
    );
    assert!(!stored.contains('\t'), "no tab may be stored: {stored:?}");
}

/// The hostile bytes are escaped rather than dropped, so the message stays
/// legible — that escaping is what makes the stored value display-safe.
fn assert_escaped_in_place(stored: &str) {
    assert!(
        stored.contains("bad") && stored.contains("text") && stored.contains("red"),
        "the readable text survives: {stored:?}"
    );
    assert!(
        stored.contains("\\u{7}") && stored.contains("\\u{202e}") && stored.contains("\\u{1b}[31m"),
        "hostile bytes are escaped in place: {stored:?}"
    );
}

#[test]
fn set_error_stores_inert_text() {
    let mut app = test_app();
    app.set_error(HOSTILE);
    let stored = app.error().expect("error line stored");
    assert_inert(stored);
    assert_escaped_in_place(stored);
}

#[test]
fn set_status_stores_inert_text() {
    let mut app = test_app();
    app.set_status(HOSTILE);
    let stored = app.status().expect("status line stored");
    assert_inert(stored);
    assert_escaped_in_place(stored);
}

#[test]
fn clean_text_round_trips_unchanged() {
    let mut app = test_app();
    let clean = "session saved — 3 turns";

    app.set_status(clean);
    assert_eq!(app.status(), Some(clean), "clean status is stored verbatim");

    app.set_error(clean);
    assert_eq!(app.error(), Some(clean), "clean error is stored verbatim");

    // Storing already-normalized text is a no-op (the normalizer borrows it),
    // so a re-assignment cannot accumulate escapes.
    let stored = app.status().unwrap_or_default().to_string();
    app.set_status(stored);
    assert_eq!(app.status(), Some(clean));
}

#[test]
fn empty_text_is_a_present_blank_line() {
    let mut app = test_app();
    // An empty message is still a message: `Some("")` draws a blank row and
    // reserves it in the layout, which is a different state from "no message".
    app.set_status(String::new());
    assert_eq!(app.status(), Some(""));

    app.set_error(String::new());
    assert_eq!(app.error(), Some(""));

    app.clear_status();
    app.clear_error();
    assert_eq!(app.status(), None);
    assert_eq!(app.error(), None);
}

/// A hostile quit message: the constructs that do real damage on a terminal
/// that is back in its normal screen once the TUI has exited — a BEL, a
/// screen-clear CSI (`ESC[2J`), an OSC-52 clipboard write, a right-to-left
/// override, and a tab.
const HOSTILE_QUIT: &str = "bye\u{7}\u{1b}[2J\u{1b}]52;c;cGF3bmVk\u{7}\u{202e}dekooc\tmsg";

#[test]
fn quit_message_is_stored_inert() {
    // The quit message is the only text the TUI prints outside ratatui's frame
    // buffer (`run_app` writes it with `println!` on the restored terminal), so
    // ingest is the sole guard between a stored byte and a live terminal
    // sequence.  Asserting on the stored string — not on captured stdout — keeps
    // the test on the invariant itself: whatever the print site does, it cannot
    // emit an escape that was already neutralized here.
    let mut app = test_app();
    app.set_quit_message(HOSTILE_QUIT);

    let stored = app.quit_message().expect("quit message stored");
    assert_inert(stored);
    assert!(
        !stored.contains('\u{1b}'),
        "no live escape may reach the print site: {stored:?}"
    );
    // The hostile bytes are escaped in place rather than dropped, so the reason
    // stays legible instead of silently losing its tail.
    assert!(
        stored.contains("\\u{1b}[2J") && stored.contains("\\u{1b}]52;c;cGF3bmVk"),
        "CSI and OSC are escaped in place: {stored:?}"
    );
    assert!(
        stored.starts_with("bye") && stored.ends_with("msg"),
        "the readable text survives: {stored:?}"
    );
}

#[test]
fn quit_message_is_absent_on_a_normal_quit() {
    // Nothing set means nothing printed: the exit path only writes when a
    // reason was stored.
    assert!(test_app().quit_message().is_none());
}

#[test]
fn quit_message_set_if_absent_keeps_the_specific_reason() {
    // A reason already stored is strictly more specific than the generic
    // fallback a later layer would install, so the first writer wins — and the
    // fallback is normalized on the path that does store it.
    let mut app = test_app();
    app.set_quit_message("the server is shutting down");
    app.set_quit_message_if_absent(HOSTILE_QUIT);
    assert_eq!(app.quit_message(), Some("the server is shutting down"));

    let mut app = test_app();
    app.set_quit_message_if_absent(HOSTILE_QUIT);
    let stored = app.quit_message().expect("fallback stored");
    assert_inert(stored);
}
