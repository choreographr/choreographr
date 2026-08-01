use crate::connection::{handle_daemon_message, handle_terminal_event};
use crate::markdown_render::*;
use crate::state::*;
use crate::test_util::test_app;
use choreo_client_core::TurnEventHandler;
use choreo_proto::{
    ClientMessage, DaemonMessage, DisplayedImageRecord, ImageMetadata, ReasoningCapability,
    SessionStatus, TokenUsage, Turn,
};
use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::text::Line;
use tui_prompts::State;

/// Add a UserText turn to the session, mimicking what the daemon sends after
/// processing a RunInput.
fn add_user_text(app: &mut App, content: &str) {
    let turn_id = app.next_request_id;
    app.next_request_id += 1;
    let turn = Turn {
        created_at: choreo_proto::TimestampMs::now(),
        undone: false,
        error: None,
        user_text: Some(content.to_string()),
        assistant_text: None,
        assistant_reasoning: None,
        tool_calls: vec![],
        token_usage: None,
        tool_results: vec![],
        displayed_images: vec![],
    };
    app.display_for(0).view.insert_or_replace(turn_id, turn);
    app.rebuild_height_prefix();
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
fn terminal_event_appends_characters() {
    let mut app = test_app();
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
    let mut app = test_app();
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
fn terminal_event_esc_noop_on_chat() {
    let (tx, _rx) = std::sync::mpsc::channel();

    let mut app = test_app();
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("handle esc");

    assert_eq!(app.page, Page::Chat);
    assert!(!app.should_quit);
}

#[test]
fn terminal_event_ctrl_c_noop_on_chat() {
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = test_app();

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        &mut app,
        &tx,
    )
    .expect("handle ctrl+c");

    assert_eq!(app.page, Page::Chat);
    assert!(!app.should_quit);
    assert!(
        app.input.text.is_empty(),
        "Ctrl+C should not insert literal 'c'"
    );
    assert_eq!(app.input.cursor, 0, "cursor should remain at 0");
}

#[test]
fn global_ctrl_q_quits_from_chat() {
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = test_app();

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
        &mut app,
        &tx,
    )
    .expect("handle ctrl+q");

    assert!(app.should_quit);
}

#[test]
fn global_ctrl_q_quits_from_session_manager() {
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = test_app();
    app.page = Page::SessionManager;

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
        &mut app,
        &tx,
    )
    .expect("handle ctrl+q from session manager");

    assert!(app.should_quit);
}

#[test]
fn global_ctrl_q_quits_from_ai_providers() {
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = test_app();
    app.page = Page::AIProviders;

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
        &mut app,
        &tx,
    )
    .expect("handle ctrl+q from ai providers");

    assert!(app.should_quit);
}

#[test]
fn ctrl_p_does_not_insert_char_on_chat() {
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = test_app();
    app.input.text = "hello".to_string();
    app.input.cursor = 5;

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL)),
        &mut app,
        &tx,
    )
    .expect("handle ctrl+p");

    assert_eq!(app.input.text, "hello", "Ctrl+P should not insert 'p'");
    assert_eq!(app.input.cursor, 5);
    assert!(!app.should_quit);
}

#[test]
fn alt_x_does_not_insert_char_on_chat() {
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = test_app();
    app.input.text = "hello".to_string();
    app.input.cursor = 5;

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::ALT)),
        &mut app,
        &tx,
    )
    .expect("handle alt+x");

    assert_eq!(app.input.text, "hello", "Alt+X should not insert 'x'");
    assert_eq!(app.input.cursor, 5);
    assert!(!app.should_quit);
}

#[test]
fn chat_ctrl_h_toggles_help() {
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = test_app();
    app.show_ctrl_help = false;

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::CONTROL)),
        &mut app,
        &tx,
    )
    .expect("handle ctrl+h");

    assert!(app.show_ctrl_help, "first press should enable help");
}

#[test]
fn chat_ctrl_h_double_toggle_returns_to_off() {
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = test_app();
    app.show_ctrl_help = false;

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::CONTROL)),
        &mut app,
        &tx,
    )
    .expect("handle ctrl+h (first)");
    assert!(app.show_ctrl_help, "first press should enable help");

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::CONTROL)),
        &mut app,
        &tx,
    )
    .expect("handle ctrl+h (second)");
    assert!(!app.show_ctrl_help, "second press should disable help");
}

