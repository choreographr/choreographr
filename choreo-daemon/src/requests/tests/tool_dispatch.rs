//! tool dispatch tests (see the parent `tests` module).

use super::*;

#[test]
fn execute_tool_normal_completion() {
    let (_cancel_tx, cancel_rx) = crossbeam_channel::unbounded::<()>();
    let (result, cancelled, _cmd_rx) = run_exec_tool(
        FastTestTool,
        "_test_fast",
        "{}",
        Duration::from_secs(60),
        &cancel_rx,
    );
    assert!(!result.is_error, "expected success: {}", result.content);
    assert!(result.content.contains("fast result"), "{}", result.content);
    assert!(!cancelled, "completion must not report a cancel");
}

#[test]
fn execute_tool_cancelled_before_execution() {
    let (cancel_tx, cancel_rx) = crossbeam_channel::unbounded::<()>();
    cancel_tx.send(()).expect("send cancel");
    drop(cancel_tx);

    // A blocking tool makes the outcome deterministic: the cancel is
    // pre-sent, so the wait-loop's biased cancel arm fires first and,
    // with the tool still running, the "cancelled" output is produced
    // (a just-completed fast tool could otherwise race the cancel drain
    // and return its real result alongside the sticky cancel flag).
    let (proceed_tx, proceed_rx) = mpsc::channel::<()>();
    let (result, cancelled, _cmd_rx) = run_exec_tool(
        BlockingTestTool {
            proceed: std::sync::Mutex::new(Some(proceed_rx)),
        },
        "_test_blocking",
        "{}",
        Duration::from_secs(60),
        &cancel_rx,
    );
    assert!(result.is_error, "expected error: {}", result.content);
    assert!(result.content.contains("cancelled"), "{}", result.content);
    // The wait observed the cancellation signal; the caller must stop the
    // request (the sticky-cancel contract).
    assert!(cancelled, "cancel must be reported to the caller");
    // The serial path now carries the invocation description onto the
    // synthesized cancel output (the concurrent path always did), so the
    // transcript renders the same invocation context for both phases.
    assert_eq!(result.invocation_description, "test invocation");

    // Release the still-blocked tool so its execution thread exits.
    drop(proceed_tx);
}

#[test]
fn execute_tool_timeout() {
    let (_cancel_tx, cancel_rx) = crossbeam_channel::unbounded::<()>();
    let (proceed_tx, proceed_rx) = mpsc::channel::<()>();

    // A zero-duration timeout is deterministic: `crossbeam_channel::after`
    // with a deadline that is already in the past is immediately ready in
    // the biased select's fast path (the `at` flavor's `try_recv` checks
    // `Instant::now() >= delivery_time`), so the deadline arm fires
    // without any time-based wait — no sleeps in unit tests (AGENTS.md).
    // The blocking tool can never win the race (it has not finished), and
    // no cancel is pending, so the deadline arm is the only ready one.
    let (result, cancelled, _cmd_rx) = run_exec_tool(
        BlockingTestTool {
            proceed: std::sync::Mutex::new(Some(proceed_rx)),
        },
        "_test_blocking",
        "{}",
        Duration::ZERO,
        &cancel_rx,
    );

    assert!(result.is_error, "expected error: {}", result.content);
    assert!(result.content.contains("timed out"), "{}", result.content);
    assert!(!cancelled, "a timeout is not a cancellation");

    drop(proceed_tx);
}

// -- drain_queued_or_synthesize tests ---------------------------------
//
// Deterministic: each test fully populates (or deliberately leaves
// empty / disconnects) the exec channel before calling the function, so
// the outcome is fully ordered — no time-based waits (AGENTS.md).

#[test]
fn drain_queued_result_beats_stop_message() {
    // A result that queued in the same instant the stop fired must win
    // over the synthesized stop message — the finish-vs-stop race is
    // resolved in favor of the real outcome.
    let (tx, rx) = crossbeam_channel::unbounded::<Result<ToolOutput, ToolError>>();
    tx.send(Ok(ToolOutput {
        content: "real result".into(),
        invocation_description: "real desc".into(),
        ..Default::default()
    }))
    .expect("send result");

    let (output, cancelled) = drain_queued_or_synthesize(
        "_test",
        std::time::Instant::now(),
        "test invocation",
        &rx,
        "tool '_test' cancelled".to_string(),
        true,
    );
    assert_eq!(output.content, "real result");
    assert!(!output.is_error);
    assert_eq!(output.invocation_description, "real desc");
    // The sticky-cancel flag still travels with a drained result: the
    // cancel signal was consumed, so the caller must stop the request.
    assert!(cancelled, "sticky cancel must survive a drained result");
}

