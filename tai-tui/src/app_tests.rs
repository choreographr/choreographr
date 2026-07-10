use crate::connection::handle_terminal_event;
use crate::db;
use crate::markdown_render::*;
use crate::state::*;
use crate::test_util::test_app;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use ratatui::text::Line;
use tai_client_core::DaemonMessageHandler;
use tai_proto::{ClientMessage, OutputStream};

#[test]
fn app_push_text_trims_history_to_limit() {
    let mut app = test_app("/tmp/tai.sock", "Halfblocks");
    for index in 0..600 {
        app.push_text(format!("line {index}"));
    }
    assert_eq!(app.client.history.len(), 500);
    match &app.client.history[0] {
        HistoryItem::Text(text) => assert!(text.contains("line 100")),
        HistoryItem::SessionMessage(_)
        | HistoryItem::Streaming(_)
        | HistoryItem::Image(_)
        | HistoryItem::Diff(_) => {
            panic!("expected text history item")
        }
    }
}

#[test]
fn drop_request_removes_active_request() {
    let mut app = test_app("/tmp/tai.sock", "Kitty");
    app.active.insert(42);
    app.begin_stream(42);
    app.drop_request(42);
    assert!(!app.active.contains(&42));
    assert!(!app.client.in_progress.contains_key(&42));
}

#[test]
fn append_stream_text_updates_mutable_history_entry() {
    let mut app = test_app("/tmp/tai.sock", "Kitty");
    app.begin_stream(7);
    app.append_stream_text(7, OutputStream::Reasoning, "thinking");
    app.append_stream_text(7, OutputStream::Answer, "hello");
    app.append_stream_text(7, OutputStream::Answer, " world");

    let index = app.client.in_progress[&7];
    match &app.client.history[index] {
        HistoryItem::Streaming(text) => {
            assert_eq!(text.request_id, 7);
            assert_eq!(text.reasoning, "thinking");
            assert_eq!(text.answer, "hello world");
        }
        _ => panic!("expected streaming text item"),
    }
}

#[test]
fn append_stream_text_preserves_manual_scroll_position() {
    let mut app = test_app("/tmp/tai.sock", "Kitty");
    app.history_viewport.width = 10;
    app.history_viewport.height = 1;
    app.push_text("older");
    app.push_text("older still");
    app.begin_stream(7);
    app.scroll_up(3);

    app.append_stream_text(7, OutputStream::Answer, "hello");

    tracing::info!(
        "A1 scroll={} comp={} eff={} fol={}",
        app.history_scroll.scroll(),
        app.history_scroll.scroll_compensation(),
        app.effective_scroll(),
        app.history_scroll.follow_output()
    );
    assert_eq!(app.history_scroll.scroll(), 3);
    assert_eq!(app.history_scroll.scroll_compensation(), 2);
    assert_eq!(app.effective_scroll(), 5);
    assert!(!app.history_scroll.follow_output());
}

#[test]
fn append_stream_text_keeps_following_when_at_bottom() {
    let mut app = test_app("/tmp/tai.sock", "Kitty");
    app.begin_stream(7);

    app.append_stream_text(7, OutputStream::Answer, "hello");

    assert_eq!(app.history_scroll.scroll(), 0);
    assert_eq!(app.history_scroll.scroll_compensation(), 0);
    assert!(app.history_scroll.follow_output());
}

#[test]
fn history_text_height_accounts_for_wrapping_and_blank_lines() {
    assert_eq!(history_text_height("hello", 10), 1);
    assert_eq!(history_text_height("hello world", 5), 3);
    assert_eq!(history_text_height("a\nb\n", 10), 3);
    assert_eq!(history_text_height("", 10), 1);
    assert_eq!(history_text_height("\n", 10), 2);
}

#[test]
fn display_width_treats_emoji_as_terminal_cells() {
    assert_eq!(display_width("😀"), 2);
    assert_eq!(display_width("A😀B"), 4);
    assert_eq!(display_width("👨‍👩‍👧‍👦"), 2);
}

#[test]
fn wrapped_line_height_uses_terminal_display_width() {
    assert_eq!(lines_height(&[Line::from("😀😀")], 2), 2);
    assert_eq!(lines_height(&[Line::from("👨‍👩‍👧‍👦")], 2), 1);
}

#[test]
fn streaming_text_lines_include_reasoning_and_answer() {
    let lines = streaming_text_lines(
        &StreamingTextItem {
            request_id: 9,
            reasoning: "step by step".to_string(),
            answer: "final".to_string(),
        },
        80,
    );

    assert_eq!(lines[0].to_string(), "[9]");
    assert_eq!(lines[2].to_string(), "reasoning: step by step");
    assert_eq!(lines[4].to_string(), "answer: final");
}

#[test]
fn streaming_text_lines_preserve_newlines() {
    let lines = streaming_text_lines(
        &StreamingTextItem {
            request_id: 3,
            reasoning: "line one\nline two".to_string(),
            answer: "final one\nfinal two".to_string(),
        },
        80,
    );

    assert_eq!(lines[0].to_string(), "[3]");
    assert_eq!(lines[2].to_string(), "reasoning: line one");
    assert_eq!(lines[3].to_string(), "line two");
    assert_eq!(lines[5].to_string(), "answer: final one");
    assert_eq!(lines[6].to_string(), "final two");
}

