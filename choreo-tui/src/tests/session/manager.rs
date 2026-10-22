use crate::state::*;
use crate::tests::support::{make_session_full, test_app};
use choreo_proto::{ClientMessage, ClientMessageType};
use crossterm::event::Event;

// ── Session Manager tests ─────────────────────────────────────

// ── Session Manager tests ─────────────────────────────────────

#[test]
fn app_starts_in_chat_page() {
    let app = test_app();
    assert_eq!(app.page, Page::Chat);
    assert!(
        app.show_help_overlay,
        "help overlay should be visible by default"
    );
}

// The suggested rewrite (asserting on a match of the concrete collection type)
// is worse; clippy::assert_is_empty fires on plain is_empty asserts here.
#[expect(clippy::assert_is_empty)]
#[test]
fn session_manager_state_new_is_empty() {
    let state = SessionManagerState::new();
    assert!(state.sessions.is_empty());
    assert!(state.selection.is_none());
    assert_eq!(state.view, SessionManagerView::List);
    assert_eq!(state.scroll, 0);
    assert_eq!(state.viewport_height, 0);
    assert!(state.detail_data.is_none());
}

#[expect(clippy::assert_is_empty)] // plain is_empty asserts read best here
#[test]
fn session_manager_set_sessions_empty() {
    let mut state = SessionManagerState::new();
    state.set_sessions(vec![]);
    assert!(state.sessions.is_empty());
    assert!(state.selection.is_none());
}

#[test]
fn session_manager_set_sessions_selects_first() {
    let mut state = SessionManagerState::new();
    state.set_sessions(vec![
        make_session_full(1, "a", "m1", 5),
        make_session_full(2, "b", "m2", 3),
    ]);
    assert_eq!(state.sessions.len(), 2);
    assert_eq!(state.selection, Some(0));
}

#[test]
fn session_manager_select_up_down() {
    let mut state = SessionManagerState::new();
    state.set_sessions(vec![
        make_session_full(1, "a", "m1", 5),
        make_session_full(2, "b", "m2", 3),
        make_session_full(3, "c", "m3", 7),
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
        make_session_full(10, "test-session", "gpt-4", 5),
        make_session_full(20, "other", "claude", 3),
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
    state.set_sessions(vec![make_session_full(1, "a", "m1", 0)]);
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
        make_session_full(1, "a", "m1", 0),
        make_session_full(2, "b", "m2", 0),
        make_session_full(3, "c", "m3", 0),
    ]);
    state.select_down();
    state.select_down();
    assert_eq!(state.selection, Some(2));

    // Refresh sessions — should preserve selection on session 3
    state.set_sessions(vec![
        make_session_full(1, "a", "m1", 0),
        make_session_full(3, "c", "m3", 5),
        make_session_full(4, "d", "m4", 0),
    ]);
    assert_eq!(state.selection, Some(1)); // session 3 is now at index 1
}

use crate::connection::handle_terminal_event;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

fn make_sm_app() -> App {
    let mut app = test_app();
    app.page = Page::SessionManager;
    app.session_mgr.set_sessions(vec![
        make_session_full(1, "first", "gpt-4", 3),
        make_session_full(2, "second", "claude", 5),
    ]);
    app
}

#[test]
fn session_manager_esc_returns_to_chat() {
    let (tx, _rx) = crossbeam_channel::unbounded();
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
    let (tx, _rx) = crossbeam_channel::unbounded();
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
    let (tx, _rx) = crossbeam_channel::unbounded();
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
    let (tx, rx) = crossbeam_channel::unbounded();
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
    assert_eq!(msg.inner, ClientMessageType::UnsubscribeSessionsSummary);
    let msg = rx.recv().expect("sent message");
    assert_eq!(
        msg.inner,
        ClientMessageType::AttachSession { session_id: 1 }
    );
}

#[test]
fn session_manager_ctrl_c_does_nothing() {
    let (tx, _rx) = crossbeam_channel::unbounded();
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
fn session_manager_ctrl_a_does_not_archive() {
    // The list's actions are bare-letter keys; a Ctrl chord must never fire
    // one.  `Ctrl+A` is readline beginning-of-line on the Chat input and
    // must not archive the highlighted session here.
    let (tx, rx) = crossbeam_channel::unbounded();
    let mut app = make_sm_app();
    app.session_mgr
        .set_sessions(vec![make_session_full(7, "s", "m", 1)]);
    app.session_mgr.selection = Some(0);

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL)),
        &mut app,
        &tx,
    )
    .expect("handle ctrl+a");

    assert!(
        rx.try_recv().is_err(),
        "Ctrl+A must not send SetSessionArchived"
    );
}

