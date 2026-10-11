//! Session token-accounting tests: accumulation reconstruction, the live
//! last-prompt-tokens readout, mid-turn usage sync, and the usage carried on
//! snapshots and summaries.

use super::super::tokens::reconstruct_accumulated_usage;
use super::*;
use crate::broadcast::test_sink;
use crate::tools::ToolRegistry;

// ── Token accumulation tests ──────────────────────────────────────────

#[test]
fn accumulated_usage_starts_at_zero() {
    let state = SessionState::empty();
    assert_eq!(state.config.accumulated_usage.input_tokens, 0);
    assert_eq!(state.config.accumulated_usage.output_tokens, 0);
    assert_eq!(state.config.accumulated_usage.total_tokens, 0);
}

#[test]
fn accumulated_usage_reconstructed_from_turns() {
    let mut state = SessionState::empty();

    // Turn 0: no token_usage (should be filtered out)
    state.turns.insert(
        0,
        Turn {
            created_at: TimestampMs::now(),
            undone: false,
            error: None,
            user_text: Some("hello".into()),
            assistant_text: Some("hi".into()),
            assistant_reasoning: None,
            tool_calls: Vec::new(),
            token_usage: None,
            tool_results: Vec::new(),
            displayed_images: Vec::new(),
            reasoning_artifact: None,
            reasoning_producer: None,
        },
    );

    // Turn 1: has token_usage
    state.turns.insert(
        1,
        Turn {
            created_at: TimestampMs::now(),
            undone: false,
            error: None,
            user_text: Some("turn 2".into()),
            assistant_text: Some("response 2".into()),
            assistant_reasoning: None,
            tool_calls: Vec::new(),
            token_usage: Some(TokenUsage {
                input_tokens: 10,
                output_tokens: 20,
                total_tokens: 30,
                cached_tokens: 0,
                cache_write_tokens: 0,
            }),
            tool_results: Vec::new(),
            displayed_images: Vec::new(),
            reasoning_artifact: None,
            reasoning_producer: None,
        },
    );

    // Turn 2: another with token_usage
    state.turns.insert(
        2,
        Turn {
            created_at: TimestampMs::now(),
            undone: false,
            error: None,
            user_text: Some("turn 3".into()),
            assistant_text: Some("response 3".into()),
            assistant_reasoning: None,
            tool_calls: Vec::new(),
            token_usage: Some(TokenUsage {
                input_tokens: 100,
                output_tokens: 50,
                total_tokens: 150,
                cached_tokens: 0,
                cache_write_tokens: 0,
            }),
            tool_results: Vec::new(),
            displayed_images: Vec::new(),
            reasoning_artifact: None,
            reasoning_producer: None,
        },
    );

    // Turn 3: no token_usage (should be filtered out)
    state.turns.insert(
        3,
        Turn {
            created_at: TimestampMs::now(),
            undone: false,
            error: None,
            user_text: Some("no usage".into()),
            assistant_text: None,
            assistant_reasoning: None,
            tool_calls: Vec::new(),
            token_usage: None,
            tool_results: Vec::new(),
            displayed_images: Vec::new(),
            reasoning_artifact: None,
            reasoning_producer: None,
        },
    );

    // Run the same reconstruction logic from session_main
    state.config.accumulated_usage = reconstruct_accumulated_usage(&state.turns);
    state.config.last_prompt_tokens = state.live_last_prompt_tokens();

    // Expected: 10+100 = 110 input, 20+50 = 70 output, 30+150 = 180 total
    assert_eq!(state.config.accumulated_usage.input_tokens, 110);
    assert_eq!(state.config.accumulated_usage.output_tokens, 70);
    assert_eq!(state.config.accumulated_usage.total_tokens, 180);
    // last_prompt_tokens should be the most recent turn's input_tokens (turn 2 = 100)
    assert_eq!(state.config.last_prompt_tokens, Some(100));
}

