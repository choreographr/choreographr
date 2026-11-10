//! agent loop tests (see the parent `tests` module).

use super::*;

#[test]
fn agent_loop_failure_marks_and_finalizes_turn() {
    // A provider-level failure (e.g. 402 Insufficient Balance) must be
    // recorded on the turn and the turn finalized + broadcast so clients
    // render a red "Error:" block in the transcript and the failure
    // survives a daemon restart — while the loop still reports the
    // original inference error to the caller (RequestOutcome::Failed).
    let mut h = agent_loop_harness(ToolRegistry::new());
    let provider = make_failing_provider();
    let (_cancel_tx, cancel_rx) = crossbeam_channel::unbounded::<()>();

    let result = run_agent_loop(
        &provider,
        &mut h.session,
        "test-model",
        7,
        &cancel_rx,
        &h.ctx,
        Some("hi"),
    );

    // The inference error propagates to the caller unchanged.
    let err = result.expect_err("the failing provider must fail the request");
    let msg = err.to_string();
    assert!(
        msg.contains("402") && msg.contains("Insufficient Balance"),
        "expected the 402 client error, got: {msg}"
    );

    // The open turn carries the failure so clients can render it.
    let turn = h.session.turns.get(&0).expect("turn 0 exists");
    assert_eq!(
        turn.error.as_deref(),
        Some("client error (402): Insufficient Balance")
    );
    assert_eq!(turn.user_text.as_deref(), Some("hi"));

    // The turn was finalized: a TurnAppended broadcast carries the error
    // to clients (the authoritative turn keeps it too, for the DB write).
    // The stream also contains mid-turn TurnAppended broadcasts (the
    // user-text append) that legitimately carry no error, so require only
    // that an error-bearing TurnAppended arrived.
    let mut saw_error_appended = false;
    while let Ok(msg) = h.cmd_rx.try_recv() {
        if let SessionCommand::Broadcast(DaemonMessageType::Session {
            event: SessionEvent::TurnAppended { turn, .. },
            ..
        }) = msg
            && let Some(err) = turn.error
        {
            assert_eq!(err, "client error (402): Insufficient Balance");
            saw_error_appended = true;
        }
    }
    assert!(
        saw_error_appended,
        "expected a TurnAppended broadcast carrying the failure"
    );
}

#[test]
fn agent_loop_persists_each_tool_turn_at_its_iteration_boundary() {
    // Durability boundary: a tool-loop turn must reach the database when the
    // iteration that produced it completes, not only when the whole request
    // finishes. The loop owns the turn set for the duration of a request — the
    // session thread only merges the worker's snapshot at `RequestFinished` —
    // and one request can run unbounded tool iterations over a long
    // wall-clock window, so a write deferred to the end loses every turn a
    // mid-request process death cuts off.
    //
    // `handle_request_finished` is deliberately NOT called here: any turn row
    // in the database can therefore only have come from the loop's own
    // per-iteration checkpoint.
    // A registry holding only `random` — a pure computation, so the loop
    // dispatches a real tool with no fixture, no filesystem and no credential.
    let mut registry = ToolRegistry::new();
    registry.register(crate::tools::random::Random);
    let mut h = agent_loop_harness(registry);
    // Two tool turns, then a final answer: three turns inside one request.
    let provider = make_tool_loop_provider(2);
    let (_cancel_tx, cancel_rx) = crossbeam_channel::unbounded::<()>();

    let result = run_agent_loop(
        &provider,
        &mut h.session,
        "test-model",
        7,
        &cancel_rx,
        &h.ctx,
        Some("hi"),
    );
    assert!(
        !result.unwrap(),
        "the final-answer turn completes without another tool call"
    );

    let persisted = crate::db::read_turns(&h.db, 1).unwrap();
    assert_eq!(
        persisted.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
        vec![0, 1, 2],
        "both tool-loop turns and the final turn must be on disk"
    );

    // The tool turns are complete on disk, not bare placeholders: each carries
    // the assistant's text, its call, and the recorded result for that call.
    for turn_id in [0u32, 1u32] {
        let turn = &persisted[turn_id as usize].1;
        let expected_text = format!("step {turn_id}");
        assert_eq!(
            turn.user_text.is_some(),
            turn_id == 0,
            "only the request's first turn carries the user's text"
        );
        assert_eq!(turn.assistant_text.as_deref(), Some(expected_text.as_str()));
        assert_eq!(turn.tool_calls.len(), 1);
        assert_eq!(turn.tool_calls[0].call_id, format!("call_{turn_id}"));
        assert_eq!(turn.tool_results.len(), 1);
        assert_eq!(turn.tool_results[0].call_id, format!("call_{turn_id}"));
        assert!(
            !turn.tool_results[0].is_error,
            "the random tool must succeed"
        );
    }
    assert_eq!(
        persisted[2].1.assistant_text.as_deref(),
        Some("done"),
        "the final turn is persisted too"
    );
}