#[test]
fn markdown_lines_render_tables() {
    let lines = markdown_lines(
        "| Name | Role | Years |\n|:--|:--:|--:|\n| Ada Lovelace | Mathematician | 1842 |\n| Grace Hopper | Computer Scientist | 1943 |",
        60,
    );

    let rendered = lines
        .iter()
        .map(Line::to_string)
        .collect::<Vec<_>>()
        .join("\n");

    assert!(rendered.contains("┌"));
    assert!(rendered.contains("Ada Lovelace"));
    assert!(rendered.contains("Grace Hopper"));
    assert!(rendered.contains("Mathematician"));
}

#[test]
fn markdown_lines_render_lists_with_item_text() {
    let lines = markdown_lines("- one\n- [x] done\n1. first\n2. second", 80);

    let rendered = lines
        .iter()
        .map(Line::to_string)
        .collect::<Vec<_>>()
        .join("\n");

    assert!(rendered.contains("• one"));
    assert!(rendered.contains("• [x] done"));
    assert!(rendered.contains("1. first"));
    assert!(rendered.contains("2. second"));
}

#[test]
fn oversized_history_item_keeps_visible_tail() {
    let wrapped = history_text_height("123456789", 3);
    assert_eq!(wrapped, 3);

    let rows_remaining = 2;
    let rows_to_skip = 0;
    let bottom_line = wrapped.saturating_sub(rows_to_skip);
    let top_line = bottom_line.saturating_sub(rows_remaining);

    assert_eq!(top_line, 1);
}

#[test]
fn image_item_height_uses_protocol_size_for() {
    let picker = ratatui_image::picker::Picker::halfblocks();
    let svg = br#"<svg xmlns='http://www.w3.org/2000/svg' width='100' height='50'><rect width='100' height='50' fill='red'/></svg>"#;
    let image = tai_tui::build_rendered_image(
        &picker,
        tai_proto::ImageMetadata {
            image_id: 1,
            mime_type: "image/svg+xml".to_string(),
            width: 100,
            height: 50,
            byte_len: svg.len() as u64,
            alt: None,
        },
        svg.to_vec(),
    )
    .expect("svg should render");
    let item = crate::state::HistoryItem::Image(Box::new(image));

    let viewport = crate::state::HistoryViewport {
        width: 80,
        height: 24,
    };
    let height = viewport.item_height(&item);
    assert!(height >= 1, "image item height must be at least 1");
    assert!(
        height <= 12,
        "image should not exceed half the viewport height (24/2=12)"
    );
}

#[test]
fn terminal_event_appends_characters() {
    let mut app = test_app("/tmp/tai.sock", "Kitty");
    let (tx, rx) = std::sync::mpsc::channel();

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("handle key");
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("handle key");

    assert_eq!(app.input.text, "hi");
    assert_eq!(app.input.cursor, 2);
    assert!(rx.try_recv().is_err());
}

#[test]
fn terminal_event_submits_run_input() {
    let mut app = test_app("/tmp/tai.sock", "Kitty");
    app.input.text = "hello".to_string();
    let (tx, rx) = std::sync::mpsc::channel();

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("handle enter");

    assert!(app.input.is_empty());
    assert_eq!(app.input.cursor, 0);
    let message = rx.recv().expect("sent message");
    assert_eq!(
        message,
        ClientMessage::RunInput {
            request_id: 1,
            input: b"hello".to_vec(),
        }
    );
}

#[test]
fn terminal_event_esc_opens_home() {
    let (tx, _rx) = std::sync::mpsc::channel();

    let mut app = test_app("/tmp/tai.sock", "Kitty");
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("handle esc");

    assert_eq!(app.page, Page::Home);
    assert_eq!(app.previous_page, Page::Chat);
}

#[test]
fn terminal_event_ctrl_c_opens_settings() {
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = test_app("/tmp/tai.sock", "Kitty");

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        &mut app,
        &tx,
    )
    .expect("handle ctrl+c");

    assert_eq!(app.page, Page::Settings);
}

// ── Cursor & editing tests ────────────────────────────────────

#[test]
fn insert_char_at_cursor_appends_when_at_end() {
    let mut app = test_app("/tmp/tai.sock", "Kitty");
    app.input.insert_char_at_cursor('a');
    app.input.insert_char_at_cursor('b');
    app.input.insert_char_at_cursor('c');
    assert_eq!(app.input.text, "abc");
    assert_eq!(app.input.cursor, 3);
}

#[test]
fn insert_char_at_cursor_inserts_in_middle() {
    let mut app = test_app("/tmp/tai.sock", "Kitty");
    app.input.text = "abde".to_string();
    app.input.cursor = 2;
    app.input.insert_char_at_cursor('c');
    assert_eq!(app.input.text, "abcde");
    assert_eq!(app.input.cursor, 3);
}

#[test]
fn insert_char_at_cursor_works_at_start() {
    let mut app = test_app("/tmp/tai.sock", "Kitty");
    app.input.text = "bc".to_string();
    app.input.cursor = 0;
    app.input.insert_char_at_cursor('a');
    assert_eq!(app.input.text, "abc");
    assert_eq!(app.input.cursor, 1);
}

#[test]
fn cursor_left_moves_back_by_one_grapheme() {
    let mut app = test_app("/tmp/tai.sock", "Kitty");
    app.input.text = "abcd".to_string();
    app.input.cursor = 4;
    app.input.cursor_left();
    assert_eq!(app.input.cursor, 3);
    app.input.cursor_left();
    assert_eq!(app.input.cursor, 2);
}

