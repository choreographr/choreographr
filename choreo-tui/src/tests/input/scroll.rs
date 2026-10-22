use crate::state::*;
use crate::tests::support::test_app;
use ratatui::layout::{Constraint, Direction, Layout, Rect};

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
    app.set_status("some transient status".to_string());

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
    let help_height = if app.show_help_overlay { 2u16 } else { 0u16 };
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
    let help_height = if app.show_help_overlay { 2u16 } else { 0u16 };
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