#[test]
fn chat_ctrl_a_enters_ai_providers() {
    let (tx, rx) = std::sync::mpsc::channel();
    let mut app = test_app();

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL)),
        &mut app,
        &tx,
    )
    .expect("handle ctrl+a");

    assert_eq!(app.page, Page::AIProviders);
    let msg = rx.recv().expect("sent message");
    assert_eq!(msg, ClientMessage::ListAccounts);
}

#[test]
fn chat_ctrl_up_sends_undo() {
    let (tx, rx) = std::sync::mpsc::channel();
    let mut app = test_app();

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::CONTROL)),
        &mut app,
        &tx,
    )
    .expect("handle ctrl+up");

    let msg = rx.recv().expect("sent message");
    assert_eq!(msg, ClientMessage::Undo);
}

#[test]
fn chat_ctrl_down_sends_redo() {
    let (tx, rx) = std::sync::mpsc::channel();
    let mut app = test_app();

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::CONTROL)),
        &mut app,
        &tx,
    )
    .expect("handle ctrl+down");

    let msg = rx.recv().expect("sent message");
    assert_eq!(msg, ClientMessage::Redo);
}

#[test]
fn chat_esc_stops_active_session() {
    let (tx, rx) = std::sync::mpsc::channel();
    let mut app = test_app();
    app.attached_session_id = Some(42);

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("handle esc");

    let msg = rx.recv().expect("sent message");
    assert_eq!(msg, ClientMessage::Cancel { request_id: 0 });
    assert!(app.status.is_none(), "no status message on success");
}

#[test]
fn chat_esc_no_session_shows_status() {
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = test_app();
    app.attached_session_id = None;

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("handle esc");

    assert_eq!(app.status.as_deref(), Some("no session attached"));
}

#[test]
fn chat_alt_enter_continues_generation() {
    let (tx, rx) = std::sync::mpsc::channel();
    let mut app = test_app();
    app.attached_session_id = Some(42);
    let next_id = app.next_request_id;

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT)),
        &mut app,
        &tx,
    )
    .expect("handle alt+enter");

    let msg = rx.recv().expect("sent message");
    assert_eq!(
        msg,
        ClientMessage::ContinueGeneration {
            request_id: next_id
        }
    );
    assert!(
        app.display_for(0).active.contains(&next_id),
        "request_id should be in active set"
    );
    assert_eq!(
        app.next_request_id,
        next_id.wrapping_add(1),
        "next_request_id should be incremented"
    );
    assert!(app.status.is_none(), "no status message on success");
}

#[test]
fn chat_alt_enter_no_session_shows_status() {
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = test_app();
    app.attached_session_id = None;

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT)),
        &mut app,
        &tx,
    )
    .expect("handle alt+enter");

    assert_eq!(app.status.as_deref(), Some("no session attached"));
}

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
fn paste_event_inserts_into_credential_input() {
    let mut app = test_app();
    let (tx, _rx) = std::sync::mpsc::channel();

    app.page = Page::AIProviders;
    app.ai_providers.credential_target = Some("my-account".to_string());
    handle_terminal_event(Event::Paste("sk-abc123".to_string()), &mut app, &tx)
        .expect("handle paste into credential input");
    assert_eq!(app.ai_providers.credential_input.text, "sk-abc123");
    assert_eq!(app.ai_providers.credential_input.cursor, 9);
}

#[test]
fn paste_event_inserts_into_new_form_name_field() {
    let mut app = test_app();
    let (tx, _rx) = std::sync::mpsc::channel();

    app.page = Page::AIProviders;
    app.ai_providers.view = AIProvidersView::NewForm;
    app.ai_providers.new_name_state.focus();
    handle_terminal_event(Event::Paste("My Account".to_string()), &mut app, &tx)
        .expect("handle paste into name field");
    assert_eq!(app.ai_providers.new_name_state.value(), "My Account");
    assert_eq!(app.ai_providers.new_name_state.position(), 10);
}

