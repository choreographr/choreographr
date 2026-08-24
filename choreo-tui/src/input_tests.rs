use crate::connection::handle_terminal_event;
use crate::state::*;
use crate::test_util::{add_user_text, test_app};
use choreo_proto::{ClientMessage, Turn};
use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::{Constraint, Direction, Layout, Rect};

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
    let (tx, _rx) = std::sync::mpsc::channel();

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
    let (tx, _rx) = std::sync::mpsc::channel();

    app.input.text = "heorld".to_string();
    app.input.cursor = 2;
    handle_terminal_event(Event::Paste("llo w".to_string()), &mut app, &tx).expect("handle paste");
    assert_eq!(app.input.text, "hello world");
    assert_eq!(app.input.cursor, 7);
}

#[test]
fn paste_event_ignored_during_fullscreen_overlay() {
    let mut app = test_app();
    let (tx, _rx) = std::sync::mpsc::channel();

    app.input.text = "original".to_string();
    app.input.cursor = 8;
    app.fullscreen_image_target = Some((0, 0, 0));
    handle_terminal_event(Event::Paste("should be ignored".to_string()), &mut app, &tx)
        .expect("handle paste during fullscreen");
    // Text should be unchanged.
    assert_eq!(app.input.text, "original");
    assert_eq!(app.input.cursor, 8);
}

#[test]
fn paste_event_noop_on_unhandled_page() {
    let mut app = test_app();
    let (tx, _rx) = std::sync::mpsc::channel();

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

fn vl_text<'a>(vl: &VisualLineInfo, source: &'a str) -> &'a str {
    &source[vl.start_byte..vl.end_byte]
}

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
    assert!(!buf.text[..buf.cursor].contains('\n'));
    // same col 3 lands at byte 3 ('l' in "hello")
    assert_eq!(buf.cursor, 3);
}

#[test]
fn cursor_down_simple() {
    let mut buf = InputBuffer::new();
    buf.text = "hello\nworld".to_string();
    buf.cursor = 2; // byte 2 = 'l', visual col 2 within "hello"
    buf.cursor_down(80);
    assert!(buf.text[..buf.cursor].contains('\n'));
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

// ── byte_offset_at_click (mouse click → cursor) ─────────────

#[test]
fn byte_offset_at_click_single_line() {
    let mut buf = InputBuffer::new();
    buf.text = "hello".to_string();
    assert_eq!(buf.byte_offset_at_click(80, 1, 0, 0), 0);
    assert_eq!(buf.byte_offset_at_click(80, 1, 0, 2), 2);
    assert_eq!(buf.byte_offset_at_click(80, 1, 0, 5), 5);
    // Past the right edge of the line clamps to the line end.
    assert_eq!(buf.byte_offset_at_click(80, 1, 0, 100), 5);
}

#[test]
fn byte_offset_at_click_multiline_text() {
    let mut buf = InputBuffer::new();
    buf.text = "abc\ndef".to_string();
    assert_eq!(buf.byte_offset_at_click(80, 2, 0, 2), 2);
    assert_eq!(buf.byte_offset_at_click(80, 2, 1, 1), 5); // 'e' in "def" → byte 4+1
    assert_eq!(buf.byte_offset_at_click(80, 2, 1, 3), 7); // end of "def"
}

#[test]
fn byte_offset_at_click_wrapped_lines() {
    let mut buf = InputBuffer::new();
    buf.text = "aaa bbb ccc ddd".to_string();
    // max_width 7 wraps as: line 0 = "aaa bbb" (bytes 0..7), line 1 = "ccc ddd" (bytes 8..15)
    assert_eq!(buf.byte_offset_at_click(7, 2, 0, 4), 4); // after "aaa " → the space
    assert_eq!(buf.byte_offset_at_click(7, 2, 1, 3), 11); // after "ccc" → the space
    assert_eq!(buf.byte_offset_at_click(7, 2, 1, 7), 15); // end of line 1
}

#[test]
fn byte_offset_at_click_below_last_line_goes_to_end() {
    let mut buf = InputBuffer::new();
    buf.text = "abc".to_string();
    // Row 5 is past the single visual line → cursor at end of buffer.
    assert_eq!(buf.byte_offset_at_click(80, 1, 5, 0), 3);
}

#[test]
fn byte_offset_at_click_uses_scroll_offset() {
    let mut buf = InputBuffer::new();
    buf.text = "aaa bbb ccc ddd".to_string();
    buf.scroll_offset = 1;
    // Content row 0 now displays visual line 1 ("ccc ddd", bytes 8..15).
    assert_eq!(buf.byte_offset_at_click(7, 1, 0, 0), 8);
}

#[test]
fn byte_offset_at_click_clamps_stale_scroll_offset() {
    let mut buf = InputBuffer::new();
    buf.text = "aaa bbb ccc ddd".to_string();
    // A scroll_offset left over from a wider box (e.g. after a shrink-resize
    // before the next re-clamp) must be clamped to the renderer's visible
    // window instead of sending every click to the end of the buffer.
    buf.scroll_offset = 10;
    // With visible_height 1 the window shows one line at a time: it starts at
    // the last line (line 1, bytes 8..15).
    assert_eq!(buf.byte_offset_at_click(7, 1, 0, 0), 8);
    assert_eq!(buf.byte_offset_at_click(7, 1, 0, 3), 11);
    // With visible_height 2 the whole text fits, so the window starts at 0.
    assert_eq!(buf.byte_offset_at_click(7, 2, 0, 0), 0);
    // A click below the last visible line still resolves to the end.
    assert_eq!(buf.byte_offset_at_click(7, 1, 3, 0), 15);
}

#[test]
fn byte_offset_at_click_is_grapheme_aware() {
    let mut buf = InputBuffer::new();
    buf.text = "a😀b".to_string();
    // 😀 is 4 bytes wide; clicking at display column 1 places the cursor
    // between 'a' and the emoji (byte 1), not inside the emoji's bytes.
    assert_eq!(buf.byte_offset_at_click(80, 1, 0, 1), 1);
    assert_eq!(buf.byte_offset_at_click(80, 1, 0, 3), 5); // after the emoji, before 'b'
}

#[test]
fn byte_offset_at_click_right_half_of_wide_char_places_after() {
    let mut buf = InputBuffer::new();
    buf.text = "a😀b".to_string();
    // 😀 occupies display columns 1..3.  A click on its left cell (col 1)
    // places the cursor before it; a click on its right cell (col 2) or one
    // column past it (col 3) places the cursor after it.
    assert_eq!(buf.byte_offset_at_click(80, 1, 0, 1), 1); // left cell → before
    assert_eq!(buf.byte_offset_at_click(80, 1, 0, 2), 5); // right cell → after
    assert_eq!(buf.byte_offset_at_click(80, 1, 0, 3), 5); // one past → after
}

#[test]
fn byte_offset_at_click_never_splits_zwj_grapheme_cluster() {
    let mut buf = InputBuffer::new();
    // The ZWJ family emoji is a single extended grapheme cluster of 7
    // codepoints (bytes 0..25) occupying 2 display cells, followed by 'x'.
    buf.text = "👨‍👩‍👧‍👦x".to_string();
    // Any click inside the cluster's cells must land on a grapheme boundary —
    // before the cluster (byte 0) or after it (byte 25), never mid-cluster.
    assert_eq!(buf.byte_offset_at_click(80, 1, 0, 0), 0); // left cell → before
    assert_eq!(buf.byte_offset_at_click(80, 1, 0, 1), 25); // right cell → after
    assert_eq!(buf.byte_offset_at_click(80, 1, 0, 2), 25); // 'x' (byte 25)
    assert_eq!(buf.byte_offset_at_click(80, 1, 0, 3), 26); // end of buffer
}

#[test]
fn handle_key_enter_submits_without_shift() {
    let mut buf = InputBuffer::new();
    buf.text = "hello".to_string();
    buf.cursor = 5;
    // Plain Enter returns false (caller should submit)
    assert!(!buf.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));
    assert_eq!(buf.text, "hello"); // unchanged
}