#[test]
fn cursor_left_stops_at_start() {
    let mut app = test_app("/tmp/tai.sock", "Kitty");
    app.input.text = "a".to_string();
    app.input.cursor = 1;
    app.input.cursor_left();
    assert_eq!(app.input.cursor, 0);
    app.input.cursor_left();
    assert_eq!(app.input.cursor, 0);
}

#[test]
fn cursor_left_is_grapheme_aware() {
    let mut app = test_app("/tmp/tai.sock", "Kitty");
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
    let mut app = test_app("/tmp/tai.sock", "Kitty");
    app.input.text = "abcd".to_string();
    app.input.cursor = 0;
    app.input.cursor_right();
    assert_eq!(app.input.cursor, 1);
    app.input.cursor_right();
    assert_eq!(app.input.cursor, 2);
}

#[test]
fn cursor_right_stops_at_end() {
    let mut app = test_app("/tmp/tai.sock", "Kitty");
    app.input.text = "a".to_string();
    app.input.cursor = 0;
    app.input.cursor_right();
    assert_eq!(app.input.cursor, 1);
    app.input.cursor_right();
    assert_eq!(app.input.cursor, 1);
}

#[test]
fn cursor_right_is_grapheme_aware() {
    let mut app = test_app("/tmp/tai.sock", "Kitty");
    app.input.text = "a😀b".to_string();
    app.input.cursor = 0;
    app.input.cursor_right();
    assert_eq!(app.input.cursor, 1); // after 'a'
    app.input.cursor_right();
    assert_eq!(app.input.cursor, 5); // after 4-byte emoji
}

#[test]
fn cursor_home_moves_to_start() {
    let mut app = test_app("/tmp/tai.sock", "Kitty");
    app.input.text = "hello".to_string();
    app.input.cursor = 5;
    app.input.cursor_home();
    assert_eq!(app.input.cursor, 0);
}

#[test]
fn cursor_end_moves_to_end() {
    let mut app = test_app("/tmp/tai.sock", "Kitty");
    app.input.text = "hello".to_string();
    app.input.cursor = 0;
    app.input.cursor_end();
    assert_eq!(app.input.cursor, 5);
}

#[test]
fn backspace_at_cursor_removes_before_cursor() {
    let mut app = test_app("/tmp/tai.sock", "Kitty");
    app.input.text = "abcd".to_string();
    app.input.cursor = 3;
    app.input.backspace_at_cursor();
    assert_eq!(app.input.text, "abd");
    assert_eq!(app.input.cursor, 2);
}

#[test]
fn backspace_at_cursor_does_nothing_at_start() {
    let mut app = test_app("/tmp/tai.sock", "Kitty");
    app.input.text = "a".to_string();
    app.input.cursor = 0;
    app.input.backspace_at_cursor();
    assert_eq!(app.input.text, "a");
    assert_eq!(app.input.cursor, 0);
}

#[test]
fn backspace_at_cursor_is_grapheme_aware() {
    let mut app = test_app("/tmp/tai.sock", "Kitty");
    app.input.text = "a😀".to_string();
    app.input.cursor = 5;
    app.input.backspace_at_cursor();
    assert_eq!(app.input.text, "a");
    assert_eq!(app.input.cursor, 1);
}

#[test]
fn delete_at_cursor_removes_at_cursor() {
    let mut app = test_app("/tmp/tai.sock", "Kitty");
    app.input.text = "abcd".to_string();
    app.input.cursor = 1;
    app.input.delete_at_cursor();
    assert_eq!(app.input.text, "acd");
    assert_eq!(app.input.cursor, 1);
}

#[test]
fn delete_at_cursor_does_nothing_at_end() {
    let mut app = test_app("/tmp/tai.sock", "Kitty");
    app.input.text = "a".to_string();
    app.input.cursor = 1;
    app.input.delete_at_cursor();
    assert_eq!(app.input.text, "a");
    assert_eq!(app.input.cursor, 1);
}

#[test]
fn delete_at_cursor_is_grapheme_aware() {
    let mut app = test_app("/tmp/tai.sock", "Kitty");
    app.input.text = "a😀b".to_string();
    app.input.cursor = 1;
    app.input.delete_at_cursor();
    assert_eq!(app.input.text, "ab");
    assert_eq!(app.input.cursor, 1);
}

#[test]
fn word_left_moves_to_previous_word() {
    let mut app = test_app("/tmp/tai.sock", "Kitty");
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
    let mut app = test_app("/tmp/tai.sock", "Kitty");
    app.input.text = "hello".to_string();
    app.input.cursor = 0;
    app.input.word_left();
    assert_eq!(app.input.cursor, 0);
}

#[test]
fn word_right_moves_to_next_word() {
    let mut app = test_app("/tmp/tai.sock", "Kitty");
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
    let mut app = test_app("/tmp/tai.sock", "Kitty");
    app.input.text = "hello".to_string();
    app.input.cursor = 5;
    app.input.word_right();
    assert_eq!(app.input.cursor, 5);
}

#[test]
fn word_right_skips_whitespace() {
    let mut app = test_app("/tmp/tai.sock", "Kitty");
    app.input.text = "  hello  ".to_string();
    app.input.cursor = 0;
    app.input.word_right();
    assert_eq!(app.input.cursor, 2); // start of "hello", skipping leading spaces
}

#[test]
fn delete_word_backward_removes_previous_word() {
    let mut app = test_app("/tmp/tai.sock", "Kitty");
    app.input.text = "hello world".to_string();
    app.input.cursor = 11;
    app.input.delete_word_backward();
    assert_eq!(app.input.text, "hello ");
    assert_eq!(app.input.cursor, 6);
}