#[test]
fn last_prompt_tokens_from_latest_usage_turn() {
    let mut state = SessionState::empty();

    // Turn 0: no token_usage
    state.turns.insert(
        0,
        Turn {
            created_at: TimestampMs::now(),
            undone: false,
            error: None,
            user_text: Some("no usage".into()),
            assistant_text: None,
            assistant_reasoning: None,
            tool_calls: Vec::new(),
            token_usage: None,
            tool_results: Vec::new(),
            displayed_images: Vec::new(),
            reasoning_artifact: None,
            reasoning_producer: None,
        },
    );

    // Turn 1: has token_usage
    state.turns.insert(
        1,
        Turn {
            created_at: TimestampMs::now(),
            undone: false,
            error: None,
            user_text: Some("first".into()),
            assistant_text: Some("response".into()),
            assistant_reasoning: None,
            tool_calls: Vec::new(),
            token_usage: Some(TokenUsage {
                input_tokens: 5,
                output_tokens: 10,
                total_tokens: 15,
                cached_tokens: 0,
                cache_write_tokens: 0,
            }),
            tool_results: Vec::new(),
            displayed_images: Vec::new(),
            reasoning_artifact: None,
            reasoning_producer: None,
        },
    );

    // Turn 2: has token_usage with larger input
    state.turns.insert(
        2,
        Turn {
            created_at: TimestampMs::now(),
            undone: false,
            error: None,
            user_text: Some("second".into()),
            assistant_text: Some("response 2".into()),
            assistant_reasoning: None,
            tool_calls: Vec::new(),
            token_usage: Some(TokenUsage {
                input_tokens: 42,
                output_tokens: 7,
                total_tokens: 49,
                cached_tokens: 0,
                cache_write_tokens: 0,
            }),
            tool_results: Vec::new(),
            displayed_images: Vec::new(),
            reasoning_artifact: None,
            reasoning_producer: None,
        },
    );

    // Reconstruct
    state.config.accumulated_usage = reconstruct_accumulated_usage(&state.turns);
    state.config.last_prompt_tokens = state.live_last_prompt_tokens();

    // total usage: 5+42 = 47 input, 10+7 = 17 output, 15+49 = 64 total
    assert_eq!(state.config.accumulated_usage.input_tokens, 47);
    assert_eq!(state.config.accumulated_usage.output_tokens, 17);
    assert_eq!(state.config.accumulated_usage.total_tokens, 64);
    // Most recent turn with usage is turn 2 → input_tokens = 42
    assert_eq!(state.config.last_prompt_tokens, Some(42));
}

#[test]
fn last_prompt_tokens_none_when_no_turns_have_usage() {
    let mut state = SessionState::empty();
    state.turns.insert(
        0,
        Turn {
            created_at: TimestampMs::now(),
            undone: false,
            error: None,
            user_text: Some("no usage".into()),
            assistant_text: None,
            assistant_reasoning: None,
            tool_calls: Vec::new(),
            token_usage: None,
            tool_results: Vec::new(),
            displayed_images: Vec::new(),
            reasoning_artifact: None,
            reasoning_producer: None,
        },
    );

    state.config.accumulated_usage = reconstruct_accumulated_usage(&state.turns);
    state.config.last_prompt_tokens = state.live_last_prompt_tokens();

    assert_eq!(state.config.accumulated_usage.input_tokens, 0);
    assert_eq!(state.config.last_prompt_tokens, None);
}

#[test]
fn live_last_prompt_tokens_skips_undone_and_usage_less() {
    let mut state = SessionState::empty();
    state.turns.insert(0, turn_with_usage(Some(10), false));
    state.turns.insert(1, turn_with_usage(Some(42), false));
    // A newer turn with no usage (cancelled / user-only) contributes nothing.
    state.turns.insert(2, turn_with_usage(None, false));
    // The most recent turn is undone, so it is skipped.
    state.turns.insert(3, turn_with_usage(Some(99), true));

    assert_eq!(state.live_last_prompt_tokens(), Some(42));

    // Undoing the newest live turn drops back to the earlier measurement.
    state.turns.get_mut(&1).expect("turn 1 exists").undone = true;
    assert_eq!(state.live_last_prompt_tokens(), Some(10));

    // With nothing live left, there is no context size to show.
    state.turns.get_mut(&0).expect("turn 0 exists").undone = true;
    assert_eq!(state.live_last_prompt_tokens(), None);
}

