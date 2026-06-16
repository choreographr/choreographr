use crate::connection::handle_terminal_event;
use crate::state::*;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use tai_proto::{ClientMessage, OutputStream};
use tokio::sync::mpsc;

#[test]
fn app_push_text_trims_history_to_limit() {
    let mut app = App::new("/tmp/tai.sock".to_string(), "Halfblocks".to_string());
    for index in 0..600 {
        app.push_text(format!("line {index}"));
    }
    assert_eq!(app.client.history.len(), 500);
    match &app.client.history[0] {
        HistoryItem::Text(text) => assert!(text.contains("line 100")),
        HistoryItem::SessionMessage(_) | HistoryItem::Streaming(_) | HistoryItem::Image(_) => {
            panic!("expected text history item")
        }
    }
}

#[test]
fn drop_request_removes_active_request() {
    let mut app = App::new("/tmp/tai.sock".to_string(), "Kitty".to_string());
    app.active.insert(42);
    app.begin_stream(42);
    app.drop_request(42);
    assert!(!app.active.contains(&42));
    assert!(!app.client.in_progress.contains_key(&42));
}

#[test]
fn append_stream_text_updates_mutable_history_entry() {
    let mut app = App::new("/tmp/tai.sock".to_string(), "Kitty".to_string());
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
    let mut app = App::new("/tmp/tai.sock".to_string(), "Kitty".to_string());
    app.history_viewport.width = 10;
    app.history_viewport.height = 1;
    app.push_text("older");
    app.push_text("older still");
    app.begin_stream(7);
    app.scroll_up(3);

    app.append_stream_text(7, OutputStream::Answer, "hello");

    assert_eq!(app.history_scroll.scroll(), 3);
    assert_eq!(app.history_scroll.scroll_compensation(), 1);
    assert_eq!(app.effective_scroll(), 4);
    assert!(!app.history_scroll.follow_output());
}

#[test]
fn append_stream_text_keeps_following_when_at_bottom() {
    let mut app = App::new("/tmp/tai.sock".to_string(), "Kitty".to_string());
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
    assert_eq!(lines_height(&["😀😀".into()], 2), 2);
    assert_eq!(lines_height(&["👨‍👩‍👧‍👦".into()], 2), 1);
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

    assert_eq!(lines[0], "[9]");
    assert_eq!(lines[1], "reasoning: step by step");
    assert_eq!(lines[2], "answer: final");
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

    assert_eq!(lines[0], "[3]");
    assert_eq!(lines[1], "reasoning: line one");
    assert_eq!(lines[2], "line two");
    assert_eq!(lines[3], "answer: final one");
    assert_eq!(lines[4], "final two");
}

#[test]
fn markdown_lines_render_tables() {
    let lines = markdown_lines(
        "| Name | Role | Years |\n|:--|:--:|--:|\n| Ada Lovelace | Mathematician | 1842 |\n| Grace Hopper | Computer Scientist | 1943 |",
        60,
    );

    let rendered = lines.join("\n");

    assert!(rendered.contains("┌"));
    assert!(rendered.contains("Ada Lovelace"));
    assert!(rendered.contains("Grace Hopper"));
    assert!(rendered.contains("Mathematician"));
}

#[test]
fn markdown_lines_render_lists_with_item_text() {
    let lines = markdown_lines("- one\n- [x] done\n1. first\n2. second", 80);

    let rendered = lines.join("\n");

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
fn image_history_height_caps_to_twelve_rows() {
    assert_eq!(image_block_height(0), 0);
    assert_eq!(image_block_height(4), 4);
    assert_eq!(image_block_height(20), 12);
}

#[tokio::test]
async fn terminal_event_appends_characters() {
    let mut app = App::new("/tmp/tai.sock".to_string(), "Kitty".to_string());
    let (tx, mut rx) = mpsc::channel(1);

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .await
    .expect("handle key");
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .await
    .expect("handle key");

    assert_eq!(app.input, "hi");
    assert!(rx.try_recv().is_err());
}

#[tokio::test]
async fn terminal_event_submits_run_input() {
    let mut app = App::new("/tmp/tai.sock".to_string(), "Kitty".to_string());
    app.input = "hello".to_string();
    let (tx, mut rx) = mpsc::channel(1);

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .await
    .expect("handle enter");

    assert!(app.input.is_empty());
    let message = rx.recv().await.expect("sent message");
    assert_eq!(
        message,
        ClientMessage::RunInput {
            request_id: 1,
            input: b"hello".to_vec(),
        }
    );
}

#[tokio::test]
async fn terminal_event_quits_only_when_input_empty() {
    let (tx, _rx) = mpsc::channel(1);

    let mut app = App::new("/tmp/tai.sock".to_string(), "Kitty".to_string());
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .await
    .expect("handle q");
    assert!(app.should_quit);

    let mut app = App::new("/tmp/tai.sock".to_string(), "Kitty".to_string());
    app.input = "q".to_string();
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .await
    .expect("handle q");
    assert!(!app.should_quit);
    assert_eq!(app.input, "qq");
}

#[tokio::test]
async fn terminal_event_ctrl_c_quits() {
    let (tx, _rx) = mpsc::channel(1);
    let mut app = App::new("/tmp/tai.sock".to_string(), "Kitty".to_string());

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        &mut app,
        &tx,
    )
    .await
    .expect("handle ctrl+c");

    assert!(app.should_quit);
}