#[test]
fn cancelled_agent_loop_persists_the_partial_turn() {
    // The tool-dispatch cancel arm checkpoints the turn it marked up, exactly
    // like the normal path. Assert the turn reaches disk WITHOUT
    // `handle_request_finished` running, so the only possible writer is the
    // iteration-boundary checkpoint on the cancel path.
    let (cancel_tx, cancel_rx) = crossbeam_channel::unbounded::<()>();
    let (proceed_tx, proceed_rx) = mpsc::channel::<()>();

    let mut registry = ToolRegistry::new();
    registry.register(CancelThenBlockTool {
        cancel_tx: cancel_tx.clone(),
        proceed: std::sync::Mutex::new(Some(proceed_rx)),
    });
    let mut h = agent_loop_harness(registry);
    // One tool turn: the request cancels during the first batch, before any
    // final answer, so exactly one turn is produced.
    let provider = make_tool_loop_provider_with_tool(1, "_test_cancel_block");

    let cancelled = run_agent_loop(
        &provider,
        &mut h.session,
        "test-model",
        7,
        &cancel_rx,
        &h.ctx,
        Some("hi"),
    )
    .unwrap();
    assert!(cancelled, "the request must report cancellation");

    // Release the still-blocked tool thread so it can exit; its late result is
    // discarded because the wait-loop already stopped on the kill.
    drop(proceed_tx);

    let persisted = crate::db::read_turns(&h.db, 1).unwrap();
    assert_eq!(
        persisted.len(),
        1,
        "the cancelled turn must be checkpointed to disk"
    );
    let turn = &persisted[0].1;
    assert_eq!(turn.user_text.as_deref(), Some("hi"));
    assert_eq!(turn.assistant_text.as_deref(), Some("step 0"));
    assert_eq!(turn.tool_calls.len(), 1);
    assert_eq!(turn.tool_calls[0].name, "_test_cancel_block");
    assert_eq!(
        turn.tool_results.len(),
        1,
        "the blocked call still records a result placeholder"
    );
    assert!(
        turn.tool_results[0].is_error,
        "a cancelled call is recorded as an error result"
    );
}

#[test]
fn agent_loop_broadcasts_forward_only_streaming_phases() {
    // Reasoning, then answer, then a trailing reasoning delta: the phase must
    // reach `Responding` and STAY there (the later reasoning chunk must not
    // demote it back to `Reasoning`), and neither `Sending` nor `Waiting` must
    // be re-emitted.
    let mut h = agent_loop_harness(ToolRegistry::new());
    let provider = make_scripted_stream_provider(
        vec![
            StreamEvent::Reasoning("think".into()),
            StreamEvent::Answer("hi".into()),
            StreamEvent::Reasoning("more".into()),
        ],
        false,
    );
    let (_cancel_tx, cancel_rx) = crossbeam_channel::unbounded::<()>();

    let result = run_agent_loop(
        &provider,
        &mut h.session,
        "test-model",
        7,
        &cancel_rx,
        &h.ctx,
        Some("hi"),
    );
    assert!(
        !result.unwrap(),
        "a final-answer turn completes without a tool call"
    );

    assert_eq!(
        collect_status_broadcasts(&h.cmd_rx),
        vec![
            SessionStatus::Sending,
            SessionStatus::Waiting,
            SessionStatus::Reasoning,
            SessionStatus::Responding,
        ],
        "exactly the forward-only Sending→Waiting→Reasoning→Responding phases, no demotion",
    );
}