#[test]
fn reconstruct_accumulated_usage_includes_undone_turns() {
    // Billing is a lifetime total: undo hides turns from the model but does not
    // refund tokens already billed, so the reconstruction still sums them.
    let mut turns = BTreeMap::new();
    turns.insert(0, turn_with_usage(Some(10), false));
    turns.insert(1, turn_with_usage(Some(42), true));

    let accumulated = reconstruct_accumulated_usage(&turns);
    assert_eq!(accumulated.input_tokens, 52);
}

/// A turn with distinctive nonzero counts on *every* usage field, cache
/// included, so a summation that drops a field shows up in the assertion.
fn turn_with_cache_usage(
    input: u32,
    output: u32,
    cached: u32,
    cache_write: u32,
    undone: bool,
) -> Turn {
    Turn {
        created_at: TimestampMs::now(),
        undone,
        error: None,
        user_text: Some("u".into()),
        assistant_text: Some("a".into()),
        assistant_reasoning: None,
        tool_calls: Vec::new(),
        token_usage: Some(TokenUsage {
            input_tokens: input,
            output_tokens: output,
            total_tokens: input + output,
            cached_tokens: cached,
            cache_write_tokens: cache_write,
        }),
        tool_results: Vec::new(),
        displayed_images: Vec::new(),
        reasoning_artifact: None,
        reasoning_producer: None,
    }
}

#[test]
fn reconstruct_accumulated_usage_includes_cache_counts() {
    // The reconstruction sums *every* usage field, not just the
    // input/output/total triple: the prompt-cache read/write counts are billed
    // separately, so dropping them silently under-reports a session's cache
    // spend. The undone turn still counts — billing is a lifetime total.
    let mut turns = BTreeMap::new();
    turns.insert(0, turn_with_cache_usage(100, 20, 40, 5, false));
    turns.insert(1, turn_with_cache_usage(200, 30, 150, 25, true));

    let accumulated = reconstruct_accumulated_usage(&turns);
    // input: 100 + 200, output: 20 + 30, total: 120 + 230
    assert_eq!(accumulated.input_tokens, 300);
    assert_eq!(accumulated.output_tokens, 50);
    assert_eq!(accumulated.total_tokens, 350);
    // The previously-dropped cache counts must survive the sum.
    assert_eq!(accumulated.cached_tokens, 190);
    assert_eq!(accumulated.cache_write_tokens, 30);
}

#[test]
fn undo_and_redo_recompute_last_prompt_tokens_from_live_turns() {
    let mut state = SessionState::empty();
    state.turns.insert(0, turn_with_usage(Some(10), false));
    state.turns.insert(1, turn_with_usage(Some(42), false));
    state.config.last_prompt_tokens = state.live_last_prompt_tokens();
    assert_eq!(state.config.last_prompt_tokens, Some(42));

    // Undo turn 1 → the context shrinks to turn 0's exact measurement.
    let undone = state.undo_turns().expect("a user turn to undo");
    assert_eq!(undone, vec![1]);
    assert_eq!(state.config.last_prompt_tokens, Some(10));

    // Redo restores turn 1 → the context grows back.
    state.redo_turns().expect("turns to redo");
    assert_eq!(state.config.last_prompt_tokens, Some(42));
}

