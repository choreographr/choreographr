use crate::connection::handle_terminal_event;
use crate::markdown_render::*;
use crate::state::*;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use tai_client_core::DaemonMessageHandler;
use tai_proto::{ClientMessage, OutputStream};

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

    assert_eq!(app.input, "hi");
    assert!(rx.try_recv().is_err());
}

#[tokio::test]
async fn terminal_event_submits_run_input() {
    let mut app = App::new("/tmp/tai.sock".to_string(), "Kitty".to_string());
    app.input = "hello".to_string();
    let (tx, rx) = std::sync::mpsc::channel();

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("handle enter");

    assert!(app.input.is_empty());
    let message = rx.recv().expect("sent message");
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
    let (tx, _rx) = std::sync::mpsc::channel();

    let mut app = App::new("/tmp/tai.sock".to_string(), "Kitty".to_string());
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("handle q");
    assert!(app.should_quit);

    let mut app = App::new("/tmp/tai.sock".to_string(), "Kitty".to_string());
    app.input = "q".to_string();
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("handle q");
    assert!(!app.should_quit);
    assert_eq!(app.input, "qq");
}

#[tokio::test]
async fn terminal_event_ctrl_c_quits() {
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = App::new("/tmp/tai.sock".to_string(), "Kitty".to_string());

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        &mut app,
        &tx,
    )
    .expect("handle ctrl+c");

    assert!(app.should_quit);
}

#[tokio::test]
async fn mouse_scroll_outside_history_box_does_not_change_scroll() {
    let (tx, _rx) = std::sync::mpsc::channel();
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

// ── Session Manager tests ─────────────────────────────────────

#[test]
fn app_starts_in_chat_page() {
    let app = App::new("/tmp/tai.sock".to_string(), "Halfblocks".to_string());
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
        parent_session_id: None,
        cwd: None,
        created_at: 1705314000,
        message_count: count,
        max_turns: None,
        status: tai_proto::SessionStatus::Inactive,
    }
}

#[test]
fn session_manager_set_sessions_selects_first() {
    let mut state = SessionManagerState::new();
    state.set_sessions(vec![make_session(1, "a", "m1", 5), make_session(2, "b", "m2", 3)]);
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
        let mut app = App::new("/tmp/tai.sock".to_string(), "Kitty".to_string());
        app.page = Page::SessionManager;
        app.session_mgr.set_sessions(vec![
            make_session(1, "first", "gpt-4", 3),
            make_session(2, "second", "claude", 5),
        ]);
        app
    }

    #[tokio::test]
    async fn session_manager_esc_returns_to_chat() {
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

    #[tokio::test]
    async fn session_manager_q_returns_to_chat() {
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

    #[tokio::test]
    async fn session_manager_j_moves_selection_down() {
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

    #[tokio::test]
    async fn session_manager_enter_switches_session_and_returns_to_chat() {
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

    #[tokio::test]
    async fn session_manager_ctrl_c_still_quits() {
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

    #[tokio::test]
    async fn chat_ctrl_s_enters_session_manager() {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut app = App::new("/tmp/tai.sock".to_string(), "Kitty".to_string());
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

    #[tokio::test]
    async fn session_manager_i_enters_detail() {
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

    #[tokio::test]
    async fn session_manager_detail_b_returns_to_list() {
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

    #[tokio::test]
    async fn session_manager_detail_enter_switches_session() {
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

    #[tokio::test]
    async fn session_manager_n_sends_create_session() {
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
            }
        );
    }
}