#[test]
fn agent_loop_retry_then_stream_advances_phase_out_of_retrying() {
    // The upload hook fires per attempt, so a retried turn reports the
    // `Waiting` boundary for the failed attempt too, then `Retrying`, then the
    // successful attempt's `Waiting` before its first delta — the phase must
    // advance out of `Retrying` rather than leaving the status stuck there.
    let mut h = agent_loop_harness(ToolRegistry::new());
    let provider = make_scripted_stream_provider(
        vec![
            StreamEvent::Reasoning("think".into()),
            StreamEvent::Answer("hi".into()),
        ],
        true,
    );
    let (_cancel_tx, cancel_rx) = crossbeam_channel::unbounded::<()>();

    let result = run_agent_loop(
        &provider,
        &mut h.session,
        "test-model",
        7,
        &cancel_rx,
        &h.ctx,
        Some("hi"),
    );
    assert!(
        !result.unwrap(),
        "a final-answer turn completes without a tool call"
    );

    assert_eq!(
        collect_status_broadcasts(&h.cmd_rx),
        vec![
            SessionStatus::Sending,
            SessionStatus::Waiting,
            SessionStatus::Retrying {
                attempt: 1,
                max_attempts: 3,
                delay_ms: 1,
            },
            SessionStatus::Waiting,
            SessionStatus::Reasoning,
            SessionStatus::Responding,
        ],
    );
}

#[test]
fn agent_loop_recovers_from_truncated_tool_call() {
    // A provider that truncates its first streaming turn (a tool call cut off at
    // the output-token limit) must NOT dead-end the request: the loop discards
    // the partial call, records an explanatory turn, seeds the recovery
    // instruction as the next user turn, and retries — reaching the provider's
    // normal answer without the user restarting the session.
    let mut h = agent_loop_harness(ToolRegistry::new());
    let provider = make_truncating_provider(1);
    let (_cancel_tx, cancel_rx) = crossbeam_channel::unbounded::<()>();

    let result = run_agent_loop(
        &provider,
        &mut h.session,
        "test-model",
        7,
        &cancel_rx,
        &h.ctx,
        Some("hi"),
    );
    assert!(!result.unwrap(), "a recovered request completes normally");

    // Turn 0 = user "hi" + the truncation note; turn 1 = recovery instruction +
    // the final answer.
    assert_eq!(
        h.session.turns.len(),
        2,
        "exactly one recovery turn was added"
    );
    let t0 = h.session.turns.get(&0).expect("turn 0 exists");
    assert_eq!(t0.user_text.as_deref(), Some("hi"));
    assert!(
        t0.assistant_text
            .as_deref()
            .unwrap_or_default()
            .contains("output-token"),
        "turn 0 must carry the truncation note, got {:?}",
        t0.assistant_text
    );
    let t1 = h.session.turns.get(&1).expect("turn 1 exists");
    assert_eq!(
        t1.user_text.as_deref(),
        Some(TRUNCATION_RECOVERY_INSTRUCTION),
        "the recovery instruction is seeded as the retry's user turn"
    );
    assert_eq!(t1.assistant_text.as_deref(), Some("done"));
}

#[test]
fn agent_loop_gives_up_after_truncation_recovery_budget() {
    // A provider that truncates every turn: the loop must retry only up to the
    // budget, then end the request cleanly (Ok(false)) instead of spinning
    // forever — critical because this session runs with `max_turns == 0`
    // (unlimited), which cannot bound the loop itself.
    let mut h = agent_loop_harness(ToolRegistry::new());
    let provider = make_truncating_provider(MAX_TRUNCATION_RECOVERIES as usize + 5);
    let (_cancel_tx, cancel_rx) = crossbeam_channel::unbounded::<()>();

    let result = run_agent_loop(
        &provider,
        &mut h.session,
        "test-model",
        7,
        &cancel_rx,
        &h.ctx,
        Some("hi"),
    );
    assert!(
        !result.unwrap(),
        "the request ends cleanly, not with an error"
    );
    // One turn per recovery attempt, capped at the budget (the first attempt
    // starts turn 0, so the count equals MAX_TRUNCATION_RECOVERIES).
    assert_eq!(
        h.session.turns.len(),
        MAX_TRUNCATION_RECOVERIES as usize,
        "the recovery loop stops at the budget"
    );
}