#[test]
fn delete_word_backward_at_start_does_nothing() {
    let mut app = test_app("/tmp/tai.sock", "Kitty");
    app.input.text = "hello".to_string();
    app.input.cursor = 0;
    app.input.delete_word_backward();
    assert_eq!(app.input.text, "hello");
    assert_eq!(app.input.cursor, 0);
}

#[test]
fn delete_word_forward_removes_next_word() {
    let mut app = test_app("/tmp/tai.sock", "Kitty");
    app.input.text = "hello world foo".to_string();
    app.input.cursor = 6;
    app.input.delete_word_forward();
    assert_eq!(app.input.text, "hello foo");
    assert_eq!(app.input.cursor, 6);
}

#[test]
fn delete_word_forward_at_end_does_nothing() {
    let mut app = test_app("/tmp/tai.sock", "Kitty");
    app.input.text = "hello".to_string();
    app.input.cursor = 5;
    app.input.delete_word_forward();
    assert_eq!(app.input.text, "hello");
    assert_eq!(app.input.cursor, 5);
}

#[test]
fn delete_to_start_removes_from_beginning_to_cursor() {
    let mut app = test_app("/tmp/tai.sock", "Kitty");
    app.input.text = "hello world".to_string();
    app.input.cursor = 6;
    app.input.delete_to_start();
    assert_eq!(app.input.text, "world");
    assert_eq!(app.input.cursor, 0);
}

#[test]
fn delete_to_start_when_at_end_clears_input() {
    let mut app = test_app("/tmp/tai.sock", "Kitty");
    app.input.text = "hello".to_string();
    app.input.cursor = 5;
    app.input.delete_to_start();
    assert!(app.input.is_empty());
    assert_eq!(app.input.cursor, 0);
}

#[test]
fn delete_to_start_when_at_zero_does_nothing() {
    let mut app = test_app("/tmp/tai.sock", "Kitty");
    app.input.text = "hello".to_string();
    app.input.cursor = 0;
    app.input.delete_to_start();
    assert_eq!(app.input.text, "hello");
    assert_eq!(app.input.cursor, 0);
}

#[test]
fn terminal_event_submit_resets_cursor() {
    let mut app = test_app("/tmp/tai.sock", "Kitty");
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
    let mut app = test_app("/tmp/tai.sock", "Kitty");
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
    let mut app = test_app("/tmp/tai.sock", "Kitty");
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
    let mut app = test_app("/tmp/tai.sock", "Kitty");
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
    let mut app = test_app("/tmp/tai.sock", "Kitty");
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
    let mut app = test_app("/tmp/tai.sock", "Kitty");
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
fn terminal_event_ctrl_backspace_deletes_word_backward() {
    let mut app = test_app("/tmp/tai.sock", "Kitty");
    app.input.text = "hello world".to_string();
    app.input.cursor = 11;
    let (tx, _rx) = std::sync::mpsc::channel();

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::CONTROL)),
        &mut app,
        &tx,
    )
    .expect("handle ctrl+backspace");

    assert_eq!(app.input.text, "hello ");
    assert_eq!(app.input.cursor, 6);
}

#[test]
fn terminal_event_ctrl_w_deletes_word_backward() {
    let mut app = test_app("/tmp/tai.sock", "Kitty");
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
    let mut app = test_app("/tmp/tai.sock", "Kitty");
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
    let mut app = test_app("/tmp/tai.sock", "Kitty");
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
    let mut app = test_app("/tmp/tai.sock", "Kitty");
    app.input.text = "  hello  world  ".to_string();
    app.input.cursor = 8;
    // Must not panic when cursor sits within whitespace between words.
    app.input.delete_word_backward();
    assert!(app.input.cursor <= app.input.text.len());
}

#[test]
fn mouse_scroll_outside_history_box_does_not_change_scroll() {
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = test_app("/tmp/tai.sock", "Kitty");
    app.history_viewport.height = 1;
    for index in 0..8 {
        app.push_text(format!("line {index}"));
    }
    app.scroll_up(5);

    let (_, height) = crossterm::terminal::size().expect("terminal size");
    let row = height.saturating_sub(1);
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

    assert_eq!(app.history_scroll.scroll(), 5);
    assert!(!app.history_scroll.follow_output());
}

#[test]
fn scrolling_up_disables_follow_and_scrolling_back_to_bottom_enables_it() {
    let mut app = test_app("/tmp/tai.sock", "Kitty");

    app.scroll_up(3);
    assert_eq!(app.history_scroll.scroll(), 0);
    assert!(app.history_scroll.follow_output());

    app.history_viewport.height = 1;
    app.scroll_up(3);
    assert_eq!(app.history_scroll.scroll(), 3);
    assert!(!app.history_scroll.follow_output());

    app.scroll_down(3);
    assert_eq!(app.history_scroll.scroll(), 0);
    assert!(app.history_scroll.follow_output());
}

#[test]
fn push_text_respects_follow_output_mode() {
    let mut app = test_app("/tmp/tai.sock", "Kitty");
    app.history_viewport.width = 10;
    app.history_viewport.height = 1;
    for index in 0..8 {
        app.push_text(format!("line {index}"));
    }
    app.scroll_up(4);
    app.push_text("later");

    assert_eq!(app.history_scroll.scroll(), 4);
    assert_eq!(app.history_scroll.scroll_compensation(), 2);
    assert_eq!(app.effective_scroll(), 6);
    assert!(!app.history_scroll.follow_output());

    app.scroll_down(1);
    assert_eq!(app.history_scroll.scroll(), 4);
    assert_eq!(app.history_scroll.scroll_compensation(), 1);

    app.scroll_down(5);
    app.push_text("latest");
    assert_eq!(app.history_scroll.scroll(), 0);
    assert_eq!(app.history_scroll.scroll_compensation(), 0);
    assert!(app.history_scroll.follow_output());
}

