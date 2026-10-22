//! Ingest normalization: the display-only message lines are made inert at the
//! point they are stored, so no reader — a widget, a status-bar measurement, or
//! a future non-ratatui consumer — can ever see a live escape.
//!
//! These tests deliberately go through the setters (`App::set_error` /
//! `App::set_status`) rather than the private fields, because the setters *are*
//! the choke point under test.

use crate::test_util::test_app;

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