#[test]
fn agent_loop_drops_response_id_chain_on_truncation_recovery() {
    // A ResponseId-policy provider ("openai"/"gpt-5.4" in the bundled catalog)
    // chains turns via `previous_response_id`. When a turn is truncated
    // mid-tool-call, the recovery must NOT chain the retry onto the
    // pre-truncation id: that would replay a `function_call` whose matching
    // output was dropped (the function_call_output lives in `tool_results`,
    // which the truncation arm clears), an unpaired call on the Responses wire.
    // The retry must instead resend the full history (no chain), and the
    // persisted id must be cleared so a later request cannot resurrect the
    // broken chain.
    let mut h = agent_loop_harness(ToolRegistry::new());
    // Slug "openai" + model "gpt-5.4" is a ResponseId-policy pair, so the seeded
    // chain id below is restored as `prev_resp_id` on the first turn.
    let (provider, client) =
        make_truncating_provider_with_slug(MAX_TRUNCATION_RECOVERIES as usize + 5, "openai");
    let (_cancel_tx, cancel_rx) = crossbeam_channel::unbounded::<()>();
    h.session.config.last_response_id = Some("resp_seed".to_string());
    h.session.config.last_response_id_producer = Some(ReasoningProducer {
        provider_slug: "openai".to_string(),
        model: "gpt-5.4".to_string(),
    });

    let result = run_agent_loop(
        &provider,
        &mut h.session,
        "gpt-5.4",
        7,
        &cancel_rx,
        &h.ctx,
        Some("hi"),
    );
    assert!(!result.unwrap(), "the request ends cleanly at the budget");

    // The first (pre-truncation) call chains on the seeded id; every recovery
    // retry sends no chain id (full history).
    let seen = client.seen_previous_response_ids();
    assert_eq!(
        seen.first(),
        Some(&Some("resp_seed".to_string())),
        "the first call chains on the seeded response id, got {seen:?}"
    );
    assert_eq!(seen.len(), MAX_TRUNCATION_RECOVERIES as usize, "{seen:?}");
    assert!(
        seen.iter().skip(1).all(Option::is_none),
        "every recovery retry must drop the chain, got {seen:?}"
    );
    // The persisted chain is cleared so a later user request cannot chain onto
    // the pre-truncation response.
    assert_eq!(h.session.config.last_response_id, None);
    assert_eq!(h.session.config.last_response_id_producer, None);
}

// -- unnamed tool-call recovery tests ----------------------------------

#[test]
fn agent_loop_gives_up_after_unnamed_tool_call_recovery_budget() {
    // A provider that answers every turn with a tool call carrying no name: the
    // loop must never dispatch it (the registry would answer "unknown tool"
    // and the model would retry the same malformed call), must retry only up to
    // the budget, then end the request cleanly — critical because this session
    // runs with `max_turns == 0` (unlimited), which cannot bound the loop.
    let mut h = agent_loop_harness(ToolRegistry::new());
    // The last scripted reply repeats, so the provider never recovers on its
    // own and only the daemon's budget can stop it.
    let provider = make_scripted_turn_provider(vec![ScriptedTurn::ToolCalls(vec![""])]);
    let (_cancel_tx, cancel_rx) = crossbeam_channel::unbounded::<()>();

    let result = run_agent_loop(
        &provider,
        &mut h.session,
        "test-model",
        7,
        &cancel_rx,
        &h.ctx,
        Some("hi"),
    );
    assert!(
        !result.unwrap(),
        "the request ends cleanly, not with an error"
    );
    assert_eq!(
        h.session.turns.len(),
        MAX_UNNAMED_TOOL_CALL_RECOVERIES as usize,
        "one turn per recovery attempt, capped at the budget"
    );
    for (turn_id, turn) in &h.session.turns {
        assert!(
            turn.tool_calls.is_empty(),
            "an unnamed call is never recorded (turn {turn_id})"
        );
        assert!(
            turn.tool_results.is_empty(),
            "no tool ever ran (turn {turn_id})"
        );
    }
    let last = h
        .session
        .turns
        .get(&(MAX_UNNAMED_TOOL_CALL_RECOVERIES - 1))
        .expect("the final turn exists");
    assert!(
        last.assistant_text
            .as_deref()
            .unwrap_or_default()
            .contains("run stopped"),
        "the last turn says the run stopped, got {:?}",
        last.assistant_text
    );
}

