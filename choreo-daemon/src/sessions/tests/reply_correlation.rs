//! Session-thread reply correlation: each session mutation emits exactly one
//! id-stamped reply (accepted / failed) alongside its `id: None` broadcasts.

use super::*;
use crate::broadcast::test_sink;

// ── Session-thread reply correlation ──────────────────────────────────────────

#[test]
fn set_model_success_acks_accepted_and_broadcasts_model_selected() {
    let (mut state, ctx) = broadcast_setup();
    let (sink, rx) = test_sink();
    state.subscribers.insert(cid(10), sink.clone());
    let target = target_for(7, MessageKind::SetModel, &sink, &ctx);

    let _ = handle_set_model("gpt-4".into(), Some(target), &mut state, &ctx);

    let broadcasts = one_reply(
        &rx,
        7,
        &DaemonMessageType::Accepted {
            kind: MessageKind::SetModel,
        },
    );
    assert!(broadcasts.iter().any(|m| matches!(
        &m.inner,
        DaemonMessageType::Session {
            event: SessionEvent::ModelSelected { .. },
            ..
        }
    )));
}

#[test]
fn set_reasoning_effort_success_acks_accepted_and_broadcasts_effort_set() {
    let (mut state, ctx) = broadcast_setup();
    let (sink, rx) = test_sink();
    state.subscribers.insert(cid(10), sink.clone());
    let target = target_for(8, MessageKind::SetReasoningEffort, &sink, &ctx);

    let _ = handle_set_reasoning_effort("high".into(), Some(target), &mut state, &ctx);

    let broadcasts = one_reply(
        &rx,
        8,
        &DaemonMessageType::Accepted {
            kind: MessageKind::SetReasoningEffort,
        },
    );
    assert!(broadcasts.iter().any(|m| matches!(
        &m.inner,
        DaemonMessageType::Session {
            event: SessionEvent::ReasoningEffortSet { .. },
            ..
        }
    )));
}

#[test]
fn set_reasoning_effort_too_long_acks_failed_and_broadcasts_failure() {
    let (mut state, ctx) = broadcast_setup();
    let (sink, rx) = test_sink();
    state.subscribers.insert(cid(10), sink.clone());
    let target = target_for(9, MessageKind::SetReasoningEffort, &sink, &ctx);

    let _ = handle_set_reasoning_effort("x".repeat(65), Some(target), &mut state, &ctx);

    let broadcasts = one_reply(
        &rx,
        9,
        &DaemonMessageType::Failed {
            kind: MessageKind::SetReasoningEffort,
            error: "reasoning effort slug too long (65 bytes)".to_string(),
        },
    );
    assert!(broadcasts.iter().any(|m| matches!(
        &m.inner,
        DaemonMessageType::Session {
            event: SessionEvent::ReasoningEffortSetFailed { .. },
            ..
        }
    )));
}

#[test]
fn set_account_success_acks_accepted_and_broadcasts_account_set() {
    let (mut state, ctx) = broadcast_setup();
    let (sink, rx) = test_sink();
    state.subscribers.insert(cid(10), sink.clone());
    let target = target_for(11, MessageKind::SetSessionAccount, &sink, &ctx);

    let _ = handle_set_account("acct".into(), Some(target), &mut state, &ctx);

    let broadcasts = one_reply(
        &rx,
        11,
        &DaemonMessageType::Accepted {
            kind: MessageKind::SetSessionAccount,
        },
    );
    assert!(broadcasts.iter().any(|m| matches!(
        &m.inner,
        DaemonMessageType::Session {
            event: SessionEvent::SessionAccountSet { .. },
            ..
        }
    )));
}

#[test]
fn undo_success_acks_accepted_and_broadcasts_turns_undone() {
    let (mut state, ctx) = broadcast_setup();
    let (sink, rx) = test_sink();
    state.subscribers.insert(cid(10), sink.clone());
    let target = target_for(12, MessageKind::Undo, &sink, &ctx);

    let _ = handle_undo(Some(target), &mut state, &ctx);

    let broadcasts = one_reply(
        &rx,
        12,
        &DaemonMessageType::Accepted {
            kind: MessageKind::Undo,
        },
    );
    assert!(broadcasts.iter().any(|m| matches!(
        &m.inner,
        DaemonMessageType::Session {
            event: SessionEvent::TurnsUndone { .. },
            ..
        }
    )));
}