#[test]
fn handle_key_home_on_multi_line() {
    let mut buf = InputBuffer::new();
    buf.text = "abc\ndef".to_string();
    buf.cursor = 6; // in "def"
    buf.handle_key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
    assert_eq!(buf.cursor, 4); // start of "def" (not 0)
}

#[test]
fn handle_key_ctrl_home_on_multi_line() {
    let mut buf = InputBuffer::new();
    buf.text = "abc\ndef".to_string();
    buf.cursor = 6;
    buf.handle_key(KeyEvent::new(KeyCode::Home, KeyModifiers::CONTROL));
    assert_eq!(buf.cursor, 0); // document start
}

#[test]
fn handle_key_end_on_multi_line() {
    let mut buf = InputBuffer::new();
    buf.text = "abc\ndef".to_string();
    buf.cursor = 0;
    buf.handle_key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
    assert_eq!(buf.cursor, 3); // at the \n (end of "abc" line)
}

#[test]
fn handle_key_ctrl_end_on_multi_line() {
    let mut buf = InputBuffer::new();
    buf.text = "abc\ndef".to_string();
    buf.cursor = 0;
    buf.handle_key(KeyEvent::new(KeyCode::End, KeyModifiers::CONTROL));
    assert_eq!(buf.cursor, 7); // document end
}

#[test]
fn compute_visual_lines_handles_empty_text() {
    let lines = compute_visual_lines("", 80);
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].start_byte, 0);
    assert_eq!(lines[0].end_byte, 0);
}

#[test]
fn compute_visual_lines_no_wrap() {
    let lines = compute_visual_lines("hello world", 80);
    assert_eq!(lines.len(), 1);
    assert_eq!(vl_text(&lines[0], "hello world"), "hello world");
}

#[test]
fn compute_visual_lines_wraps_words() {
    let lines = compute_visual_lines("aaa bbb ccc", 7);
    assert_eq!(lines.len(), 2);
    assert_eq!(vl_text(&lines[0], "aaa bbb ccc"), "aaa bbb");
    assert_eq!(vl_text(&lines[1], "aaa bbb ccc"), "ccc");
}

#[test]
fn compute_visual_lines_respects_newlines() {
    let lines = compute_visual_lines("hello\nworld", 80);
    assert_eq!(lines.len(), 2);
    assert_eq!(vl_text(&lines[0], "hello\nworld"), "hello");
    assert_eq!(vl_text(&lines[1], "hello\nworld"), "world");
}

#[test]
fn compute_visual_lines_mixed_newlines_and_wrapping() {
    let lines = compute_visual_lines("a b c\nd e f", 4);
    assert_eq!(lines.len(), 4);
    assert_eq!(vl_text(&lines[0], "a b c\nd e f"), "a b");
    assert_eq!(vl_text(&lines[1], "a b c\nd e f"), "c");
    assert_eq!(vl_text(&lines[2], "a b c\nd e f"), "d e");
    assert_eq!(vl_text(&lines[3], "a b c\nd e f"), "f");
}

#[test]
fn compute_visual_lines_long_word_does_not_break() {
    let lines = compute_visual_lines("superlongword", 5);
    assert_eq!(lines.len(), 1);
    assert_eq!(vl_text(&lines[0], "superlongword"), "superlongword");
}

#[test]
fn byte_offset_at_column_basic() {
    assert_eq!(byte_offset_at_column("hello", 2), 2);
    assert_eq!(byte_offset_at_column("hello", 10), 5); // clamps to len
}

#[test]
fn byte_offset_at_column_cjk() {
    // Each CJK char is 2 columns wide
    assert_eq!(byte_offset_at_column("你好", 2), 3); // after first CJK char (3 bytes)
}

#[test]
fn cursor_up_preserves_column_across_wrapped_lines() {
    let mut buf = InputBuffer::new();
    buf.text = "aaaa bbbb cccc dddd".to_string();
    buf.cursor = buf.text.len(); // end of text
    // With max_width 8:
    // "aaaa" (4) + " " (1) + "bbbb" (4) = 9 > 8 → wrap
    // Line 0: "aaaa bbbb" (width 9) — wait, that's > 8...
    // Actually: "aaaa" (4) fits. " bbbb" (5) → 4+5=9 > 8 → wrap
    // Line 0: "aaaa" (width 4), Line 1: "bbbb cccc" (wait...)
    // "bbbb" (4) fits on line 1. " cccc" (5) → 4+5=9 > 8 → wrap
    // Line 1: "bbbb" (width 4), Line 2: "cccc dddd" ...
    // "cccc" (4) fits. " dddd" (5) → 4+5=9 > 8 → wrap
    // Line 2: "cccc" (width 4), Line 3: "dddd" (width 4)

    buf.cursor = 19; // end of "dddd"
    buf.cursor_up(8);
    // Should land at end of "cccc" (last word on line 2)
    let (row, _) = buf.cursor_visual_pos(8);
    assert_eq!(row, 2);
}

#[test]
fn cursor_down_from_wrapped_line() {
    let mut buf = InputBuffer::new();
    buf.text = "aaaa bbbb cccc".to_string();
    buf.cursor = 0;
    buf.cursor_down(8);
    // Should land on line 1 at column 0
    let (row, col) = buf.cursor_visual_pos(8);
    assert_eq!(row, 1);
    assert_eq!(col, 0);
}

#[test]
fn navigate_history_up_down_with_multi_line() {
    let mut app = test_app();
    // Insert a turn with user_text so history exists
    let id = app.next_request_id;
    app.display_for(0).view.insert_or_replace(
        id,
        choreo_proto::Turn {
            created_at: choreo_proto::TimestampMs::now(),
            undone: false,
            error: None,
            user_text: Some("multi\nline\ntext".into()),
            assistant_text: None,
            assistant_reasoning: None,
            tool_calls: vec![],
            token_usage: None,
            tool_results: vec![],
            displayed_images: vec![],
            reasoning_artifact: None,
            reasoning_producer: None,
        },
    );
    app.next_request_id += 1;

    // Navigation should set cursor to end of text
    app.navigate_history_up();
    assert_eq!(app.input.text, "multi\nline\ntext");
    assert_eq!(app.input.cursor, 15);
}

#[test]
fn navigate_history_up_adjusts_scroll_offset_for_long_entry() {
    let mut app = test_app();
    app.last_terminal_size = Some((80, 24));
    // Short text currently in input
    app.input.text = "x".to_string();
    app.input.cursor = 1;

    // Insert a long multi-line history entry (20 visual lines at 80-wide terminal)
    let long_text: String = (0..20).map(|i| format!("line {i}\n")).collect();
    let id = app.next_request_id;
    app.display_for(0).view.insert_or_replace(
        id,
        choreo_proto::Turn {
            created_at: choreo_proto::TimestampMs::now(),
            undone: false,
            error: None,
            user_text: Some(long_text),
            assistant_text: None,
            assistant_reasoning: None,
            tool_calls: vec![],
            token_usage: None,
            tool_results: vec![],
            displayed_images: vec![],
            reasoning_artifact: None,
            reasoning_producer: None,
        },
    );
    app.next_request_id += 1;

    app.navigate_history_up();
    // After loading a long history entry and setting cursor to end,
    // scroll_offset should be adjusted so the cursor is visible.
    let inner = input_inner_width(80);
    let visual_lines = compute_visual_lines(&app.input.text, inner);
    let (cursor_row, _) = find_cursor_pos(&app.input.text, app.input.cursor, &visual_lines);
    let visible_height = app.input_bar_content_lines(80) as usize;
    let cursor_row = cursor_row as usize;
    // Cursor should be within the visible window
    assert!(
        cursor_row >= app.input.scroll_offset,
        "cursor_row {cursor_row} should be >= scroll_offset {}",
        app.input.scroll_offset
    );
    assert!(
        cursor_row < app.input.scroll_offset + visible_height,
        "cursor_row {cursor_row} should be < scroll_offset {} + visible_height {visible_height}",
        app.input.scroll_offset
    );
}

