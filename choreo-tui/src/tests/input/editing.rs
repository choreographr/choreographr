use crate::connection::handle_terminal_event;
use crate::state::*;
use crate::tests::support::test_app;
use crossterm::event::Event;

// ── Cursor & editing tests ────────────────────────────────────

#[test]
fn insert_char_at_cursor_appends_when_at_end() {
    let mut app = test_app();
    app.input.insert_char_at_cursor('a');
    app.input.insert_char_at_cursor('b');
    app.input.insert_char_at_cursor('c');
    assert_eq!(app.input.text, "abc");
    assert_eq!(app.input.cursor, 3);
}

#[test]
fn insert_char_at_cursor_inserts_in_middle() {
    let mut app = test_app();
    app.input.text = "abde".to_string();
    app.input.cursor = 2;
    app.input.insert_char_at_cursor('c');
    assert_eq!(app.input.text, "abcde");
    assert_eq!(app.input.cursor, 3);
}

#[test]
fn insert_char_at_cursor_works_at_start() {
    let mut app = test_app();
    app.input.text = "bc".to_string();
    app.input.cursor = 0;
    app.input.insert_char_at_cursor('a');
    assert_eq!(app.input.text, "abc");
    assert_eq!(app.input.cursor, 1);
}

#[test]
fn insert_str_at_cursor_appends_at_end() {
    let mut app = test_app();
    app.input.text = "ab".to_string();
    app.input.cursor = 2;
    app.input.insert_str_at_cursor("cd");
    assert_eq!(app.input.text, "abcd");
    assert_eq!(app.input.cursor, 4);
}

#[test]
fn insert_str_at_cursor_inserts_in_middle() {
    let mut app = test_app();
    app.input.text = "abcd".to_string();
    app.input.cursor = 2;
    app.input.insert_str_at_cursor("XY");
    assert_eq!(app.input.text, "abXYcd");
    assert_eq!(app.input.cursor, 4);
}

#[test]
fn insert_str_at_cursor_works_at_start() {
    let mut app = test_app();
    app.input.text = "bc".to_string();
    app.input.cursor = 0;
    app.input.insert_str_at_cursor("a");
    assert_eq!(app.input.text, "abc");
    assert_eq!(app.input.cursor, 1);
}

#[test]
fn insert_str_at_cursor_handles_newlines() {
    let mut app = test_app();
    app.input.text = "ab".to_string();
    app.input.cursor = 1;
    app.input.insert_str_at_cursor("\ncd");
    assert_eq!(app.input.text, "a\ncdb");
    assert_eq!(app.input.cursor, 4);
}

#[test]
fn insert_str_at_cursor_empty_string_no_op() {
    let mut app = test_app();
    app.input.text = "ab".to_string();
    app.input.cursor = 1;
    app.input.insert_str_at_cursor("");
    assert_eq!(app.input.text, "ab");
    assert_eq!(app.input.cursor, 1);
}

// --- paste-event tests ---

#[test]
fn paste_event_inserts_into_chat_input() {
    let mut app = test_app();
    let (tx, _rx) = crossbeam_channel::unbounded();

    app.input.text = "hel".to_string();
    app.input.cursor = 3;
    handle_terminal_event(Event::Paste("lo world".to_string()), &mut app, &tx)
        .expect("handle paste");
    assert_eq!(app.input.text, "hello world");
    assert_eq!(app.input.cursor, 11);
}

#[test]
fn paste_event_inserts_into_chat_input_at_cursor() {
    let mut app = test_app();
    let (tx, _rx) = crossbeam_channel::unbounded();

    app.input.text = "heorld".to_string();
    app.input.cursor = 2;
    handle_terminal_event(Event::Paste("llo w".to_string()), &mut app, &tx).expect("handle paste");
    assert_eq!(app.input.text, "hello world");
    assert_eq!(app.input.cursor, 7);
}

#[test]
fn paste_event_ignored_during_fullscreen_overlay() {
    let mut app = test_app();
    let (tx, _rx) = crossbeam_channel::unbounded();

    app.input.text = "original".to_string();
    app.input.cursor = 8;
    app.fullscreen_image_target = Some((0, 0, crate::state::ImageSlot::Displayed(0)));
    handle_terminal_event(Event::Paste("should be ignored".to_string()), &mut app, &tx)
        .expect("handle paste during fullscreen");
    // Text should be unchanged.
    assert_eq!(app.input.text, "original");
    assert_eq!(app.input.cursor, 8);
}

