use crate::connection::{handle_daemon_message, handle_terminal_event};
use crate::state::*;
use crate::test_util::{add_user_text, make_session, test_app};
use choreo_proto::{
    ClientMessage, DaemonMessage, ReasoningCapability, SessionEvent, SessionStatus, TokenUsage,
};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

// ── handle_daemon_message progress bar integration ──

#[test]
fn daemon_message_session_state_updates_progress_for_attached_session() {
    let mut app = test_app();
    let (tx, _rx) = std::sync::mpsc::channel();
    app.attached_session_id = Some(7);

    handle_daemon_message(
        DaemonMessage::Session {
            session_id: Some(7),
            event: SessionEvent::SessionState {
                title: None,
                selected_model: None,
                parent_session_id: None,
                working_dir: None,
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
    // Tool groups only reach the status bar when the snapshot belongs to the
    // attached session.
    app.attached_session_id = Some(7);

    handle_daemon_message(
        DaemonMessage::Session {
            session_id: Some(7),
            event: SessionEvent::SessionState {
                title: None,
                selected_model: None,
                parent_session_id: None,
                working_dir: None,
                turns: std::collections::BTreeMap::new(),
                active_tool_groups: vec!["core".into(), "browser".into()],
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

    assert_eq!(app.attached_tool_groups, vec!["core", "browser"]);
}

#[test]
fn daemon_message_session_state_ignores_wrong_session() {
    let mut app = test_app();
    let (tx, _rx) = std::sync::mpsc::channel();
    app.attached_session_id = Some(7);

    handle_daemon_message(
        DaemonMessage::Session {
            session_id: Some(99), // different from attached_session_id
            event: SessionEvent::SessionState {
                title: None,
                selected_model: None,
                parent_session_id: None,
                working_dir: None,
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
        },
        &mut app,
        &tx,
    )
    .expect("handle_daemon_message should succeed");

    // A SessionState snapshot for a background session is routed to that
    // session's own display — it must not clobber the attached session's
    // token usage, status, or tool-group state (which the status bar reads).
    assert_eq!(
        app.display_for(7).token_usage,
        None,
        "attached display must not pick up the background session's usage"
    );
    assert_eq!(app.display_for(7).context_window, None);
    assert_eq!(app.attached_status, None);
    // The background session's own display received the snapshot.
    assert_eq!(
        app.display_for(99).token_usage,
        Some(TokenUsage {
            input_tokens: 99,
            output_tokens: 99,
            total_tokens: 99,
        })
    );
    assert_eq!(app.display_for(99).context_window, Some(1024));
    // The progress_dirty guard in connection.rs still skips non-attached
    // sessions.
    assert!(!app.display_for(7).progress_dirty);
    assert!(!app.display_for(99).progress_dirty);
}

#[test]
fn daemon_message_done_with_token_usage_updates_progress() {
    let mut app = test_app();
    let (tx, _rx) = std::sync::mpsc::channel();
    // Session 42 is the hypothetical attached session — a nonzero id the
    // daemon can actually assign (`Some(0)` would be wire-implausible: ids
    // start at 1, and `None` — not `0` — marks a connection-level reply).
    app.attached_session_id = Some(42);

    handle_daemon_message(
        DaemonMessage::Session {
            session_id: Some(42),
            event: SessionEvent::Done {
                request_id: 1,
                token_usage: Some(TokenUsage {
                    input_tokens: 5,
                    output_tokens: 10,
                    total_tokens: 15,
                }),
                last_prompt_tokens: Some(5),
            },
        },
        &mut app,
        &tx,
    )
    .expect("handle_daemon_message should succeed");

    assert_eq!(
        app.display_for(42).token_usage,
        Some(TokenUsage {
            input_tokens: 5,
            output_tokens: 10,
            total_tokens: 15,
        })
    );
    assert!(app.display_for(42).progress_dirty);
}

#[test]
fn daemon_message_done_without_token_usage_does_not_change_progress() {
    let mut app = test_app();
    let (tx, _rx) = std::sync::mpsc::channel();

    // `Done` is a requires-origin event (the daemon always carries the real
    // session id in `Some`), so the fixture uses a nonzero id.
    handle_daemon_message(
        DaemonMessage::Session {
            session_id: Some(42),
            event: SessionEvent::Done {
                request_id: 1,
                token_usage: None,
                last_prompt_tokens: None,
            },
        },
        &mut app,
        &tx,
    )
    .expect("handle_daemon_message should succeed");

    // Must remain at defaults — no data written, no dirty flag set.
    assert!(app.display_for(42).token_usage.is_none());
    assert!(!app.display_for(42).progress_dirty);
}

#[test]
fn live_output_token_count_from_background_session_does_not_pollute_status_bar() {
    let mut app = test_app();
    let (tx, _rx) = std::sync::mpsc::channel();
    // The user is viewing session 0, which already has settled token usage.
    app.attached_session_id = Some(0);
    app.display_for(0).token_usage = Some(TokenUsage {
        input_tokens: 1,
        output_tokens: 2,
        total_tokens: 3,
    });

    // Session 7 (background, streamed via SubscribeAllActivity) reports its
    // live output-token count while the user keeps looking at session 0.
    handle_daemon_message(
        DaemonMessage::Session {
            session_id: Some(7),
            event: SessionEvent::LiveOutputTokenCount {
                request_id: 50,
                output_tokens: 99,
            },
        },
        &mut app,
        &tx,
    )
    .expect("handle_daemon_message should succeed");

    // The live count goes to session 7's own display…
    assert_eq!(app.display_for(7).live_output_tokens, 99);
    // …and never lands in the display the user is viewing, so the status
    // bar's token readout (token_usage + live counts) stays session-0-only.
    assert_eq!(app.display_for(0).live_output_tokens, 0);
    assert_eq!(
        app.display_token_usage(),
        Some(TokenUsage {
            input_tokens: 1,
            output_tokens: 2,
            total_tokens: 3,
        })
    );
}

#[test]
fn live_output_token_count_updates_own_session_after_switch() {
    let mut app = test_app();
    let (tx, _rx) = std::sync::mpsc::channel();
    // The user switches to session 7 mid-stream; reset_for_session_switch
    // preserved its accumulated live token estimate.
    app.attached_session_id = Some(7);
    app.active_session_id = Some(7);
    app.display_for(7).token_usage = Some(TokenUsage {
        input_tokens: 10,
        output_tokens: 5,
        total_tokens: 15,
    });

    handle_daemon_message(
        DaemonMessage::Session {
            session_id: Some(7),
            event: SessionEvent::LiveOutputTokenCount {
                request_id: 50,
                output_tokens: 42,
            },
        },
        &mut app,
        &tx,
    )
    .expect("handle_daemon_message should succeed");

    // The count lands in the session the message belongs to, which is now
    // also the one being viewed — the status bar reflects it.
    assert_eq!(app.display_for(7).live_output_tokens, 42);
    assert_eq!(
        app.display_token_usage(),
        Some(TokenUsage {
            input_tokens: 10,
            output_tokens: 47,
            total_tokens: 57,
        })
    );
}

#[test]
fn session_state_snapshot_does_not_regress_fresher_token_usage() {
    let mut app = test_app();
    let (tx, _rx) = std::sync::mpsc::channel();
    app.attached_session_id = Some(7);

    // While the session streamed in the background, the all-activity
    // subscription accumulated the worker's fresh cumulative usage.
    app.display_for(7).token_usage = Some(TokenUsage {
        input_tokens: 10,
        output_tokens: 5,
        total_tokens: 15,
    });

    // The attach snapshot is built from the session thread's config, which
    // can lag the worker's live total for a mid-turn session — it must not
    // regress the status bar's token readout.
    handle_daemon_message(
        DaemonMessage::Session {
            session_id: Some(7),
            event: SessionEvent::SessionState {
                title: None,
                selected_model: None,
                parent_session_id: None,
                working_dir: None,
                turns: std::collections::BTreeMap::new(),
                active_tool_groups: vec![],
                token_usage: Some(TokenUsage {
                    input_tokens: 1,
                    output_tokens: 2,
                    total_tokens: 3,
                }),
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

    // The fresher accumulated value survives — no regression to pre-turn totals.
    assert_eq!(
        app.display_token_usage(),
        Some(TokenUsage {
            input_tokens: 10,
            output_tokens: 5,
            total_tokens: 15,
        })
    );
}

#[test]
fn session_state_snapshot_with_newer_larger_usage_updates_display() {
    let mut app = test_app();
    let (tx, _rx) = std::sync::mpsc::channel();
    app.attached_session_id = Some(7);

    // The display holds an older total; the snapshot is authoritative and
    // must still win when it carries a larger (newer) cumulative value.
    app.display_for(7).token_usage = Some(TokenUsage {
        input_tokens: 1,
        output_tokens: 2,
        total_tokens: 3,
    });

    handle_daemon_message(
        DaemonMessage::Session {
            session_id: Some(7),
            event: SessionEvent::SessionState {
                title: None,
                selected_model: None,
                parent_session_id: None,
                working_dir: None,
                turns: std::collections::BTreeMap::new(),
                active_tool_groups: vec![],
                token_usage: Some(TokenUsage {
                    input_tokens: 10,
                    output_tokens: 5,
                    total_tokens: 15,
                }),
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

    // The newer snapshot total wins.
    assert_eq!(
        app.display_token_usage(),
        Some(TokenUsage {
            input_tokens: 10,
            output_tokens: 5,
            total_tokens: 15,
        })
    );
}

#[test]
fn session_state_snapshot_does_not_regress_fresher_last_prompt_tokens() {
    let mut app = test_app();
    let (tx, _rx) = std::sync::mpsc::channel();
    app.attached_session_id = Some(7);

    // The display already holds the fresher value (a mid-turn
    // TokenUsageUpdate or the Done at turn end).  Unlike cumulative usage,
    // last_prompt_tokens is not monotonic, so the snapshot must gap-fill
    // rather than overwrite: a lagging attach snapshot (built from the
    // session thread's config before the latest broadcast) must not regress
    // the progress bar until the next update.
    app.display_for(7).last_prompt_tokens = Some(5000);

    handle_daemon_message(
        DaemonMessage::Session {
            session_id: Some(7),
            event: SessionEvent::SessionState {
                title: None,
                selected_model: None,
                parent_session_id: None,
                working_dir: None,
                turns: std::collections::BTreeMap::new(),
                active_tool_groups: vec![],
                token_usage: None,
                context_window: None,
                last_prompt_tokens: Some(100),
                reasoning_effort: None,
                reasoning_capability: None,
                status: SessionStatus::Inactive,
            },
        },
        &mut app,
        &tx,
    )
    .expect("handle_daemon_message should succeed");

    assert_eq!(
        app.display_for(7).last_prompt_tokens,
        Some(5000),
        "a lagging snapshot must not regress the fresher last_prompt_tokens"
    );
}

#[test]
fn session_state_snapshot_fills_missing_last_prompt_tokens() {
    let mut app = test_app();
    let (tx, _rx) = std::sync::mpsc::channel();
    app.attached_session_id = Some(7);

    // No value shown yet — the snapshot's value must still be applied
    // (gap-fill only blocks regression, not forward progress).
    assert!(app.display_for(7).last_prompt_tokens.is_none());

    handle_daemon_message(
        DaemonMessage::Session {
            session_id: Some(7),
            event: SessionEvent::SessionState {
                title: None,
                selected_model: None,
                parent_session_id: None,
                working_dir: None,
                turns: std::collections::BTreeMap::new(),
                active_tool_groups: vec![],
                token_usage: None,
                context_window: None,
                last_prompt_tokens: Some(300),
                reasoning_effort: None,
                reasoning_capability: None,
                status: SessionStatus::Inactive,
            },
        },
        &mut app,
        &tx,
    )
    .expect("handle_daemon_message should succeed");

    assert_eq!(
        app.display_for(7).last_prompt_tokens,
        Some(300),
        "a snapshot must fill last_prompt_tokens when nothing is shown yet"
    );
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
fn ctrl_r_no_active_display_shows_message() {
    let mut app = test_app();
    let (tx, rx) = std::sync::mpsc::channel();

    // No session attached at all: there is no display whose capability
    // could be consulted, so fall back to the established no-session
    // wording used by the other session-bound shortcuts.
    app.active_session_id = None;
    app.attached_session_id = None;

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL)),
        &mut app,
        &tx,
    )
    .expect("handle ctrl+r");

    assert_eq!(app.status.as_deref(), Some("no session attached"));
    assert!(rx.try_iter().next().is_none(), "no client message expected");
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
fn ctrl_r_no_model_selected_shows_message() {
    let mut app = test_app();
    let (tx, rx) = std::sync::mpsc::channel();

    // No model selected yet and no capability reported.  `None` here must
    // NOT be reported as "model does not support reasoning" — the user
    // simply has nothing to cycle yet.
    app.display_for(0).reasoning_capability = None;
    app.display_for(0).selected_model = None;

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL)),
        &mut app,
        &tx,
    )
    .expect("handle ctrl+r");

    assert_eq!(
        app.status.as_deref(),
        Some("no model selected — pick one with Ctrl+M")
    );
    // Effort should remain unchanged (still None) and nothing is sent to
    // the daemon (no cycling, so no SetReasoningEffort).
    assert!(app.display_for(0).reasoning_effort.is_none());
    assert!(rx.try_iter().next().is_none(), "no client message expected");
}

#[test]
fn ctrl_r_model_selected_capability_pending_shows_message() {
    let mut app = test_app();
    let (tx, rx) = std::sync::mpsc::channel();

    // A model is selected but the daemon has not reported its reasoning
    // capability yet — must not be reported as "does not support reasoning".
    app.display_for(0).reasoning_capability = None;
    app.display_for(0).selected_model = Some("gpt-4o".to_string());

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL)),
        &mut app,
        &tx,
    )
    .expect("handle ctrl+r");

    assert_eq!(
        app.status.as_deref(),
        Some("reasoning capability not yet available")
    );
    // Effort should remain unchanged (still None) and nothing is sent to
    // the daemon (no cycling, so no SetReasoningEffort).
    assert!(app.display_for(0).reasoning_effort.is_none());
    assert!(rx.try_iter().next().is_none(), "no client message expected");
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

    // A connection-level ReasoningEffortSet reply (bare `/reasoning` without
    // an attachment) arrives with `session_id: None` — it must resolve to the
    // attached session's display rather than a phantom entry.
    assert_eq!(
        app.display_for(42).reasoning_effort.as_deref(),
        Some("high")
    );
    assert_eq!(
        app.display_for(0).reasoning_effort.as_deref(),
        None,
        "a connection-level reply must resolve to the attached session, not a phantom display"
    );
}

#[test]
fn reasoning_effort_set_for_background_session_does_not_touch_attached_display() {
    let mut app = test_app();
    let (tx, _rx) = std::sync::mpsc::channel();
    // The user is viewing session 42.
    app.attached_session_id = Some(42);
    app.active_session_id = Some(42);
    // The display exists because the user attached to it (reset_for_session_switch
    // creates the entry).
    app.display_for(42).reasoning_effort = Some("off".to_string());

    // A background session (99, streamed via SubscribeAllActivity) reports a
    // reasoning-effort change.  The handler must route it to session 99's own
    // display — writing it to the active display would let a background
    // session's settings bleed into the status bar of the session being viewed.
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

    // The attached display must NOT pick up the background session's effort.
    assert_eq!(
        app.display_for(42).reasoning_effort.as_deref(),
        Some("off"),
        "attached display must not pick up a background session's reasoning effort"
    );
    // The background session's own display received the update.
    assert_eq!(
        app.display_for(99).reasoning_effort.as_deref(),
        Some("high")
    );
}

#[test]
fn model_selected_for_background_session_does_not_touch_attached_display() {
    let mut app = test_app();
    let (tx, _rx) = std::sync::mpsc::channel();
    // The user is viewing session 42.
    app.attached_session_id = Some(42);
    app.active_session_id = Some(42);
    // The display exists because the user attached to it.
    app.display_for(42).selected_model = Some("gpt-main".to_string());

    // A background session (99) changes its model.  The active display must
    // not show the background session's model in its status bar.
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
        app.display_for(42).selected_model.as_deref(),
        Some("gpt-main"),
        "attached display must not pick up a background session's model"
    );
    assert_eq!(
        app.display_for(99).selected_model.as_deref(),
        Some("gpt-other")
    );
}

#[test]
fn model_selected_for_attached_session_updates_display_and_summary() {
    let mut app = test_app();
    let (tx, _rx) = std::sync::mpsc::channel();
    // The user is viewing session 42, which is also the attached session.
    app.attached_session_id = Some(42);
    app.active_session_id = Some(42);
    app.display_for(42).selected_model = Some("gpt-old".to_string());
    // The summary the status bar's session-identity fields read from.
    app.session_mgr
        .set_sessions(vec![make_session(42, "a", "gpt-old", 0)]);

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

    // The attached session's own model change still updates the status bar's
    // identity fields (display + summary).
    assert_eq!(
        app.display_for(42).selected_model.as_deref(),
        Some("gpt-new")
    );
    assert_eq!(
        app.attached_session_mut()
            .unwrap()
            .selected_model
            .as_deref(),
        Some("gpt-new")
    );
}

#[test]
fn reasoning_effort_set_for_attached_session_updates_display_and_summary() {
    let mut app = test_app();
    let (tx, _rx) = std::sync::mpsc::channel();
    app.attached_session_id = Some(42);
    app.active_session_id = Some(42);
    app.display_for(42).reasoning_effort = Some("off".to_string());
    app.session_mgr
        .set_sessions(vec![make_session(42, "a", "m", 0)]);

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
        app.display_for(42).reasoning_effort.as_deref(),
        Some("high")
    );
    assert_eq!(
        app.attached_session_mut()
            .unwrap()
            .reasoning_effort
            .as_deref(),
        Some("high")
    );
}

#[test]
fn session_account_set_for_background_session_does_not_touch_attached_display() {
    let mut app = test_app();
    let (tx, _rx) = std::sync::mpsc::channel();
    app.attached_session_id = Some(42);
    app.active_session_id = Some(42);
    app.display_for(42).account_name = Some("main-account".to_string());

    // A background session (99) changes its account — the viewed session's
    // identity fields (account, provider slug) must not change.
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

    assert_eq!(
        app.display_for(42).account_name.as_deref(),
        Some("main-account"),
        "attached display must not pick up a background session's account"
    );
    assert_eq!(
        app.display_for(99).account_name.as_deref(),
        Some("bg-account")
    );
}

#[test]
fn reasoning_effort_set_failed_for_background_session_does_not_touch_attached_display() {
    let mut app = test_app();
    let (tx, _rx) = std::sync::mpsc::channel();
    app.attached_session_id = Some(42);
    app.active_session_id = Some(42);
    app.display_for(42).reasoning_effort = Some("high".to_string());

    // A background session's rejection must not flip the viewed session's
    // reasoning-effort display back to "off".
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
        app.display_for(42).reasoning_effort.as_deref(),
        Some("high"),
        "attached display must not be reset by a background session's rejection"
    );
    assert_eq!(app.display_for(99).reasoning_effort.as_deref(), Some("off"));
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