#[test]
fn navigate_history_down_adjusts_scroll_offset_for_long_draft() {
    let mut app = test_app();
    app.last_terminal_size = Some((80, 24));
    // A long multi-line draft saved in history state
    let long_draft: String = (0..20).map(|i| format!("line {i}\n")).collect();
    app.saved_draft = long_draft.clone();
    app.input.text = "x".to_string();
    app.input.cursor = 1;
    // Simulate being at the first history entry (so Down restores draft)
    app.history_index = Some(0);
    let id = app.next_request_id;
    app.display_for(0).view.insert_or_replace(
        id,
        choreo_proto::Turn {
            created_at: choreo_proto::TimestampMs::now(),
            undone: false,
            error: None,
            user_text: Some("history entry".into()),
            assistant_text: None,
            assistant_reasoning: None,
            tool_calls: vec![],
            token_usage: None,
            tool_results: vec![],
            displayed_images: vec![],
            reasoning_artifact: None,
            reasoning_producer: None,
        },
    );
    app.next_request_id += 1;

    app.navigate_history_down();
    // After restoring the long draft, scroll_offset should ensure cursor is visible.
    let inner = input_inner_width(80);
    let visual_lines = compute_visual_lines(&app.input.text, inner);
    let (cursor_row, _) = find_cursor_pos(&app.input.text, app.input.cursor, &visual_lines);
    let visible_height = app.input_bar_content_lines(80) as usize;
    let cursor_row = cursor_row as usize;
    assert!(
        cursor_row >= app.input.scroll_offset,
        "cursor_row {cursor_row} should be >= scroll_offset {}",
        app.input.scroll_offset
    );
    assert!(
        cursor_row < app.input.scroll_offset + visible_height,
        "cursor_row {cursor_row} should be < scroll_offset {} + visible_height {visible_height}",
        app.input.scroll_offset
    );
}

#[test]
fn backspace_at_cursor_removes_before_cursor() {
    let mut app = test_app();
    app.input.text = "abcd".to_string();
    app.input.cursor = 3;
    app.input.backspace_at_cursor();
    assert_eq!(app.input.text, "abd");
    assert_eq!(app.input.cursor, 2);
}

#[test]
fn backspace_at_cursor_does_nothing_at_start() {
    let mut app = test_app();
    app.input.text = "a".to_string();
    app.input.cursor = 0;
    app.input.backspace_at_cursor();
    assert_eq!(app.input.text, "a");
    assert_eq!(app.input.cursor, 0);
}

#[test]
fn backspace_at_cursor_is_grapheme_aware() {
    let mut app = test_app();
    app.input.text = "a😀".to_string();
    app.input.cursor = 5;
    app.input.backspace_at_cursor();
    assert_eq!(app.input.text, "a");
    assert_eq!(app.input.cursor, 1);
}

#[test]
fn delete_at_cursor_removes_at_cursor() {
    let mut app = test_app();
    app.input.text = "abcd".to_string();
    app.input.cursor = 1;
    app.input.delete_at_cursor();
    assert_eq!(app.input.text, "acd");
    assert_eq!(app.input.cursor, 1);
}

#[test]
fn delete_at_cursor_does_nothing_at_end() {
    let mut app = test_app();
    app.input.text = "a".to_string();
    app.input.cursor = 1;
    app.input.delete_at_cursor();
    assert_eq!(app.input.text, "a");
    assert_eq!(app.input.cursor, 1);
}

#[test]
fn delete_at_cursor_is_grapheme_aware() {
    let mut app = test_app();
    app.input.text = "a😀b".to_string();
    app.input.cursor = 1;
    app.input.delete_at_cursor();
    assert_eq!(app.input.text, "ab");
    assert_eq!(app.input.cursor, 1);
}

#[test]
fn word_left_moves_to_previous_word() {
    let mut app = test_app();
    app.input.text = "hello world foo".to_string();
    app.input.cursor = 15;
    app.input.word_left();
    assert_eq!(app.input.cursor, 12); // start of "foo"
    app.input.word_left();
    assert_eq!(app.input.cursor, 6); // start of "world"
    app.input.word_left();
    assert_eq!(app.input.cursor, 0); // start of "hello"
}

#[test]
fn word_left_stays_at_zero() {
    let mut app = test_app();
    app.input.text = "hello".to_string();
    app.input.cursor = 0;
    app.input.word_left();
    assert_eq!(app.input.cursor, 0);
}

#[test]
fn word_right_moves_to_next_word() {
    let mut app = test_app();
    app.input.text = "hello world foo".to_string();
    app.input.cursor = 0;
    app.input.word_right();
    assert_eq!(app.input.cursor, 6); // start of "world"
    app.input.word_right();
    assert_eq!(app.input.cursor, 12); // start of "foo"
    app.input.word_right();
    assert_eq!(app.input.cursor, 15); // end of string
}

#[test]
fn word_right_stays_at_end() {
    let mut app = test_app();
    app.input.text = "hello".to_string();
    app.input.cursor = 5;
    app.input.word_right();
    assert_eq!(app.input.cursor, 5);
}

#[test]
fn word_right_skips_whitespace() {
    let mut app = test_app();
    app.input.text = "  hello  ".to_string();
    app.input.cursor = 0;
    app.input.word_right();
    assert_eq!(app.input.cursor, 2); // start of "hello", skipping leading spaces
}

#[test]
fn delete_word_backward_removes_previous_word() {
    let mut app = test_app();
    app.input.text = "hello world".to_string();
    app.input.cursor = 11;
    app.input.delete_word_backward();
    assert_eq!(app.input.text, "hello ");
    assert_eq!(app.input.cursor, 6);
}

#[test]
fn delete_word_backward_at_start_does_nothing() {
    let mut app = test_app();
    app.input.text = "hello".to_string();
    app.input.cursor = 0;
    app.input.delete_word_backward();
    assert_eq!(app.input.text, "hello");
    assert_eq!(app.input.cursor, 0);
}

#[test]
fn delete_word_forward_removes_next_word() {
    let mut app = test_app();
    app.input.text = "hello world foo".to_string();
    app.input.cursor = 6;
    app.input.delete_word_forward();
    assert_eq!(app.input.text, "hello foo");
    assert_eq!(app.input.cursor, 6);
}

#[test]
fn delete_word_forward_at_end_does_nothing() {
    let mut app = test_app();
    app.input.text = "hello".to_string();
    app.input.cursor = 5;
    app.input.delete_word_forward();
    assert_eq!(app.input.text, "hello");
    assert_eq!(app.input.cursor, 5);
}

#[test]
fn delete_to_start_removes_from_beginning_to_cursor() {
    let mut app = test_app();
    app.input.text = "hello world".to_string();
    app.input.cursor = 6;
    app.input.delete_to_start();
    assert_eq!(app.input.text, "world");
    assert_eq!(app.input.cursor, 0);
}

#[test]
fn delete_to_start_when_at_end_clears_input() {
    let mut app = test_app();
    app.input.text = "hello".to_string();
    app.input.cursor = 5;
    app.input.delete_to_start();
    assert!(app.input.is_empty());
    assert_eq!(app.input.cursor, 0);
}

#[test]
fn delete_to_start_when_at_zero_does_nothing() {
    let mut app = test_app();
    app.input.text = "hello".to_string();
    app.input.cursor = 0;
    app.input.delete_to_start();
    assert_eq!(app.input.text, "hello");
    assert_eq!(app.input.cursor, 0);
}