#[test]
fn paste_event_inserts_into_new_form_api_key_field() {
    let mut app = test_app();
    let (tx, _rx) = std::sync::mpsc::channel();

    app.page = Page::AIProviders;
    app.ai_providers.view = AIProvidersView::NewForm;
    // Focus the API key field by focusing name then moving focus (simulate).
    // TextState defaults to unfocused, so we directly focus the right field.
    app.ai_providers.new_api_key_state.focus();
    handle_terminal_event(Event::Paste("sk-secret".to_string()), &mut app, &tx)
        .expect("handle paste into API key field");
    assert_eq!(app.ai_providers.new_api_key_state.value(), "sk-secret");
    assert_eq!(app.ai_providers.new_api_key_state.position(), 9);
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
        },
    );
    app.next_request_id += 1;

    app.navigate_history_up();
    // After loading a long history entry and setting cursor to end,
    // scroll_offset should be adjusted so the cursor is visible.
    let inner = 78; // 80 - 2 borders
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
        },
    );
    app.next_request_id += 1;

    app.navigate_history_down();
    // After restoring the long draft, scroll_offset should ensure cursor is visible.
    let inner = 78;
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
fn terminal_event_ctrl_backspace_deletes_word_backward() {
    let mut app = test_app();
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

// ── Session Manager tests ─────────────────────────────────────

// ── Session Manager tests ─────────────────────────────────────

#[test]
fn app_starts_in_chat_page() {
    let app = test_app();
    assert_eq!(app.page, Page::Chat);
    assert!(
        app.show_ctrl_help,
        "help overlay should be visible by default"
    );
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

fn make_session(id: u64, title: &str, model: &str, count: u32) -> choreo_proto::SessionSummary {
    choreo_proto::SessionSummary {
        session_id: id,
        title: Some(title.to_string()),
        selected_model: Some(model.to_string()),
        reasoning_effort: None,
        parent_session_id: None,
        working_dir: None,
        created_at: 1705314000,
        turn_count: count,
        max_turns: None,
        status: choreo_proto::SessionStatus::Inactive,
        active_tool_groups: Vec::new(),
        account_name: None,
        token_usage: None,
        context_window: None,
        last_prompt_tokens: None,
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
    assert_eq!(detail.turn_count, 5);
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
        let mut app = test_app();
        app.page = Page::SessionManager;
        app.session_mgr.set_sessions(vec![
            make_session(1, "first", "gpt-4", 3),
            make_session(2, "second", "claude", 5),
        ]);
        app
    }

    #[test]
    fn session_manager_esc_returns_to_chat() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = make_sm_app();

        handle_terminal_event(
            Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            &mut app,
            &tx,
        )
        .expect("handle esc");

        assert_eq!(app.page, Page::Chat);
    }

    #[test]
    fn session_manager_q_returns_to_chat() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = make_sm_app();

        handle_terminal_event(
            Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)),
            &mut app,
            &tx,
        )
        .expect("handle q");

        assert_eq!(app.page, Page::Chat);
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
    fn session_manager_ctrl_c_does_nothing() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = make_sm_app();

        handle_terminal_event(
            Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            &mut app,
            &tx,
        )
        .expect("handle ctrl+c");

        assert!(!app.should_quit);
        assert_eq!(app.page, Page::SessionManager);
    }

    #[test]
    fn chat_ctrl_s_enters_session_manager() {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut app = test_app();
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
                working_dir: None,
                max_turns: None,
                context_config: None,
                account_name: None,
                selected_model: None,
                reasoning_effort: None,
            }
        );
    }
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

// ── AI Providers new-account form (tui-prompts) ────────────

fn setup_providers_new_form(app: &mut App) {
    app.page = Page::AIProviders;
    app.ai_providers.enter_new_form();
}

#[test]
fn ai_providers_new_form_entering_focuses_name() {
    let mut app = test_app();
    setup_providers_new_form(&mut app);
    assert!(app.ai_providers.new_name_state.is_focused());
    assert!(!app.ai_providers.new_provider_state.is_focused());
    assert!(!app.ai_providers.new_api_key_state.is_focused());
}

#[test]
fn ai_providers_new_form_enter_advances_name_to_provider() {
    let mut app = test_app();
    let (tx, _rx) = std::sync::mpsc::channel();
    setup_providers_new_form(&mut app);

    // Type a valid name
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("char m");
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("char y");

    // Enter to advance
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("enter");

    assert!(!app.ai_providers.new_name_state.is_focused());
    assert!(app.ai_providers.new_provider_state.is_focused());
    assert!(!app.ai_providers.new_api_key_state.is_focused());
    assert!(app.ai_providers.add_error.is_none());
}

#[test]
fn ai_providers_new_form_enter_advances_provider_to_apikey() {
    let mut app = test_app();
    let (tx, _rx) = std::sync::mpsc::channel();
    setup_providers_new_form(&mut app);

    // Type name and advance
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("char a");
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("enter advance name");

    assert!(app.ai_providers.new_provider_state.is_focused());

    // Enter on provider advances to API key
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("enter advance provider");

    assert!(!app.ai_providers.new_provider_state.is_focused());
    assert!(app.ai_providers.new_api_key_state.is_focused());
}

