use crate::connection::handle_daemon_message;
use crate::state::*;
use crate::test_util::{make_session, test_app};
use choreo_client_core::TurnEventHandler;
use choreo_proto::{
    ClientMessage, DaemonMessage, DisplayedImageRecord, ImageMetadata, SessionEvent, SessionStatus,
    TokenUsage, Turn,
};
use crossterm::event::Event;

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
    assert_eq!(state.viewport_height, 0);
    assert!(state.detail_data.is_none());
}

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
    fn chat_ctrl_s_highlights_previously_viewed_session() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = test_app();
        // The user is viewing session 42 on the chat page.
        app.attached_session_id = Some(42);
        // The list is already loaded from an earlier visit, and the
        // previously highlighted row is a *different* session (index 2).
        app.session_mgr.set_sessions(vec![
            make_session(7, "old", "m1", 1),
            make_session(42, "viewed", "m2", 5),
            make_session(3, "other", "m3", 2),
        ]);
        app.session_mgr.selection = Some(2);

        handle_terminal_event(
            Event::Key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL)),
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
    fn chat_ctrl_s_remembers_viewed_session_before_list_arrives() {
        // First launch: no session list loaded yet when Ctrl+S is pressed,
        // so the highlight is deferred until the ListSessions reply arrives.
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = test_app();
        app.attached_session_id = Some(42);

        handle_terminal_event(
            Event::Key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL)),
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
            make_session(7, "old", "m1", 1),
            make_session(42, "viewed", "m2", 5),
            make_session(3, "other", "m3", 2),
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
                context_config: None,
                account_name: None,
                selected_model: None,
                reasoning_effort: None,
            }
        );
    }
}

// ── multi-session streaming: switching keeps accumulated content ──

/// A turn whose assistant text was streamed in via OutputChunk.
fn streamed_turn(user_text: &str, assistant_text: &str) -> Turn {
    Turn {
        created_at: choreo_proto::TimestampMs::now(),
        undone: false,
        error: None,
        user_text: Some(user_text.to_string()),
        assistant_text: Some(assistant_text.to_string()),
        assistant_reasoning: None,
        tool_calls: vec![],
        token_usage: None,
        tool_results: vec![],
        displayed_images: vec![],
        reasoning_artifact: None,
        reasoning_producer: None,
    }
}

/// The empty placeholder the daemon's attach snapshot carries for an
/// in-flight turn (what `start_turn` inserts on the main session thread
/// before the worker owns the live content).
fn placeholder_turn(user_text: &str) -> Turn {
    Turn {
        created_at: choreo_proto::TimestampMs::now(),
        undone: false,
        error: None,
        user_text: Some(user_text.to_string()),
        assistant_text: None,
        assistant_reasoning: None,
        tool_calls: vec![],
        token_usage: None,
        tool_results: vec![],
        displayed_images: vec![],
        reasoning_artifact: None,
        reasoning_producer: None,
    }
}

#[test]
fn reset_for_session_switch_preserves_accumulated_streaming_state() {
    let mut app = test_app();
    // While viewing session 0, session 7's content accumulated via the
    // all-activity subscription: a streamed turn, its request→turn routing
    // entry, the active-request set, a live token estimate, and a reasoning
    // preference.
    {
        let display = app.display_for(7);
        display
            .view
            .insert_or_replace(5, streamed_turn("user q", "partial answer"));
        display.view.request_to_turn.insert(99, 5);
        display.active.insert(99);
        display.live_output_tokens = 42;
        display.reasoning_override.insert(5, true);
    }

    app.reset_for_session_switch(7);

    let display = app.display_for(7);
    // Live state survives the switch…
    assert_eq!(
        display.view.turns.len(),
        1,
        "accumulated turns must survive the switch"
    );
    assert_eq!(
        display
            .view
            .turns
            .get(&5)
            .unwrap()
            .assistant_text
            .as_deref(),
        Some("partial answer"),
        "streamed content must survive the switch"
    );
    assert_eq!(
        display.view.request_to_turn.get(&99),
        Some(&5),
        "request→turn routing must survive the switch"
    );
    assert!(
        display.active.contains(&99),
        "active request set must survive"
    );
    assert_eq!(display.live_output_tokens, 42);
    assert_eq!(display.reasoning_override.get(&5), Some(&true));
    // …while transient render state is reset and queued for a full rebuild.
    assert!(display.markers_dirty);
    assert!(display.visible_turn_ids.is_empty());
    assert!(display.height_prefix.is_empty());
    assert!(display.progress_dirty);
}