#[test]
fn terminal_event_enter_scrolls_to_bottom_from_scrolled_up() {
    let mut app = test_app();
    let (tx, _rx) = std::sync::mpsc::channel();

    // Set a small viewport so even a few turns are scrollable.
    app.history_viewport = HistoryViewport {
        width: 80,
        height: 1,
    };

    // Add user text turns to create scrollable content beyond the viewport.
    add_user_text(&mut app, "first");
    add_user_text(&mut app, "second");
    add_user_text(&mut app, "third");

    // Start at the bottom.
    assert_eq!(
        app.effective_scroll(),
        0,
        "should start at bottom (effective_scroll = 0)"
    );

    // Scroll up to simulate a user who was reading past history.
    app.scroll_up(1);
    assert!(
        app.effective_scroll() > 0,
        "should have scrolled away from bottom, got {}",
        app.effective_scroll()
    );

    // Submit a new message via Enter.
    app.input.text = "new message".to_string();
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("handle enter");

    // After submitting, the scroll should return to the bottom so the
    // user can see their message appear in the history.
    assert_eq!(
        app.effective_scroll(),
        0,
        "should scroll to bottom after submitting a message"
    );
}

#[test]
fn terminal_event_submit_resets_cursor() {
    let mut app = test_app();
    app.input.text = "hello".to_string();
    app.input.cursor = 5;
    let (tx, _rx) = std::sync::mpsc::channel();

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("handle enter");

    assert!(app.input.is_empty());
    assert_eq!(app.input.cursor, 0);
}

#[test]
fn terminal_event_arrow_keys_move_cursor() {
    let mut app = test_app();
    app.input.text = "abc".to_string();
    app.input.cursor = 3;
    let (tx, _rx) = std::sync::mpsc::channel();

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("handle left");
    assert_eq!(app.input.cursor, 2);

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("handle right");
    assert_eq!(app.input.cursor, 3);
}

#[test]
fn terminal_event_home_end_move_cursor() {
    let mut app = test_app();
    app.input.text = "abc".to_string();
    app.input.cursor = 1;
    let (tx, _rx) = std::sync::mpsc::channel();

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("handle home");
    assert_eq!(app.input.cursor, 0);

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("handle end");
    assert_eq!(app.input.cursor, 3);
}

#[test]
fn terminal_event_delete_removes_at_cursor() {
    let mut app = test_app();
    app.input.text = "abcd".to_string();
    app.input.cursor = 1;
    let (tx, _rx) = std::sync::mpsc::channel();

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("handle delete");

    assert_eq!(app.input.text, "acd");
    assert_eq!(app.input.cursor, 1);
}

#[test]
fn terminal_event_backspace_uses_cursor() {
    let mut app = test_app();
    app.input.text = "abcd".to_string();
    app.input.cursor = 3;
    let (tx, _rx) = std::sync::mpsc::channel();

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("handle backspace");

    assert_eq!(app.input.text, "abd");
    assert_eq!(app.input.cursor, 2);
}

#[test]
fn terminal_event_inserts_char_at_cursor() {
    let mut app = test_app();
    app.input.text = "abd".to_string();
    app.input.cursor = 2;
    let (tx, _rx) = std::sync::mpsc::channel();

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("handle char");

    assert_eq!(app.input.text, "abcd");
    assert_eq!(app.input.cursor, 3);
}

#[test]
fn terminal_event_ctrl_backspace_clears_draft_prompt() {
    let mut app = test_app();
    app.input.text = "hello world".to_string();
    // Cursor parked mid-text: clearing the draft must empty the whole
    // buffer regardless of where the cursor sits.
    app.input.cursor = 6;
    let (tx, _rx) = std::sync::mpsc::channel();

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::CONTROL)),
        &mut app,
        &tx,
    )
    .expect("handle ctrl+backspace");

    assert!(
        app.input.text.is_empty(),
        "ctrl+backspace must clear the draft prompt"
    );
    assert_eq!(app.input.cursor, 0);
}

#[test]
fn terminal_event_ctrl_w_deletes_word_backward() {
    let mut app = test_app();
    app.input.text = "hello world".to_string();
    app.input.cursor = 11;
    let (tx, _rx) = std::sync::mpsc::channel();

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL)),
        &mut app,
        &tx,
    )
    .expect("handle ctrl+w");

    assert_eq!(app.input.text, "hello ");
    assert_eq!(app.input.cursor, 6);
}

#[test]
fn terminal_event_ctrl_u_deletes_to_start() {
    let mut app = test_app();
    app.input.text = "hello world".to_string();
    app.input.cursor = 6;
    let (tx, _rx) = std::sync::mpsc::channel();

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL)),
        &mut app,
        &tx,
    )
    .expect("handle ctrl+u");

    assert_eq!(app.input.text, "world");
    assert_eq!(app.input.cursor, 0);
}

#[test]
fn terminal_event_ctrl_delete_deletes_word_forward() {
    let mut app = test_app();
    app.input.text = "hello world foo".to_string();
    app.input.cursor = 6;
    let (tx, _rx) = std::sync::mpsc::channel();

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Delete, KeyModifiers::CONTROL)),
        &mut app,
        &tx,
    )
    .expect("handle ctrl+delete");

    assert_eq!(app.input.text, "hello foo");
    assert_eq!(app.input.cursor, 6);
}

#[test]
fn word_left_respects_punctuation_boundaries() {
    let mut buf = InputBuffer::new();
    // Punctuation (".") is not whitespace, so "hello.world" is treated as one token.
    buf.text = "hello.world foo".to_string();
    buf.cursor = 15;
    buf.word_left();
    assert_eq!(buf.cursor, 12); // start of "foo"
    buf.word_left();
    assert_eq!(buf.cursor, 0); // "hello.world" has no whitespace, jumps to start
}

#[test]
fn word_right_respects_punctuation_boundaries() {
    let mut buf = InputBuffer::new();
    buf.text = "hello.world foo".to_string();
    buf.cursor = 0;
    buf.word_right();
    assert_eq!(buf.cursor, 12); // after "hello.world" — no whitespace break inside it
}

#[test]
fn word_delete_within_whitespace_does_not_panic() {
    let mut app = test_app();
    app.input.text = "  hello  world  ".to_string();
    app.input.cursor = 8;
    // Must not panic when cursor sits within whitespace between words.
    app.input.delete_word_backward();
    assert!(app.input.cursor <= app.input.text.len());
}

// ── Command history tests ──────────────────────────────────────

#[test]
fn navigate_history_up_loads_most_recent() {
    let mut app = test_app();
    // Oldest first, newest last — user_texts() reverses so texts[0] = newest.
    add_user_text(&mut app, "cmd-2");
    add_user_text(&mut app, "cmd-1");
    add_user_text(&mut app, "cmd-0");
    app.input.text = "typing".to_string();
    app.input.cursor = 6;

    app.navigate_history_up();

    assert_eq!(app.history_index, Some(0));
    assert_eq!(app.input.text, "cmd-0");
    assert_eq!(app.input.cursor, 5);
    assert_eq!(app.saved_draft, "typing");
}

#[test]
fn navigate_history_up_moves_to_older() {
    let mut app = test_app();
    add_user_text(&mut app, "c");
    add_user_text(&mut app, "b");
    add_user_text(&mut app, "a");
    app.history_index = Some(0);

    app.navigate_history_up();
    assert_eq!(app.history_index, Some(1));
    assert_eq!(app.input.text, "b");

    app.navigate_history_up();
    assert_eq!(app.history_index, Some(2));
    assert_eq!(app.input.text, "c");
}

#[test]
fn navigate_history_up_stops_at_oldest() {
    let mut app = test_app();
    add_user_text(&mut app, "c");
    add_user_text(&mut app, "b");
    app.history_index = Some(1);
    app.input.text = "b".to_string();
    app.input.cursor = 1;

    app.navigate_history_up();
    assert_eq!(app.history_index, Some(1));
    assert_eq!(app.input.text, "b");
}

#[test]
fn navigate_history_up_empty_history_does_nothing() {
    let mut app = test_app();
    app.input.text = "hello".to_string();

    app.navigate_history_up();
    assert_eq!(app.input.text, "hello");
    assert!(app.history_index.is_none());
}