#[test]
fn ai_providers_new_form_name_validation_empty() {
    let mut app = test_app();
    let (tx, _rx) = std::sync::mpsc::channel();
    setup_providers_new_form(&mut app);

    // Enter on empty name should show error and stay on name
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("enter empty name");

    assert!(
        app.ai_providers.new_name_state.is_focused(),
        "should stay on name field when name is empty"
    );
    assert_eq!(
        app.ai_providers.add_error.as_deref(),
        Some("Account name is required"),
    );
}

#[test]
fn ai_providers_new_form_name_validation_invalid() {
    let mut app = test_app();
    let (tx, _rx) = std::sync::mpsc::channel();
    setup_providers_new_form(&mut app);

    // Type uppercase (invalid - must be lowercase)
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('U'), KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("char uppercase");
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("enter invalid name");

    assert!(
        app.ai_providers.new_name_state.is_focused(),
        "should stay on name field when name is invalid"
    );
    assert!(app.ai_providers.add_error.is_some());
}

#[test]
fn ai_providers_new_form_esc_cancels() {
    let mut app = test_app();
    let (tx, _rx) = std::sync::mpsc::channel();
    setup_providers_new_form(&mut app);

    assert_eq!(app.ai_providers.view, AIProvidersView::NewForm);

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("esc cancel");

    assert_eq!(app.ai_providers.view, AIProvidersView::List);
}

#[test]
fn ai_providers_new_form_enter_on_apikey_submits() {
    let mut app = test_app();
    let (tx, rx) = std::sync::mpsc::channel();
    setup_providers_new_form(&mut app);

    // Type a valid name
    for c in "my-account".chars() {
        handle_terminal_event(
            Event::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)),
            &mut app,
            &tx,
        )
        .expect("char");
    }
    // Advance to provider
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("enter to provider");
    // Advance to API key
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("enter to apikey");

    // Submit
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("enter submit");

    // Should be back in list view
    assert_eq!(app.ai_providers.view, AIProvidersView::List);
    // Should have sent AddAccount
    let msg = rx.recv().expect("AddAccount message");
    assert_eq!(
        msg,
        ClientMessage::AddAccount {
            name: "my-account".to_string(),
            provider: "openai".to_string(),
            base_url: None,
            streaming: None,
            retry_max_attempts: None,
            connect_timeout_secs: None,
            request_timeout_secs: None,
        }
    );
}

#[test]
fn ai_providers_new_form_keys_go_to_correct_field() {
    let mut app = test_app();
    let (tx, _rx) = std::sync::mpsc::channel();
    setup_providers_new_form(&mut app);

    // Name is focused by default, typing 'x' goes to name
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("char x on name");
    assert_eq!(app.ai_providers.new_name_state.value(), "x");
    assert_eq!(app.ai_providers.new_api_key_state.value(), "");

    // Advance to provider (Enter is handled by our code, not SelectState)
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("enter to provider");
    assert!(app.ai_providers.new_provider_state.is_focused());

    // Advance to API key
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("enter to apikey");
    assert!(app.ai_providers.new_api_key_state.is_focused());

    // Type in API key — goes to the API key field, not name
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("char s on apikey");
    assert_eq!(app.ai_providers.new_api_key_state.value(), "s");
    // The name field should not receive this character (still has "x")
    assert_eq!(app.ai_providers.new_name_state.value(), "x");
}

#[test]
fn ai_providers_new_form_jk_remapped_to_up_down_on_provider() {
    let mut app = test_app();
    let (tx, _rx) = std::sync::mpsc::channel();
    setup_providers_new_form(&mut app);

    // Type name and advance to provider
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("char a");
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("enter to provider");

    assert_eq!(app.ai_providers.new_provider_state.focused_index(), 0);

    // j/j should be remapped to Down by the event handler.
    // SelectState needs options rendered to navigate; test that the
    // event handler reaches the state (no panic, focus unchanged
    // because option_count is 0 before rendering).
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("j on provider");
    // j did NOT leak through as a character into any text field
    assert_eq!(app.ai_providers.new_name_state.value(), "a");
    assert_eq!(app.ai_providers.new_api_key_state.value(), "");

    // On the name field, 'k' should type normally (not remapped)
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("esc cancel");
    setup_providers_new_form(&mut app);
    // Now on name field, type 'k' — should go into the name buffer
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("k on name");
    assert_eq!(
        app.ai_providers.new_name_state.value(),
        "k",
        "k should type into name field when not on provider"
    );
}