#[tokio::test]
async fn mouse_scroll_outside_history_box_does_not_change_scroll() {
    let (tx, _rx) = mpsc::channel(1);
    let mut app = App::new("/tmp/tai.sock".to_string(), "Kitty".to_string());
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
    .await
    .expect("handle mouse");

    assert_eq!(app.history_scroll.scroll(), 5);
    assert!(!app.history_scroll.follow_output());
}

#[test]
fn scrolling_up_disables_follow_and_scrolling_back_to_bottom_enables_it() {
    let mut app = App::new("/tmp/tai.sock".to_string(), "Kitty".to_string());

    app.scroll_up(3);
    assert_eq!(app.history_scroll.scroll(), 0);
    assert!(app.history_scroll.follow_output());

    app.history_viewport.height = 1;
    app.scroll_up(3);
    assert_eq!(app.history_scroll.scroll(), 1);
    assert!(!app.history_scroll.follow_output());

    app.scroll_down(1);
    assert_eq!(app.history_scroll.scroll(), 0);
    assert!(app.history_scroll.follow_output());
}

#[test]
fn push_text_respects_follow_output_mode() {
    let mut app = App::new("/tmp/tai.sock".to_string(), "Kitty".to_string());
    app.history_viewport.width = 10;
    app.history_viewport.height = 1;
    for index in 0..8 {
        app.push_text(format!("line {index}"));
    }
    app.scroll_up(4);

    app.push_text("later");
    assert_eq!(app.history_scroll.scroll(), 4);
    assert_eq!(app.history_scroll.scroll_compensation(), 1);
    assert_eq!(app.effective_scroll(), 5);
    assert!(!app.history_scroll.follow_output());

    app.scroll_down(1);
    assert_eq!(app.history_scroll.scroll(), 4);
    assert_eq!(app.history_scroll.scroll_compensation(), 0);

    app.scroll_down(4);
    app.push_text("latest");
    assert_eq!(app.history_scroll.scroll(), 0);
    assert_eq!(app.history_scroll.scroll_compensation(), 0);
    assert!(app.history_scroll.follow_output());
}

#[test]
fn streaming_growth_above_viewport_preserves_visible_content_offset() {
    let mut app = App::new("/tmp/tai.sock".to_string(), "Kitty".to_string());
    app.history_viewport.width = 5;
    app.history_viewport.height = 1;
    app.push_text("older history");
    app.push_text("older history two");
    app.begin_stream(7);
    app.scroll_up(2);

    app.append_stream_text(7, OutputStream::Answer, "123456");

    assert_eq!(app.history_scroll.scroll(), 2);
    assert_eq!(app.history_scroll.scroll_compensation(), 2);
    assert_eq!(app.effective_scroll(), 4);
    assert!(!app.history_scroll.follow_output());
}

#[test]
fn trimming_history_reduces_scroll_by_trimmed_height() {
    let mut app = App::new("/tmp/tai.sock".to_string(), "Kitty".to_string());
    app.history_viewport.width = 10;
    app.history_viewport.height = 1;
    app.history_scroll.follow_output = false;
    app.client.history = (0..499)
        .map(|index| HistoryItem::Text(format!("line {index}")))
        .collect();
    app.history_scroll.scroll = 20;

    app.push_text("tail");
    assert_eq!(app.history_scroll.scroll(), 20);
    assert_eq!(app.history_scroll.scroll_compensation(), 1);
    assert_eq!(app.effective_scroll(), 21);

    app.push_text("tail");

    assert_eq!(app.client.history.len(), 500);
    assert_eq!(app.history_scroll.scroll(), 20);
    assert_eq!(app.history_scroll.scroll_compensation(), 1);
    assert_eq!(app.effective_scroll(), 21);
    assert!(!app.history_scroll.follow_output());
}

#[test]
fn scrolling_to_top_clamps_without_emptying_history_view() {
    let mut app = App::new("/tmp/tai.sock".to_string(), "Kitty".to_string());
    app.history_viewport.height = 1;

    app.scroll_up(100);

    assert_eq!(app.max_scroll_offset(), 1);
    assert_eq!(app.effective_scroll(), 1);
    assert_eq!(app.history_scroll.scroll(), 1);
    assert_eq!(app.history_scroll.scroll_compensation(), 0);
    assert!(!app.history_scroll.follow_output());
}