#[test]
fn streaming_growth_above_viewport_preserves_visible_content_offset() {
    let mut app = test_app("/tmp/tai.sock", "Kitty");
    app.history_viewport.width = 5;
    app.history_viewport.height = 1;
    app.push_text("older history");
    app.push_text("older history two");
    app.begin_stream(7);
    app.scroll_up(2);

    app.append_stream_text(7, OutputStream::Answer, "123456");

    assert_eq!(app.history_scroll.scroll(), 2);
    assert_eq!(app.history_scroll.scroll_compensation(), 5);
    assert_eq!(app.effective_scroll(), 7);
    assert!(!app.history_scroll.follow_output());
}

#[test]
fn trimming_history_reduces_scroll_by_trimmed_height() {
    let mut app = test_app("/tmp/tai.sock", "Kitty");
    app.history_viewport.width = 10;
    app.history_viewport.height = 1;
    app.history_scroll.follow_output = false;
    app.client.history = (0..499)
        .map(|index| HistoryItem::Text(format!("line {index}")))
        .collect();
    app.history_scroll.scroll = 20;

    app.push_text("tail");
    assert_eq!(app.history_scroll.scroll(), 20);
    assert_eq!(app.history_scroll.scroll_compensation(), 2);
    assert_eq!(app.effective_scroll(), 22);

    app.push_text("tail");

    assert_eq!(app.client.history.len(), 500);
    assert_eq!(app.history_scroll.scroll(), 20);
    assert_eq!(app.history_scroll.scroll_compensation(), 2);
    assert_eq!(app.effective_scroll(), 22);
    assert!(!app.history_scroll.follow_output());
}

#[test]
fn scrolling_to_top_clamps_without_emptying_history_view() {
    let mut app = test_app("/tmp/tai.sock", "Kitty");
    app.history_viewport.height = 1;

    app.scroll_up(100);

    assert_eq!(app.max_scroll_offset(), 3);
    assert_eq!(app.effective_scroll(), 3);
    assert_eq!(app.history_scroll.scroll(), 3);
    assert_eq!(app.history_scroll.scroll_compensation(), 0);
    assert!(!app.history_scroll.follow_output());
}

// ── Session Manager tests ─────────────────────────────────────

#[test]
fn app_starts_in_chat_page() {
    let app = test_app("/tmp/tai.sock", "Halfblocks");
    assert_eq!(app.page, Page::Chat);
}

#[test]
fn session_manager_state_new_is_empty() {
    let state = SessionManagerState::new();
    assert!(state.sessions.is_empty());
    assert!(state.selection.is_none());
    assert_eq!(state.view, SessionManagerView::List);
    assert_eq!(state.scroll, 0);
    assert!(state.detail_data.is_none());
}

#[test]
fn session_manager_set_sessions_empty() {
    let mut state = SessionManagerState::new();
    state.set_sessions(vec![]);
    assert!(state.sessions.is_empty());
    assert!(state.selection.is_none());
}

fn make_session(id: u64, title: &str, model: &str, count: u32) -> tai_proto::SessionSummary {
    tai_proto::SessionSummary {
        session_id: id,
        title: Some(title.to_string()),
        selected_model: Some(model.to_string()),
        reasoning_effort: None,
        parent_session_id: None,
        cwd: None,
        created_at: 1705314000,
        message_count: count,
        max_turns: None,
        status: tai_proto::SessionStatus::Inactive,
        active_tool_groups: Vec::new(),
        account_name: None,
        token_usage: None,
    }
}

#[test]
fn session_manager_set_sessions_selects_first() {
    let mut state = SessionManagerState::new();
    state.set_sessions(vec![
        make_session(1, "a", "m1", 5),
        make_session(2, "b", "m2", 3),
    ]);
    assert_eq!(state.sessions.len(), 2);
    assert_eq!(state.selection, Some(0));
}

#[test]
fn session_manager_select_up_down() {
    let mut state = SessionManagerState::new();
    state.set_sessions(vec![
        make_session(1, "a", "m1", 5),
        make_session(2, "b", "m2", 3),
        make_session(3, "c", "m3", 7),
    ]);

    assert_eq!(state.selection, Some(0));

    state.select_down();
    assert_eq!(state.selection, Some(1));

    state.select_down();
    assert_eq!(state.selection, Some(2));

    state.select_down();
    assert_eq!(state.selection, Some(2)); // clamped at max

    state.select_up();
    assert_eq!(state.selection, Some(1));

    state.select_up();
    assert_eq!(state.selection, Some(0));

    state.select_up();
    assert_eq!(state.selection, Some(0)); // clamped at 0
}

#[test]
fn session_manager_enter_detail_uses_selected_session() {
    let mut state = SessionManagerState::new();
    state.set_sessions(vec![
        make_session(10, "test-session", "gpt-4", 5),
        make_session(20, "other", "claude", 3),
    ]);

    state.enter_detail();
    assert_eq!(state.view, SessionManagerView::Detail);
    let detail = state.detail_data.as_ref().expect("detail data");
    assert_eq!(detail.session_id, 10);
    assert_eq!(detail.title, "test-session");
    assert_eq!(detail.selected_model, "gpt-4");
    assert_eq!(detail.message_count, 5);
}