#[test]
fn ai_providers_new_form_down_up_navigates_provider() {
    let mut app = test_app();
    let (tx, _rx) = std::sync::mpsc::channel();
    setup_providers_new_form(&mut app);

    // Type name and advance to provider
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("char a");
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("enter to provider");

    // Up/Down navigation reaches SelectState (index stays 0 because
    // option_count is 0 before rendering, but no crash).
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("down on provider");
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("up on provider");
    // No assertion on index — option_count is 0 before render.
    // This test validates that Down/Up don't trigger other side effects.
}

// ── handle_daemon_message progress bar integration ──

#[test]
fn daemon_message_session_state_updates_progress_for_attached_session() {
    let mut app = test_app();
    let (tx, _rx) = std::sync::mpsc::channel();
    app.attached_session_id = Some(7);

    handle_daemon_message(
        DaemonMessage::SessionState {
            session_id: 7,
            title: None,
            selected_model: None,
            parent_session_id: None,
            working_dir: None,
            max_turns: None,
            turns: std::collections::BTreeMap::new(),
            active_tool_groups: vec![],
            token_usage: Some(TokenUsage {
                input_tokens: 1,
                output_tokens: 2,
                total_tokens: 3,
            }),
            context_window: Some(4096),
            last_prompt_tokens: Some(1),
            reasoning_effort: None,
            reasoning_capability: None,
            status: SessionStatus::Inactive,
        },
        &mut app,
        &tx,
    )
    .expect("handle_daemon_message should succeed");

    assert_eq!(
        app.display_for(7).token_usage,
        Some(TokenUsage {
            input_tokens: 1,
            output_tokens: 2,
            total_tokens: 3,
        })
    );
    assert_eq!(app.display_for(7).context_window, Some(4096));
    assert_eq!(app.attached_status, Some(SessionStatus::Inactive));
    assert!(app.attached_tool_groups.is_empty());
    assert!(app.display_for(7).progress_dirty);
}

#[test]
fn daemon_message_session_state_sets_tool_groups() {
    let mut app = test_app();
    let (tx, _rx) = std::sync::mpsc::channel();

    handle_daemon_message(
        DaemonMessage::SessionState {
            session_id: 7,
            title: None,
            selected_model: None,
            parent_session_id: None,
            working_dir: None,
            max_turns: None,
            turns: std::collections::BTreeMap::new(),
            active_tool_groups: vec!["core".into(), "browser".into()],
            token_usage: None,
            context_window: None,
            last_prompt_tokens: None,
            reasoning_effort: None,
            reasoning_capability: None,
            status: SessionStatus::Inactive,
        },
        &mut app,
        &tx,
    )
    .expect("handle_daemon_message should succeed");

    assert_eq!(app.attached_tool_groups, vec!["core", "browser"]);
}

#[test]
fn daemon_message_session_state_ignores_wrong_session() {
    let mut app = test_app();
    let (tx, _rx) = std::sync::mpsc::channel();
    app.attached_session_id = Some(7);

    handle_daemon_message(
        DaemonMessage::SessionState {
            session_id: 99, // different from attached_session_id
            title: None,
            selected_model: None,
            parent_session_id: None,
            working_dir: None,
            max_turns: None,
            turns: std::collections::BTreeMap::new(),
            active_tool_groups: vec![],
            token_usage: Some(TokenUsage {
                input_tokens: 99,
                output_tokens: 99,
                total_tokens: 99,
            }),
            context_window: Some(1024),
            last_prompt_tokens: None,
            reasoning_effort: None,
            reasoning_capability: None,
            status: SessionStatus::Inactive,
        },
        &mut app,
        &tx,
    )
    .expect("handle_daemon_message should succeed");

    // TurnEventHandler::handle_session_state sets these fields unconditionally
    // (it does not check session_id), while progress_dirty is only set by the
    // manual guard in connection.rs which skips non-attached sessions.
    assert_eq!(
        app.display_for(7).token_usage,
        Some(TokenUsage {
            input_tokens: 99,
            output_tokens: 99,
            total_tokens: 99,
        })
    );
    assert_eq!(app.display_for(7).context_window, Some(1024));
    assert_eq!(app.attached_status, Some(SessionStatus::Inactive));
    assert!(app.attached_tool_groups.is_empty());
    assert!(!app.display_for(7).progress_dirty);
}