#[test]
fn handle_session_state_keeps_accumulated_live_turn_over_snapshot_placeholder() {
    let mut app = test_app();
    app.attached_session_id = Some(7);
    let (tx, _rx) = std::sync::mpsc::channel();

    // Accumulated via the all-activity subscription: turn 5 is live-streaming.
    {
        let display = app.display_for(7);
        display
            .view
            .insert_or_replace(5, streamed_turn("user q", "streamed answer so far"));
        display.view.request_to_turn.insert(99, 5);
        display.active.insert(99);
    }

    // Daemon attach snapshot: turn 3 is finished, turn 5 is the in-flight
    // placeholder (assistant_text is None).
    let mut snapshot_turns = std::collections::BTreeMap::new();
    snapshot_turns.insert(3, streamed_turn("old q", "finished answer"));
    snapshot_turns.insert(5, placeholder_turn("user q"));

    handle_daemon_message(
        DaemonMessage::Session {
            session_id: Some(7),
            event: SessionEvent::SessionState {
                title: None,
                selected_model: None,
                parent_session_id: None,
                working_dir: None,
                turns: snapshot_turns,
                active_tool_groups: vec![],
                token_usage: None,
                context_window: None,
                last_prompt_tokens: None,
                reasoning_effort: None,
                reasoning_capability: None,
                status: SessionStatus::Inactive,
            },
        },
        &mut app,
        &tx,
    )
    .expect("handle_daemon_message should succeed");

    let display = app.display_for(7);
    // The in-flight turn keeps its live content — the placeholder must not win.
    assert_eq!(
        display
            .view
            .turns
            .get(&5)
            .unwrap()
            .assistant_text
            .as_deref(),
        Some("streamed answer so far"),
        "accumulated live content must survive the attach snapshot merge"
    );
    // Finished turns come from the snapshot.
    assert_eq!(
        display
            .view
            .turns
            .get(&3)
            .unwrap()
            .assistant_text
            .as_deref(),
        Some("finished answer")
    );
    // Request routing and the active set survive the merge.
    assert_eq!(display.view.request_to_turn.get(&99), Some(&5));
    assert!(display.active.contains(&99));
}

#[test]
fn done_for_background_session_does_not_pollute_attached_display() {
    let mut app = test_app();
    app.attached_session_id = Some(0);
    let (tx, _rx) = std::sync::mpsc::channel();
    // The attached session already has its own token usage.
    app.display_for(0).token_usage = Some(TokenUsage {
        input_tokens: 1,
        output_tokens: 2,
        total_tokens: 3,
    });
    // Session 7 (background, streamed via SubscribeAllActivity) has an
    // in-flight request that is about to finish.
    {
        let display = app.display_for(7);
        display.view.request_to_turn.insert(50, 5);
        display.active.insert(50);
    }

    handle_daemon_message(
        DaemonMessage::Session {
            session_id: Some(7),
            event: SessionEvent::Done {
                request_id: 50,
                token_usage: Some(TokenUsage {
                    input_tokens: 99,
                    output_tokens: 99,
                    total_tokens: 99,
                }),
                last_prompt_tokens: Some(99),
            },
        },
        &mut app,
        &tx,
    )
    .expect("handle_daemon_message should succeed");

    // The attached display is untouched — no progress-bar clobber from a
    // background session finishing.
    assert_eq!(
        app.display_for(0).token_usage,
        Some(TokenUsage {
            input_tokens: 1,
            output_tokens: 2,
            total_tokens: 3,
        })
    );
    assert!(!app.display_for(0).progress_dirty);
    // The background session's own display got the token usage and its
    // request was cleaned up via the generic handle_done dispatch.
    assert_eq!(
        app.display_for(7).token_usage,
        Some(TokenUsage {
            input_tokens: 99,
            output_tokens: 99,
            total_tokens: 99,
        })
    );
    assert!(!app.display_for(7).active.contains(&50));
    assert!(!app.display_for(7).view.request_to_turn.contains_key(&50));
}