#[test]
fn navigate_history_down_restores_draft() {
    let mut app = test_app();
    add_user_text(&mut app, "cmd");
    app.history_index = Some(0);
    app.saved_draft = "draft".to_string();
    app.input.text = "cmd".to_string();

    app.navigate_history_down();

    assert!(app.history_index.is_none());
    assert_eq!(app.input.text, "draft");
    assert!(app.saved_draft.is_empty());
}

#[test]
fn navigate_history_down_moves_to_newer() {
    let mut app = test_app();
    add_user_text(&mut app, "c");
    add_user_text(&mut app, "b");
    add_user_text(&mut app, "a");
    app.history_index = Some(2);
    app.input.text = "c".to_string();

    app.navigate_history_down();
    assert_eq!(app.history_index, Some(1));
    assert_eq!(app.input.text, "b");

    app.navigate_history_down();
    assert_eq!(app.history_index, Some(0));
    assert_eq!(app.input.text, "a");

    app.navigate_history_down();
    assert!(app.history_index.is_none());
}

#[test]
fn navigate_history_down_survives_shrunk_history() {
    let mut app = test_app();
    add_user_text(&mut app, "oldest");
    add_user_text(&mut app, "older");
    add_user_text(&mut app, "recent");
    // Simulate having browsed back to the oldest entry...
    app.history_index = Some(2);
    app.input.text = "oldest".to_string();
    app.saved_draft = "draft".to_string();

    // ...then the conversation changes underneath us: the turn list is reset
    // so only a single user text remains (e.g. a session switch mid-nav).
    {
        let view = &mut app.display_for(0).view;
        view.turns.clear();
        view.turns.insert(
            0,
            Turn {
                created_at: choreo_proto::TimestampMs::now(),
                undone: false,
                error: None,
                user_text: Some("recent".to_string()),
                assistant_text: None,
                assistant_reasoning: None,
                tool_calls: vec![],
                token_usage: None,
                tool_results: vec![],
                displayed_images: vec![],
                reasoning_artifact: None,
                reasoning_producer: None,
            },
        );
    }
    app.rebuild_height_prefix();

    // The stale index (2) must be clamped down to the newest remaining entry
    // (index 0) instead of panicking with an out-of-bounds index.
    app.navigate_history_down();
    assert_eq!(app.history_index, Some(0));
    assert_eq!(app.input.text, "recent");

    // And the next Down still exits back to the saved draft.
    app.navigate_history_down();
    assert!(app.history_index.is_none());
    assert_eq!(app.input.text, "draft");
    assert!(app.saved_draft.is_empty());
}

#[test]
fn navigate_history_down_empty_history_restores_draft() {
    let mut app = test_app();
    app.history_index = Some(3);
    app.saved_draft = "draft".to_string();
    app.input.text = "stale".to_string();

    // No turns at all: Down must fall back to the draft, not panic.
    app.navigate_history_down();
    assert!(app.history_index.is_none());
    assert_eq!(app.input.text, "draft");
    assert!(app.saved_draft.is_empty());
}

#[test]
fn navigate_history_up_survives_shrunk_history() {
    let mut app = test_app();
    add_user_text(&mut app, "oldest");
    add_user_text(&mut app, "older");
    add_user_text(&mut app, "recent");
    // Browsed all the way to the oldest entry...
    app.history_index = Some(2);
    app.input.text = "oldest".to_string();
    app.saved_draft = "draft".to_string();

    // ...then the turn list shrinks to a single entry underneath us (e.g. a
    // session switch mid-nav).
    {
        let view = &mut app.display_for(0).view;
        view.turns.clear();
        view.turns.insert(
            0,
            Turn {
                created_at: choreo_proto::TimestampMs::now(),
                undone: false,
                error: None,
                user_text: Some("recent".to_string()),
                assistant_text: None,
                assistant_reasoning: None,
                tool_calls: vec![],
                token_usage: None,
                tool_results: vec![],
                displayed_images: vec![],
                reasoning_artifact: None,
                reasoning_producer: None,
            },
        );
    }
    app.rebuild_height_prefix();

    // The stale index (2) must clamp to the oldest remaining entry (0) and
    // resync the displayed text — not silently no-op while showing stale text
    // from the pre-shrink list.
    app.navigate_history_up();
    assert_eq!(app.history_index, Some(0));
    assert_eq!(app.input.text, "recent");

    // Further Up presses stay at the oldest remaining entry.
    app.navigate_history_up();
    assert_eq!(app.history_index, Some(0));
    assert_eq!(app.input.text, "recent");

    // Down still walks back to the saved draft.
    app.navigate_history_down();
    assert!(app.history_index.is_none());
    assert_eq!(app.input.text, "draft");
    assert!(app.saved_draft.is_empty());
}

#[test]
fn history_nav_resets_after_commit() {
    let mut app = test_app();
    add_user_text(&mut app, "old");
    app.history_index = Some(0);
    app.saved_draft = "draft".to_string();

    app.commit_to_history();

    assert!(app.history_index.is_none());
    assert!(app.saved_draft.is_empty());
}

#[test]
fn terminal_event_up_down_navigates_history() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = test_app();
    add_user_text(&mut app, "older");
    add_user_text(&mut app, "recent");

    // Press Up — loads most recent
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("handle up");
    assert_eq!(app.input.text, "recent");
    assert!(app.saved_draft.is_empty());

    // Press Up again — loads older
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("handle up");
    assert_eq!(app.input.text, "older");

    // Press Down — goes back to recent
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("handle down");
    assert_eq!(app.input.text, "recent");

    // Press Down — past newest, restores draft (empty)
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("handle down");
    assert_eq!(app.input.text, "");
    assert!(app.saved_draft.is_empty());
}

#[test]
fn terminal_event_history_up_empty_does_nothing() {
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = test_app();

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("handle up");
    assert_eq!(app.input.text, "");
}

#[test]
fn commit_does_not_duplicate_user_text() {
    let mut app = test_app();
    add_user_text(&mut app, "hello");

    app.commit_to_history();

    assert_eq!(app.user_texts().len(), 1);
    assert_eq!(app.user_texts()[0], "hello");
}

#[test]
fn click_on_reasoning_header_toggles_collapse() {
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 20;

    let turn = Turn {
        created_at: choreo_proto::TimestampMs::now(),
        undone: false,
        error: None,
        user_text: None,
        assistant_text: Some("Response text.".into()),
        assistant_reasoning: Some("Hidden thinking.".into()),
        tool_calls: vec![],
        token_usage: None,
        tool_results: vec![],
        displayed_images: vec![],
        reasoning_artifact: None,
        reasoning_producer: None,
    };
    app.active_display()
        .unwrap()
        .view
        .insert_or_replace(1, turn);
    app.rebuild_height_prefix();

    // No user text: assistant separator (0), padding (1), response (2),
    // blank separator (3), then the reasoning header at rows [4,5).
    let (start, end) = app.active_display().unwrap().turn_layouts[0]
        .reasoning_header_range
        .expect("reasoning header range should exist");
    assert_eq!((start, end), (4, 5), "header sits below the response");

    // `find_turn_at_row` maps screen rows linearly only when the history
    // fills the viewport, so size the viewport to the content and click the
    // header's first row, which corresponds to content line `start`.
    let total = app.active_display().unwrap().total_history_height();
    app.history_viewport.height = total as u16;
    let row = start as u16;

    handle_terminal_event(
        Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 5,
            row,
            modifiers: KeyModifiers::NONE,
        }),
        &mut app,
        &tx,
    )
    .expect("handle click");

    // Default was collapsed (response present) → the click expands it.
    assert_eq!(
        app.active_display().unwrap().reasoning_override.get(&1),
        Some(&true),
        "clicking the header should expand reasoning"
    );
}