#[test]
fn daemon_message_done_with_token_usage_updates_progress() {
    let mut app = test_app();
    let (tx, _rx) = std::sync::mpsc::channel();

    handle_daemon_message(
        DaemonMessage::Done {
            session_id: 0,
            request_id: 1,
            token_usage: Some(TokenUsage {
                input_tokens: 5,
                output_tokens: 10,
                total_tokens: 15,
            }),
            last_prompt_tokens: Some(5),
        },
        &mut app,
        &tx,
    )
    .expect("handle_daemon_message should succeed");

    assert_eq!(
        app.display_for(0).token_usage,
        Some(TokenUsage {
            input_tokens: 5,
            output_tokens: 10,
            total_tokens: 15,
        })
    );
    assert!(app.display_for(0).progress_dirty);
}

#[test]
fn daemon_message_done_without_token_usage_does_not_change_progress() {
    let mut app = test_app();
    let (tx, _rx) = std::sync::mpsc::channel();

    handle_daemon_message(
        DaemonMessage::Done {
            session_id: 0,
            request_id: 1,
            token_usage: None,
            last_prompt_tokens: None,
        },
        &mut app,
        &tx,
    )
    .expect("handle_daemon_message should succeed");

    // Must remain at defaults — no data written, no dirty flag set.
    assert!(app.display_for(0).token_usage.is_none());
    assert!(!app.display_for(0).progress_dirty);
}

#[test]
fn handle_session_status_changed_updates_attached_status() {
    let mut app = test_app();
    assert!(app.attached_status.is_none());

    // With no attached session, status should not be cached.
    app.handle_session_status_changed(42, &SessionStatus::Inference);
    assert!(app.attached_status.is_none());

    // Once attached, a status change for that session should be cached.
    app.attached_session_id = Some(42);
    app.handle_session_status_changed(42, &SessionStatus::Inference);
    assert_eq!(app.attached_status, Some(SessionStatus::Inference));

    // A status change for a different session should not overwrite.
    app.handle_session_status_changed(99, &SessionStatus::Sleeping);
    assert_eq!(app.attached_status, Some(SessionStatus::Inference));

    // A subsequent change for the attached session should update.
    app.handle_session_status_changed(42, &SessionStatus::ToolCall("test".into()));
    assert_eq!(
        app.attached_status,
        Some(SessionStatus::ToolCall("test".into()))
    );
}

#[test]
fn handle_turn_appended_with_displayed_image_populates_rendered_images() {
    let mut app = test_app();
    let metadata = ImageMetadata {
        mime_type: "image/png".to_string(),
        width: 640,
        height: 480,
        byte_len: 100,
        alt: Some("test image".to_string()),
    };
    let turn = Turn {
        created_at: choreo_proto::TimestampMs::now(),
        undone: false,
        error: None,
        user_text: Some("generate an image".to_string()),
        assistant_text: None,
        assistant_reasoning: None,
        tool_calls: vec![],
        token_usage: None,
        tool_results: vec![],
        displayed_images: vec![DisplayedImageRecord {
            metadata: metadata.clone(),
            data: vec![0u8; 100],
            tool_call_id: None,
        }],
    };
    app.handle_turn_appended(0, 1, turn);

    let images = app.rendered_images.get(&0).and_then(|m| m.get(&1)).unwrap();
    assert_eq!(images.len(), 1);
    let img = images.get(&0).unwrap();
    assert_eq!(img.metadata.mime_type, "image/png");
    assert_eq!(img.metadata.width, 640);
    assert_eq!(img.metadata.height, 480);
    assert_eq!(img.data.len(), 100);
    assert!(img.pending_job.is_none());
    assert!(img.protocols.is_empty());
}

// ── InputBuffer scroll_offset ──────────────────────────────────

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

// ── Entry handling for Continue / Stop / Undo / Redo ──────────────────

#[test]
fn enter_continue_when_attached_sends_continue_generation() {
    let mut app = test_app();
    app.attached_session_id = Some(1);
    app.input.text = "/continue".to_string();
    let (tx, rx) = std::sync::mpsc::channel();

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("handle enter");

    assert_eq!(app.status.as_deref(), Some("> continue"));
    assert!(app.display_for(0).active.contains(&1));
    let msg = rx.recv().expect("should send ContinueGeneration");
    assert_eq!(msg, ClientMessage::ContinueGeneration { request_id: 1 });
}

#[test]
fn enter_continue_when_not_attached_shows_error() {
    let mut app = test_app();
    app.attached_session_id = None;
    app.input.text = "/continue".to_string();
    let (tx, _rx) = std::sync::mpsc::channel();

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("handle enter");

    assert_eq!(app.status.as_deref(), Some("no session attached"));
}