#[test]
fn drain_queued_empty_synthesizes_stop_message() {
    // No result queued (sender alive, tool still running): the stop
    // message is synthesized with the caller's invocation description.
    let (_tx, rx) = crossbeam_channel::unbounded::<Result<ToolOutput, ToolError>>();
    let (output, cancelled) = drain_queued_or_synthesize(
        "_test",
        std::time::Instant::now(),
        "test invocation",
        &rx,
        "tool '_test' timed out after 60s".to_string(),
        false,
    );
    assert_eq!(output.content, "tool '_test' timed out after 60s");
    assert!(output.is_error);
    assert_eq!(output.invocation_description, "test invocation");
    assert!(!cancelled, "a timeout is not a request cancel");
}

#[test]
fn drain_queued_disconnected_reports_panic_not_stop() {
    // The execution thread died (sender dropped) at the stop instant: the
    // real cause is a panic, not the stop message — a deadline/cancel arm
    // must not mislabel a dead execution thread as "timed out" or
    // "cancelled".
    let (tx, rx) = crossbeam_channel::unbounded::<Result<ToolOutput, ToolError>>();
    drop(tx);
    let (output, cancelled) = drain_queued_or_synthesize(
        "_test",
        std::time::Instant::now(),
        "test invocation",
        &rx,
        "tool '_test' cancelled".to_string(),
        true,
    );
    assert_eq!(output.content, "tool execution thread panicked");
    assert!(output.is_error);
    assert_eq!(output.invocation_description, "test invocation");
    assert!(cancelled, "sticky flag still applies on the panic path");
}

#[test]
fn execute_tool_forwards_streaming_output() {
    let (_cancel_tx, cancel_rx) = crossbeam_channel::unbounded::<()>();
    let (result, cancelled, cmd_rx) = run_exec_tool(
        StreamingTestTool,
        "_test_streaming",
        "{}",
        Duration::from_secs(60),
        &cancel_rx,
    );

    assert!(!result.is_error, "expected success: {}", result.content);
    assert!(
        result.content.contains("streaming done"),
        "{}",
        result.content
    );
    assert!(!cancelled, "completion must not report a cancel");

    // The invocation description is no longer streamed as a chunk (it
    // rides on ToolCallStarted + the seeded placeholder); the only chunk
    // is the tool's own payload from execute_streaming.
    match cmd_rx.recv() {
        Ok(SessionCommand::Broadcast(DaemonMessageType::Session {
            event: SessionEvent::ToolResultChunk { data, .. },
            ..
        })) => {
            assert_eq!(data, b"streamed payload");
        }
        Ok(_other) => panic!("expected ToolResultChunk, got unexpected SessionCommand"),
        Err(e) => panic!("channel disconnected while waiting for streaming output: {e}"),
    }
}

// -- forwarding-thread tests -----------------------------------------
//
// These exercise `spawn_forwarding_thread` directly and deterministically:
// the returned `JoinHandle` lets the test observe thread exit without any
// time-based waits (AGENTS.md forbids sleeps in unit tests).

#[test]
fn forwarding_thread_drains_queued_output_before_kill() {
    let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded::<SessionCommand>();
    let (output_tx, output_rx) = crossbeam_channel::unbounded::<Vec<u8>>();
    let (kill_tx, kill_rx) = crossbeam_channel::unbounded::<()>();

    let handle = spawn_forwarding_thread(cmd_tx, 1, 1, "call_1".into(), output_rx, kill_rx);

    // Queue a chunk and then a kill back-to-back. The test sends the chunk
    // BEFORE the kill, so the forwarder can never observe the kill arm as
    // ready while the output arm is still pending — the biased select
    // (output first) must therefore forward the chunk before honoring the
    // kill, in any interleaving.
    output_tx
        .send(b"queued chunk".to_vec())
        .expect("send chunk");
    kill_tx.send(()).expect("send kill");

    match cmd_rx.recv() {
        Ok(SessionCommand::Broadcast(DaemonMessageType::Session {
            event: SessionEvent::ToolResultChunk { data, .. },
            ..
        })) => {
            assert_eq!(data, b"queued chunk");
        }
        Ok(_other) => panic!("expected ToolResultChunk, got unexpected SessionCommand"),
        Err(e) => panic!("channel disconnected while waiting for chunk: {e}"),
    }
    // Only the kill can terminate the forwarder now (output_tx still alive),
    // so a successful join proves the kill was honored after the drain.
    handle.join().expect("forwarder should exit after kill");
}