#[test]
fn click_on_reasoning_header_toggles_collapse_when_content_fits_viewport() {
    // Regression: on sessions whose history is shorter than the viewport (no
    // scrollbar), the content is anchored to the bottom of the viewport.
    // Clicking the reasoning header must resolve to the right content line
    // despite the blank band above it.
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 20;

    let turn = Turn {
        created_at: choreo_proto::TimestampMs::now(),
        undone: false,
        error: None,
        user_text: None,
        assistant_text: Some("Response text.".into()),
        assistant_reasoning: Some("Hidden thinking.".into()),
        tool_calls: vec![],
        token_usage: None,
        tool_results: vec![],
        displayed_images: vec![],
        reasoning_artifact: None,
        reasoning_producer: None,
    };
    app.active_display()
        .unwrap()
        .view
        .insert_or_replace(1, turn);
    app.rebuild_height_prefix();

    let (start, total) = {
        let display = app.active_display().unwrap();
        let (start, _end) = display.turn_layouts[0]
            .reasoning_header_range
            .expect("reasoning header range should exist");
        (start, display.total_history_height())
    };
    assert!(
        total < app.history_viewport.height as usize,
        "test requires a session too short to need the scrollbar"
    );

    // The header renders at the bottom-anchored position.
    let row = (app.history_viewport.height as usize - total + start) as u16;
    handle_terminal_event(
        Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 5,
            row,
            modifiers: KeyModifiers::NONE,
        }),
        &mut app,
        &tx,
    )
    .expect("handle click");

    assert_eq!(
        app.active_display().unwrap().reasoning_override.get(&1),
        Some(&true),
        "clicking the header on a short session should expand reasoning"
    );
}

#[test]
fn click_on_tool_result_header_toggles_collapse() {
    // Clicking a tool result's header row (triangle + description) toggles
    // that result's collapsible body.  A quiet tool (read_file) defaults to
    // collapsed, so the first click expands it and the second collapses it.
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 20;

    let turn = Turn {
        created_at: choreo_proto::TimestampMs::now(),
        undone: false,
        error: None,
        user_text: None,
        assistant_text: None,
        assistant_reasoning: None,
        tool_calls: vec![],
        token_usage: None,
        tool_results: vec![choreo_proto::ToolResultRecord {
            call_id: "call-1".into(),
            name: "read_file".into(),
            content: "file contents".into(),
            is_error: false,
            invocation_description: "Reading file `src/main.rs`.".into(),
            image: None,
        }],
        displayed_images: vec![],
        reasoning_artifact: None,
        reasoning_producer: None,
    };
    app.active_display()
        .unwrap()
        .view
        .insert_or_replace(1, turn);
    app.rebuild_height_prefix();

    // The collapsed quiet result renders a single header line, so its click
    // range is content lines [0, 1).
    let (start, end) = app.active_display().unwrap().turn_layouts[0]
        .tool_result_header_ranges
        .first()
        .copied()
        .expect("quiet tool result must have a header range");
    assert_eq!((start, end), (0, 1));

    // Size the viewport to the content so screen row == content line.
    let total = app.active_display().unwrap().total_history_height();
    app.history_viewport.height = total as u16;
    let row = start as u16;

    // First click: expands the collapsed quiet result.
    handle_terminal_event(
        Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 3,
            row,
            modifiers: KeyModifiers::NONE,
        }),
        &mut app,
        &tx,
    )
    .expect("handle click");
    assert_eq!(
        app.active_display()
            .unwrap()
            .tool_collapse_override
            .get(&1)
            .and_then(|m| m.get("call-1")),
        Some(&false),
        "clicking the header of a collapsed quiet result should expand it"
    );

    // Second click: collapses again.
    handle_terminal_event(
        Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 3,
            row,
            modifiers: KeyModifiers::NONE,
        }),
        &mut app,
        &tx,
    )
    .expect("handle click");
    assert_eq!(
        app.active_display()
            .unwrap()
            .tool_collapse_override
            .get(&1)
            .and_then(|m| m.get("call-1")),
        Some(&true),
        "clicking the header again should collapse the result"
    );
}

#[test]
fn click_on_reasoning_header_toggles_collapse_when_scrolled() {
    // Regression: on sessions with a scrollbar, once the user scrolls away
    // from the bottom the click mapping must account for the scroll offset
    // (content line `c` sits at screen row `vh - total + scroll + c`).  A
    // naive "content starts at row 0" mapping broke header clicks here.
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 10;

    // Several full turns so the history overflows the viewport.
    for i in 0..5 {
        let turn = Turn {
            created_at: choreo_proto::TimestampMs::now(),
            undone: false,
            error: None,
            user_text: Some(format!("user {i}")),
            assistant_text: Some(format!("assistant response {i}")),
            assistant_reasoning: None,
            tool_calls: vec![],
            token_usage: None,
            tool_results: vec![],
            displayed_images: vec![],
            reasoning_artifact: None,
            reasoning_producer: None,
        };
        app.active_display()
            .unwrap()
            .view
            .insert_or_replace(i as u32, turn);
    }
    // The reasoning-bearing turn to click on.
    let target = Turn {
        created_at: choreo_proto::TimestampMs::now(),
        undone: false,
        error: None,
        user_text: None,
        assistant_text: Some("Response text.".into()),
        assistant_reasoning: Some("Hidden thinking.".into()),
        tool_calls: vec![],
        token_usage: None,
        tool_results: vec![],
        displayed_images: vec![],
        reasoning_artifact: None,
        reasoning_producer: None,
    };
    app.active_display()
        .unwrap()
        .view
        .insert_or_replace(99, target);
    app.rebuild_height_prefix();

    let total = app.active_display().unwrap().total_history_height();
    let vh = app.history_viewport.height as usize;
    assert!(total > vh, "test requires a session with a scrollbar");

    // Find the target turn's reasoning header range, then scroll so the
    // header is visible near the bottom of the viewport.
    let target_idx = {
        let display = app.active_display().unwrap();
        display
            .visible_turn_ids
            .iter()
            .position(|&id| id == 99)
            .expect("target turn should be visible")
    };
    let header_start = {
        let display = app.active_display().unwrap();
        let (start, _end) = display.turn_layouts[target_idx]
            .reasoning_header_range
            .expect("reasoning header range should exist");
        start
    };
    // Absolute content line of the header (turn start + in-turn offset).
    let header_content_line = display_content_line(&app, target_idx) + header_start;

    // Scroll so the header lands at row `vh - 2` (near the bottom, on
    // screen).  Content line `c` sits at screen row `c - (total - scroll -
    // vh)`, so solve for scroll and clamp into [0, max_scroll].
    let max_scroll = total - vh;
    let target_row = (vh - 2) as isize;
    let scroll = (target_row + total as isize - vh as isize - header_content_line as isize)
        .clamp(0, max_scroll as isize) as usize;
    app.scroll_to(scroll);

    // Sanity: the header must be visible at the expected row, mapped to its
    // in-turn offset.
    let header_row =
        (vh as isize - total as isize + scroll as isize + header_content_line as isize) as u16;
    let (_idx, offset) = find_turn_at_row(&app, header_row).expect("row must map");
    assert_eq!(
        offset, header_start,
        "header content line must be at the expected row"
    );

    handle_terminal_event(
        Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 5,
            row: header_row,
            modifiers: KeyModifiers::NONE,
        }),
        &mut app,
        &tx,
    )
    .expect("handle click");

    assert_eq!(
        app.active_display().unwrap().reasoning_override.get(&99),
        Some(&true),
        "clicking the header on a scrolled session should expand reasoning"
    );
}

/// Content line where the turn at `turn_idx` starts (0 for the first turn).
fn display_content_line(app: &App, turn_idx: usize) -> usize {
    app.active_display_ref()
        .and_then(|d| {
            turn_idx
                .checked_sub(1)
                .and_then(|prev| d.height_prefix.get(prev))
        })
        .copied()
        .unwrap_or(0)
}