#[test]
fn undo_broadcasts_recomputed_context_size() {
    let (mut state, ctx) = broadcast_setup();
    // Two live turns with usage; the second carries the larger context.
    state.turns.clear();
    state.turns.insert(0, turn_with_usage(Some(10), false));
    state.turns.insert(1, turn_with_usage(Some(42), false));
    state.config.last_prompt_tokens = Some(42);
    let (sink, rx) = test_sink();
    state.subscribers.insert(cid(10), sink.clone());
    let target = target_for(16, MessageKind::Undo, &sink, &ctx);

    let _ = handle_undo(Some(target), &mut state, &ctx);

    let broadcasts = one_reply(
        &rx,
        16,
        &DaemonMessageType::Accepted {
            kind: MessageKind::Undo,
        },
    );
    assert!(
        broadcasts.iter().any(|m| matches!(
            &m.inner,
            DaemonMessageType::Session {
                event: SessionEvent::TokenUsageUpdate {
                    last_prompt_tokens: Some(10),
                    ..
                },
                ..
            }
        )),
        "undo must broadcast the surviving turn's recomputed context size"
    );
}

#[test]
fn undo_nothing_to_undo_acks_failed() {
    let (mut state, ctx) = broadcast_setup();
    state.turns.clear();
    let (sink, rx) = test_sink();
    state.subscribers.insert(cid(10), sink.clone());
    let target = target_for(13, MessageKind::Undo, &sink, &ctx);

    let _ = handle_undo(Some(target), &mut state, &ctx);

    one_reply(
        &rx,
        13,
        &DaemonMessageType::Failed {
            kind: MessageKind::Undo,
            error: "nothing to undo".to_string(),
        },
    );
}

#[test]
fn redo_nothing_to_redo_acks_failed() {
    let (mut state, ctx) = broadcast_setup();
    let (sink, rx) = test_sink();
    state.subscribers.insert(cid(10), sink.clone());
    let target = target_for(14, MessageKind::Redo, &sink, &ctx);

    let _ = handle_redo(Some(target), &mut state, &ctx);

    one_reply(
        &rx,
        14,
        &DaemonMessageType::Failed {
            kind: MessageKind::Redo,
            error: "nothing to redo".to_string(),
        },
    );
}

#[test]
fn undo_refused_while_session_not_idle() {
    // A request in flight means the worker owns the turn history on a private
    // clone, so undo must be refused outright — and must not touch that
    // history — rather than race the merge.
    let (mut state, ctx) = broadcast_setup();
    let (cancel_tx, _cancel_rx) = crossbeam_channel::unbounded::<()>();
    state.active_requests.insert(
        7,
        ActiveRequest {
            cancel_tx,
            turn_id: 0,
        },
    );
    let before = state.turns.clone();
    let (sink, rx) = test_sink();
    state.subscribers.insert(cid(10), sink.clone());
    let target = target_for(17, MessageKind::Undo, &sink, &ctx);

    let _ = handle_undo(Some(target), &mut state, &ctx);

    one_reply(
        &rx,
        17,
        &DaemonMessageType::Failed {
            kind: MessageKind::Undo,
            error: "session is not idle".to_string(),
        },
    );
    assert_eq!(
        state.turns, before,
        "a refused undo must not mutate history"
    );
    assert!(
        state.redo_stack.is_empty(),
        "a refused undo must not push a batch"
    );
}

#[test]
fn redo_refused_while_session_not_idle() {
    let (mut state, ctx) = broadcast_setup();
    let (cancel_tx, _cancel_rx) = crossbeam_channel::unbounded::<()>();
    state.active_requests.insert(
        8,
        ActiveRequest {
            cancel_tx,
            turn_id: 0,
        },
    );
    // A redoable batch exists (turn 0 hidden), but the session is busy.
    state.turns.get_mut(&0).expect("seeded turn").undone = true;
    state.redo_stack.push(vec![0]);
    let (sink, rx) = test_sink();
    state.subscribers.insert(cid(10), sink.clone());
    let target = target_for(18, MessageKind::Redo, &sink, &ctx);

    let _ = handle_redo(Some(target), &mut state, &ctx);

    one_reply(
        &rx,
        18,
        &DaemonMessageType::Failed {
            kind: MessageKind::Redo,
            error: "session is not idle".to_string(),
        },
    );
    assert!(
        state.turns[&0].undone,
        "a refused redo must leave the turn hidden"
    );
    assert_eq!(
        state.redo_stack,
        vec![vec![0]],
        "a refused redo must not pop the stack"
    );
}

