//! Turn-lifecycle tests: `start_turn` id assignment, in-place tool-result
//! seeding and update, and undo / redo batching.

use super::*;
use crate::tools::ToolOutput;

// -- start_turn / undo_turns / redo_turns tests -----------------------

#[test]
fn start_turn_assigns_increasing_ids() {
    let mut state = SessionState::empty();
    let (id0, _) = state.start_turn(Some("first".into()));
    let (id1, _) = state.start_turn(Some("second".into()));
    assert_eq!(id0, 0);
    assert_eq!(id1, 1);
    assert_eq!(state.turns.len(), 2);
}

#[test]
fn seed_then_update_tool_results_preserves_call_order() {
    // The model issued calls a, b, c; the tools finish in reverse order.
    // Because results are seeded in call order and updated in place by
    // call_id, the turn's tool_results must stay a, b, c at all times.
    let mut state = SessionState::empty();
    let (tid, _) = state.start_turn(Some("run tools".into()));
    let calls = vec![
        AssistantToolCallRecord {
            call_id: "a".into(),
            name: "read_file".into(),
            arguments_json: "{}".into(),
            caller: None,
        },
        AssistantToolCallRecord {
            call_id: "b".into(),
            name: "grep".into(),
            arguments_json: "{}".into(),
            caller: None,
        },
        AssistantToolCallRecord {
            call_id: "c".into(),
            name: "sh".into(),
            arguments_json: "{}".into(),
            caller: None,
        },
    ];
    state.seed_tool_results(
        tid,
        &calls,
        &[
            "Reading `a`.".into(),
            "Grepping `b`.".into(),
            "Running `c`.".into(),
        ],
    );

    let order_of = |state: &SessionState| {
        state.turns.get(&tid).map_or_default(|t| {
            t.tool_results
                .iter()
                .map(|r| r.call_id.clone())
                .collect::<Vec<_>>()
        })
    };
    assert_eq!(order_of(&state), vec!["a", "b", "c"]);
    // Each placeholder carries its invocation description from the start
    // (the live header matches the final record's).
    assert_eq!(
        state.turns[&tid].tool_results[0].invocation_description,
        "Reading `a`."
    );
    assert_eq!(
        state.turns[&tid].tool_results[1].invocation_description,
        "Grepping `b`."
    );
    assert_eq!(
        state.turns[&tid].tool_results[2].invocation_description,
        "Running `c`."
    );

    // c finishes first — its placeholder is filled in place.
    state.update_tool_result(
        tid,
        "c",
        "sh".into(),
        &ToolOutput {
            content: "c-out".into(),
            is_error: false,
            invocation_description: String::new(),
            ..Default::default()
        },
    );
    assert_eq!(order_of(&state), vec!["a", "b", "c"]);
    assert_eq!(state.turns[&tid].tool_results[2].content, "c-out");

    // Then a, then b — order still unchanged.
    state.update_tool_result(
        tid,
        "a",
        "read_file".into(),
        &ToolOutput {
            content: "a-out".into(),
            is_error: false,
            invocation_description: String::new(),
            ..Default::default()
        },
    );
    state.update_tool_result(
        tid,
        "b",
        "grep".into(),
        &ToolOutput {
            content: "b-out".into(),
            is_error: false,
            invocation_description: String::new(),
            ..Default::default()
        },
    );
    assert_eq!(order_of(&state), vec!["a", "b", "c"]);
    assert_eq!(state.turns[&tid].tool_results[0].content, "a-out");
    assert_eq!(state.turns[&tid].tool_results[1].content, "b-out");

    // Error results also land in place.
    state.update_tool_result(
        tid,
        "b",
        "grep".into(),
        &ToolOutput {
            content: "boom".into(),
            is_error: true,
            invocation_description: String::new(),
            ..Default::default()
        },
    );
    assert_eq!(order_of(&state), vec!["a", "b", "c"]);
    assert!(state.turns[&tid].tool_results[1].is_error);
    assert_eq!(state.turns[&tid].tool_results[1].content, "boom");
}

#[test]
fn update_tool_result_unknown_call_id_is_noop() {
    // An update for an unseeded call id must not append — it would break
    // the call-order invariant.
    let mut state = SessionState::empty();
    let (tid, _) = state.start_turn(None);
    state.seed_tool_results(tid, &[], &[]);
    state.update_tool_result(
        tid,
        "ghost",
        "read_file".into(),
        &ToolOutput {
            content: "x".into(),
            is_error: false,
            invocation_description: String::new(),
            ..Default::default()
        },
    );
    assert_eq!(
        state.turns[&tid].tool_results,
        [] as [choreo_proto::ToolResultRecord; 0]
    );
}