#[test]
fn forwarding_thread_exits_when_output_disconnects() {
    let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded::<SessionCommand>();
    let (output_tx, output_rx) = crossbeam_channel::unbounded::<Vec<u8>>();
    let (_kill_tx, kill_rx) = crossbeam_channel::unbounded::<()>();

    let handle = spawn_forwarding_thread(cmd_tx, 1, 1, "call_2".into(), output_rx, kill_rx);

    output_tx.send(b"last chunk".to_vec()).expect("send chunk");
    // Tool finished: dropping the output sender makes the forwarder's next
    // `recv(output_rx)` return Err (disconnect) → drain → exit.
    drop(output_tx);

    match cmd_rx.recv() {
        Ok(SessionCommand::Broadcast(DaemonMessageType::Session {
            event: SessionEvent::ToolResultChunk { data, .. },
            ..
        })) => {
            assert_eq!(data, b"last chunk");
        }
        Ok(_other) => panic!("expected ToolResultChunk, got unexpected SessionCommand"),
        Err(e) => panic!("channel disconnected while waiting for chunk: {e}"),
    }
    handle
        .join()
        .expect("forwarder should exit on output disconnect");
}

#[test]
fn forwarding_thread_exits_when_kill_sender_dropped() {
    let (cmd_tx, _cmd_rx) = crossbeam_channel::unbounded::<SessionCommand>();
    let (output_tx, output_rx) = crossbeam_channel::unbounded::<Vec<u8>>();
    let (kill_tx, kill_rx) = crossbeam_channel::unbounded::<()>();

    let handle = spawn_forwarding_thread(cmd_tx, 1, 1, "call_3".into(), output_rx, kill_rx);

    // Dropping the kill sender disconnects kill_rx; with no output traffic
    // the select returns on the kill arm immediately and the thread exits.
    drop(kill_tx);
    handle
        .join()
        .expect("forwarder should exit when kill sender dropped");
    drop(output_tx);
}

#[test]
fn forwarding_thread_honors_kill_while_output_is_still_alive() {
    // A tool that keeps streaming keeps the output arm always-ready, which
    // would starve the biased-last kill arm if the forwarder only checked
    // the kill channel via the select.  The between-chunk kill re-check
    // must stop the thread even though the output sender is still alive
    // and has chunks queued — otherwise a busy stream would forward
    // forever after the caller stopped waiting.  Deterministic: the test
    // sends chunks and then a kill; the forwarder forwards the queued
    // burst (bounded by the queue length at kill time) and then exits,
    // never waiting on the output channel to disconnect.
    let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded::<SessionCommand>();
    let (output_tx, output_rx) = crossbeam_channel::unbounded::<Vec<u8>>();
    let (kill_tx, kill_rx) = crossbeam_channel::unbounded::<()>();

    let handle = spawn_forwarding_thread(cmd_tx, 1, 1, "call_4".into(), output_rx, kill_rx);

    for i in 0..5 {
        output_tx
            .send(format!("chunk {i}").into_bytes())
            .expect("send chunk");
    }
    kill_tx.send(()).expect("send kill");

    // The first queued chunk is forwarded (FIFO) before the kill is
    // honored; the rest of the kill-time burst may be drained too.
    match cmd_rx.recv() {
        Ok(SessionCommand::Broadcast(DaemonMessageType::Session {
            event: SessionEvent::ToolResultChunk { data, .. },
            ..
        })) => {
            assert_eq!(data, b"chunk 0", "first queued chunk should be forwarded");
        }
        Ok(_other) => panic!("expected ToolResultChunk, got unexpected SessionCommand"),
        Err(e) => panic!("channel disconnected while waiting for chunk: {e}"),
    }
    // output_tx is still alive, so the thread can only terminate by
    // honoring the kill between chunks — a successful join proves the
    // busy-stream kill starvation is closed.
    handle
        .join()
        .expect("forwarder should exit on kill while output is still live");
    drop(output_tx);
    drop(kill_tx);
}