#[test]
fn set_title_success_acks_accepted_and_broadcasts_title_set() {
    let (mut state, ctx) = broadcast_setup();
    let (sink, rx) = test_sink();
    state.subscribers.insert(cid(10), sink.clone());
    // Title changes have no wire client message (the agent tool drives them), so
    // there is no `MessageKind` variant for one; the filler kind exercises the
    // generic ack path.
    let target = target_for(15, MessageKind::CreateSession, &sink, &ctx);

    let _ = handle_set_title("new title", Some(target), &mut state, &ctx);

    let (replies, broadcasts) = split_replies(&rx);
    assert_eq!(replies.len(), 1, "exactly one id-stamped reply");
    assert_eq!(replies[0].id, Some(15));
    assert!(matches!(
        &replies[0].inner,
        DaemonMessageType::Accepted { .. }
    ));
    assert!(broadcasts.iter().any(|m| matches!(
        &m.inner,
        DaemonMessageType::Session {
            event: SessionEvent::SessionTitleSet { .. },
            ..
        }
    )));
}

#[test]
fn set_title_too_long_acks_failed() {
    let (mut state, ctx) = broadcast_setup();
    let (sink, rx) = test_sink();
    state.subscribers.insert(cid(10), sink.clone());
    let target = target_for(16, MessageKind::CreateSession, &sink, &ctx);

    let _ = handle_set_title(
        &"x".repeat(MAX_TITLE_CHARS + 1),
        Some(target),
        &mut state,
        &ctx,
    );

    let (replies, _broadcasts) = split_replies(&rx);
    assert_eq!(replies.len(), 1, "exactly one id-stamped reply");
    assert_eq!(replies[0].id, Some(16));
    assert!(matches!(
        &replies[0].inner,
        DaemonMessageType::Failed { error, .. } if error == "title too long"
    ));
}

#[test]
fn set_working_dir_success_acks_accepted_and_broadcasts_workdir_set() {
    let (mut state, ctx) = broadcast_setup();
    let (sink, rx) = test_sink();
    state.subscribers.insert(cid(10), sink.clone());
    // Same filler as `set_title` (no wire client message exists).
    let target = target_for(17, MessageKind::CreateSession, &sink, &ctx);
    let (tool_tx, _tool_rx) = mpsc::channel();

    let _ = handle_set_working_dir(
        &PathBuf::from("/tmp/new-wd"),
        &tool_tx,
        Some(target),
        &mut state,
        &ctx,
    );

    let (replies, broadcasts) = split_replies(&rx);
    assert_eq!(replies.len(), 1, "exactly one id-stamped reply");
    assert_eq!(replies[0].id, Some(17));
    assert!(matches!(
        &replies[0].inner,
        DaemonMessageType::Accepted { .. }
    ));
    assert!(broadcasts.iter().any(|m| matches!(
        &m.inner,
        DaemonMessageType::Session {
            event: SessionEvent::SessionWorkingDirSet { .. },
            ..
        }
    )));
}

#[test]
fn run_input_empty_acks_failed_and_broadcasts_stream_failure() {
    let (mut state, ctx) = broadcast_setup();
    let (sink, rx) = test_sink();
    state.subscribers.insert(cid(10), sink.clone());
    let target = target_for(18, MessageKind::RunInput, &sink, &ctx);
    let mut shutdown = false;

    let _ = handle_run_input(b"", Some(target), &mut state, &mut shutdown, &ctx);

    let broadcasts = one_reply(
        &rx,
        18,
        &DaemonMessageType::Failed {
            kind: MessageKind::RunInput,
            error: "empty input".to_string(),
        },
    );
    // The daemon assigns the stream id (test_state starts the counter at 1);
    // a REJECTED run broadcasts ONLY the `Failed` (`id: None`), never a
    // `Started` — there is no stream to open, and a phantom `Started` would
    // register a live stream on every subscriber until the `Failed` cleared it.
    assert!(
        !broadcasts.iter().any(|m| matches!(
            &m.inner,
            DaemonMessageType::Session {
                event: SessionEvent::Started { .. },
                ..
            }
        )),
        "a rejected run must not broadcast a Started"
    );
    assert!(broadcasts.iter().any(|m| matches!(
        &m.inner,
        DaemonMessageType::Session {
            event: SessionEvent::Failed { stream_id: 1, .. },
            ..
        }
    )));
}

