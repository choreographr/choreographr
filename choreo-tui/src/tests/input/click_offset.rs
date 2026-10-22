use crate::state::*;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

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