#[test]
fn chat_alt_s_enters_session_manager() {
    let (tx, rx) = crossbeam_channel::unbounded();
    let mut app = test_app();
    assert_eq!(app.page, Page::Chat);

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::ALT)),
        &mut app,
        &tx,
    )
    .expect("handle ctrl+s");

    assert_eq!(app.page, Page::SessionManager);
    let msg = rx.recv().expect("sent message");
    assert_eq!(msg.inner, ClientMessageType::ListSessions);
    let msg = rx.recv().expect("sent message");
    assert_eq!(msg.inner, ClientMessageType::SubscribeSessionsSummary);
}

#[test]
fn chat_alt_s_highlights_previously_viewed_session() {
    let (tx, _rx) = crossbeam_channel::unbounded();
    let mut app = test_app();
    // The user is viewing session 42 on the chat page.
    app.attached_session_id = Some(42);
    // The list is already loaded from an earlier visit, and the
    // previously highlighted row is a *different* session (index 2).
    app.session_mgr.set_sessions(vec![
        make_session_full(7, "old", "m1", 1),
        make_session_full(42, "viewed", "m2", 5),
        make_session_full(3, "other", "m3", 2),
    ]);
    app.session_mgr.selection = Some(2);

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::ALT)),
        &mut app,
        &tx,
    )
    .expect("handle ctrl+s");

    assert_eq!(app.page, Page::SessionManager);
    let sel = app.session_mgr.selection.expect("selection set");
    assert_eq!(
        app.session_mgr.sessions[sel].session_id, 42,
        "highlight must follow the session the user was viewing"
    );
}

#[test]
fn chat_alt_s_remembers_viewed_session_before_list_arrives() {
    // First launch: no session list loaded yet when Alt+S is pressed,
    // so the highlight is deferred until the ListSessions reply arrives.
    let (tx, _rx) = crossbeam_channel::unbounded();
    let mut app = test_app();
    app.attached_session_id = Some(42);

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::ALT)),
        &mut app,
        &tx,
    )
    .expect("handle ctrl+s");

    assert_eq!(app.page, Page::SessionManager);
    assert_eq!(app.session_mgr.pending_select, Some(42));
    assert_eq!(app.session_mgr.selection, None);

    // The daemon's Sessions reply populates the list; the pending
    // highlight is applied (and consumed).
    app.session_mgr.set_sessions(vec![
        make_session_full(7, "old", "m1", 1),
        make_session_full(42, "viewed", "m2", 5),
        make_session_full(3, "other", "m3", 2),
    ]);
    let sel = app
        .session_mgr
        .selection
        .expect("selection set after reply");
    assert_eq!(app.session_mgr.sessions[sel].session_id, 42);
    assert_eq!(app.session_mgr.pending_select, None);
}