#[test]
fn session_manager_enter_detail_fails_when_no_selection() {
    let mut state = SessionManagerState::new();
    state.enter_detail();
    assert_eq!(state.view, SessionManagerView::List);
    assert!(state.detail_data.is_none());
}

#[test]
fn session_manager_leave_detail_returns_to_list() {
    let mut state = SessionManagerState::new();
    state.set_sessions(vec![make_session(1, "a", "m1", 0)]);
    state.enter_detail();
    assert_eq!(state.view, SessionManagerView::Detail);

    state.leave_detail();
    assert_eq!(state.view, SessionManagerView::List);
    assert!(state.detail_data.is_none());
}

#[test]
fn session_manager_set_sessions_preserves_selection_by_id() {
    let mut state = SessionManagerState::new();
    state.set_sessions(vec![
        make_session(1, "a", "m1", 0),
        make_session(2, "b", "m2", 0),
        make_session(3, "c", "m3", 0),
    ]);
    state.select_down();
    state.select_down();
    assert_eq!(state.selection, Some(2));

    // Refresh sessions — should preserve selection on session 3
    state.set_sessions(vec![
        make_session(1, "a", "m1", 0),
        make_session(3, "c", "m3", 5),
        make_session(4, "d", "m4", 0),
    ]);
    assert_eq!(state.selection, Some(1)); // session 3 is now at index 1
}

#[cfg(test)]
mod session_manager_key_tests {
    use super::*;
    use crate::connection::handle_terminal_event;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn make_sm_app() -> App {
        let mut app = test_app("/tmp/tai.sock", "Kitty");
        app.page = Page::SessionManager;
        app.session_mgr.set_sessions(vec![
            make_session(1, "first", "gpt-4", 3),
            make_session(2, "second", "claude", 5),
        ]);
        app
    }

    #[test]
    fn session_manager_esc_returns_to_home() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = make_sm_app();

        handle_terminal_event(
            Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            &mut app,
            &tx,
        )
        .expect("handle esc");

        assert_eq!(app.page, Page::Home);
        assert_eq!(app.previous_page, Page::SessionManager);
    }

    #[test]
    fn session_manager_q_returns_to_home() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = make_sm_app();

        handle_terminal_event(
            Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)),
            &mut app,
            &tx,
        )
        .expect("handle q");

        assert_eq!(app.page, Page::Home);
        assert_eq!(app.previous_page, Page::SessionManager);
    }

    #[test]
    fn session_manager_j_moves_selection_down() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = make_sm_app();
        assert_eq!(app.session_mgr.selection, Some(0));

        handle_terminal_event(
            Event::Key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE)),
            &mut app,
            &tx,
        )
        .expect("handle j");

        assert_eq!(app.session_mgr.selection, Some(1));
    }

    #[test]
    fn session_manager_enter_switches_session_and_returns_to_chat() {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut app = make_sm_app();

        handle_terminal_event(
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            &mut app,
            &tx,
        )
        .expect("handle enter");

        assert_eq!(app.page, Page::Chat);
        assert_eq!(app.attached_session_id, Some(1));
        let msg = rx.recv().expect("sent message (unsub)");
        assert_eq!(msg, ClientMessage::UnsubscribeSessionsSummary);
        let msg = rx.recv().expect("sent message");
        assert_eq!(msg, ClientMessage::AttachSession { session_id: 1 });
    }

    #[test]
    fn session_manager_ctrl_c_still_quits() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = make_sm_app();

        handle_terminal_event(
            Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            &mut app,
            &tx,
        )
        .expect("handle ctrl+c");

        assert!(app.should_quit);
    }

    #[test]
    fn chat_ctrl_s_enters_session_manager() {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut app = test_app("/tmp/tai.sock", "Kitty");
        assert_eq!(app.page, Page::Chat);

        handle_terminal_event(
            Event::Key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL)),
            &mut app,
            &tx,
        )
        .expect("handle ctrl+s");

        assert_eq!(app.page, Page::SessionManager);
        let msg = rx.recv().expect("sent message");
        assert_eq!(msg, ClientMessage::ListSessions);
        let msg = rx.recv().expect("sent message");
        assert_eq!(msg, ClientMessage::SubscribeSessionsSummary);
    }

    #[test]
    fn session_manager_i_enters_detail() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = make_sm_app();

        handle_terminal_event(
            Event::Key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE)),
            &mut app,
            &tx,
        )
        .expect("handle i");

        assert_eq!(app.session_mgr.view, SessionManagerView::Detail);
        assert!(app.session_mgr.detail_data.is_some());
    }

    #[test]
    fn session_manager_detail_b_returns_to_list() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = make_sm_app();
        app.session_mgr.enter_detail();
        assert_eq!(app.session_mgr.view, SessionManagerView::Detail);

        handle_terminal_event(
            Event::Key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE)),
            &mut app,
            &tx,
        )
        .expect("handle b");

        assert_eq!(app.session_mgr.view, SessionManagerView::List);
        assert!(app.session_mgr.detail_data.is_none());
    }

    #[test]
    fn session_manager_detail_enter_switches_session() {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut app = make_sm_app();
        app.session_mgr.enter_detail();

        handle_terminal_event(
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            &mut app,
            &tx,
        )
        .expect("handle enter");

        assert_eq!(app.page, Page::Chat);
        assert_eq!(app.attached_session_id, Some(1));
        let msg = rx.recv().expect("sent message (unsub)");
        assert_eq!(msg, ClientMessage::UnsubscribeSessionsSummary);
        let msg = rx.recv().expect("sent message");
        assert_eq!(msg, ClientMessage::AttachSession { session_id: 1 });
    }

    #[test]
    fn session_manager_n_sends_create_session() {
        let mut app = make_sm_app();
        let (tx, rx) = std::sync::mpsc::channel::<ClientMessage>();

        handle_terminal_event(
            Event::Key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE)),
            &mut app,
            &tx,
        )
        .expect("handle n");

        let msg = rx.recv().expect("sent message");
        assert_eq!(
            msg,
            ClientMessage::CreateSession {
                title: None,
                parent_session_id: None,
                cwd: None,
                max_turns: None,
                context_config: None,
                account_name: None,
            }
        );
    }
}