#[test]
fn agent_loop_unnamed_tool_call_recovery_seeds_the_instruction() {
    // The recovered turn keeps the model's prose plus a note counting what was
    // dropped, and the next turn's user text carries the recovery instruction,
    // so the model is told what went wrong before it is asked for another reply.
    let mut h = agent_loop_harness(ToolRegistry::new());
    let provider = make_scripted_turn_provider(vec![
        ScriptedTurn::ToolCalls(vec![""]),
        ScriptedTurn::FinalAnswer,
    ]);
    let (_cancel_tx, cancel_rx) = crossbeam_channel::unbounded::<()>();

    let result = run_agent_loop(
        &provider,
        &mut h.session,
        "test-model",
        7,
        &cancel_rx,
        &h.ctx,
        Some("hi"),
    );
    assert!(!result.unwrap(), "a recovered request completes normally");
    assert_eq!(h.session.turns.len(), 2, "exactly one recovery turn");

    let t0 = h.session.turns.get(&0).expect("turn 0 exists");
    let text = t0.assistant_text.as_deref().unwrap_or_default();
    assert!(
        text.contains("working"),
        "the model's prose is kept, got {text:?}"
    );
    assert!(
        text.contains("Tool calls the provider will not accept: 1"),
        "the note counts the discarded call, got {text:?}"
    );

    let t1 = h.session.turns.get(&1).expect("turn 1 exists");
    assert_eq!(
        t1.user_text.as_deref(),
        Some(UNNAMED_TOOL_CALL_RECOVERY_INSTRUCTION),
        "the recovery instruction is seeded as the retry's user turn"
    );
    assert_eq!(t1.assistant_text.as_deref(), Some("done"));
}

#[test]
fn agent_loop_runs_the_named_calls_of_a_mixed_reply() {
    // A reply mixing a valid call with an unnamed one still does its real work:
    // the named call runs, only it is recorded, and the next turn tells the
    // model one of its calls was dropped. The mixed turn takes no strike, so the
    // request reaches the provider's final answer instead of being cut short.
    let mut registry = ToolRegistry::new();
    registry.register(crate::tools::random::Random);
    let mut h = agent_loop_harness(registry);
    let provider = make_scripted_turn_provider(vec![
        ScriptedTurn::ToolCalls(vec!["random", ""]),
        ScriptedTurn::FinalAnswer,
    ]);
    let (_cancel_tx, cancel_rx) = crossbeam_channel::unbounded::<()>();

    let result = run_agent_loop(
        &provider,
        &mut h.session,
        "test-model",
        7,
        &cancel_rx,
        &h.ctx,
        Some("hi"),
    );
    assert!(!result.unwrap(), "the run is not cut short");
    assert_eq!(
        h.session.turns.len(),
        2,
        "a mixed reply takes no strike, so the run continues to the final answer"
    );

    let t0 = h.session.turns.get(&0).expect("turn 0 exists");
    assert_eq!(t0.tool_calls.len(), 1, "only the named call is recorded");
    assert_eq!(t0.tool_calls[0].name, "random");
    assert_eq!(t0.tool_results.len(), 1);
    assert!(
        !t0.tool_results[0].is_error,
        "the valid call of a mixed reply still runs"
    );

    let t1 = h.session.turns.get(&1).expect("turn 1 exists");
    assert_eq!(
        t1.user_text.as_deref(),
        Some(UNNAMED_TOOL_CALL_RECOVERY_INSTRUCTION),
        "the dropped call is reported to the model on the next turn"
    );
}

