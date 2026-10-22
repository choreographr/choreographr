//! `status_error_height` wrapping and precedence.

use crate::test_util::test_app;

// ── status_error_height ──

#[test]
fn status_error_height_neither_set_returns_zero() {
    let app = test_app();
    assert_eq!(app.status_error_height(80), 0);
}

#[test]
fn status_error_height_short_error_returns_one() {
    let mut app = test_app();
    app.set_error("oops");
    assert_eq!(app.status_error_height(80), 1);
}

#[test]
fn status_error_height_short_status_returns_one() {
    let mut app = test_app();
    app.set_status("all good");
    assert_eq!(app.status_error_height(80), 1);
}

#[test]
fn status_error_height_error_preferred_over_status() {
    let mut app = test_app();
    app.set_error("error");
    app.set_status("status");
    // Should use error text, not status text
    assert_eq!(app.status_error_height(80), 1);
}

#[test]
fn status_error_height_wrapping() {
    let mut app = test_app();
    // The status Paragraph wraps at width-2 (the inset `notify_area`), so
    // at width 5 the inner width is 3: "12345 7890" hard-splits to
    // ["123", "45 ", "789", "0"] → 4 rows (matches what ratatui draws).
    app.set_error("12345 7890");
    assert_eq!(app.status_error_height(5), 4);
}

#[test]
fn status_error_height_multi_line() {
    let mut app = test_app();
    // Three explicit lines via \n
    app.set_status("line a\nline b\nline c");
    // Each line fits in width 80, so total = 3
    assert_eq!(app.status_error_height(80), 3);
}

#[test]
fn status_error_height_multi_line_with_wrapping() {
    let mut app = test_app();
    // Two lines; at width 5 the inner wrap width is 3: "hello" hard-splits
    // to ["hel", "lo"] (2 rows) and "12345 7890" to 4 rows — 6 total,
    // matching the rows the inset status Paragraph actually draws.
    app.set_error("hello\n12345 7890");
    assert_eq!(app.status_error_height(5), 6);
}

#[test]
fn status_error_height_empty_after_clearing() {
    let mut app = test_app();
    app.set_error("error");
    app.clear_error();
    assert_eq!(app.status_error_height(80), 0);
}

#[test]
fn status_error_height_status_takes_over_when_error_cleared() {
    let mut app = test_app();
    app.set_status("status");
    assert_eq!(app.status_error_height(80), 1);
}