#[test]
fn handle_session_status_changed_updates_attached_status() {
    let mut app = test_app();
    assert!(app.attached_status.is_none());

    // With no attached session, status should not be cached.
    app.handle_session_status_changed(42, &SessionStatus::Inference, 1705314000000);
    assert!(app.attached_status.is_none());

    // Once attached, a status change for that session should be cached.
    app.attached_session_id = Some(42);
    app.handle_session_status_changed(42, &SessionStatus::Inference, 1705314000000);
    assert_eq!(app.attached_status, Some(SessionStatus::Inference));

    // A status change for a different session should not overwrite.
    app.handle_session_status_changed(99, &SessionStatus::Sleeping, 1705314000000);
    assert_eq!(app.attached_status, Some(SessionStatus::Inference));

    // A subsequent change for the attached session should update.
    app.handle_session_status_changed(42, &SessionStatus::ToolCall("test".into()), 1705314000000);
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
        reasoning_artifact: None,
        reasoning_producer: None,
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

// ── Background-session metadata must not write the global status/error line ──
//
// The TUI subscribes to ALL activity (SubscribeAllActivity), so ModelSelected,
// ReasoningEffortSet, ReasoningEffortSetFailed and SessionAccountSet arrive for
// background sessions too.  Before the fix these fell through to
// dispatch_daemon_message, whose generic arms write `app.status` / `app.error`
// — overwriting the global status line while the user views another session,
// which changes `status_error_height` and reflows the viewed history viewport.
// The per-session display routing stays; only the global write must be gated
// on the message belonging to the attached session.

#[test]
fn model_selected_for_background_session_does_not_write_global_status() {
    let mut app = test_app();
    let (tx, _rx) = std::sync::mpsc::channel();
    // The user is viewing session 42.
    app.attached_session_id = Some(42);
    app.active_session_id = Some(42);
    assert!(app.status.is_none() && app.error.is_none());

    // A background session (99) changes its model.  Its per-session display
    // is updated, but the global status/error line must stay untouched.
    handle_daemon_message(
        DaemonMessage::Session {
            session_id: Some(99),
            event: SessionEvent::ModelSelected {
                model: "gpt-other".to_string(),
                reasoning_capability: None,
            },
        },
        &mut app,
        &tx,
    )
    .expect("handle ModelSelected");

    assert_eq!(
        app.status, None,
        "a background session's model change must not write the global status line"
    );
    assert_eq!(
        app.error, None,
        "a background session's model change must not write the global error line"
    );
    assert_eq!(
        app.display_for(99).selected_model.as_deref(),
        Some("gpt-other"),
        "per-session display routing must still happen"
    );
}

#[test]
fn model_selected_for_attached_session_writes_status_feedback() {
    let mut app = test_app();
    let (tx, _rx) = std::sync::mpsc::channel();
    app.attached_session_id = Some(42);
    app.active_session_id = Some(42);

    // The user's own `/model` command must still print its confirmation via
    // the generic dispatch fall-through.
    handle_daemon_message(
        DaemonMessage::Session {
            session_id: Some(42),
            event: SessionEvent::ModelSelected {
                model: "gpt-new".to_string(),
                reasoning_capability: None,
            },
        },
        &mut app,
        &tx,
    )
    .expect("handle ModelSelected");

    assert_eq!(
        app.status.as_deref(),
        Some("[daemon] selected model: gpt-new"),
        "attached-session /model feedback must be preserved"
    );
    assert_eq!(
        app.display_for(42).selected_model.as_deref(),
        Some("gpt-new")
    );
}

#[test]
fn reasoning_effort_set_for_background_session_does_not_write_global_status() {
    let mut app = test_app();
    let (tx, _rx) = std::sync::mpsc::channel();
    app.attached_session_id = Some(42);
    app.active_session_id = Some(42);
    assert!(app.status.is_none() && app.error.is_none());

    handle_daemon_message(
        DaemonMessage::Session {
            session_id: Some(99),
            event: SessionEvent::ReasoningEffortSet {
                effort: "high".to_string(),
            },
        },
        &mut app,
        &tx,
    )
    .expect("handle ReasoningEffortSet");

    assert_eq!(app.status, None);
    assert_eq!(app.error, None);
    assert_eq!(
        app.display_for(99).reasoning_effort.as_deref(),
        Some("high")
    );
}

#[test]
fn reasoning_effort_set_for_attached_session_writes_status_feedback() {
    let mut app = test_app();
    let (tx, _rx) = std::sync::mpsc::channel();
    app.attached_session_id = Some(42);
    app.active_session_id = Some(42);

    handle_daemon_message(
        DaemonMessage::Session {
            session_id: Some(42),
            event: SessionEvent::ReasoningEffortSet {
                effort: "high".to_string(),
            },
        },
        &mut app,
        &tx,
    )
    .expect("handle ReasoningEffortSet");

    assert_eq!(
        app.status.as_deref(),
        Some("[daemon] reasoning effort: high"),
        "attached-session /reasoning feedback must be preserved"
    );
    assert_eq!(
        app.display_for(42).reasoning_effort.as_deref(),
        Some("high")
    );
}

#[test]
fn session_account_set_for_background_session_does_not_write_global_status() {
    let mut app = test_app();
    let (tx, _rx) = std::sync::mpsc::channel();
    app.attached_session_id = Some(42);
    app.active_session_id = Some(42);
    assert!(app.status.is_none() && app.error.is_none());

    handle_daemon_message(
        DaemonMessage::Session {
            session_id: Some(99),
            event: SessionEvent::SessionAccountSet {
                account: "bg-account".to_string(),
            },
        },
        &mut app,
        &tx,
    )
    .expect("handle SessionAccountSet");

    assert_eq!(app.status, None);
    assert_eq!(app.error, None);
    assert_eq!(
        app.display_for(99).account_name.as_deref(),
        Some("bg-account")
    );
}

#[test]
fn session_account_set_for_attached_session_writes_status_feedback() {
    let mut app = test_app();
    let (tx, _rx) = std::sync::mpsc::channel();
    app.attached_session_id = Some(42);
    app.active_session_id = Some(42);

    handle_daemon_message(
        DaemonMessage::Session {
            session_id: Some(42),
            event: SessionEvent::SessionAccountSet {
                account: "main".to_string(),
            },
        },
        &mut app,
        &tx,
    )
    .expect("handle SessionAccountSet");

    assert_eq!(
        app.status.as_deref(),
        Some("[daemon] session account set: main"),
        "attached-session /account feedback must be preserved"
    );
    assert_eq!(app.display_for(42).account_name.as_deref(), Some("main"));
}

#[test]
fn reasoning_effort_set_failed_for_background_session_does_not_write_global_status_or_error() {
    let mut app = test_app();
    let (tx, _rx) = std::sync::mpsc::channel();
    app.attached_session_id = Some(42);
    app.active_session_id = Some(42);
    assert!(app.status.is_none() && app.error.is_none());

    // A background session's rejection previously leaked through BOTH the
    // explicit `app.status` write and the generic dispatch's `app.error`.
    handle_daemon_message(
        DaemonMessage::Session {
            session_id: Some(99),
            event: SessionEvent::ReasoningEffortSetFailed {
                effort: "high".to_string(),
                error: "model does not support reasoning".to_string(),
            },
        },
        &mut app,
        &tx,
    )
    .expect("handle ReasoningEffortSetFailed");

    assert_eq!(
        app.status, None,
        "a background session's rejection must not write the global status line"
    );
    assert_eq!(
        app.error, None,
        "a background session's rejection must not write the global error line"
    );
    assert_eq!(app.display_for(99).reasoning_effort.as_deref(), Some("off"));
}

#[test]
fn reasoning_effort_set_failed_for_attached_session_writes_status_and_error() {
    let mut app = test_app();
    let (tx, _rx) = std::sync::mpsc::channel();
    app.attached_session_id = Some(42);
    app.active_session_id = Some(42);

    // The user's own `/reasoning` command failed — the rejection notice stays
    // on the status line and the generic dispatch records the error.
    handle_daemon_message(
        DaemonMessage::Session {
            session_id: Some(42),
            event: SessionEvent::ReasoningEffortSetFailed {
                effort: "high".to_string(),
                error: "model does not support reasoning".to_string(),
            },
        },
        &mut app,
        &tx,
    )
    .expect("handle ReasoningEffortSetFailed");

    assert_eq!(
        app.status.as_deref(),
        Some("reasoning effort rejected: model does not support reasoning")
    );
    assert_eq!(
        app.error.as_deref(),
        Some("[daemon] failed to set reasoning effort high: model does not support reasoning")
    );
    assert_eq!(app.display_for(42).reasoning_effort.as_deref(), Some("off"));
}

// ── Connection-level `session_id: None` and ModelSelectionFailed gating ──
//
// The daemon replies to bare `/reasoning` (GetReasoningEffort) without an
// attachment, and sends other connection-level errors, with `session_id: None`
// meaning "resolve to the attached session".  Those must keep their
// fall-through feedback — they are the user's own command replies, not
// background noise.  ModelSelectionFailed is the failure counterpart of
// ModelSelected and must be gated the same way for background sessions.

#[test]
fn reasoning_effort_set_connection_level_writes_status_feedback_for_attached_session() {
    let mut app = test_app();
    let (tx, _rx) = std::sync::mpsc::channel();
    app.attached_session_id = Some(42);
    app.active_session_id = Some(42);

    // A connection-level ReasoningEffortSet (bare `/reasoning` without an
    // attachment) arrives with `session_id: None` and resolves to the
    // attached session.  It must be treated as the user's own feedback, not
    // swallowed as background noise.
    handle_daemon_message(
        DaemonMessage::Session {
            session_id: None,
            event: SessionEvent::ReasoningEffortSet {
                effort: "high".to_string(),
            },
        },
        &mut app,
        &tx,
    )
    .expect("handle ReasoningEffortSet");

    assert_eq!(
        app.status.as_deref(),
        Some("[daemon] reasoning effort: high"),
        "bare /reasoning feedback must be preserved for a connection-level reply"
    );
    assert_eq!(
        app.display_for(42).reasoning_effort.as_deref(),
        Some("high"),
        "the current effort must land in the attached session's display"
    );
    assert_eq!(
        app.display_for(0).reasoning_effort.as_deref(),
        None,
        "a connection-level reply must not write a phantom session-0 display"
    );
    assert_eq!(app.error, None);
}

#[test]
fn reasoning_effort_set_failed_connection_level_writes_status_and_error() {
    let mut app = test_app();
    let (tx, _rx) = std::sync::mpsc::channel();
    app.attached_session_id = Some(42);
    app.active_session_id = Some(42);

    // The daemon sends ReasoningEffortSetFailed with `session_id: None` for
    // connection-level rejections ("no session attached").  The user must see
    // the rejection of their own /reasoning command.
    handle_daemon_message(
        DaemonMessage::Session {
            session_id: None,
            event: SessionEvent::ReasoningEffortSetFailed {
                effort: "high".to_string(),
                error: "no session attached".to_string(),
            },
        },
        &mut app,
        &tx,
    )
    .expect("handle ReasoningEffortSetFailed");

    assert_eq!(
        app.status.as_deref(),
        Some("reasoning effort rejected: no session attached")
    );
    assert_eq!(
        app.error.as_deref(),
        Some("[daemon] failed to set reasoning effort high: no session attached")
    );
    assert_eq!(
        app.display_for(42).reasoning_effort.as_deref(),
        Some("off"),
        "the attached session's effort is reset on rejection"
    );
}

#[test]
fn model_selection_failed_for_background_session_does_not_write_global_error() {
    let mut app = test_app();
    let (tx, _rx) = std::sync::mpsc::channel();
    app.attached_session_id = Some(42);
    app.active_session_id = Some(42);
    assert!(app.status.is_none() && app.error.is_none());

    handle_daemon_message(
        DaemonMessage::Session {
            session_id: Some(99),
            event: SessionEvent::ModelSelectionFailed {
                model: "gpt-other".to_string(),
                error: "model not found".to_string(),
            },
        },
        &mut app,
        &tx,
    )
    .expect("handle ModelSelectionFailed");

    assert_eq!(
        app.error, None,
        "a background session's model rejection must not write the global error line"
    );
    assert_eq!(app.status, None);
}

#[test]
fn model_selection_failed_for_attached_session_writes_global_error() {
    let mut app = test_app();
    let (tx, _rx) = std::sync::mpsc::channel();
    app.attached_session_id = Some(42);
    app.active_session_id = Some(42);

    // The user's own /model command failed — the rejection must still be
    // surfaced via the generic dispatch's error write.
    handle_daemon_message(
        DaemonMessage::Session {
            session_id: Some(42),
            event: SessionEvent::ModelSelectionFailed {
                model: "gpt-x".to_string(),
                error: "model not found".to_string(),
            },
        },
        &mut app,
        &tx,
    )
    .expect("handle ModelSelectionFailed");

    assert_eq!(
        app.error.as_deref(),
        Some("[daemon] failed to select model gpt-x: model not found"),
        "attached-session /model rejection feedback must be preserved"
    );
}

#[test]
fn model_selection_failed_connection_level_writes_global_error() {
    let mut app = test_app();
    let (tx, _rx) = std::sync::mpsc::channel();
    app.attached_session_id = Some(42);
    app.active_session_id = Some(42);

    // No session attached at the daemon connection level — the `None` reply
    // must keep its error feedback, not be swallowed as background noise.
    handle_daemon_message(
        DaemonMessage::Session {
            session_id: None,
            event: SessionEvent::ModelSelectionFailed {
                model: "gpt-x".to_string(),
                error: "no session attached".to_string(),
            },
        },
        &mut app,
        &tx,
    )
    .expect("handle ModelSelectionFailed");

    assert_eq!(
        app.error.as_deref(),
        Some("[daemon] failed to select model gpt-x: no session attached")
    );
}

// ── SessionCreated must not auto-attach to agent-spawned sub-sessions ──

#[test]
fn session_created_for_sub_session_does_not_hijack_chat_view() {
    let mut app = test_app();
    let (tx, rx) = std::sync::mpsc::channel();
    // The user is viewing session 42 on the Chat page.
    app.attached_session_id = Some(42);
    app.active_session_id = Some(42);

    // spawn_subsession makes the daemon broadcast SessionCreated with
    // parent_session_id = Some(parent).  The TUI must not switch to it.
    handle_daemon_message(
        DaemonMessage::Session {
            session_id: Some(99),
            event: SessionEvent::SessionCreated {
                parent_session_id: Some(42),
                title: None,
                working_dir: None,
                account_name: None,
                selected_model: None,
                reasoning_effort: None,
            },
        },
        &mut app,
        &tx,
    )
    .expect("handle SessionCreated");

    // The view must stay on the session the user was reading.
    assert_eq!(
        app.attached_session_id,
        Some(42),
        "sub-session creation must not change the attached session"
    );
    assert_eq!(
        app.active_session_id,
        Some(42),
        "sub-session creation must not change the active session"
    );
    // No AttachSession may be sent for the sub-session, and — because the
    // Chat page renders `Sessions` replies into the status line — no
    // unsolicited list refresh either (it would rewrite the status line and
    // reflow the viewed viewport).
    let msgs: Vec<ClientMessage> = rx.try_iter().collect();
    assert!(
        !msgs
            .iter()
            .any(|m| matches!(m, ClientMessage::AttachSession { session_id } if *session_id == 99)),
        "sub-session creation must not auto-attach"
    );
    assert!(
        !msgs
            .iter()
            .any(|m| matches!(m, ClientMessage::ListSessions)),
        "on the Chat page a sub-session creation must not refresh the session list — \
         the Sessions reply would rewrite the status line"
    );
}

#[test]
fn session_created_for_sub_session_on_session_manager_refreshes_list() {
    let mut app = test_app();
    let (tx, rx) = std::sync::mpsc::channel();
    app.attached_session_id = Some(42);
    app.active_session_id = Some(42);
    app.page = Page::SessionManager;

    handle_daemon_message(
        DaemonMessage::Session {
            session_id: Some(99),
            event: SessionEvent::SessionCreated {
                parent_session_id: Some(42),
                title: None,
                working_dir: None,
                account_name: None,
                selected_model: None,
                reasoning_effort: None,
            },
        },
        &mut app,
        &tx,
    )
    .expect("handle SessionCreated");

    // Still no auto-attach, but the list refresh is harmless here: the reply
    // renders into the session list, not the status line.
    assert_eq!(app.attached_session_id, Some(42));
    assert_eq!(app.active_session_id, Some(42));
    let msgs: Vec<ClientMessage> = rx.try_iter().collect();
    assert!(
        !msgs
            .iter()
            .any(|m| matches!(m, ClientMessage::AttachSession { session_id } if *session_id == 99)),
        "sub-session creation must not auto-attach"
    );
    assert!(
        msgs.iter()
            .any(|m| matches!(m, ClientMessage::ListSessions)),
        "on the Session Manager page the list must refresh so the sub-session is visible"
    );
}

#[test]
fn session_created_for_user_session_attaches_on_chat_page() {
    let mut app = test_app();
    let (tx, rx) = std::sync::mpsc::channel();
    app.attached_session_id = Some(42);
    app.active_session_id = Some(42);

    // A user-created session (parent_session_id = None) keeps the old
    // behavior: switch the view and attach.
    handle_daemon_message(
        DaemonMessage::Session {
            session_id: Some(99),
            event: SessionEvent::SessionCreated {
                parent_session_id: None,
                title: None,
                working_dir: None,
                account_name: Some("acct".to_string()),
                selected_model: Some("gpt-new".to_string()),
                reasoning_effort: Some("off".to_string()),
            },
        },
        &mut app,
        &tx,
    )
    .expect("handle SessionCreated");

    assert_eq!(app.attached_session_id, Some(99));
    assert_eq!(app.active_session_id, Some(99));
    // The new session's display fields are primed from the creation params.
    assert_eq!(app.display_for(99).account_name.as_deref(), Some("acct"));
    assert_eq!(
        app.display_for(99).selected_model.as_deref(),
        Some("gpt-new")
    );
    let msgs: Vec<ClientMessage> = rx.try_iter().collect();
    assert!(
        msgs.iter()
            .any(|m| matches!(m, ClientMessage::AttachSession { session_id } if *session_id == 99)),
        "a user-created session must still auto-attach on the Chat page"
    );
    assert!(
        msgs.iter()
            .any(|m| matches!(m, ClientMessage::ListSessions))
    );
}

// ── Sub-session finish auto-switches back to the parent ─────────────

/// Build a session summary for a running sub-session: same fields as
/// `make_session`, plus a parent id and an active (streaming) status.
fn make_subsession(id: u64, title: &str, parent_id: u64) -> choreo_proto::SessionSummary {
    let mut s = make_session(id, title, "m1", 0);
    s.parent_session_id = Some(parent_id);
    s.status = choreo_proto::SessionStatus::Inference;
    s
}

#[test]
fn attached_subsession_finished_detects_active_to_idle_only() {
    let mut app = test_app();
    app.attached_session_id = Some(99);
    app.active_session_id = Some(99);
    app.session_mgr.set_sessions(vec![
        make_session(42, "parent session", "m1", 3),
        make_subsession(99, "child task", 42),
    ]);

    // Viewing the running sub-session on the Chat page: an active → idle
    // transition means it finished — report the parent id.
    assert_eq!(
        app.attached_subsession_finished(99, &SessionStatus::Inactive),
        Some(42)
    );
    assert_eq!(
        app.attached_subsession_finished(99, &SessionStatus::Sleeping),
        Some(42)
    );
    // The new status must be idle: a same-status re-broadcast of an active
    // status is not a finish.
    assert_eq!(
        app.attached_subsession_finished(99, &SessionStatus::Inference),
        None
    );

    // Idle → idle (the child already finished; this is a duplicate or a
    // summary refresh) must not re-fire.
    app.handle_session_status_changed(99, &SessionStatus::Inactive, 1705315000000);
    assert_eq!(
        app.attached_subsession_finished(99, &SessionStatus::Inactive),
        None,
        "idle → idle must not be treated as a finish"
    );

    // Not attached / different attached session: no switch.
    app.attached_session_id = Some(7);
    assert_eq!(
        app.attached_subsession_finished(99, &SessionStatus::Inactive),
        None
    );
    app.attached_session_id = Some(99);

    // Browsing the Session Manager page: no auto-jump away from it.
    app.page = Page::SessionManager;
    assert_eq!(
        app.attached_subsession_finished(99, &SessionStatus::Inactive),
        None
    );
    app.page = Page::Chat;

    // A top-level session (no parent) is not a sub-session: no switch.
    app.attached_session_id = Some(42);
    assert_eq!(
        app.attached_subsession_finished(42, &SessionStatus::Inactive),
        None
    );

    // The parent no longer exists in the summary (deleted while the child
    // ran): switching would attach to a dead session id — no switch.
    app.attached_session_id = Some(99);
    app.session_mgr
        .set_sessions(vec![make_subsession(99, "orphan task", 42)]);
    assert_eq!(
        app.attached_subsession_finished(99, &SessionStatus::Inactive),
        None
    );
}

#[test]
fn subsession_finish_switches_back_to_parent_with_notification() {
    let mut app = test_app();
    let (tx, rx) = std::sync::mpsc::channel();
    // The user opened the sub-session 99 from the Session Manager and is now
    // reading it on the Chat page while it streams.
    app.attached_session_id = Some(99);
    app.active_session_id = Some(99);
    app.session_mgr.set_sessions(vec![
        make_session(42, "parent session", "m1", 3),
        make_subsession(99, "child task", 42),
    ]);

    handle_daemon_message(
        DaemonMessage::Session {
            session_id: Some(99),
            event: SessionEvent::SessionStatusChanged {
                status: SessionStatus::Inactive,
                last_modified: 1705315000000,
            },
        },
        &mut app,
        &tx,
    )
    .expect("handle SessionStatusChanged");

    // The view jumped back to the parent session.
    assert_eq!(app.attached_session_id, Some(42));
    assert_eq!(app.active_session_id, Some(42));
    // The daemon was asked to attach to the parent, mirroring the Session
    // Manager Enter path (summary unsubscription first, then attach).
    let msgs: Vec<ClientMessage> = rx.try_iter().collect();
    assert!(
        msgs.iter().any(|m| matches!(
            m,
            ClientMessage::AttachSession { session_id } if *session_id == 42
        )),
        "must attach to the parent session"
    );
    assert!(
        msgs.iter()
            .any(|m| matches!(m, ClientMessage::UnsubscribeSessionsSummary)),
        "must unsubscribe from the sessions summary like other attach paths"
    );
    // The notification names both sessions.
    assert_eq!(
        app.status.as_deref(),
        Some("Subsession \"child task\" finished. Switched back to parent \"parent session\".")
    );
    // The status bar immediately reflects the parent's (idle) summary status
    // instead of holding the child's just-applied "inactive" until the
    // daemon's SessionAttached reply lands.
    assert_eq!(app.attached_status, Some(SessionStatus::Inactive));
}

#[test]
fn subsession_finish_does_not_fire_on_duplicate_idle_broadcast() {
    let mut app = test_app();
    let (tx, rx) = std::sync::mpsc::channel();
    // The child already finished before the user opened it; its summary is
    // idle.  A re-broadcast of the idle status (summary refresh / re-attach)
    // must not yank the view back to the parent.
    app.attached_session_id = Some(99);
    app.active_session_id = Some(99);
    app.session_mgr.set_sessions(vec![
        make_session(42, "parent session", "m1", 3),
        make_subsession(99, "child task", 42),
    ]);
    app.handle_session_status_changed(99, &SessionStatus::Inactive, 1705315000000);

    handle_daemon_message(
        DaemonMessage::Session {
            session_id: Some(99),
            event: SessionEvent::SessionStatusChanged {
                status: SessionStatus::Inactive,
                last_modified: 1705315000000,
            },
        },
        &mut app,
        &tx,
    )
    .expect("handle SessionStatusChanged");

    assert_eq!(app.attached_session_id, Some(99));
    assert_eq!(app.status, None);
    assert!(
        rx.try_iter().next().is_none(),
        "no attach may be sent for an idle → idle broadcast"
    );
}

#[test]
fn top_level_session_finish_does_not_switch() {
    let mut app = test_app();
    let (tx, rx) = std::sync::mpsc::channel();
    app.attached_session_id = Some(42);
    app.active_session_id = Some(42);
    app.session_mgr
        .set_sessions(vec![make_session(42, "my session", "m1", 3)]);

    handle_daemon_message(
        DaemonMessage::Session {
            session_id: Some(42),
            event: SessionEvent::SessionStatusChanged {
                status: SessionStatus::Inactive,
                last_modified: 1705315000000,
            },
        },
        &mut app,
        &tx,
    )
    .expect("handle SessionStatusChanged");

    // A top-level session finishing is normal lifecycle — no switch, no
    // notification, no messages.
    assert_eq!(app.attached_session_id, Some(42));
    assert_eq!(app.status, None);
    assert!(rx.try_iter().next().is_none());
}

#[test]
fn subsession_finish_with_missing_parent_does_not_switch() {
    let mut app = test_app();
    let (tx, rx) = std::sync::mpsc::channel();
    // The parent was deleted while the child ran; the summary only holds the
    // orphaned child.  The switch-back must not fire — attaching to a dead
    // session id would strand the user on a session the daemon rejects.
    app.attached_session_id = Some(99);
    app.active_session_id = Some(99);
    app.session_mgr
        .set_sessions(vec![make_subsession(99, "orphan task", 42)]);

    handle_daemon_message(
        DaemonMessage::Session {
            session_id: Some(99),
            event: SessionEvent::SessionStatusChanged {
                status: SessionStatus::Inactive,
                last_modified: 1705315000000,
            },
        },
        &mut app,
        &tx,
    )
    .expect("handle SessionStatusChanged");

    // The view stays on the finished child; no attach is attempted.
    assert_eq!(app.attached_session_id, Some(99));
    assert_eq!(app.active_session_id, Some(99));
    assert_eq!(app.status, None);
    assert!(
        rx.try_iter().next().is_none(),
        "no attach may be sent when the parent is missing from the summary"
    );
}

#[test]
fn session_attached_does_not_regress_accumulated_live_state() {
    let mut app = test_app();
    let (tx, _rx) = std::sync::mpsc::channel();
    // The user is viewing session 42 and its display already accumulated live
    // state via the all-activity subscription while it was in the background:
    // fresher per-turn token usage plus a live streaming count.
    app.attached_session_id = Some(42);
    app.active_session_id = Some(42);
    let display = app.display_for(42);
    display.token_usage = Some(TokenUsage {
        input_tokens: 50,
        output_tokens: 60,
        total_tokens: 110,
    });
    display.live_output_tokens = 5;
    display.selected_model = Some("gpt-live".to_string());

    // The session summary (refreshed on ListSessions, potentially stale) has
    // older values for the same fields.
    app.session_mgr
        .set_sessions(vec![make_session(42, "a", "gpt-stale", 0)]);
    {
        let s = app.session_mgr.sessions.first_mut().unwrap();
        s.token_usage = Some(TokenUsage {
            input_tokens: 1,
            output_tokens: 2,
            total_tokens: 3,
        });
        s.context_window = Some(4096);
    }

    handle_daemon_message(
        DaemonMessage::Session {
            session_id: Some(42),
            event: SessionEvent::SessionAttached,
        },
        &mut app,
        &tx,
    )
    .expect("handle SessionAttached");

    // The accumulated (fresher) display state wins — the stale summary must
    // not regress the status bar's token readout or model right after switch.
    assert_eq!(
        app.display_for(42).token_usage,
        Some(TokenUsage {
            input_tokens: 50,
            output_tokens: 60,
            total_tokens: 110,
        })
    );
    assert_eq!(app.display_for(42).live_output_tokens, 5);
    assert_eq!(
        app.display_for(42).selected_model.as_deref(),
        Some("gpt-live")
    );
    // Fields the display never accumulated come from the summary.
    assert_eq!(app.display_for(42).context_window, Some(4096));
}

#[test]
fn handle_sessions_auto_attach_prefers_top_level_session() {
    let (tx, rx) = std::sync::mpsc::channel();
    // Fresh App: nothing attached yet, on the chat page.
    let mut app = App::new();
    app.image_job_tx = None;

    // Session 7 is a sub-session (parent_session_id = Some) and is the most
    // recently modified (its requests completed while the parent's agent
    // worked, each bumping last_modified).
    // Session 3 is the most recently modified top-level session.
    let mut child = make_session(7, "sub", "m", 0);
    child.parent_session_id = Some(3);
    child.last_modified = 3000;
    let mut top = make_session(3, "main", "m", 0);
    top.last_modified = 2000;
    let mut older = make_session(1, "old", "m", 0);
    older.last_modified = 1000;

    app.handle_sessions(&[child.clone(), top.clone(), older.clone()], &tx)
        .expect("handle_sessions should succeed");

    // The auto-attach must pick the top-level session, not the sub-session
    // that happens to be the most recently modified.
    let msg = rx.recv().expect("auto-attach message");
    assert_eq!(msg, ClientMessage::AttachSession { session_id: 3 });
    assert_eq!(app.attached_session_id, Some(3));
    assert_eq!(app.active_session_id, Some(3));

    // A second Sessions reply in the same tick must not re-fire: attachment
    // state was set locally, so the guard skips the auto-attach.
    let (tx2, rx2) = std::sync::mpsc::channel();
    app.handle_sessions(&[child.clone(), top.clone(), older.clone()], &tx2)
        .expect("handle_sessions should succeed");
    assert!(
        rx2.try_recv().is_err(),
        "auto-attach must not fire twice for the same startup tick"
    );
}

#[test]
fn handle_sessions_auto_attach_falls_back_to_child_when_no_top_level() {
    let mut app = App::new();
    app.image_job_tx = None;
    let (tx, rx) = std::sync::mpsc::channel();

    let mut child = make_session(7, "sub", "m", 0);
    child.parent_session_id = Some(3);
    app.handle_sessions(&[child], &tx)
        .expect("handle_sessions should succeed");

    // With no top-level session at all, fall back to the most recent session.
    let msg = rx.recv().expect("auto-attach message");
    assert_eq!(msg, ClientMessage::AttachSession { session_id: 7 });
}