#[test]
fn agent_loop_resets_the_recovery_budget_when_a_turn_produces_output() {
    // Two recoveries, then a turn that runs a real tool, then two more: the
    // request only completes if the productive turn reset the counter. Without
    // the reset the third recovery would exhaust the budget mid-request and end
    // it with the "run stopped" note instead of the model's answer.
    let mut registry = ToolRegistry::new();
    registry.register(crate::tools::random::Random);
    let mut h = agent_loop_harness(registry);
    let provider = make_scripted_turn_provider(vec![
        ScriptedTurn::ToolCalls(vec![""]),
        ScriptedTurn::ToolCalls(vec![""]),
        ScriptedTurn::ToolCalls(vec!["random"]),
        ScriptedTurn::ToolCalls(vec![""]),
        ScriptedTurn::ToolCalls(vec![""]),
        ScriptedTurn::FinalAnswer,
    ]);
    let (_cancel_tx, cancel_rx) = crossbeam_channel::unbounded::<()>();

    let result = run_agent_loop(
        &provider,
        &mut h.session,
        "test-model",
        7,
        &cancel_rx,
        &h.ctx,
        Some("hi"),
    );
    assert!(
        !result.unwrap(),
        "the request reaches the final answer, not the budget"
    );
    assert_eq!(
        h.session.turns.len(),
        6,
        "four recoveries fit because the productive turn reset the budget"
    );
    assert_eq!(
        h.session
            .turns
            .get(&5)
            .expect("turn 5 exists")
            .assistant_text
            .as_deref(),
        Some("done"),
        "the run ends on the model's answer"
    );
}

#[test]
fn agent_loop_keeps_reasoning_across_an_unnamed_tool_call_recovery() {
    // The active route declares reasoning it re-emits on every assistant
    // message, so a recovered turn recorded with text but without its artifact
    // would violate that policy on the next request. `deepseek-v4-pro` is the
    // route that requires the `reasoning_content` echo field.
    let mut h = agent_loop_harness(ToolRegistry::new());
    let provider = make_scripted_turn_provider_with_slug(
        vec![
            ScriptedTurn::ToolCallsWithReasoning {
                names: vec![""],
                reasoning: "thinking it over",
                artifact_bytes: b"captured",
            },
            ScriptedTurn::FinalAnswer,
        ],
        "deepseek",
    );
    let (_cancel_tx, cancel_rx) = crossbeam_channel::unbounded::<()>();

    let result = run_agent_loop(
        &provider,
        &mut h.session,
        "deepseek-v4-pro",
        7,
        &cancel_rx,
        &h.ctx,
        Some("hi"),
    );
    assert!(!result.unwrap(), "a recovered request completes normally");

    let t0 = h.session.turns.get(&0).expect("turn 0 exists");
    assert_eq!(t0.assistant_reasoning.as_deref(), Some("thinking it over"));
    assert_eq!(
        t0.reasoning_artifact,
        Some(artifact(b"captured")),
        "the recovered turn keeps its reasoning artifact"
    );
    assert!(
        t0.reasoning_producer.is_some(),
        "the recovered turn keeps its reasoning producer"
    );

    // And the next request's built messages still carry the payload.
    let messages =
        build_chat_request_messages(&h.session, None, "deepseek", "deepseek-v4-pro", None);
    let assistants = assistant_messages(&messages);
    assert_eq!(
        assistants[0].reasoning_artifact,
        Some(artifact(b"captured")),
        "the replay policy still sees the artifact on the next request"
    );
    assert!(
        assistants[0]
            .content
            .as_deref()
            .unwrap_or_default()
            .contains("Tool calls the provider will not accept"),
        "the recovered turn's note rides the next request too"
    );
}

// -- resolve_reasoning_effort tests ------------------------------------