#[test]
fn scroll_mouse_outside_history_box_does_not_update_accumulator() {
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = test_app();
    app.history_viewport.height = 1;

    let (_, height) = crossterm::terminal::size().expect("terminal size");
    let row = height.saturating_sub(1); // input area, outside history box

    handle_terminal_event(
        Event::Mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 0,
            row,
            modifiers: KeyModifiers::NONE,
        }),
        &mut app,
        &tx,
    )
    .expect("handle mouse");

    assert_eq!(
        app.scroll_accumulator, 0,
        "accumulator must remain unchanged"
    );
}

// ── Mouse click positions the input cursor ───────────────────

/// Send a left-click at terminal (column, row) through the event pipeline.
fn click_input(app: &mut App, tx: &std::sync::mpsc::Sender<ClientMessage>, column: u16, row: u16) {
    handle_terminal_event(
        Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }),
        app,
        tx,
    )
    .expect("handle mouse click");
}

#[test]
fn mouse_click_in_input_box_positions_cursor() {
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = test_app();
    app.last_terminal_size = Some((80, 24));
    app.input.text = "hello".to_string();
    app.input.cursor = 0;
    app.update_viewport_from_terminal_size();

    // 80x24, no status/error, help shown: the input box occupies rows 20..23
    // with content on row 21, and content starts at column INPUT_PAD (2).
    // Column 4 is therefore content column 2 → cursor between 'h' and 'e'.
    click_input(&mut app, &tx, 4, 21);
    assert_eq!(app.input.cursor, 2);
}

#[test]
fn mouse_click_in_input_box_left_padding_clamps_to_start() {
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = test_app();
    app.last_terminal_size = Some((80, 24));
    app.input.text = "hello".to_string();
    app.input.cursor = 4;
    app.update_viewport_from_terminal_size();

    // Column 1 is inside the left padding (before INPUT_PAD=2).
    click_input(&mut app, &tx, 1, 21);
    assert_eq!(app.input.cursor, 0);
}

#[test]
fn mouse_click_in_input_box_past_line_end_clamps_to_end() {
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = test_app();
    app.last_terminal_size = Some((80, 24));
    app.input.text = "hello".to_string();
    app.input.cursor = 0;
    app.update_viewport_from_terminal_size();

    // Column 79 is well past the text's right edge → cursor at end.
    click_input(&mut app, &tx, 79, 21);
    assert_eq!(app.input.cursor, 5);
}

#[test]
fn mouse_click_on_input_box_border_does_not_move_cursor() {
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = test_app();
    app.last_terminal_size = Some((80, 24));
    app.input.text = "hello".to_string();
    app.input.cursor = 1;
    app.update_viewport_from_terminal_size();

    // Row 20 is the top border, row 22 the bottom border — neither should
    // reposition the cursor.
    click_input(&mut app, &tx, 4, 20);
    assert_eq!(app.input.cursor, 1);
    click_input(&mut app, &tx, 4, 22);
    assert_eq!(app.input.cursor, 1);
}

#[test]
fn mouse_click_in_input_box_second_line_of_multiline_text() {
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = test_app();
    app.last_terminal_size = Some((80, 24));
    app.input.text = "abc\ndef".to_string();
    app.input.cursor = 0;
    app.update_viewport_from_terminal_size();

    // Two content lines → box occupies rows 19..22 (content rows 20, 21).
    click_input(&mut app, &tx, 3, 20); // content row 0, col 1 → 'b' in "abc"
    assert_eq!(app.input.cursor, 1);
    click_input(&mut app, &tx, 3, 21); // content row 1, col 1 → 'e' in "def"
    assert_eq!(app.input.cursor, 5);
}

#[test]
fn mouse_click_in_input_box_wrapped_line() {
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = test_app();
    app.last_terminal_size = Some((80, 24));
    // Two words totalling 101 display columns: at inner width 76 this wraps
    // as line 0 = 76 'a's (bytes 0..76), line 1 = 24 'b's (bytes 77..101).
    app.input.text = "a".repeat(76) + " " + &"b".repeat(24);
    app.input.cursor = 0;
    app.update_viewport_from_terminal_size();

    // Two content lines → box occupies rows 19..22 (content rows 20, 21).
    click_input(&mut app, &tx, 30, 20); // visual line 0, content col 28
    assert_eq!(app.input.cursor, 28);
    click_input(&mut app, &tx, 10, 21); // visual line 1, content col 8
    assert_eq!(app.input.cursor, 77 + 8);
}

// ── InputBuffer scroll_offset ──────────────────────────────────

#[test]
fn input_bar_height_uses_renderer_inner_width() {
    // Regression test for the prompt-entry wrap bug: the height estimation
    // used `term_width - 2` while the renderer wraps text at `term_width - 4`
    // (INPUT_PAD padding on each side, no side borders).  A word that wrapped
    // at the drawing width therefore did not grow the input box — the wrapped
    // text vanished off line 1 and the second line only appeared after two
    // more characters had pushed the text past the wider height-calc width.
    let mut app = test_app();
    app.last_terminal_size = Some((80, 24));

    // At an 80-wide terminal the renderer draws the input at 76 columns.
    assert_eq!(input_inner_width(80), 76);

    // A word that exactly fills 76 columns, then a space and another word:
    // this wraps onto a second line at width 76 but still fits on a single
    // line at width 78 (the old height-calc width).
    let text = "a".repeat(76) + " b";
    assert_eq!(compute_visual_lines(&text, 76).len(), 2);
    assert_eq!(compute_visual_lines(&text, 78).len(), 1);

    // The box height must reflect the renderer's 2 wrapped lines immediately.
    // (Bump generation — every real text mutation goes through methods that do.)
    app.input.text = text;
    app.input.generation += 1;
    assert_eq!(app.input_bar_content_lines(80), 2);
    assert_eq!(app.input_bar_height(80), 4);

    // Short text that fits comfortably still yields a single content line.
    app.input.text = "hello world".to_string();
    app.input.generation += 1;
    assert_eq!(app.input_bar_content_lines(80), 1);
}

#[test]
fn input_box_rect_sits_directly_above_status_bar() {
    let mut app = test_app();
    app.last_terminal_size = Some((80, 24));
    app.input.text = "hello".to_string();
    app.input.generation += 1;

    let r = app.input_box_rect(80, 24);
    assert_eq!(r.x, 0);
    assert_eq!(r.width, 80);
    assert_eq!(r.height, 3); // 1 content line + 2 borders
    assert_eq!(
        r.y,
        24 - r.height - 1,
        "box sits directly above the status bar"
    );
}

#[test]
fn input_box_rect_unaffected_by_status_and_help_rows() {
    // The input box is anchored to the status bar at the bottom of the
    // screen; a status message or the help overlay above it must not shift
    // its position (only the history area shrinks).
    let mut app = test_app();
    app.last_terminal_size = Some((80, 30));
    app.input.text = "hello".to_string();
    app.input.generation += 1;
    app.status = Some("some transient status".to_string());

    let r = app.input_box_rect(80, 30);
    assert_eq!(r.height, 3);
    assert_eq!(r.y, 30 - r.height - 1);
    assert!(app.status_error_height(80) > 0, "status must be visible");
}

#[test]
fn input_box_rect_matches_renderer_layout() {
    // Replicate render_chat's layout and assert the input chunk it produces
    // equals input_box_rect, so mouse hit-testing and rendering can never
    // drift apart.
    let mut app = test_app();
    app.last_terminal_size = Some((80, 30));
    app.input.text = "a".repeat(76) + " b"; // wraps to 2 lines at width 76
    app.input.generation += 1;
    app.update_viewport_from_terminal_size();

    let status_error_height = app.status_error_height(80);
    let help_height = if app.show_ctrl_help { 2u16 } else { 0u16 };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),
            Constraint::Length(status_error_height),
            Constraint::Length(help_height),
            Constraint::Length(app.input_bar_height(80)),
            Constraint::Length(STATUS_BAR_HEIGHT),
        ])
        .split(Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 30,
        });

    assert_eq!(app.input_box_rect(80, 30), chunks[3]);
}