// ── Scroll accumulator tests ───────────────────────────────

#[test]
fn scroll_accumulator_increments_on_scroll_up() {
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = test_app("/tmp/tai.sock", "Kitty");
    app.history_viewport.height = 10;

    handle_terminal_event(
        Event::Mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        }),
        &mut app,
        &tx,
    )
    .expect("handle scroll up");

    assert_eq!(app.scroll_accumulator, 1);
}

#[test]
fn scroll_accumulator_decrements_on_scroll_down() {
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = test_app("/tmp/tai.sock", "Kitty");
    app.history_viewport.height = 10;

    handle_terminal_event(
        Event::Mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        }),
        &mut app,
        &tx,
    )
    .expect("handle scroll down");

    assert_eq!(app.scroll_accumulator, -1);
}

#[test]
fn scroll_accumulator_accumulates_multiple_events() {
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = test_app("/tmp/tai.sock", "Kitty");
    app.history_viewport.height = 10;

    for _ in 0..3 {
        handle_terminal_event(
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: 0,
                row: 0,
                modifiers: KeyModifiers::NONE,
            }),
            &mut app,
            &tx,
        )
        .expect("handle scroll up");
    }

    assert_eq!(app.scroll_accumulator, 3);
}

#[test]
fn apply_scroll_delta_consumes_accumulator_scroll_up() {
    let mut app = test_app("/tmp/tai.sock", "Kitty");
    app.history_viewport.width = 10;
    app.history_viewport.height = 1;
    for _ in 0..5 {
        app.push_text("line");
    }
    app.scroll_accumulator = 2;

    // Sanity: accumulator is consumed and scroll position advances.
    let before = app.effective_scroll();
    app.apply_scroll_delta();
    assert_eq!(app.scroll_accumulator, 0);
    assert_eq!(app.effective_scroll(), before + 2);
}

#[test]
fn apply_scroll_delta_consumes_accumulator_scroll_down() {
    let mut app = test_app("/tmp/tai.sock", "Kitty");
    app.history_viewport.width = 10;
    app.history_viewport.height = 1;
    for _ in 0..5 {
        app.push_text("line");
    }
    // Scroll up first, then set a negative delta to scroll back.
    app.scroll_up(3);
    let before = app.effective_scroll();
    app.scroll_accumulator = -2;

    app.apply_scroll_delta();
    assert_eq!(app.scroll_accumulator, 0);
    assert_eq!(app.effective_scroll(), before - 2);
}

#[test]
fn apply_scroll_delta_zero_does_nothing() {
    let mut app = test_app("/tmp/tai.sock", "Kitty");
    app.history_viewport.width = 10;
    app.history_viewport.height = 1;
    for _ in 0..5 {
        app.push_text("line");
    }
    app.scroll_up(2);
    let before = app.effective_scroll();

    app.apply_scroll_delta();
    assert_eq!(app.scroll_accumulator, 0);
    assert_eq!(app.effective_scroll(), before);
}

#[test]
fn scroll_up_mouse_inside_history_box_via_accumulator() {
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = test_app("/tmp/tai.sock", "Kitty");
    app.history_viewport.width = 10;
    app.history_viewport.height = 1;
    for _ in 0..5 {
        app.push_text("line");
    }

    handle_terminal_event(
        Event::Mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        }),
        &mut app,
        &tx,
    )
    .expect("handle scroll up");

    assert_eq!(app.scroll_accumulator, 1);

    app.apply_scroll_delta();
    assert!(!app.history_scroll.follow_output());
    assert_ne!(app.effective_scroll(), 0);
}

#[test]
fn scroll_down_mouse_inside_history_box_via_accumulator() {
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = test_app("/tmp/tai.sock", "Kitty");
    app.history_viewport.width = 10;
    app.history_viewport.height = 1;
    for _ in 0..5 {
        app.push_text("line");
    }
    // Pre-scroll up so there is room to scroll down.
    app.scroll_up(3);

    handle_terminal_event(
        Event::Mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        }),
        &mut app,
        &tx,
    )
    .expect("handle scroll down");

    assert_eq!(app.scroll_accumulator, -1);

    let before = app.effective_scroll();
    app.apply_scroll_delta();
    assert_eq!(app.effective_scroll(), before - 1);
}

// ── Command history tests ──────────────────────────────────────

#[test]
fn commit_to_history_adds_entry() {
    let mut app = test_app("/tmp/tai.sock", "Kitty");
    assert!(app.command_history.is_empty());

    app.commit_to_history("hello".to_string());
    assert_eq!(app.command_history.len(), 1);
    assert_eq!(app.command_history[0], "hello");
    assert!(app.history_index.is_none());
    assert!(app.saved_draft.is_empty());
}