#[test]
fn update_tool_result_sets_all_record_fields_from_output() {
    // The collapsed signature takes the five per-record fields from a
    // `ToolOutput`; this pins that it still populates every field on the
    // matched record, including the vision image reference (which was the
    // 7th parameter the vision commit added).
    let mut state = SessionState::empty();
    let (tid, _) = state.start_turn(Some("look".into()));
    let calls = vec![AssistantToolCallRecord {
        call_id: "a".into(),
        name: "read_image".into(),
        arguments_json: "{}".into(),
        caller: None,
    }];
    state.seed_tool_results(tid, &calls, &[String::new()]);

    let output = ToolOutput {
        content: "pixel".into(),
        is_error: true,
        invocation_description: "Reading `a`.".into(),
        image_ref: Some(choreo_proto::ImageReference {
            path: "/tmp/x.png".into(),
            mime_type: "image/jpeg".into(),
            width: 3,
            height: 2,
            data: Vec::new(),
        }),
        ..Default::default()
    };
    state.update_tool_result(tid, "a", "read_image".into(), &output);

    let record = &state.turns[&tid].tool_results[0];
    assert_eq!(record.name, "read_image");
    assert_eq!(record.content, "pixel");
    assert!(record.is_error);
    assert_eq!(record.invocation_description, "Reading `a`.");
    let img = record.image.as_ref().expect("image reference set");
    assert_eq!(img.path, "/tmp/x.png");
    assert_eq!(img.mime_type, "image/jpeg");
    assert_eq!(img.width, 3);
    assert_eq!(img.height, 2);
}

#[test]
fn mark_unexecuted_tool_results_marks_only_unexecuted() {
    // The model issued calls a, b, c; only a's result was recorded before
    // the request was cancelled. b and c must be marked "[cancelled —
    // result not recorded]" so the transcript and the next provider
    // request don't carry empty tool messages for calls whose outcome is
    // unknown.
    let mut state = SessionState::empty();
    let (tid, _) = state.start_turn(Some("run tools".into()));
    let calls = vec![
        AssistantToolCallRecord {
            call_id: "a".into(),
            name: "read_file".into(),
            arguments_json: "{}".into(),
            caller: None,
        },
        AssistantToolCallRecord {
            call_id: "b".into(),
            name: "grep".into(),
            arguments_json: "{}".into(),
            caller: None,
        },
        AssistantToolCallRecord {
            call_id: "c".into(),
            name: "sh".into(),
            arguments_json: "{}".into(),
            caller: None,
        },
    ];
    state.seed_tool_results(tid, &calls, &[String::new(), String::new(), String::new()]);
    // Only a's result was recorded before the cancel.
    state.update_tool_result(
        tid,
        "a",
        "read_file".into(),
        &ToolOutput {
            content: "a-out".into(),
            is_error: false,
            invocation_description: String::new(),
            ..Default::default()
        },
    );
    let executed = HashSet::from(["a".to_string()]);
    state.mark_unexecuted_tool_results(tid, &executed);

    let results = &state.turns[&tid].tool_results;
    assert_eq!(results[0].content, "a-out");
    assert!(!results[0].is_error);
    assert_eq!(results[1].content, "[cancelled — result not recorded]");
    assert!(results[1].is_error);
    assert_eq!(results[2].content, "[cancelled — result not recorded]");
    assert!(results[2].is_error);
}

#[test]
fn mark_unexecuted_tool_results_preserves_recorded_error_results() {
    // A result that WAS recorded — even an error like a timeout — has its
    // call_id in the executed set and must not be overwritten by the
    // sweep.
    let mut state = SessionState::empty();
    let (tid, _) = state.start_turn(Some("run tools".into()));
    let calls = vec![AssistantToolCallRecord {
        call_id: "a".into(),
        name: "sh".into(),
        arguments_json: "{}".into(),
        caller: None,
    }];
    state.seed_tool_results(tid, &calls, &[String::new(), String::new(), String::new()]);
    state.update_tool_result(
        tid,
        "a",
        "sh".into(),
        &ToolOutput {
            content: "timed out".into(),
            is_error: true,
            invocation_description: String::new(),
            ..Default::default()
        },
    );
    state.mark_unexecuted_tool_results(tid, &HashSet::from(["a".to_string()]));
    assert_eq!(state.turns[&tid].tool_results[0].content, "timed out");
    assert!(state.turns[&tid].tool_results[0].is_error);
}

#[test]
fn undo_turns_marks_range_and_returns_ids() {
    let mut state = SessionState::empty();
    let _ = state.start_turn(Some("user 1".into()));
    let _ = state.start_turn(Some("user 2".into()));
    assert!(state.turns.values().all(|t| !t.undone));

    let ids = state
        .undo_turns()
        .expect("undo_turns should find a user turn");
    assert_eq!(ids.len(), 1, "only the most recent user turn");
    assert!(state.turns.get(&1).unwrap().undone);
}

#[test]
fn undo_turns_returns_none_when_no_user_turn() {
    let mut state = SessionState::empty();
    let _ = state.start_turn(None); // No user_text, only system
    assert!(state.undo_turns().is_none());
}

#[test]
fn redo_turns_restores_undone_turns() {
    let mut state = SessionState::empty();
    let _ = state.start_turn(Some("user".into()));
    let ids = state.undo_turns().expect("undo succeeds");
    assert_ne!(ids, [] as [u32; 0]);

    let restored = state.redo_turns().expect("redo succeeds");
    assert_eq!(restored.len(), ids.len());
    assert!(state.turns.values().all(|t| !t.undone));
}