// -- determine_tool_timeout tests ----------------------------------

#[test]
fn determine_tool_timeout_subsession_none() {
    assert!(determine_tool_timeout("spawn_subsession", "{}").is_none());
}

#[test]
fn determine_tool_timeout_shell_300() {
    for name in &["sh", "nushell", "fish", "exec"] {
        assert_eq!(
            determine_tool_timeout(name, "{}"),
            Some(Duration::from_secs(300)),
            "tool {name} should have 300s timeout",
        );
    }
}

#[test]
fn determine_tool_timeout_default_60() {
    for name in &[
        "read_file",
        "write_file",
        "run_riscv",
        "grep",
        "http_request",
    ] {
        assert_eq!(
            determine_tool_timeout(name, "{}"),
            Some(Duration::from_secs(60)),
            "tool {name} should have 60s timeout",
        );
    }
}

#[test]
fn determine_tool_timeout_requested_below_base_keeps_base() {
    // The name-based default is a FLOOR: a shell tool requesting its default
    // (or anything below it) still gets the full 300s outer deadline.
    assert_eq!(
        determine_tool_timeout("sh", r#"{"timeout": 30000}"#),
        Some(Duration::from_secs(300)),
    );
}

#[test]
fn determine_tool_timeout_requested_above_base_raises_deadline() {
    // A long legitimate command (a full workspace build) must be allowed to
    // run to its requested timeout: the outer deadline is raised to the
    // request plus the teardown grace (5s).
    assert_eq!(
        determine_tool_timeout("sh", r#"{"timeout": 1800000}"#),
        Some(Duration::from_mins(30) + TOOL_TIMEOUT_GRACE),
    );
}

#[test]
fn determine_tool_timeout_missing_or_malformed_falls_back_to_base() {
    // Missing field, non-numeric, zero, and non-JSON args all fall back to
    // the name-based default — the guard always exists.
    for args in ["{}", r#"{"timeout": 0}"#, r#"{"timeout": "big"}"#, "oops"] {
        assert_eq!(
            determine_tool_timeout("sh", args),
            Some(Duration::from_secs(300)),
            "args {args} should fall back to the 300s default",
        );
    }
}

#[test]
fn determine_tool_timeout_ignored_for_non_shell_tools() {
    // Non-shell tools have no `timeout` field in their schema; a stray field
    // (if a model ever sends one) must not raise their deadline.
    assert_eq!(
        determine_tool_timeout("grep", r#"{"timeout": 999999999}"#),
        Some(Duration::from_secs(60)),
    );
}

#[test]
fn determine_tool_timeout_generate_image_covers_adapter_worst_case() {
    // glm-image `hd` renders ~20 s per the z.ai docs, but the adapters own
    // bounded worst cases (2 POST attempts × the 180 s agent deadline, plus
    // the z.ai URL download's 3-fetch budget) that the generic 60 s default
    // discards as a timeout AFTER a paid generation completes. The floor is
    // DERIVED from the shared adapter constants plus a headroom margin —
    // asserted here against the same computation rather than a pinned
    // literal, so the floor always covers the adapters' configured worst
    // case by construction — and it must be name-gated ONLY on
    // generate_image, not reach other tools.
    let adapter_worst_case = u64::from(
        choreo_ai_protocols::images::IMAGE_MAX_ATTEMPTS
            + choreo_ai_protocols::images::IMAGE_DOWNLOAD_ATTEMPTS,
    ) * choreo_ai_protocols::images::IMAGE_TOTAL_TIMEOUT_SECS;
    let derived = Duration::from_secs(adapter_worst_case + 60);
    assert_eq!(
        determine_tool_timeout("generate_image", "{}"),
        Some(derived),
    );
    // Non-shell tools still cannot raise their deadline via a stray
    // `timeout` argument, generate_image included (no raise path). And
    // other image-adjacent tool names do NOT inherit the derived floor —
    // the branches are name-exact, not prefix-matched.
    assert_eq!(
        determine_tool_timeout("generate_image", r#"{"timeout": 999999999}"#),
        Some(derived),
    );
    assert_eq!(
        determine_tool_timeout("display_image", "{}"),
        Some(Duration::from_secs(60)),
    );
}

// -- spawn_single_tool tests ---------------------------------------

#[test]
fn spawn_single_tool_fast_returns_result() {
    let handle = run_spawn_single_tool(
        FastTestTool,
        "_test_fast",
        "{}",
        Some(Duration::from_secs(60)),
    );
    assert!(
        !handle.output.is_error,
        "expected success: {}",
        handle.output.content
    );
    assert!(
        handle.output.content.contains("fast result"),
        "{}",
        handle.output.content
    );
    assert!(handle.image.is_none(), "expected no image from fast tool");
}

#[test]
fn spawn_single_tool_no_timeout_still_completes() {
    let handle = run_spawn_single_tool(FastTestTool, "_test_fast", "{}", None);
    assert!(
        !handle.output.is_error,
        "expected success: {}",
        handle.output.content
    );
    assert!(
        handle.output.content.contains("fast result"),
        "{}",
        handle.output.content
    );
}

#[test]
fn concurrent_tools_deliver_in_completion_order() {
    // Tool A (dispatched first) blocks until released; tool B (dispatched
    // second) completes immediately. B must arrive through the shared
    // batch channel before A — a fast tool is no longer gated by the
    // slowest tool the model listed before it.
    let (proceed_tx, proceed_rx) = mpsc::channel::<()>();
    let (ctx, cmd_tx) = spawn_test_ctx();

    let mut registry = ToolRegistry::new();
    registry.register(BlockingTestTool {
        proceed: std::sync::Mutex::new(Some(proceed_rx)),
    });
    registry.register(FastTestTool);
    let registry = registry.build();

    let slow_call = ChatToolCall {
        id: "call_slow".into(),
        name: "_test_blocking".into(),
        arguments_json: "{}".into(),
        caller: None,
    };
    let fast_call = ChatToolCall {
        id: "call_fast".into(),
        name: "_test_fast".into(),
        arguments_json: "{}".into(),
        caller: None,
    };

    let (batch_tx, batch_rx) = crossbeam_channel::unbounded::<ToolHandle>();

    // Dispatch the slow tool first, then the fast one.  The slow tool's
    // 5s timeout is a deadlock guard only: in the correct implementation
    // the fast result arrives immediately and the slow tool is released
    // well before its deadline, so the blocking `recv`s below never wait
    // on a timer — they are deterministic (AGENTS.md forbids time-based
    // waits in unit tests).  If a regression made the collector wait in
    // dispatch order, the slow tool would hit its timeout instead and
    // the name assertion below would fail the test rather than hang it.
    // The kill senders are held for the drain's lifetime — dropping them
    // would disconnect the kill channels and stop the wait-loops early.
    let _slow_kill = spawn_single_tool(SpawnToolArgs {
        tool_call: slow_call,
        timeout: Some(Duration::from_secs(5)),
        stream_id: 1,
        session_id: 1,
        registry: Arc::clone(&registry),
        session_tools: Arc::new(crate::mcp::ProjectToolSet::empty()),
        cmd_tx: cmd_tx.clone(),
        x_credentials: None,
        working_dir: None,
        ctx: ctx.clone(),
        invocation_description: String::new(),
        started_at: Instant::now(),
        result_tx: batch_tx.clone(),
    });
    let _fast_kill = spawn_single_tool(SpawnToolArgs {
        tool_call: fast_call,
        timeout: Some(Duration::from_secs(60)),
        stream_id: 1,
        session_id: 1,
        registry,
        session_tools: Arc::new(crate::mcp::ProjectToolSet::empty()),
        cmd_tx,
        x_credentials: None,
        working_dir: None,
        ctx,
        invocation_description: String::new(),
        started_at: Instant::now(),
        result_tx: batch_tx,
    });

    // The fast tool must arrive first despite being dispatched second.
    let first = batch_rx.recv().expect("expected a first tool result");
    assert_eq!(first.tool_call.name, "_test_fast");
    assert!(
        first.output.content.contains("fast result"),
        "{}",
        first.output.content
    );

    // Release the slow tool; its result arrives second.
    drop(proceed_tx);
    let second = batch_rx.recv().expect("expected the slow tool result");
    assert_eq!(second.tool_call.name, "_test_blocking");
    assert!(
        second.output.content.contains("blocked tool done"),
        "{}",
        second.output.content
    );
}

#[test]
fn wait_loop_honors_kill_while_tool_is_still_running() {
    // A tool that blocks until released; a collector kill must stop the
    // wait-loop (forwarder + cooperative flag) and deliver a "cancelled"
    // result immediately, without waiting for the tool to finish — even
    // with NO timeout, where the wait-loop would otherwise block on the
    // tool's result channel forever.
    let (proceed_tx, proceed_rx) = mpsc::channel::<()>();
    let (ctx, cmd_tx) = spawn_test_ctx();
    let cancel_flag = Arc::clone(&ctx.cancelled);

    let mut registry = ToolRegistry::new();
    registry.register(BlockingTestTool {
        proceed: std::sync::Mutex::new(Some(proceed_rx)),
    });
    let registry = registry.build();

    let tool_call = ChatToolCall {
        id: "call_kill".into(),
        name: "_test_blocking".into(),
        arguments_json: "{}".into(),
        caller: None,
    };

    let (result_tx, result_rx) = crossbeam_channel::unbounded::<ToolHandle>();
    let kill_tx = spawn_single_tool(SpawnToolArgs {
        tool_call,
        timeout: None, // unbounded — the kill is the only wakeup
        stream_id: 1,
        session_id: 1,
        registry,
        session_tools: Arc::new(crate::mcp::ProjectToolSet::empty()),
        cmd_tx,
        x_credentials: None,
        working_dir: None,
        ctx,
        invocation_description: String::new(),
        started_at: Instant::now(),
        result_tx,
    });

    // The tool is blocked inside execute_streaming_json; the kill must
    // reach the wait-loop and produce a cancelled result without the tool
    // finishing.  Deterministic: `recv` blocks until the cancelled handle
    // arrives, and the wait-loop sends it only after setting the flag.
    kill_tx.send(()).expect("send kill");

    let handle = result_rx
        .recv()
        .expect("cancelled result must be delivered");
    assert!(handle.output.is_error, "{}", handle.output.content);
    assert!(
        handle.output.content.contains("cancelled"),
        "{}",
        handle.output.content
    );
    // The cooperative flag must be set so the tool itself can stop early.
    assert!(
        cancel_flag.load(Ordering::Relaxed),
        "cooperative cancel flag must be set"
    );

    // Release the still-blocked tool so its execution thread exits.
    drop(proceed_tx);
}

// -- missing_calls tests ------------------------------------------------

#[test]
fn missing_calls_skips_delivered_by_id_not_index() {
    // A (slow, dies before delivering), B (fast, delivered), C (slow,
    // dies): handles arrive in completion order, so B is delivered first
    // and received == 1.  Skipping the first 1 entry by *index* would
    // mark A delivered and synthesize C in its place — misattributing the
    // panic to the wrong tool.  Filtering by call_id must synthesize A
    // and C (in dispatch order), never B.
    let call_infos = vec![
        CallInfo {
            call_id: "a".into(),
            tool_name: "slow_1".into(),
            arguments_json: "{}".into(),
            invocation_description: "a".into(),
            started_at: Instant::now(),
            // Never sent to — the kill channel is irrelevant to the
            // missing-call filter under test.
            kill_tx: crossbeam_channel::unbounded().0,
        },
        CallInfo {
            call_id: "b".into(),
            tool_name: "fast".into(),
            arguments_json: "{}".into(),
            invocation_description: "b".into(),
            started_at: Instant::now(),
            kill_tx: crossbeam_channel::unbounded().0,
        },
        CallInfo {
            call_id: "c".into(),
            tool_name: "slow_2".into(),
            arguments_json: "{}".into(),
            invocation_description: "c".into(),
            started_at: Instant::now(),
            kill_tx: crossbeam_channel::unbounded().0,
        },
    ];
    let delivered = HashSet::from(["b".to_string()]);
    let missing: Vec<&str> = missing_calls(&call_infos, &delivered)
        .map(|info| info.call_id.as_str())
        .collect();
    assert_eq!(missing, vec!["a", "c"]);
}

#[test]
fn missing_calls_empty_when_all_delivered() {
    let call_infos = vec![CallInfo {
        call_id: "a".into(),
        tool_name: "read_file".into(),
        arguments_json: "{}".into(),
        invocation_description: "a".into(),
        started_at: Instant::now(),
        // Never sent to — the kill channel is irrelevant to the
        // missing-call filter under test.
        kill_tx: crossbeam_channel::unbounded().0,
    }];
    let delivered = HashSet::from(["a".to_string()]);
    assert_eq!(missing_calls(&call_infos, &delivered).count(), 0);
}

// -- extract_json_string tests ------------------------------------------