#[test]
fn accumulated_usage_in_snapshot() {
    let mut state = SessionState::empty();
    state.config.accumulated_usage = TokenUsage {
        input_tokens: 50,
        output_tokens: 25,
        total_tokens: 75,
        cached_tokens: 0,
        cache_write_tokens: 0,
    };
    let snap = state.snapshot();
    assert_eq!(snap.config.accumulated_usage.input_tokens, 50);
    assert_eq!(snap.config.accumulated_usage.output_tokens, 25);
    assert_eq!(snap.config.accumulated_usage.total_tokens, 75);
}

#[test]
fn accumulated_usage_in_session_summary() {
    let (mut state, ctx) = broadcast_setup();
    state.config.accumulated_usage = TokenUsage {
        input_tokens: 80,
        output_tokens: 40,
        total_tokens: 120,
        cached_tokens: 0,
        cache_write_tokens: 0,
    };

    let (reply, rx) = mpsc::channel();
    let mut shutdown = false;
    process_command(
        SessionCommand::GetSummary { reply },
        &mut state,
        &mut shutdown,
        &ctx,
    );

    let summary: SessionSummary = rx.recv().unwrap();
    let summary_usage = summary
        .token_usage
        .expect("token_usage should be present in SessionSummary");
    assert_eq!(summary_usage.input_tokens, 80);
    assert_eq!(summary_usage.output_tokens, 40);
    assert_eq!(summary_usage.total_tokens, 120);
}

#[test]
fn accumulated_usage_in_attach_snapshot() {
    let (mut state, ctx) = broadcast_setup();
    state.config.accumulated_usage = TokenUsage {
        input_tokens: 30,
        output_tokens: 15,
        total_tokens: 45,
        cached_tokens: 0,
        cache_write_tokens: 0,
    };

    let (sub_tx, sub_rx) = test_sink();
    let mut shutdown = false;
    process_command(
        SessionCommand::Attach {
            client_id: cid(42),
            tx: sub_tx,
        },
        &mut state,
        &mut shutdown,
        &ctx,
    );

    let msg = sub_rx.recv().unwrap();
    match msg.inner {
        DaemonMessageType::Session {
            event: SessionEvent::SessionState { token_usage, .. },
            ..
        } => {
            let usage = token_usage.expect("token_usage in SessionState");
            assert_eq!(usage.input_tokens, 30);
            assert_eq!(usage.output_tokens, 15);
            assert_eq!(usage.total_tokens, 45);
        }
        other => panic!("expected SessionState, got {other:?}"),
    }
}

#[test]
fn sync_accumulated_usage_updates_config_and_broadcasts() {
    // A live daemon channel is required here: `broadcast_setup()` drops the
    // receiver, but this test asserts the `UpdateMetadata` refresh lands.
    let db = test_db();
    let tool_registry = ToolRegistry::new().build().into_shared();
    let (daemon_tx, daemon_rx) = crossbeam_channel::unbounded();
    let (cmd_tx, _cmd_rx) = crossbeam_channel::unbounded();
    let ctx = RequestContext {
        cmd_tx,
        session_id: 1,
        db,
        tool_registry,
        daemon_tx,
        max_turns: 0,
        lag_limits: LagLimits::default(),
        global_lag: Arc::new(AtomicUsize::new(0)),
        substrate_credential: None,
        warm_policy: crate::cache_warm::WarmPolicy::default(),
    };
    let mut state = test_state();

    let (sub_tx, sub_rx) = test_sink();
    state.subscribers.insert(cid(42), sub_tx);

    // The worker's cumulative total, as routed from the private clone in
    // `broadcast_token_usage` (requests.rs).
    let synced = TokenUsage {
        input_tokens: 30,
        output_tokens: 15,
        total_tokens: 45,
        cached_tokens: 0,
        cache_write_tokens: 0,
    };
    let mut shutdown = false;
    process_command(
        SessionCommand::SyncAccumulatedUsage {
            token_usage: synced,
            last_prompt_tokens: Some(30),
        },
        &mut state,
        &mut shutdown,
        &ctx,
    );

    // The authoritative config is updated first, so attach snapshots and
    // session metadata read the fresh mid-turn totals.
    assert_eq!(state.config.accumulated_usage, synced);
    assert_eq!(state.config.last_prompt_tokens, Some(30));

    // ...and the update is broadcast from the authoritative state.
    let msg = sub_rx.recv().unwrap();
    match msg.inner {
        DaemonMessageType::Session {
            session_id: Some(session_id),
            event:
                SessionEvent::TokenUsageUpdate {
                    token_usage,
                    last_prompt_tokens,
                },
        } => {
            assert_eq!(session_id, ctx.session_id);
            assert_eq!(token_usage, synced);
            assert_eq!(last_prompt_tokens, Some(30));
        }
        other => panic!("expected TokenUsageUpdate, got {other:?}"),
    }

    // `broadcast` first forwards the activity to the daemon, then the
    // handler refreshes the session-metadata index — both on the same
    // thread, so ordering is deterministic.
    match daemon_rx.recv().unwrap() {
        DaemonCommand::BroadcastActivity { .. } => {}
        _ => panic!("expected BroadcastActivity forward before UpdateMetadata"),
    }
    match daemon_rx.recv().unwrap() {
        DaemonCommand::UpdateMetadata {
            session_id,
            metadata,
        } => {
            assert_eq!(session_id, ctx.session_id);
            assert_eq!(metadata.accumulated_usage, synced);
        }
        _ => panic!("expected UpdateMetadata with refreshed accumulated usage"),
    }
    assert!(!shutdown);
}