#[test]
fn enter_continue_scrolls_to_bottom() {
    let mut app = test_app();
    app.attached_session_id = Some(1);
    app.history_viewport = HistoryViewport {
        width: 80,
        height: 5,
    };
    // Add enough content to be scrollable.
    add_user_text(&mut app, "a");
    add_user_text(&mut app, "b");
    add_user_text(&mut app, "c");
    // Scroll up so we're not at the bottom.
    app.scroll_up(2);
    let scrolled = app.effective_scroll();
    assert!(scrolled > 0, "should be scrolled up");
    app.input.text = "/continue".to_string();
    let (tx, _rx) = std::sync::mpsc::channel();

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("handle enter");

    assert_eq!(app.effective_scroll(), 0, "should scroll to bottom");
}

#[test]
fn enter_stop_when_attached_sends_cancel_all() {
    let mut app = test_app();
    app.attached_session_id = Some(1);
    app.input.text = "/stop".to_string();
    let (tx, rx) = std::sync::mpsc::channel();

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("handle enter");

    assert_eq!(app.status.as_deref(), Some("> stop"));
    let msg = rx.recv().expect("should send Cancel");
    assert_eq!(msg, ClientMessage::Cancel { request_id: 0 });
}

#[test]
fn enter_stop_when_not_attached_shows_error() {
    let mut app = test_app();
    app.attached_session_id = None;
    app.input.text = "/stop".to_string();
    let (tx, _rx) = std::sync::mpsc::channel();

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("handle enter");

    assert_eq!(app.status.as_deref(), Some("no session attached"));
}

#[test]
fn enter_undo_sends_undo() {
    let mut app = test_app();
    app.input.text = "/undo".to_string();
    let (tx, rx) = std::sync::mpsc::channel();

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("handle enter");

    assert_eq!(app.status.as_deref(), Some("> undo"));
    let msg = rx.recv().expect("should send Undo");
    assert_eq!(msg, ClientMessage::Undo);
}

#[test]
fn enter_redo_sends_redo() {
    let mut app = test_app();
    app.input.text = "/redo".to_string();
    let (tx, rx) = std::sync::mpsc::channel();

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("handle enter");

    assert_eq!(app.status.as_deref(), Some("> redo"));
    let msg = rx.recv().expect("should send Redo");
    assert_eq!(msg, ClientMessage::Redo);
}

#[test]
fn enter_stop_does_not_scroll() {
    let mut app = test_app();
    app.attached_session_id = Some(1);
    app.history_viewport = HistoryViewport {
        width: 80,
        height: 5,
    };
    add_user_text(&mut app, "a");
    add_user_text(&mut app, "b");
    app.scroll_up(1);
    let scrolled = app.effective_scroll();
    assert!(scrolled > 0, "should be scrolled up");
    app.input.text = "/stop".to_string();
    let (tx, _rx) = std::sync::mpsc::channel();

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("handle enter");

    // Stop should NOT scroll to bottom (unlike Continue).
    assert!(
        app.effective_scroll() > 0,
        "should preserve scroll position"
    );
}

// ── Ctrl+R reasoning effort cycling ──────────────────────────────

#[test]
fn ctrl_r_no_session_shows_message() {
    let mut app = test_app();
    let (tx, _rx) = std::sync::mpsc::channel();

    app.attached_session_id = None;
    app.display_for(0).reasoning_capability = Some(ReasoningCapability {
        available_effort_levels: vec![
            "off".to_string(),
            "low".to_string(),
            "medium".to_string(),
            "high".to_string(),
        ],
    });

    // Ctrl+R should show message even without session attached
    // (the handler checks capability, not session_id).
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL)),
        &mut app,
        &tx,
    )
    .expect("handle ctrl+r");

    assert_eq!(app.display_for(0).reasoning_effort.as_deref(), Some("low"));
    assert_eq!(app.status.as_deref(), Some("reasoning: low"));
}