#[test]
fn session_manager_i_enters_detail() {
    let (tx, _rx) = crossbeam_channel::unbounded();
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
fn session_manager_tab_toggles_archived_view() {
    let (tx, _rx) = crossbeam_channel::unbounded();
    let mut app = make_sm_app();
    assert_eq!(app.session_mgr.view, SessionManagerView::List);

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("handle tab");
    assert_eq!(app.session_mgr.view, SessionManagerView::Archived);

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("handle tab");
    assert_eq!(app.session_mgr.view, SessionManagerView::List);
}

#[test]
fn session_manager_p_sends_toggled_pin() {
    let (tx, rx) = crossbeam_channel::unbounded::<ClientMessage>();
    let mut app = make_sm_app();
    // The highlighted session (1) starts un-pinned.
    assert_eq!(app.session_mgr.sessions[0].session_id, 1);

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("handle p");
    assert_eq!(
        rx.recv().expect("sent message").inner,
        ClientMessageType::SetSessionPinned {
            session_id: 1,
            pinned: true,
        }
    );

    // After the daemon's broadcast applies the pin, `p` sends the inverse.
    app.session_mgr.apply_session_flags(1, true, None);
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("handle p");
    assert_eq!(
        rx.recv().expect("sent message").inner,
        ClientMessageType::SetSessionPinned {
            session_id: 1,
            pinned: false,
        }
    );
}

#[test]
fn session_manager_a_archives_on_list_and_unarchives_on_archived() {
    let (tx, rx) = crossbeam_channel::unbounded::<ClientMessage>();
    let mut app = make_sm_app();

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("handle a");
    assert_eq!(
        rx.recv().expect("sent message").inner,
        ClientMessageType::SetSessionArchived {
            session_id: 1,
            archived: true,
        }
    );

    // After the broadcast archives session 1, the archived view holds it
    // and `a` must send the unarchive request.
    app.session_mgr
        .apply_session_flags(1, false, Some(1_705_314_000_500));
    app.session_mgr.toggle_view();
    assert_eq!(app.session_mgr.view, SessionManagerView::Archived);
    assert_eq!(app.session_mgr.sessions[0].session_id, 1);
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("handle a");
    assert_eq!(
        rx.recv().expect("sent message").inner,
        ClientMessageType::SetSessionArchived {
            session_id: 1,
            archived: false,
        }
    );
}

#[test]
fn session_manager_detail_b_returns_to_list() {
    let (tx, _rx) = crossbeam_channel::unbounded();
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
    let (tx, rx) = crossbeam_channel::unbounded();
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
    assert_eq!(msg.inner, ClientMessageType::UnsubscribeSessionsSummary);
    let msg = rx.recv().expect("sent message");
    assert_eq!(
        msg.inner,
        ClientMessageType::AttachSession { session_id: 1 }
    );
}

#[test]
fn session_manager_n_sends_create_session() {
    let mut app = make_sm_app();
    let (tx, rx) = crossbeam_channel::unbounded::<ClientMessage>();

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("handle n");

    let msg = rx.recv().expect("sent message");
    assert_eq!(
        msg.inner,
        ClientMessageType::CreateSession {
            title: None,
            parent_session_id: None,
            working_dir: None,
            context_config: None,
            account_name: None,
            selected_model: None,
            reasoning_effort: None,
        }
    );
}

#[test]
fn session_manager_click_selects_and_attaches_session() {
    let (tx, rx) = crossbeam_channel::unbounded();
    let mut app = make_sm_app();
    app.last_terminal_size = Some((100, 40));
    // The list renderer uses `window(viewport_height)` where viewport
    // height = terminal height − 4; the click handler uses the same value.
    app.session_mgr.viewport_height = 36;

    let content = page_list_content_rect(Some((100, 40))).expect("geometry");
    // The two sessions are session 1 (index 0) and session 2 (index 1).
    // The table header occupies content row 0; clicking the second
    // session's row (content.y + 2) selects index 1 and attaches to 2.
    handle_terminal_event(
        Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: content.x + 3,
            row: content.y + 2,
            modifiers: KeyModifiers::NONE,
        }),
        &mut app,
        &tx,
    )
    .expect("handle click");

    assert_eq!(
        app.page,
        Page::Chat,
        "clicking a session is equivalent to selecting it and pressing Enter"
    );
    assert_eq!(app.attached_session_id, Some(2));
    let msg = rx.recv().expect("sent message (unsub)");
    assert_eq!(msg.inner, ClientMessageType::UnsubscribeSessionsSummary);
    let msg = rx.recv().expect("sent message");
    assert_eq!(
        msg.inner,
        ClientMessageType::AttachSession { session_id: 2 }
    );
}

#[test]
fn session_manager_click_selects_the_highlighted_session() {
    let (tx, _rx) = crossbeam_channel::unbounded();
    let mut app = make_sm_app();
    app.last_terminal_size = Some((100, 40));
    app.session_mgr.viewport_height = 36;

    let content = page_list_content_rect(Some((100, 40))).expect("geometry");
    handle_terminal_event(
        Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: content.x + 3,
            row: content.y + 2,
            modifiers: KeyModifiers::NONE,
        }),
        &mut app,
        &tx,
    )
    .expect("handle click");

    // The attach succeeded (page flipped to Chat) but the selection still
    // tracks the clicked session in case the send had failed.
    assert_eq!(app.session_mgr.selection, Some(1));
}

#[test]
fn session_manager_click_header_and_border_are_noop() {
    let (tx, rx) = crossbeam_channel::unbounded();
    let mut app = make_sm_app();
    app.last_terminal_size = Some((100, 40));
    app.session_mgr.viewport_height = 36;

    let content = page_list_content_rect(Some((100, 40))).expect("geometry");
    // Click the header row (content.y), and the top block border (0, 0).
    handle_terminal_event(
        Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: content.x + 3,
            row: content.y,
            modifiers: KeyModifiers::NONE,
        }),
        &mut app,
        &tx,
    )
    .expect("handle header click");
    assert_eq!(app.page, Page::SessionManager);
    assert!(rx.try_recv().is_err(), "a header click must not attach");

    handle_terminal_event(
        Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        }),
        &mut app,
        &tx,
    )
    .expect("handle border click");
    assert_eq!(app.page, Page::SessionManager);
    assert!(rx.try_recv().is_err());
}

#[test]
fn session_manager_wheel_scrolls_highlight() {
    let (tx, _rx) = crossbeam_channel::unbounded();
    let mut app = make_sm_app();
    app.last_terminal_size = Some((100, 40));
    app.session_mgr.viewport_height = 36;

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
    assert_eq!(app.session_mgr.selection, Some(1));

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
    assert_eq!(app.session_mgr.selection, Some(0));
}