#[test]
fn attach_snapshot_carries_mid_turn_accumulated_usage() {
    // Regression test for the stale-snapshot bug: the worker accumulates
    // usage on its private session clone, so without `SyncAccumulatedUsage`
    // the main config would still carry the pre-request total and the
    // attach snapshot would leak that stale value for the whole turn.
    let (mut state, ctx) = broadcast_setup();

    let synced = TokenUsage {
        input_tokens: 30,
        output_tokens: 15,
        total_tokens: 45,
        cached_tokens: 0,
        cache_write_tokens: 0,
    };
    let mut shutdown = false;
    process_command(
        SessionCommand::SyncAccumulatedUsage {
            token_usage: synced,
            last_prompt_tokens: Some(30),
        },
        &mut state,
        &mut shutdown,
        &ctx,
    );

    let (sub_tx, sub_rx) = test_sink();
    process_command(
        SessionCommand::Attach {
            client_id: cid(42),
            tx: sub_tx,
        },
        &mut state,
        &mut shutdown,
        &ctx,
    );

    let msg = sub_rx.recv().unwrap();
    match msg.inner {
        DaemonMessageType::Session {
            event: SessionEvent::SessionState { token_usage, .. },
            ..
        } => {
            let usage = token_usage.expect("token_usage in SessionState");
            assert_eq!(usage.input_tokens, 30);
            assert_eq!(usage.output_tokens, 15);
            assert_eq!(usage.total_tokens, 45);
        }
        other => panic!("expected SessionState, got {other:?}"),
    }
}