#[test]
fn paste_event_noop_on_unhandled_page() {
    let mut app = test_app();
    let (tx, _rx) = crossbeam_channel::unbounded();

    // SessionManager page has no paste handler — should be a no-op.
    app.page = Page::SessionManager;
    app.input.text = "unchanged".to_string();
    app.input.cursor = 9;
    handle_terminal_event(Event::Paste("data".to_string()), &mut app, &tx)
        .expect("handle paste on session manager page");
    assert_eq!(app.input.text, "unchanged");
    assert_eq!(app.input.cursor, 9);
}

#[test]
fn cursor_left_moves_back_by_one_grapheme() {
    let mut app = test_app();
    app.input.text = "abcd".to_string();
    app.input.cursor = 4;
    app.input.cursor_left();
    assert_eq!(app.input.cursor, 3);
    app.input.cursor_left();
    assert_eq!(app.input.cursor, 2);
}

#[test]
fn cursor_left_stops_at_start() {
    let mut app = test_app();
    app.input.text = "a".to_string();
    app.input.cursor = 1;
    app.input.cursor_left();
    assert_eq!(app.input.cursor, 0);
    app.input.cursor_left();
    assert_eq!(app.input.cursor, 0);
}

#[test]
fn cursor_left_is_grapheme_aware() {
    let mut app = test_app();
    app.input.text = "a😀b".to_string();
    app.input.cursor = 6;
    app.input.cursor_left();
    assert_eq!(app.input.cursor, 5); // start of 'b' after emoji
    app.input.cursor_left();
    assert_eq!(app.input.cursor, 1); // start of 😀 (4-byte emoji at byte 1)
    app.input.cursor_left();
    assert_eq!(app.input.cursor, 0); // start of 'a'
}

#[test]
fn cursor_right_moves_forward_by_one_grapheme() {
    let mut app = test_app();
    app.input.text = "abcd".to_string();
    app.input.cursor = 0;
    app.input.cursor_right();
    assert_eq!(app.input.cursor, 1);
    app.input.cursor_right();
    assert_eq!(app.input.cursor, 2);
}

#[test]
fn cursor_right_stops_at_end() {
    let mut app = test_app();
    app.input.text = "a".to_string();
    app.input.cursor = 0;
    app.input.cursor_right();
    assert_eq!(app.input.cursor, 1);
    app.input.cursor_right();
    assert_eq!(app.input.cursor, 1);
}

#[test]
fn cursor_right_is_grapheme_aware() {
    let mut app = test_app();
    app.input.text = "a😀b".to_string();
    app.input.cursor = 0;
    app.input.cursor_right();
    assert_eq!(app.input.cursor, 1); // after 'a'
    app.input.cursor_right();
    assert_eq!(app.input.cursor, 5); // after 4-byte emoji
}

#[test]
fn cursor_home_moves_to_start() {
    let mut app = test_app();
    app.input.text = "hello".to_string();
    app.input.cursor = 5;
    app.input.cursor_home();
    assert_eq!(app.input.cursor, 0);
}

#[test]
fn cursor_end_moves_to_end() {
    let mut app = test_app();
    app.input.text = "hello".to_string();
    app.input.cursor = 0;
    app.input.cursor_end();
    assert_eq!(app.input.cursor, 5);
}

// ── Multi-line input tests ─────────────────────────────────

#[test]
fn cursor_home_line_with_newline() {
    let mut buf = InputBuffer::new();
    buf.text = "abc\ndef".to_string();
    buf.cursor = 6; // after "def" → line "def"
    buf.cursor_home_line();
    assert_eq!(buf.cursor, 4); // start of "def" (after \n)

    buf.cursor = 3; // at the \n
    buf.cursor_home_line();
    assert_eq!(buf.cursor, 0); // start of "abc"
}