#[test]
fn commit_to_history_skips_empty() {
    let mut app = test_app("/tmp/tai.sock", "Kitty");
    app.commit_to_history("".to_string());
    assert!(app.command_history.is_empty());
}

#[test]
fn commit_to_history_skips_duplicate_of_most_recent() {
    let mut app = test_app("/tmp/tai.sock", "Kitty");
    app.commit_to_history("hello".to_string());
    app.commit_to_history("hello".to_string());
    assert_eq!(app.command_history.len(), 1);
}

#[test]
fn commit_to_history_inserts_newest_first() {
    let mut app = test_app("/tmp/tai.sock", "Kitty");
    app.commit_to_history("first".to_string());
    app.commit_to_history("second".to_string());
    assert_eq!(app.command_history.len(), 2);
    assert_eq!(app.command_history[0], "second");
    assert_eq!(app.command_history[1], "first");
}

#[test]
fn navigate_history_up_loads_most_recent() {
    let mut app = test_app("/tmp/tai.sock", "Kitty");
    app.command_history = vec!["cmd-2".into(), "cmd-1".into(), "cmd-0".into()];
    app.input.text = "typing".to_string();
    app.input.cursor = 6;

    app.navigate_history_up();

    assert_eq!(app.history_index, Some(0));
    assert_eq!(app.input.text, "cmd-2");
    assert_eq!(app.input.cursor, 5);
    assert_eq!(app.saved_draft, "typing");
}

#[test]
fn navigate_history_up_moves_to_older() {
    let mut app = test_app("/tmp/tai.sock", "Kitty");
    app.command_history = vec!["c".into(), "b".into(), "a".into()];
    app.history_index = Some(0);

    app.navigate_history_up();
    assert_eq!(app.history_index, Some(1));
    assert_eq!(app.input.text, "b");

    app.navigate_history_up();
    assert_eq!(app.history_index, Some(2));
    assert_eq!(app.input.text, "a");
}

#[test]
fn navigate_history_up_stops_at_oldest() {
    let mut app = test_app("/tmp/tai.sock", "Kitty");
    app.command_history = vec!["c".into(), "b".into()];
    app.history_index = Some(1);

    app.input.text = "b".to_string();
    app.input.cursor = 1;

    app.navigate_history_up();
    assert_eq!(app.history_index, Some(1)); // no change
    assert_eq!(app.input.text, "b");
}

#[test]
fn navigate_history_up_empty_history_does_nothing() {
    let mut app = test_app("/tmp/tai.sock", "Kitty");
    app.input.text = "hello".to_string();

    app.navigate_history_up();
    assert_eq!(app.input.text, "hello");
    assert!(app.history_index.is_none());
}

#[test]
fn navigate_history_down_restores_draft() {
    let mut app = test_app("/tmp/tai.sock", "Kitty");
    app.command_history = vec!["cmd".into()];
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
    let mut app = test_app("/tmp/tai.sock", "Kitty");
    app.command_history = vec!["c".into(), "b".into(), "a".into()];
    app.history_index = Some(2);
    app.input.text = "a".to_string();

    app.navigate_history_down();
    assert_eq!(app.history_index, Some(1));
    assert_eq!(app.input.text, "b");

    app.navigate_history_down();
    assert_eq!(app.history_index, Some(0));
    assert_eq!(app.input.text, "c");

    app.navigate_history_down();
    assert!(app.history_index.is_none()); // past newest
}

#[test]
fn history_nav_resets_after_commit() {
    let mut app = test_app("/tmp/tai.sock", "Kitty");
    app.command_history = vec!["old".into()];
    app.history_index = Some(0);
    app.saved_draft = "draft".to_string();

    app.commit_to_history("new".to_string());

    assert!(app.history_index.is_none());
    assert!(app.saved_draft.is_empty());
    assert_eq!(app.command_history[0], "new");
}

#[test]
fn terminal_event_up_down_navigates_history() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = test_app("/tmp/tai.sock", "Kitty");
    app.command_history = vec!["recent".into(), "older".into()];

    // Press Up — loads most recent
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("handle up");
    assert_eq!(app.input.text, "recent");
    assert!(app.saved_draft.is_empty()); // nothing typed yet, draft is empty

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
    assert!(app.input.is_empty());
    assert!(app.history_index.is_none());
}

#[test]
fn terminal_event_enter_saves_to_history() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    let (tx, rx) = std::sync::mpsc::channel::<ClientMessage>();
    let mut app = test_app("/tmp/tai.sock", "Kitty");
    app.input.text = "test command".to_string();
    app.input.cursor = 12;

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("handle enter");

    assert_eq!(app.command_history.len(), 1);
    assert_eq!(app.command_history[0], "test command");
    assert!(app.input.is_empty());

    // Verify the message was sent
    let msg = rx.recv().expect("sent message");
    assert_eq!(
        msg,
        ClientMessage::RunInput {
            request_id: 1,
            input: b"test command".to_vec(),
        }
    );
}

#[test]
fn terminal_event_enter_empty_does_not_save_to_history() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = test_app("/tmp/tai.sock", "Kitty");

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("handle enter");

    assert!(app.command_history.is_empty());
}

#[test]
fn scroll_mouse_outside_history_box_does_not_update_accumulator() {
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = test_app("/tmp/tai.sock", "Kitty");
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