#[test]
fn ctrl_r_cycles_through_valid_slugs() {
    let mut app = test_app();
    let (tx, rx) = std::sync::mpsc::channel();

    app.display_for(0).reasoning_capability = Some(ReasoningCapability {
        available_effort_levels: vec![
            "off".to_string(),
            "low".to_string(),
            "medium".to_string(),
            "high".to_string(),
        ],
    });

    // First Ctrl+R: off -> low
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL)),
        &mut app,
        &tx,
    )
    .expect("handle ctrl+r 1");
    assert_eq!(app.display_for(0).reasoning_effort.as_deref(), Some("low"));
    assert_eq!(app.status.as_deref(), Some("reasoning: low"));
    let msg = rx.recv().expect("SetReasoningEffort 1");
    assert_eq!(
        msg,
        ClientMessage::SetReasoningEffort {
            effort: "low".to_string()
        }
    );

    // Second Ctrl+R: low -> medium
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL)),
        &mut app,
        &tx,
    )
    .expect("handle ctrl+r 2");
    assert_eq!(
        app.display_for(0).reasoning_effort.as_deref(),
        Some("medium")
    );
    let msg = rx.recv().expect("SetReasoningEffort 2");
    assert_eq!(
        msg,
        ClientMessage::SetReasoningEffort {
            effort: "medium".to_string()
        }
    );

    // Third: medium -> high
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL)),
        &mut app,
        &tx,
    )
    .expect("handle ctrl+r 3");
    assert_eq!(app.display_for(0).reasoning_effort.as_deref(), Some("high"));
    let msg = rx.recv().expect("SetReasoningEffort 3");
    assert_eq!(
        msg,
        ClientMessage::SetReasoningEffort {
            effort: "high".to_string()
        }
    );

    // Fourth: high -> off (wraps around)
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL)),
        &mut app,
        &tx,
    )
    .expect("handle ctrl+r 4");
    assert_eq!(app.display_for(0).reasoning_effort.as_deref(), Some("off"));
    let msg = rx.recv().expect("SetReasoningEffort 4");
    assert_eq!(
        msg,
        ClientMessage::SetReasoningEffort {
            effort: "off".to_string()
        }
    );
}

#[test]
fn ctrl_r_non_reasoning_model_shows_message() {
    let mut app = test_app();
    let (tx, _rx) = std::sync::mpsc::channel();

    // No reasoning capability set (model does not support reasoning).
    app.display_for(0).reasoning_capability = None;

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL)),
        &mut app,
        &tx,
    )
    .expect("handle ctrl+r");

    assert_eq!(
        app.status.as_deref(),
        Some("model does not support reasoning")
    );
    // Effort should remain unchanged (still None).
    assert!(app.display_for(0).reasoning_effort.is_none());
}

#[test]
fn ctrl_r_google_off_on() {
    let mut app = test_app();
    let (tx, rx) = std::sync::mpsc::channel();

    // Google Gemini style: only "off" and "on".
    app.display_for(0).reasoning_capability = Some(ReasoningCapability {
        available_effort_levels: vec!["off".to_string(), "on".to_string()],
    });

    // First Ctrl+R: off -> on
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL)),
        &mut app,
        &tx,
    )
    .expect("handle ctrl+r 1");
    assert_eq!(app.display_for(0).reasoning_effort.as_deref(), Some("on"));
    let msg = rx.recv().expect("SetReasoningEffort 1");
    assert_eq!(
        msg,
        ClientMessage::SetReasoningEffort {
            effort: "on".to_string()
        }
    );

    // Second Ctrl+R: on -> off (wraps)
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL)),
        &mut app,
        &tx,
    )
    .expect("handle ctrl+r 2");
    assert_eq!(app.display_for(0).reasoning_effort.as_deref(), Some("off"));
    let msg = rx.recv().expect("SetReasoningEffort 2");
    assert_eq!(
        msg,
        ClientMessage::SetReasoningEffort {
            effort: "off".to_string()
        }
    );
}

#[test]
fn reasoning_effort_set_updates_session_state() {
    let mut app = test_app();
    let (tx, _rx) = std::sync::mpsc::channel();

    app.attached_session_id = Some(42);

    handle_daemon_message(
        DaemonMessage::ReasoningEffortSet {
            session_id: 0,
            effort: "high".to_string(),
        },
        &mut app,
        &tx,
    )
    .expect("handle ReasoningEffortSet");

    assert_eq!(app.display_for(0).reasoning_effort.as_deref(), Some("high"));
}

#[test]
fn ctrl_r_with_empty_capability_shows_message() {
    let mut app = test_app();
    let (tx, _rx) = std::sync::mpsc::channel();

    // Capability exists but has empty available_effort_levels.
    app.display_for(0).reasoning_capability = Some(ReasoningCapability {
        available_effort_levels: vec![],
    });

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL)),
        &mut app,
        &tx,
    )
    .expect("handle ctrl+r");

    assert_eq!(
        app.status.as_deref(),
        Some("model does not support reasoning")
    );
}