#[test]
fn input_box_rect_matches_renderer_layout_in_overflow() {
    // A terminal too small for the fixed chrome (help + input + status bar)
    // to fit: the layout solver shrinks the input box instead of placing it
    // at a fixed distance above the status bar.  Replicate render_chat's
    // layout verbatim and assert the hit-test rect agrees with the chunk the
    // renderer draws — the old bottom-anchored formula would report y=0 h=12.
    let mut app = test_app();
    app.last_terminal_size = Some((80, 10));
    // 12 logical lines → input_bar_height clamps to 12 (MAX_INPUT_CONTENT_LINES
    // + 2 borders), pushing the fixed chrome past the 10-row terminal.
    app.input.text = "line\n".repeat(12);
    app.input.generation += 1;
    app.update_viewport_from_terminal_size();

    let status_error_height = app.status_error_height(80);
    let help_height = if app.show_ctrl_help { 2u16 } else { 0u16 };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),
            Constraint::Length(status_error_height),
            Constraint::Length(help_height),
            Constraint::Length(app.input_bar_height(80)),
            Constraint::Length(STATUS_BAR_HEIGHT),
        ])
        .split(Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 10,
        });

    assert_eq!(app.input_box_rect(80, 10), chunks[3]);
}

#[test]
fn ensure_cursor_visible_short_text_keeps_offset_at_zero() {
    let mut buf = InputBuffer::new();
    buf.insert_str_at_cursor("short text");
    // 1 visual line, visible_height=10 → no scrolling needed
    buf.ensure_cursor_visible(40, 10);
    assert_eq!(buf.scroll_offset, 0);
}

#[test]
fn ensure_cursor_visible_clears_offset_when_text_fits() {
    let mut buf = InputBuffer::new();
    // Build 5 visual lines of text, visible_height=10 → all fit
    for i in 0..5 {
        buf.insert_str_at_cursor(&format!("line {i}\n"));
    }
    buf.scroll_offset = 3; // artificially set to an invalid position
    buf.ensure_cursor_visible(40, 10);
    assert_eq!(buf.scroll_offset, 0);
}

#[test]
fn ensure_cursor_visible_scrolls_to_cursor_at_end() {
    let mut buf = InputBuffer::new();
    // 15 lines of text (no trailing newline → exactly 15 visual lines)
    for i in 0..14 {
        buf.insert_str_at_cursor(&format!("line {i}\n"));
    }
    buf.insert_str_at_cursor("line 14");
    // Cursor is at the end (last visual line = 14)
    buf.ensure_cursor_visible(40, 10);
    // Should scroll so the last line is visible: offset = 15 - 10 = 5
    assert_eq!(buf.scroll_offset, 5);
}

#[test]
fn ensure_cursor_visible_keeps_offset_at_zero_when_cursor_at_start() {
    let mut buf = InputBuffer::new();
    for i in 0..15 {
        buf.insert_str_at_cursor(&format!("line {i}\n"));
    }
    // Move cursor to the beginning
    buf.cursor = 0;
    buf.ensure_cursor_visible(40, 10);
    // Cursor is on visual line 0, should see from the start
    assert_eq!(buf.scroll_offset, 0);
}

#[test]
fn ensure_cursor_visible_scrolls_up_when_cursor_moves_above_window() {
    let mut buf = InputBuffer::new();
    // 15 lines (no trailing newline → exactly 15 visual lines)
    for i in 0..14 {
        buf.insert_str_at_cursor(&format!("line {i}\n"));
    }
    buf.insert_str_at_cursor("line 14");

    // Start with scroll_offset = 5 (showing lines 5-14)
    buf.scroll_offset = 5;
    buf.ensure_cursor_visible(40, 10);
    // Cursor is at the end (line 14), still visible at offset 5
    // (lines 5-14 contain line 14)
    assert_eq!(buf.scroll_offset, 5);

    // Move cursor to line 2 (visual row 2, which is above the window)
    // "line 2\n" starts at byte 14 (after "line 0\n" at 0-6, "line 1\n" at 7-13)
    buf.cursor = 14;
    buf.ensure_cursor_visible(40, 10);
    // Should scroll up to show line 2
    assert_eq!(buf.scroll_offset, 2);
}

#[test]
fn scroll_offset_resets_on_clear() {
    let mut buf = InputBuffer::new();
    for i in 0..15 {
        buf.insert_str_at_cursor(&format!("line {i}\n"));
    }
    buf.ensure_cursor_visible(40, 10);
    assert!(buf.scroll_offset > 0);

    buf.clear();
    assert_eq!(buf.scroll_offset, 0);
    assert_eq!(buf.text, "");
    assert_eq!(buf.cursor, 0);
}

#[test]
fn ensure_cursor_visible_cursor_on_partial_line() {
    let mut buf = InputBuffer::new();
    // 15 lines plus trailing content on the last line
    for i in 0..14 {
        buf.insert_str_at_cursor(&format!("line {i}\n"));
    }
    buf.insert_str_at_cursor("extra at end");
    // 15 visual lines (14 numbered + "extra at end"), cursor at end
    buf.ensure_cursor_visible(40, 10);
    assert_eq!(buf.scroll_offset, 5);
}

#[test]
fn ensure_cursor_visible_handles_visible_height_one() {
    let mut buf = InputBuffer::new();
    // 5 lines (no trailing newline → exactly 5 visual lines)
    for i in 0..4 {
        buf.insert_str_at_cursor(&format!("line {i}\n"));
    }
    buf.insert_str_at_cursor("line 4");
    // visible_height = 1: only one line visible at a time
    buf.ensure_cursor_visible(40, 1);
    // Cursor at end (visual line 4), max offset = 5 - 1 = 4
    assert_eq!(buf.scroll_offset, 4);
}

#[test]
fn scroll_offset_clamped_to_valid_range() {
    let mut buf = InputBuffer::new();
    // 5 lines (no trailing newline → exactly 5 visual lines)
    for i in 0..4 {
        buf.insert_str_at_cursor(&format!("line {i}\n"));
    }
    buf.insert_str_at_cursor("line 4");
    // Artificially set scroll_offset way out of range
    buf.scroll_offset = 100;
    buf.ensure_cursor_visible(40, 3);
    // 5 visual lines, visible_height=3, max offset = 5-3 = 2
    // Cursor at end (visual line 4), scroll = 4+1-3 = 2
    assert_eq!(buf.scroll_offset, 2);
}

// ── Kitty keyboard protocol shift normalisation (end-to-end) ──
//
// With REPORT_ALL_KEYS_AS_ESCAPE_CODES enabled, kitty-protocol terminals
// report shifted text keys as unshifted codepoints + SHIFT modifier.  The
// normalisation in handle_terminal_event must restore legacy-equivalent
// behaviour everywhere.

#[test]
fn ctrl_shift_m_opens_selector_like_ctrl_m() {
    let (tx, rx) = std::sync::mpsc::channel();
    let mut app = test_app();

    // Ctrl+Shift+M arrives as Char('m') + CONTROL + SHIFT under the kitty
    // protocol; legacy sent the same byte as Ctrl+M, so both must open the
    // selector.
    handle_terminal_event(
        Event::Key(KeyEvent::new(
            KeyCode::Char('m'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        )),
        &mut app,
        &tx,
    )
    .expect("handle ctrl+shift+m");

    assert!(
        app.model_selector.is_open(),
        "ctrl+shift+m opens the selector"
    );
    let msg = rx.recv().expect("sent message");
    assert_eq!(msg, ClientMessage::ListModels);
}

#[test]
fn shift_letter_inserts_uppercase_into_chat_input() {
    let (tx, _rx) = std::sync::mpsc::channel();
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
    let (tx, _rx) = std::sync::mpsc::channel();
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
    let (tx, _rx) = std::sync::mpsc::channel();
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
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = test_app();
    app.model_selector.open();
    app.model_selector
        .apply_models(vec!["GPT-4O".to_string()], None);

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
    let (tx, _rx) = std::sync::mpsc::channel();
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