#[test]
fn redo_turns_returns_none_when_nothing_to_redo() {
    let mut state = SessionState::empty();
    let _ = state.start_turn(Some("user".into()));
    assert!(state.redo_turns().is_none());
}

#[test]
fn undo_redo_are_lifo_batches() {
    let mut state = SessionState::empty();
    let _ = state.start_turn(Some("user 1".into()));
    let _ = state.start_turn(Some("user 2".into()));
    let _ = state.start_turn(Some("user 3".into()));

    // First undo hides only turn 2.
    assert_eq!(state.undo_turns(), Some(vec![2]));
    // Second undo records only turn 1 — the already-hidden turn 2 is not
    // re-recorded, so it is not swept into the same redo batch.
    assert_eq!(state.undo_turns(), Some(vec![1]));
    assert!(state.turns[&1].undone && state.turns[&2].undone);

    // One redo restores exactly one batch (turn 1), NOT every hidden turn.
    let restored = state.redo_turns().expect("redo succeeds");
    assert_eq!(restored.keys().copied().collect::<Vec<_>>(), vec![1]);
    assert!(!state.turns[&1].undone, "turn 1 is live again");
    assert!(
        state.turns[&2].undone,
        "turn 2 stays hidden until its own redo batch is popped"
    );

    // The next redo restores the earlier batch (turn 2).
    let restored = state.redo_turns().expect("second redo succeeds");
    assert_eq!(restored.keys().copied().collect::<Vec<_>>(), vec![2]);
    assert!(state.turns.values().all(|t| !t.undone));

    // Nothing left to redo.
    assert!(state.redo_turns().is_none());
}

#[test]
fn redo_turns_cleared_by_new_turn_start() {
    let mut state = SessionState::empty();
    let _ = state.start_turn(Some("first".into()));
    state.undo_turns();
    // New user turn clears redo stack
    let _ = state.start_turn(Some("second".into()));
    assert!(state.redo_turns().is_none());
}

#[test]
fn undo_clears_last_response_id_for_chain_invalidation() {
    // A persisted `previous_response_id` points at a server-side response
    // whose conversation includes the undone turns — restoring it on the
    // next request would leak the undone context back into the model. Undo
    // must clear the id (and its provenance) so the next request falls
    // back to a non-chained one carrying only the visible turns.
    let (mut state, ctx) = broadcast_setup();
    // Simulate a session that chained a Responses-policy response.
    state.config.last_response_id = Some("resp_9".into());
    state.config.last_response_id_producer = Some(ReasoningProducer {
        provider_slug: "openai".into(),
        model: "gpt-5.4".into(),
    });
    let _ = state.start_turn(Some("user 2".into()));

    let mut shutdown = false;
    process_command(
        SessionCommand::Undo { reply: None },
        &mut state,
        &mut shutdown,
        &ctx,
    );

    assert_eq!(state.config.last_response_id, None);
    assert_eq!(state.config.last_response_id_producer, None);
    // The cleared id must also be persisted so a daemon restart cannot
    // resurrect the stale chain from the on-disk record.
    let record = SessionRecord::from(&state);
    assert_eq!(record.last_response_id, None);
    assert!(!shutdown);
}

#[test]
fn undo_without_response_id_leaves_session_untouched() {
    // A session that never chained must be unaffected by the undo path's
    // chain-invalidation (no record write, no id churn).
    let (mut state, ctx) = broadcast_setup();
    let _ = state.start_turn(Some("user 2".into()));
    let mut shutdown = false;
    process_command(
        SessionCommand::Undo { reply: None },
        &mut state,
        &mut shutdown,
        &ctx,
    );
    assert_eq!(state.config.last_response_id, None);
    assert_eq!(state.config.last_response_id_producer, None);
    // The most recent user turn was marked undone; the seeded turn 0 is
    // older and stays visible (undo marks only the newest user turn).
    assert!(state.turns.get(&1).expect("turn exists").undone);
    assert!(!state.turns.get(&0).expect("turn exists").undone);
    assert!(!shutdown);
}

#[test]
fn request_finished_without_undo_restores_chain_id() {
    // The worker's new response id must survive the snapshot merge — the
    // normal chaining path.
    let (mut state, ctx) = broadcast_setup();
    let mut snapshot = state.snapshot();
    snapshot.config.last_response_id = Some("resp_10".into());
    snapshot.config.last_response_id_producer = Some(ReasoningProducer {
        provider_slug: "openai".into(),
        model: "gpt-5.4".into(),
    });
    let mut shutdown = false;
    process_command(
        SessionCommand::RequestFinished {
            stream_id: 1,
            snapshot,
        },
        &mut state,
        &mut shutdown,
        &ctx,
    );
    assert_eq!(state.config.last_response_id.as_deref(), Some("resp_10"));
    assert_eq!(
        state
            .config
            .last_response_id_producer
            .as_ref()
            .unwrap()
            .model,
        "gpt-5.4",
    );
    assert!(!shutdown);
}
