use crate::connection::handle_daemon_message;
use crate::tests::support::test_app;
use choreo_proto::{DaemonMessage, DaemonMessageType, SessionEvent, SessionStatus, TokenUsage};

// ── handle_daemon_message progress bar integration ──

// fn-level #[allow]: clippy's assert_is_empty rewrite is worse than the
// deliberate `assert!(x.is_empty())` check below.
#[expect(clippy::assert_is_empty)]
#[test]
fn daemon_message_session_state_updates_progress_for_attached_session() {
    let mut app = test_app();
    let (tx, _rx) = crossbeam_channel::unbounded();
    app.attached_session_id = Some(7);

    handle_daemon_message(
        DaemonMessage::broadcast(DaemonMessageType::Session {
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
                    cached_tokens: 0,
                    cache_write_tokens: 0,
                }),
                context_window: Some(4096),
                last_prompt_tokens: Some(1),
                reasoning_effort: None,
                reasoning_capability: None,
                status: SessionStatus::Inactive,
            },
        }),
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
            cached_tokens: 0,
            cache_write_tokens: 0,
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
    let (tx, _rx) = crossbeam_channel::unbounded();
    // Tool groups only reach the status bar when the snapshot belongs to the
    // attached session.
    app.attached_session_id = Some(7);

    handle_daemon_message(
        DaemonMessage::broadcast(DaemonMessageType::Session {
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
        }),
        &mut app,
        &tx,
    )
    .expect("handle_daemon_message should succeed");

    assert_eq!(app.attached_tool_groups, vec!["core", "browser"]);
}

#[test]
fn daemon_message_session_state_ignores_wrong_session() {
    let mut app = test_app();
    let (tx, _rx) = crossbeam_channel::unbounded();
    app.attached_session_id = Some(7);

    handle_daemon_message(
        DaemonMessage::broadcast(DaemonMessageType::Session {
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
                    cached_tokens: 0,
                    cache_write_tokens: 0,
                }),
                context_window: Some(1024),
                last_prompt_tokens: None,
                reasoning_effort: None,
                reasoning_capability: None,
                status: SessionStatus::Inactive,
            },
        }),
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
            cached_tokens: 0,
            cache_write_tokens: 0,
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
    let (tx, _rx) = crossbeam_channel::unbounded();
    // Session 42 is the hypothetical attached session — a nonzero id the
    // daemon can actually assign (`Some(0)` would be wire-implausible: ids
    // start at 1, and `None` — not `0` — marks a connection-level reply).
    app.attached_session_id = Some(42);

    handle_daemon_message(
        DaemonMessage::broadcast(DaemonMessageType::Session {
            session_id: Some(42),
            event: SessionEvent::Done {
                stream_id: 1,
                token_usage: Some(TokenUsage {
                    input_tokens: 5,
                    output_tokens: 10,
                    total_tokens: 15,
                    cached_tokens: 0,
                    cache_write_tokens: 0,
                }),
                last_prompt_tokens: Some(5),
            },
        }),
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
            cached_tokens: 0,
            cache_write_tokens: 0,
        })
    );
    assert!(app.display_for(42).progress_dirty);
}

#[test]
fn daemon_message_done_without_token_usage_does_not_change_progress() {
    let mut app = test_app();
    let (tx, _rx) = crossbeam_channel::unbounded();

    // `Done` is a requires-origin event (the daemon always carries the real
    // session id in `Some`), so the fixture uses a nonzero id.
    handle_daemon_message(
        DaemonMessage::broadcast(DaemonMessageType::Session {
            session_id: Some(42),
            event: SessionEvent::Done {
                stream_id: 1,
                token_usage: None,
                last_prompt_tokens: None,
            },
        }),
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
    let (tx, _rx) = crossbeam_channel::unbounded();
    // The user is viewing session 0, which already has settled token usage.
    app.attached_session_id = Some(0);
    app.display_for(0).token_usage = Some(TokenUsage {
        input_tokens: 1,
        output_tokens: 2,
        total_tokens: 3,
        cached_tokens: 0,
        cache_write_tokens: 0,
    });

    // Session 7 (background, streamed via SubscribeAllActivity) reports its
    // live output-token count while the user keeps looking at session 0.
    handle_daemon_message(
        DaemonMessage::broadcast(DaemonMessageType::Session {
            session_id: Some(7),
            event: SessionEvent::LiveOutputTokenCount {
                stream_id: 50,
                output_tokens: 99,
            },
        }),
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
            cached_tokens: 0,
            cache_write_tokens: 0,
        })
    );
}

#[test]
fn live_output_token_count_updates_own_session_after_switch() {
    let mut app = test_app();
    let (tx, _rx) = crossbeam_channel::unbounded();
    // The user switches to session 7 mid-stream; reset_for_session_switch
    // preserved its accumulated live token estimate.
    app.attached_session_id = Some(7);
    app.active_session_id = Some(7);
    app.display_for(7).token_usage = Some(TokenUsage {
        input_tokens: 10,
        output_tokens: 5,
        total_tokens: 15,
        cached_tokens: 0,
        cache_write_tokens: 0,
    });

    handle_daemon_message(
        DaemonMessage::broadcast(DaemonMessageType::Session {
            session_id: Some(7),
            event: SessionEvent::LiveOutputTokenCount {
                stream_id: 50,
                output_tokens: 42,
            },
        }),
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
            cached_tokens: 0,
            cache_write_tokens: 0,
        })
    );
}

