use crate::connection::handle_terminal_event;
use crate::state::*;
use crate::tests::support::{add_user_text, test_app};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

// ── readline / emacs editing chords ──────────────────────────────
//
// The editing kernel honours the shell's readline movement and kill keys:
// `Ctrl+A/E` (line start/end), `Ctrl+B/F` (char), `Ctrl+D` (delete-char),
// `Ctrl+H` (backspace), `Ctrl+K` (kill-line), `Ctrl+T` (transpose), and the
// Alt (Meta) word chords `Alt+B/F/D` / `Alt+Backspace`.

#[test]
fn handle_key_ctrl_a_and_e_move_to_line_edges() {
    let mut buf = InputBuffer::new();
    buf.text = "abc\ndef".to_string();
    buf.cursor = 6; // inside "def"
    buf.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
    assert_eq!(
        buf.cursor, 4,
        "Ctrl+A goes to the start of the logical line"
    );
    buf.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL));
    assert_eq!(buf.cursor, 7, "Ctrl+E goes to the end of the logical line");
}

#[test]
fn handle_key_ctrl_b_and_f_move_by_char() {
    let mut buf = InputBuffer::new();
    buf.text = "abc".to_string();
    buf.cursor = 1;
    buf.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL));
    assert_eq!(buf.cursor, 2);
    buf.handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
    assert_eq!(buf.cursor, 1);
}

#[test]
fn handle_key_ctrl_d_deletes_char_at_cursor() {
    let mut buf = InputBuffer::new();
    buf.text = "abc".to_string();
    buf.cursor = 1;
    buf.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL));
    assert_eq!(buf.text, "ac");
}

#[test]
fn handle_key_ctrl_h_deletes_char_backward() {
    // On a kitty terminal Ctrl+H arrives as Char('h')+CONTROL (legacy sends
    // Backspace); both must delete the character before the cursor.
    let mut buf = InputBuffer::new();
    buf.text = "abc".to_string();
    buf.cursor = 3;
    buf.handle_key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::CONTROL));
    assert_eq!(buf.text, "ab");
    assert_eq!(buf.cursor, 2);
}

#[test]
fn handle_key_ctrl_k_kills_to_end_of_logical_line() {
    let mut buf = InputBuffer::new();
    buf.text = "abc\ndef".to_string();
    buf.cursor = 1;
    buf.handle_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL));
    assert_eq!(
        buf.text, "a\ndef",
        "Ctrl+K kills to the newline, not the end"
    );
    assert_eq!(buf.cursor, 1);

    // At end-of-line the newline itself is never consumed, so it is a no-op.
    let mut buf = InputBuffer::new();
    buf.text = "abc\ndef".to_string();
    buf.cursor = 3;
    let before = buf.generation;
    buf.handle_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL));
    assert_eq!(buf.text, "abc\ndef");
    assert_eq!(
        buf.generation, before,
        "a no-op kill must not bump generation"
    );
}

#[test]
fn handle_key_ctrl_t_transposes_chars() {
    let mut buf = InputBuffer::new();
    buf.text = "abc".to_string();
    buf.cursor = 1;
    buf.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL));
    assert_eq!(buf.text, "bac");
    assert_eq!(buf.cursor, 2, "cursor lands after the swapped pair");

    // At the end of the buffer the two preceding chars swap instead.
    let mut buf = InputBuffer::new();
    buf.text = "ab".to_string();
    buf.cursor = 2;
    buf.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL));
    assert_eq!(buf.text, "ba");
    assert_eq!(buf.cursor, 2);
}

#[test]
fn handle_key_alt_b_and_f_move_by_word() {
    let mut buf = InputBuffer::new();
    buf.text = "foo bar".to_string();
    buf.cursor = 7;
    buf.handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::ALT));
    assert_eq!(
        buf.cursor, 4,
        "Alt+B moves to the start of the previous word"
    );
    buf.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::ALT));
    assert_eq!(buf.cursor, 7, "Alt+F moves past the next word");
}

#[test]
fn handle_key_alt_d_kills_word_forward() {
    let mut buf = InputBuffer::new();
    buf.text = "foo bar".to_string();
    buf.cursor = 0;
    buf.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::ALT));
    assert_eq!(buf.text, "bar");
}

#[test]
fn handle_key_alt_backspace_kills_word_backward() {
    let mut buf = InputBuffer::new();
    buf.text = "foo bar".to_string();
    buf.cursor = 7;
    buf.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::ALT));
    assert_eq!(buf.text, "foo ");
    assert_eq!(buf.cursor, 4);
}

#[test]
fn readline_cursor_moves_do_not_bump_generation() {
    // A pure cursor move must leave `generation` untouched, so a recalled
    // history entry stays attached (see `App::edit_input`); a kill does not.
    let mut buf = InputBuffer::new();
    buf.text = "abc def".to_string();
    buf.cursor = 7;
    let before = buf.generation;
    buf.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
    buf.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL));
    buf.handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::ALT));
    assert_eq!(buf.generation, before, "cursor moves are not edits");
    buf.handle_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL));
    assert_ne!(buf.generation, before, "a kill is an edit");
}

/// The source slice a `VisualLineInfo` describes.
fn vl_text<'a>(vl: &VisualLineInfo, source: &'a str) -> &'a str {
    // Visual-line byte offsets are char boundaries within `source` (produced
    // by `compute_visual_lines`); `.get()` keeps the slice total.
    source.get(vl.start_byte..vl.end_byte).unwrap_or("")
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
    let id = 0;
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
    // Navigation should set cursor to end of text
    app.navigate_history_up();
    assert_eq!(app.input.text, "multi\nline\ntext");
    assert_eq!(app.input.cursor, 15);
}

#[test]
fn navigate_history_up_adjusts_scroll_offset_for_long_entry() {
    let mut app = test_app();
    app.last_terminal_size = Some((80, 24));
    // Recall requires an empty draft, so the input starts empty (its default).

    // Insert a long multi-line history entry (20 visual lines at 80-wide terminal)
    // Vec-then-concat instead of format!.collect::<String>() (clippy::format_collect)
    let long_text: String = (0..20)
        .map(|i| format!("line {i}\n"))
        .collect::<Vec<_>>()
        .concat();
    let id = 0;
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
    let (tx, _rx) = crossbeam_channel::unbounded();

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
    let (tx, _rx) = crossbeam_channel::unbounded();

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
    let (tx, _rx) = crossbeam_channel::unbounded();

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
    let (tx, _rx) = crossbeam_channel::unbounded();

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
    let (tx, _rx) = crossbeam_channel::unbounded();

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
    let (tx, _rx) = crossbeam_channel::unbounded();

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
    let (tx, _rx) = crossbeam_channel::unbounded();

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
    let (tx, _rx) = crossbeam_channel::unbounded();

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
    let (tx, _rx) = crossbeam_channel::unbounded();

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
    let (tx, _rx) = crossbeam_channel::unbounded();

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
    let (tx, _rx) = crossbeam_channel::unbounded();

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