#[test]
fn run_input_accept_acks_targeted_started_and_broadcasts_started() {
    use choreo_ai_protocols::openai::{OpenAiClient, RequestAuth, ServiceConfig};

    let (mut state, ctx) = broadcast_setup();
    // A closed local endpoint: the spawned worker fails fast without touching
    // the network. The acceptance reply the test asserts is produced
    // SYNCHRONOUSLY before the worker is spawned, so the test never races it.
    let client = OpenAiClient::new(
        ServiceConfig {
            base_url: "http://127.0.0.1:9/v1".to_string(),
            ..ServiceConfig::default()
        },
        RequestAuth::api_key("k"),
        &state.registry,
    )
    .expect("build a client against the dead endpoint");
    state.provider = Some(InferenceProvider::from_openai(client));

    let (sink, rx) = test_sink();
    state.subscribers.insert(cid(10), sink.clone());
    let target = target_for(19, MessageKind::RunInput, &sink, &ctx);
    let mut shutdown = false;

    let _ = handle_run_input(b"hello", Some(target), &mut state, &mut shutdown, &ctx);

    let broadcasts = one_reply(
        &rx,
        19,
        &DaemonMessageType::Session {
            session_id: Some(1),
            event: SessionEvent::Started {
                stream_id: 1,
                turn_id: 1,
                estimated_prompt_tokens: 0,
            },
        },
    );
    // The identical `Started` is broadcast (`id: None`) to every subscriber.
    assert!(broadcasts.iter().any(|m| matches!(
        &m.inner,
        DaemonMessageType::Session {
            event: SessionEvent::Started { stream_id: 1, .. },
            ..
        }
    )));
}

#[test]
fn two_clients_same_session_get_distinct_daemon_assigned_stream_ids() {
    use choreo_ai_protocols::openai::{OpenAiClient, RequestAuth, ServiceConfig};

    let (mut state, ctx) = broadcast_setup();
    // A dead local endpoint so each accepted run's worker fails fast without
    // touching the network; the acceptance reply the test reads is produced
    // SYNCHRONOUSLY before any worker is spawned.
    let client = OpenAiClient::new(
        ServiceConfig {
            base_url: "http://127.0.0.1:9/v1".to_string(),
            ..ServiceConfig::default()
        },
        RequestAuth::api_key("k"),
        &state.registry,
    )
    .expect("build a client against the dead endpoint");
    state.provider = Some(InferenceProvider::from_openai(client));

    // Two distinct clients subscribed to the SAME session.
    let (sink_a, rx_a) = test_sink();
    let (sink_b, rx_b) = test_sink();
    state.subscribers.insert(cid(10), sink_a.clone());
    state.subscribers.insert(cid(11), sink_b.clone());

    let mut shutdown = false;
    // Client A's run is accepted; the daemon assigns the stream id.
    let target_a = target_for(20, MessageKind::RunInput, &sink_a, &ctx);
    let _ = handle_run_input(b"from a", Some(target_a), &mut state, &mut shutdown, &ctx);
    let (replies_a, _) = split_replies(&rx_a);
    let stream_a = targeted_started_stream_id(&replies_a, 20);

    // Client B runs on the same session. This unit test drives the session
    // thread directly, so the in-flight guard from A's run is reset by hand to
    // let B's run be accepted too.
    state.active_requests.clear();
    let target_b = target_for(21, MessageKind::RunInput, &sink_b, &ctx);
    let _ = handle_run_input(b"from b", Some(target_b), &mut state, &mut shutdown, &ctx);
    let (replies_b, _) = split_replies(&rx_b);
    let stream_b = targeted_started_stream_id(&replies_b, 21);

    // Daemon-assignment (a per-session monotonic counter) is what stops two
    // clients' runs from colliding on a stream id they each picked themselves.
    assert_ne!(
        stream_a, stream_b,
        "each client's run must get its own daemon-assigned stream id"
    );
}