#[test]
fn session_state_snapshot_does_not_regress_fresher_token_usage() {
    let mut app = test_app();
    let (tx, _rx) = crossbeam_channel::unbounded();
    app.attached_session_id = Some(7);

    // While the session streamed in the background, the all-activity
    // subscription accumulated the worker's fresh cumulative usage.
    app.display_for(7).token_usage = Some(TokenUsage {
        input_tokens: 10,
        output_tokens: 5,
        total_tokens: 15,
        cached_tokens: 0,
        cache_write_tokens: 0,
    });

    // The attach snapshot is built from the session thread's config, which
    // can lag the worker's live total for a mid-turn session — it must not
    // regress the status bar's token readout.
    handle_daemon_message(
        DaemonMessage::broadcast(DaemonMessageType::Session {
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
                    cached_tokens: 0,
                    cache_write_tokens: 0,
                }),
                context_window: None,
                last_prompt_tokens: None,
                reasoning_effort: None,
                reasoning_capability: None,
                status: SessionStatus::Inactive,
            },
        }),
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
            cached_tokens: 0,
            cache_write_tokens: 0,
        })
    );
}

#[test]
fn session_state_snapshot_with_newer_larger_usage_updates_display() {
    let mut app = test_app();
    let (tx, _rx) = crossbeam_channel::unbounded();
    app.attached_session_id = Some(7);

    // The display holds an older total; the snapshot is authoritative and
    // must still win when it carries a larger (newer) cumulative value.
    app.display_for(7).token_usage = Some(TokenUsage {
        input_tokens: 1,
        output_tokens: 2,
        total_tokens: 3,
        cached_tokens: 0,
        cache_write_tokens: 0,
    });

    handle_daemon_message(
        DaemonMessage::broadcast(DaemonMessageType::Session {
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
                    cached_tokens: 0,
                    cache_write_tokens: 0,
                }),
                context_window: None,
                last_prompt_tokens: None,
                reasoning_effort: None,
                reasoning_capability: None,
                status: SessionStatus::Inactive,
            },
        }),
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
            cached_tokens: 0,
            cache_write_tokens: 0,
        })
    );
}

#[test]
fn session_state_snapshot_does_not_regress_fresher_last_prompt_tokens() {
    let mut app = test_app();
    let (tx, _rx) = crossbeam_channel::unbounded();
    app.attached_session_id = Some(7);

    // The display already holds the fresher value (a mid-turn
    // TokenUsageUpdate or the Done at turn end).  Unlike cumulative usage,
    // last_prompt_tokens is not monotonic, so the snapshot must gap-fill
    // rather than overwrite: a lagging attach snapshot (built from the
    // session thread's config before the latest broadcast) must not regress
    // the progress bar until the next update.
    app.display_for(7).last_prompt_tokens = Some(5000);

    handle_daemon_message(
        DaemonMessage::broadcast(DaemonMessageType::Session {
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
        }),
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
    let (tx, _rx) = crossbeam_channel::unbounded();
    app.attached_session_id = Some(7);

    // No value shown yet — the snapshot's value must still be applied
    // (gap-fill only blocks regression, not forward progress).
    assert!(app.display_for(7).last_prompt_tokens.is_none());

    handle_daemon_message(
        DaemonMessage::broadcast(DaemonMessageType::Session {
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
        }),
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

#[test]
fn token_usage_update_overwrites_and_clears_last_prompt_tokens() {
    let mut app = test_app();
    let (tx, _rx) = crossbeam_channel::unbounded();
    app.attached_session_id = Some(7);

    // A larger value is already shown (the pre-undo context).
    app.display_for(7).last_prompt_tokens = Some(5000);

    // The recomputed post-undo size lowers it. TokenUsageUpdate is the
    // authoritative token refresh, so it overwrites unconditionally —
    // unlike the gap-filling attach-snapshot path just above.
    handle_daemon_message(
        DaemonMessage::broadcast(DaemonMessageType::Session {
            session_id: Some(7),
            event: SessionEvent::TokenUsageUpdate {
                token_usage: TokenUsage {
                    input_tokens: 100,
                    output_tokens: 0,
                    total_tokens: 100,
                    cached_tokens: 0,
                    cache_write_tokens: 0,
                },
                last_prompt_tokens: Some(10),
            },
        }),
        &mut app,
        &tx,
    )
    .expect("handle_daemon_message should succeed");

    assert_eq!(
        app.display_for(7).last_prompt_tokens,
        Some(10),
        "a recomputed context size must overwrite the stale pre-undo value"
    );

    // Every turn undone → the update carries None, which must CLEAR the
    // readout rather than being ignored.
    handle_daemon_message(
        DaemonMessage::broadcast(DaemonMessageType::Session {
            session_id: Some(7),
            event: SessionEvent::TokenUsageUpdate {
                token_usage: TokenUsage::default(),
                last_prompt_tokens: None,
            },
        }),
        &mut app,
        &tx,
    )
    .expect("handle_daemon_message should succeed");

    assert!(
        app.display_for(7).last_prompt_tokens.is_none(),
        "a None (nothing left to measure) must clear the readout"
    );
}