#[test]
fn sync_accumulated_usage_never_regresses_config() {
    // The config must be monotonic even if a sync arrives out of order or
    // from an overlapping worker: a per-field max, never a blind assign.
    let db = test_db();
    let tool_registry = ToolRegistry::new().build().into_shared();
    let (daemon_tx, _daemon_rx) = crossbeam_channel::unbounded();
    let (cmd_tx, _cmd_rx) = crossbeam_channel::unbounded();
    let ctx = RequestContext {
        cmd_tx,
        session_id: 1,
        db,
        tool_registry,
        daemon_tx,
        max_turns: 0,
        lag_limits: LagLimits::default(),
        global_lag: Arc::new(AtomicUsize::new(0)),
        substrate_credential: None,
        warm_policy: crate::cache_warm::WarmPolicy::default(),
    };
    let mut state = test_state();
    let mut shutdown = false;

    // A fresher (larger) total lands first…
    process_command(
        SessionCommand::SyncAccumulatedUsage {
            token_usage: TokenUsage {
                input_tokens: 30,
                output_tokens: 15,
                total_tokens: 45,
                cached_tokens: 0,
                cache_write_tokens: 0,
            },
            last_prompt_tokens: Some(30),
        },
        &mut state,
        &mut shutdown,
        &ctx,
    );
    // …then a stale (smaller) sync that a blind assignment would regress.
    process_command(
        SessionCommand::SyncAccumulatedUsage {
            token_usage: TokenUsage {
                input_tokens: 5,
                output_tokens: 3,
                total_tokens: 8,
                cached_tokens: 0,
                cache_write_tokens: 0,
            },
            last_prompt_tokens: None,
        },
        &mut state,
        &mut shutdown,
        &ctx,
    );

    assert_eq!(state.config.accumulated_usage.total_tokens, 45);
    assert_eq!(state.config.accumulated_usage.input_tokens, 30);
    assert_eq!(state.config.accumulated_usage.output_tokens, 15);
    assert_eq!(state.config.last_prompt_tokens, Some(30));
}

#[test]
fn attach_with_active_requests_sends_started_to_new_subscriber() {
    let (mut state, ctx) = broadcast_setup();

    // Populate active requests as if a request is in flight.
    let (cancel_tx1, _cancel_rx1) = crossbeam_channel::unbounded::<()>();
    let (cancel_tx2, _cancel_rx2) = crossbeam_channel::unbounded::<()>();
    state.active_requests.insert(
        10,
        ActiveRequest {
            cancel_tx: cancel_tx1,
            turn_id: 3,
        },
    );
    state.active_requests.insert(
        20,
        ActiveRequest {
            cancel_tx: cancel_tx2,
            turn_id: 7,
        },
    );

    let (sub_tx, sub_rx) = test_sink();
    let mut shutdown = false;
    process_command(
        SessionCommand::Attach {
            client_id: cid(42),
            tx: sub_tx,
        },
        &mut state,
        &mut shutdown,
        &ctx,
    );

    // Expect Start messages for each active request, in insertion order.
    match sub_rx.recv().unwrap().inner {
        DaemonMessageType::Session {
            session_id: Some(1),
            event:
                SessionEvent::Started {
                    stream_id: 10,
                    turn_id: 3,
                    estimated_prompt_tokens: 0,
                },
        } => {}
        other => panic!("expected Started(10, turn=3), got {other:?}"),
    }
    match sub_rx.recv().unwrap().inner {
        DaemonMessageType::Session {
            session_id: Some(1),
            event:
                SessionEvent::Started {
                    stream_id: 20,
                    turn_id: 7,
                    estimated_prompt_tokens: 0,
                },
        } => {}
        other => panic!("expected Started(20, turn=7), got {other:?}"),
    }

    // Followed by SessionState.
    match sub_rx.recv().unwrap().inner {
        DaemonMessageType::Session {
            event: SessionEvent::SessionState { .. },
            ..
        } => {}
        other => panic!("expected SessionState, got {other:?}"),
    }

    assert!(!shutdown);
}

#[test]
fn attach_without_active_requests_does_not_send_started() {
    let (mut state, ctx) = broadcast_setup();
    // No active_requests — default empty.

    let (sub_tx, sub_rx) = test_sink();
    let mut shutdown = false;
    process_command(
        SessionCommand::Attach {
            client_id: cid(42),
            tx: sub_tx,
        },
        &mut state,
        &mut shutdown,
        &ctx,
    );

    // Only SessionState — no Started messages.
    match sub_rx.recv().unwrap().inner {
        DaemonMessageType::Session {
            event: SessionEvent::SessionState { .. },
            ..
        } => {}
        other => panic!("expected SessionState, got {other:?}"),
    }

    // No more messages.
    assert!(sub_rx.try_recv().is_err());
    assert!(!shutdown);
}