#[test]
fn cursor_end_line_with_newline() {
    let mut buf = InputBuffer::new();
    buf.text = "abc\ndef".to_string();
    buf.cursor = 0; // start of "abc"
    buf.cursor_end_line();
    assert_eq!(buf.cursor, 3); // at the \n (end of "abc" line)

    buf.cursor = 4; // start of "def"
    buf.cursor_end_line();
    assert_eq!(buf.cursor, 7); // end of text
}

#[test]
fn cursor_visual_pos_single_line() {
    let mut buf = InputBuffer::new();
    buf.text = "hello".to_string();
    buf.cursor = 3;
    let (row, col) = buf.cursor_visual_pos(80);
    assert_eq!(row, 0);
    assert_eq!(col, 3);
}

#[test]
fn cursor_visual_pos_multi_line() {
    let mut buf = InputBuffer::new();
    buf.text = "hello\nworld".to_string();
    buf.cursor = 11; // after "world"
    let (row, col) = buf.cursor_visual_pos(80);
    assert_eq!(row, 1);
    assert_eq!(col, 5); // "world" has width 5
}

#[test]
fn cursor_visual_pos_wrapped() {
    let mut buf = InputBuffer::new();
    buf.text = "aaa bbb ccc ddd".to_string();
    buf.cursor = 15; // end of entire text
    let (row, col) = buf.cursor_visual_pos(7);
    // With max_width 7, greedy wrapping:
    // "aaa bbb" (width 7) on line 0
    // "ccc ddd" (width 7) on line 1
    assert_eq!(row, 1);
    assert_eq!(col, 7);
}

#[test]
fn cursor_up_simple() {
    let mut buf = InputBuffer::new();
    buf.text = "hello\nworld".to_string();
    buf.cursor = 9; // byte 9 = 'l', visual col 3 within "world"
    buf.cursor_up(80);
    // The cursor is always a char boundary; total slice via `.get()`.
    assert!(!buf.text.get(..buf.cursor).unwrap_or("").contains('\n'));
    // same col 3 lands at byte 3 ('l' in "hello")
    assert_eq!(buf.cursor, 3);
}

#[test]
fn cursor_down_simple() {
    let mut buf = InputBuffer::new();
    buf.text = "hello\nworld".to_string();
    buf.cursor = 2; // byte 2 = 'l', visual col 2 within "hello"
    buf.cursor_down(80);
    // The cursor is always a char boundary; total slice via `.get()`.
    assert!(buf.text.get(..buf.cursor).unwrap_or("").contains('\n'));
    // same col 2 lands at byte 8 ('r' in "world")
    assert_eq!(buf.cursor, 8);
}

#[test]
fn cursor_up_stays_at_top() {
    let mut buf = InputBuffer::new();
    buf.text = "hello".to_string();
    buf.cursor = 2;
    buf.cursor_up(80);
    assert_eq!(buf.cursor, 2); // unchanged — already on first visual line
}

#[test]
fn cursor_down_stays_at_bottom() {
    let mut buf = InputBuffer::new();
    buf.text = "hello".to_string();
    buf.cursor = 2;
    buf.cursor_down(80);
    assert_eq!(buf.cursor, 2); // unchanged — already on last visual line
}

#[test]
fn is_on_first_visual_line_returns_true_at_top() {
    let mut buf = InputBuffer::new();
    buf.text = "hello\nworld".to_string();
    buf.cursor = 2;
    assert!(buf.is_on_first_visual_line(80));
}

#[test]
fn is_on_first_visual_line_returns_false_on_second_line() {
    let mut buf = InputBuffer::new();
    buf.text = "hello\nworld".to_string();
    buf.cursor = 8; // in "world"
    assert!(!buf.is_on_first_visual_line(80));
}

#[test]
fn is_on_last_visual_line_returns_true_at_bottom() {
    let mut buf = InputBuffer::new();
    buf.text = "hello\nworld".to_string();
    buf.cursor = 8; // in "world"
    assert!(buf.is_on_last_visual_line(80));
}

#[test]
fn is_on_last_visual_line_returns_false_on_first_line() {
    let mut buf = InputBuffer::new();
    buf.text = "hello\nworld".to_string();
    buf.cursor = 2;
    assert!(!buf.is_on_last_visual_line(80));
}
