use crate::requests::system_content::{CollectToolResultParams, collect_tool_result};
use crate::sessions::{RequestContext, SessionCommand, SessionState, turn_for_client};
use crate::tools::context::ToolContext;
use crate::tools::{
    PreparedImage, STREAMING_CHANNEL_CAPACITY, ToolError, ToolOutput, ToolOutputFormat,
    ToolRegistry, sanitize_transcript, truncate_tool_output,
};
use choreo_ai_protocols::{ChatToolCall, ToolResultItem};
use choreo_keystore::ServiceCredential;
use choreo_proto::{DaemonMessage, DisplayedImageRecord, ImageMetadata, SessionEvent, TokenUsage};

/// Extra time added on top of a tool's requested `timeout` when raising the
/// outer deadline: the inner watchdog kills the child at exactly the
/// requested instant, and the grace covers killing the (possibly deep)
/// process tree, draining buffered output, and serializing the result.
pub(crate) const TOOL_TIMEOUT_GRACE: Duration = Duration::from_secs(5);

use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use std::time::Instant;
use tracing::{debug, warn};
/// Broadcast a TurnAppended message to all session subscribers, if the
/// given turn_id exists in the session's turn map.
pub(crate) fn broadcast_turn_appended(
    cmd_tx: &mpsc::Sender<SessionCommand>,
    session: &SessionState,
    session_id: u64,
    turn_id: u32,
) {
    if let Some(turn) = session.turns.get(&turn_id)
        && let Err(e) = cmd_tx.send(SessionCommand::Broadcast(DaemonMessage::Session {
            session_id: Some(session_id),
            event: SessionEvent::TurnAppended {
                turn_id,
                turn: turn_for_client(turn),
            },
        }))
    {
        warn!(%turn_id, error = %e, "failed to broadcast TurnAppended");
    }
}

/// Persist a `PreparedImage` to the session's current active turn and
/// broadcast it to live subscribers immediately (mid-turn) so the image
/// appears as soon as the tool finishes rather than waiting for request
/// completion.  Used by both the serial and concurrent tool paths.
pub(crate) fn emit_image(
    cmd_tx: &mpsc::Sender<SessionCommand>,
    image: PreparedImage,
    tool_call_id: Option<String>,
    session: &mut SessionState,
    session_id: u64,
    turn_id: u32,
) {
    let record = DisplayedImageRecord {
        metadata: ImageMetadata {
            mime_type: image.mime_type,
            width: image.width,
            height: image.height,
            byte_len: image.data.len() as u64,
            alt: image.alt,
        },
        data: image.data,
        tool_call_id,
    };
    session.add_displayed_image(turn_id, record.clone());
    broadcast_turn_appended(cmd_tx, session, session_id, turn_id);
}

/// Spawn a forwarding thread that relays streaming output chunks to session
/// subscribers in real time.  Exits when the output channel is disconnected
/// (tool finished) or a kill signal is received (caller stopped waiting).
///
/// Fully event-driven: `select_biased!` blocks until either an output chunk
/// or a kill signal arrives, so a kill is observed the instant it is sent
/// instead of at the next poll boundary.  The output arm is listed first and,
/// when a kill is observed between chunks, the output already queued at that
/// instant is drained — bounded by the queue length sampled at kill time —
/// before the thread stops (output priority, matching the old recv_timeout
/// semantics; the biased select only makes the output arm *more likely* to
/// win a simultaneous race, and a chunk is either forwarded or dropped
/// whole, never partially recorded).  A continuously-streaming tool keeps the
/// output arm always-ready, which would starve the kill arm's preference, so
/// after every forwarded chunk the thread also re-checks the kill channel
/// non-blocking: on a busy stream the kill is honored after one bounded final
/// drain instead of streaming on forever.
///
/// Returns the spawned `JoinHandle` so tests can deterministically observe
/// thread exit (no polling); production callers discard it.
pub(crate) fn spawn_forwarding_thread(
    cmd_tx: mpsc::Sender<SessionCommand>,
    session_id: u64,
    request_id: u32,
    call_id: String,
    output_rx: crossbeam_channel::Receiver<Vec<u8>>,
    kill_rx: crossbeam_channel::Receiver<()>,
) -> std::thread::JoinHandle<()> {
    thread::spawn(move || {
        loop {
            crossbeam_channel::select_biased! {
                recv(output_rx) -> msg => match msg {
                    Ok(data) => {
                        if cmd_tx
                            .send(SessionCommand::Broadcast(DaemonMessage::Session {
                                session_id: Some(session_id),
                                event: SessionEvent::ToolResultChunk {
                                    request_id,
                                    call_id: call_id.clone(),
                                    data,
                                },
                            }))
                            .is_err()
                        {
                            // Subscribers are gone — nothing left to stream to.
                            break;
                        }
                        // A continuously-streaming tool keeps the output arm
                        // always ready, so the biased kill arm below would
                        // never fire until the stream pauses.  Re-check the
                        // kill channel between chunks (non-blocking): a kill
                        // message OR a dropped kill sender means "stop".
                        // Before stopping, drain the output already queued at
                        // this instant — the budget is the queue length
                        // sampled here, so even a tool that keeps producing
                        // cannot extend the drain forever: at most
                        // `drain_budget` chunks are forwarded (the transcript
                        // keeps that produced output) and then the thread
                        // exits.  A chunk the tool produces *during* the
                        // drain may slip in and be forwarded within the
                        // budget — acceptable, since the total is still
                        // capped — and everything produced after the drain is
                        // dropped.
                        if matches!(
                            kill_rx.try_recv(),
                            Ok(()) | Err(crossbeam_channel::TryRecvError::Disconnected)
                        ) {
                            let drain_budget = output_rx.len();
                            for _ in 0..drain_budget {
                                let Ok(data) = output_rx.try_recv() else {
                                    break;
                                };
                                if cmd_tx
                                    .send(SessionCommand::Broadcast(
                                        DaemonMessage::Session {
                                            session_id: Some(session_id),
                                            event: SessionEvent::ToolResultChunk {
                                                request_id,
                                                call_id: call_id.clone(),
                                                data,
                                            },
                                        },
                                    ))
                                    .is_err()
                                {
                                    break;
                                }
                            }
                            break;
                        }
                    }
                    // Output sender dropped → the tool finished; stop forwarding.
                    Err(_) => break,
                },
                // A kill message OR the kill sender being dropped both mean
                // "stop" — identical to the old Ok(()) | Disconnected => break.
                recv(kill_rx) -> _ => break,
            }
        }
    })
}

/// Check whether a cancellation signal has been received.
///
/// This is a one-shot check — call this when you only need to check once
/// and don't need to cache the result across loop iterations.  The channel
/// is crossbeam so callers that need to *wait* on cancellation (rather than
/// poll) can `select!` on it directly.
pub(crate) fn is_cancelled_once(rx: &crossbeam_channel::Receiver<()>) -> bool {
    rx.try_recv().is_ok()
}

/// Accumulate per-turn token usage into the session-level counter and log it.
pub(crate) fn accumulate_token_usage(
    session: &mut SessionState,
    token_usage: &Option<TokenUsage>,
    turn: u32,
    ctx: &RequestContext,
) {
    if let Some(u) = token_usage {
        session.config.accumulated_usage.input_tokens += u.input_tokens;
        session.config.accumulated_usage.output_tokens += u.output_tokens;
        session.config.accumulated_usage.total_tokens += u.total_tokens;
        // Track the latest prompt_tokens (the actual context size being sent
        // to the model) separately from the accumulated billing counter.
        session.config.last_prompt_tokens = Some(u.input_tokens);
        debug!(
            session_id = ctx.session_id,
            turn,
            input_tokens = u.input_tokens,
            output_tokens = u.output_tokens,
            total_tokens = u.total_tokens,
            accumulated_input = session.config.accumulated_usage.input_tokens,
            accumulated_output = session.config.accumulated_usage.output_tokens,
            "accumulated token usage"
        );
    }
}

/// Route the worker's cumulative token usage through the main session thread
/// so the authoritative `config.accumulated_usage` is updated BEFORE the
/// update is broadcast.  The worker accumulates usage on its private clone of
/// `SessionState` and only merges it back at `RequestFinished`; a direct
/// broadcast here would leave the main thread's config at the pre-request
/// value for the whole turn, leaking stale totals into attach snapshots and
/// session metadata.  Routing through `SyncAccumulatedUsage` keeps every
/// consumer fresh mid-turn.
pub(crate) fn broadcast_token_usage(ctx: &RequestContext, session: &SessionState) {
    let _ = ctx.cmd_tx.send(SessionCommand::SyncAccumulatedUsage {
        token_usage: session.config.accumulated_usage,
        last_prompt_tokens: session.config.last_prompt_tokens,
    });
}

/// Resolve the execution timeout for a tool by name and arguments.
///
/// Returns `None` for sub-sessions (run indefinitely) and `Some(duration)`
/// for all other tools so that hanging tools are eventually killed.
///
/// The name-based defaults act as a FLOOR, not a cap: shell tools accept a
/// per-invocation `timeout` argument in milliseconds, and long legitimate
/// commands (full workspace builds, release packaging) exceed the 300s
/// shell default. When the tool's arguments request a larger timeout, the
/// outer deadline is raised to cover it plus [`TOOL_TIMEOUT_GRACE`] — the
/// inner watchdog kills the child at the requested instant; the grace only
/// covers result serialization and teardown after that kill. A missing,
/// malformed, or zero `timeout` argument falls back to the name-based
/// default, so the guard always exists.
pub(crate) fn determine_tool_timeout(name: &str, arguments_json: &str) -> Option<Duration> {
    if name == "spawn_subsession" {
        // Sub-sessions run their own agent loop which may need many
        // turns across multiple LLM calls — no wall-clock timeout.
        return None;
    }
    let base = if matches!(name, "sh" | "nushell" | "fish" | "exec") {
        // Shell commands may involve compilation, tests, or long-running
        // processes that need more time than the default.
        Duration::from_secs(300)
    } else {
        Duration::from_secs(60)
    };
    // The tool's own requested timeout raises the outer deadline when (and
    // only when) it exceeds the base: parse the `timeout` field out of the
    // raw args JSON without deserializing the tool's full arg type (this
    // function runs before dispatch and must not depend on any tool's
    // schema). ONLY shell tools accept a `timeout` argument — a stray field
    // sent to a tool whose schema lacks it must not raise that tool's
    // deadline, so the raise is gated on the shell-tool set.
    let requested = if matches!(name, "sh" | "nushell" | "fish" | "exec") {
        serde_json::from_str::<serde_json::Value>(arguments_json)
            .ok()
            .and_then(|args| args.get("timeout").and_then(|t| t.as_u64()))
            .filter(|ms| *ms > 0)
            .map(Duration::from_millis)
    } else {
        None
    };
    let effective = requested
        .map(|r| r + TOOL_TIMEOUT_GRACE)
        .map_or(base, |raised| raised.max(base));
    if effective > base {
        debug!(
            tool = name,
            requested_ms = requested.map(|r| r.as_millis() as u64),
            effective_secs = effective.as_secs(),
            "outer tool deadline raised to cover the requested timeout"
        );
    }
    Some(effective)
}

/// Aggregated result of a single concurrent tool execution, including any
/// image the tool emitted through its streaming channel.
pub(crate) struct ToolHandle {
    pub(crate) tool_call: ChatToolCall,
    pub(crate) output: ToolOutput,
    pub(crate) image: Option<PreparedImage>,
    /// When the wait-loop thread started (≈ dispatch time), carried on the
    /// handle so the collector can log per-tool elapsed independent of the
    /// batch's arrival order.
    pub(crate) started_at: Instant,
}

/// Parameters for spawning a single concurrent tool call.
pub(crate) struct SpawnToolArgs {
    pub(crate) tool_call: ChatToolCall,
    pub(crate) timeout: Option<Duration>,
    pub(crate) request_id: u32,
    pub(crate) session_id: u64,
    pub(crate) registry: Arc<ToolRegistry>,
    pub(crate) cmd_tx: mpsc::Sender<SessionCommand>,
    pub(crate) x_credentials: Option<ServiceCredential>,
    pub(crate) working_dir: Option<PathBuf>,
    pub(crate) ctx: ToolContext,
    pub(crate) invocation_description: String,
    /// Dispatch-time instant, threaded through the wait-loop thread onto the
    /// handle so the collector logs per-tool elapsed from dispatch (not from
    /// whenever the wait-loop thread happened to start), and reused by the
    /// panic-synthesis fallback so both paths agree on the timestamp.
    pub(crate) started_at: Instant,
    /// Shared batch channel: the wait-loop thread delivers its final
    /// `ToolHandle` here the moment the tool completes, so the caller can
    /// collect results in completion order without joining.
    pub(crate) result_tx: crossbeam_channel::Sender<ToolHandle>,
}

/// Dispatch-order metadata for a single concurrent tool call, retained so a
/// wait-loop thread that dies before delivering its result can be
/// synthesized with the tool's real name, arguments, description, and start
/// time — matching what the old join-based path reconstructed.
pub(crate) struct CallInfo {
    pub(crate) call_id: String,
    pub(crate) tool_name: String,
    pub(crate) arguments_json: String,
    pub(crate) invocation_description: String,
    pub(crate) started_at: Instant,
    /// Collector-side sender of the per-tool kill channel.  Held for the
    /// whole batch drain and sent to on cancel, so a still-running wait-loop
    /// stops its forwarder, sets the cooperative `ToolContext.cancelled`
    /// flag, and delivers a "cancelled" result instead of waiting for the
    /// tool to finish.  Dropping this sender (the batch drain ended) is
    /// itself a stop signal to any wait-loop still blocked on the channel.
    pub(crate) kill_tx: crossbeam_channel::Sender<()>,
}

/// The subset of dispatched calls whose results were never delivered.
///
/// Handles arrive in *completion* order, not dispatch order, so the missing
/// set must be computed by `call_id` — skipping the first N entries by index
/// would misattribute the fallback to the wrong tools whenever a fast tool
/// finished before a slower one that died.
pub(crate) fn missing_calls<'a>(
    call_infos: &'a [CallInfo],
    delivered: &HashSet<String>,
) -> impl Iterator<Item = &'a CallInfo> {
    call_infos
        .iter()
        .filter(move |info| !delivered.contains(&info.call_id))
}

/// Rebuild the `ToolHandle` for a call whose wait-loop thread died before
/// delivering — the same "tool thread panicked" output the old join-based
/// path produced, using the dispatch-order metadata so the synthesized result
/// carries the tool's real name, arguments, description, and start time.
/// Shared by the normal batch-end path and the post-cancel drain.
pub(crate) fn panic_tool_handle(info: &CallInfo) -> ToolHandle {
    ToolHandle {
        tool_call: ChatToolCall {
            id: info.call_id.clone(),
            name: info.tool_name.clone(),
            arguments_json: info.arguments_json.clone(),
            caller: None,
        },
        output: ToolOutput {
            content: "tool thread panicked".to_string(),
            is_error: true,
            invocation_description: info.invocation_description.clone(),
            ..Default::default()
        },
        image: None,
        started_at: info.started_at,
    }
}

/// Channels wiring one tool execution, returned by [`spawn_tool_execution`].
pub(crate) struct SpawnedToolExecution {
    /// The execution thread delivers its final result here.
    exec_rx: crossbeam_channel::Receiver<Result<ToolOutput, ToolError>>,
    /// Send to stop the forwarding thread (also stops when the sender is
    /// dropped, e.g. by the serial path's drop guard).
    kill_tx: crossbeam_channel::Sender<()>,
    /// The tool may emit one image here during execution.
    image_rx: mpsc::Receiver<PreparedImage>,
    /// Forwarding-thread handle, kept alive for this frame then detached
    /// (never joined) so the thread can finish a busy stream in the
    /// background after the caller stops waiting.
    _forwarder: std::thread::JoinHandle<()>,
}

/// Spawn the forwarding thread and the tool execution thread for one call,
/// wiring them to fresh channels.  Shared by the serial
/// (`execute_tool_with_timeout`) and concurrent (`spawn_single_tool`) paths so
/// the channel topology — bounded streaming output, forwarder kill, exec
/// result, image — exists in exactly one place.
///
/// The streaming output channel is *bounded* ([`STREAMING_CHANNEL_CAPACITY`]):
/// a tool that streams faster than the forwarder can broadcast to subscribers
/// blocks on `send` instead of buffering an unbounded number of chunks in
/// memory (backpressure, matching the SSE reader's bounded channel).  The
/// forwarder drains continuously and the session command channel it forwards
/// into is unbounded (std `mpsc::Sender::send` never blocks), so this cannot
/// deadlock; when the forwarder exits it drops the receiver, failing any
/// blocked `send`.
#[expect(clippy::too_many_arguments)]
pub(crate) fn spawn_tool_execution(
    tool_call: &ChatToolCall,
    format: ToolOutputFormat,
    registry: Arc<ToolRegistry>,
    x_credentials: Option<ServiceCredential>,
    working_dir: Option<PathBuf>,
    tool_ctx: ToolContext,
    cmd_tx: mpsc::Sender<SessionCommand>,
    session_id: u64,
    request_id: u32,
) -> SpawnedToolExecution {
    // The execution thread delivers its final result here.
    let (exec_tx, exec_rx) = crossbeam_channel::unbounded::<Result<ToolOutput, ToolError>>();

    // Streaming output forwarded to subscribers in real time (bounded — see
    // the function docs for the backpressure rationale).
    let (output_tx, output_rx) = crossbeam_channel::bounded::<Vec<u8>>(STREAMING_CHANNEL_CAPACITY);

    // Kill signal for the forwarding thread — sent when the caller stops
    // waiting (also fires when the sender is dropped).
    let (kill_tx, kill_rx) = crossbeam_channel::unbounded::<()>();

    // The tool may emit one image during execution.
    let (image_tx, image_rx) = mpsc::channel::<PreparedImage>();

    // ── Forwarding thread ──────────────────────────────────────────
    //
    // Forwards streaming output chunks to subscribers as they arrive.
    // Exits when the output channel is disconnected (tool finished) or
    // a kill signal is received (we stopped waiting).
    let _forwarder = spawn_forwarding_thread(
        cmd_tx,
        session_id,
        request_id,
        tool_call.id.clone(),
        output_rx,
        kill_rx,
    );

    // ── Execution thread ───────────────────────────────────────────
    let tc = tool_call.clone();
    thread::spawn(move || {
        let result = registry.execute_streaming_json(
            &tc,
            format,
            output_tx,
            x_credentials.as_ref(),
            working_dir.as_deref(),
            Some(&tool_ctx),
            Some(image_tx),
        );
        let _ = exec_tx.send(result);
    });

    SpawnedToolExecution {
        exec_rx,
        kill_tx,
        image_rx,
        _forwarder,
    }
}

/// Spawn a single tool call on a dedicated thread with its own forwarding
/// channel, timeout guard, and image drain.
///
/// The wait-loop thread delivers the final `ToolHandle` through the shared
/// `result_tx` batch channel the moment the tool completes — the caller
/// collects results in *completion* order without joining, so a fast tool is
/// never gated by a slower one the model listed before it. The spawned
/// thread handles all channel wiring, timeouts, and error recording
/// internally.
///
/// Returns the collector-side sender of the per-tool *kill* channel.  The
/// collector keeps one per call and sends to it when the request is
/// cancelled, so a still-running wait-loop stops its forwarder (streaming
/// halts promptly), sets the cooperative `ToolContext.cancelled` flag (the
/// tool can stop early), and delivers a "cancelled" result instead of
/// waiting for the tool to finish.  Dropping the returned sender is itself
/// a stop signal — a wait-loop whose kill receiver disconnects selects
/// immediately — so the sender must live exactly as long as the batch drain.
pub(crate) fn spawn_single_tool(args: SpawnToolArgs) -> crossbeam_channel::Sender<()> {
    let SpawnToolArgs {
        tool_call,
        timeout,
        request_id,
        session_id,
        registry,
        cmd_tx,
        x_credentials,
        working_dir,
        ctx,
        invocation_description,
        started_at,
        result_tx,
    } = args;
    // Cooperative cancellation flag shared with the tool's context: set when
    // this wait-loop observes a kill (or the deadline expires), so a tool
    // that consults `ToolContext.cancelled` can stop early.  This is the one
    // sanctioned shared-state exception to the repo's channel-only thread-
    // communication rule (see AGENTS.md): a channel message cannot interrupt
    // a blocking tool call, so the flag is a best-effort, data-free stop hint.
    let cancel_flag = Arc::clone(&ctx.cancelled);

    // Kill signal for *this wait-loop* — sent by the collector when the
    // request is cancelled (a message), or implicitly when it drops the
    // sender after the batch drain ends (a disconnect).  The wait-loop
    // selects on it so a cancel stops streaming and flags the tool without
    // waiting for the tool to finish.
    let (tool_kill_tx, tool_kill_rx) = crossbeam_channel::unbounded::<()>();

    // Shared channel wiring: the forwarding thread, the execution thread, and
    // the channels between them (see [`spawn_tool_execution`]).  The wait-loop
    // below consumes `exec_rx`/`image_rx` and stops the forwarder via
    // `kill_tx`; the forwarder's handle lives in this frame and is detached
    // (never joined) when this function returns.
    let SpawnedToolExecution {
        exec_rx,
        kill_tx,
        image_rx,
        _forwarder,
    } = spawn_tool_execution(
        &tool_call,
        ToolOutputFormat::Text,
        registry,
        x_credentials,
        working_dir,
        ctx,
        cmd_tx,
        session_id,
        request_id,
    );

    // ── Wait loop ──────────────────────────────────────────────────
    //
    // Two modes:
    //   Some(dur) — bounded wait with an exact deadline timer.
    //   None      — unbounded wait; blocks until the tool completes or a
    //               kill arrives.
    let deadline = timeout.map(|d| Instant::now() + d);
    thread::spawn(move || {
        // What woke this wait-loop: the tool's result (delivered through the
        // exec channel), a collector kill (message or dropped sender), or —
        // in the bounded mode — the exact deadline timer.
        enum WaitOutcome {
            Result(Result<Result<ToolOutput, ToolError>, crossbeam_channel::RecvError>),
            Kill,
            Deadline,
        }

        // Dispatch-time instant captured by the caller (not when this thread
        // happened to start), carried on the handle so the collector can log
        // per-tool elapsed independent of batch arrival order.
        let outcome = match deadline {
            // No timeout — block until the tool finishes or the collector
            // kills this wait-loop.  Result arm first: a tool that finished
            // in the same instant as the kill still delivers its real result.
            None => crossbeam_channel::select_biased! {
                recv(exec_rx) -> msg => WaitOutcome::Result(msg),
                recv(tool_kill_rx) -> _ => WaitOutcome::Kill,
            },
            Some(deadline) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                // Wait for the tool's result, a kill, or an exact timer for
                // the remaining budget.  Result first: a tool that finished
                // just before its deadline is not reported as timed out, and
                // one that finished in the same instant as a kill still
                // delivers its real result.  A zero `remaining` makes
                // `after(Duration::ZERO)` fire immediately, so this also
                // covers the deadline-already-passed case without a separate
                // pre-check (which could itself miss a result queued in the
                // same instant).
                crossbeam_channel::select_biased! {
                    recv(exec_rx) -> msg => WaitOutcome::Result(msg),
                    recv(tool_kill_rx) -> _ => WaitOutcome::Kill,
                    recv(crossbeam_channel::after(remaining)) -> _ => WaitOutcome::Deadline,
                }
            }
        };
        let output = match outcome {
            WaitOutcome::Result(msg) => {
                // Shared message→output mapping, used by the serial path too:
                // records the per-tool execution metric once per outcome and
                // carries the invocation description onto error/panic outputs
                // (the concurrent path previously dropped both).
                tool_result_from_channel(
                    &tool_call.name,
                    started_at,
                    msg,
                    &invocation_description,
                    false,
                )
                .0
            }
            WaitOutcome::Deadline => {
                // The exact deadline timer fired.  A result that queued in
                // the same instant is still a real result — drain it
                // (non-blocking) before reporting the timeout, closing the
                // finish-vs-deadline race as far as a deadline-based wait
                // can.  The tool is still running past its budget, so flag
                // its context to let it stop if it can.
                cancel_flag.store(true, Ordering::Relaxed);
                drain_queued_or_synthesize(
                    &tool_call.name,
                    started_at,
                    &invocation_description,
                    &exec_rx,
                    format!(
                        "tool '{}' timed out after {}s",
                        tool_call.name,
                        timeout.unwrap_or(Duration::ZERO).as_secs(),
                    ),
                    false,
                )
                .0
            }
            WaitOutcome::Kill => {
                // The collector killed this wait-loop (or dropped the kill
                // sender, e.g. the batch drain ended).  Stop the forwarder so
                // streaming halts promptly, flag the tool so it can stop
                // early, and deliver a "cancelled" result: the collector's
                // post-cancel drain always collects this handle, so the
                // transcript records the cancelled outcome deterministically
                // (it is never dropped by a race with the placeholder sweep).
                // The tool's *execution thread* keeps
                // running (Rust threads cannot be interrupted mid-call) but
                // every channel it would send to — exec, streaming output,
                // image — has been dropped by this exit, so its late result
                // is discarded harmlessly.
                cancel_flag.store(true, Ordering::Relaxed);
                let _ = kill_tx.send(());
                let image = image_rx.try_recv().ok();
                let content = format!("tool '{}' cancelled", tool_call.name);
                // Record the execution metric for this outcome too — the
                // serial path's cancel arm does the same, so both phases
                // account for a cancelled tool exactly once.
                crate::metrics::record_tool_execution(
                    &tool_call.name,
                    started_at.elapsed().as_secs_f64(),
                    true,
                );
                let _ = result_tx.send(ToolHandle {
                    tool_call,
                    output: ToolOutput {
                        content,
                        is_error: true,
                        invocation_description: invocation_description.clone(),
                        ..Default::default()
                    },
                    image,
                    started_at,
                });
                return;
            }
        };

        // Drain any image that was emitted during execution.
        let image = image_rx.try_recv().ok();

        // Signal the forwarding thread to stop — we have our result and
        // won't be streaming any more output from this tool call.
        let _ = kill_tx.send(());

        let _ = result_tx.send(ToolHandle {
            tool_call,
            output,
            image,
            started_at,
        });
    });
    tool_kill_tx
}

/// Persist the finalized turn to the database and broadcast it to all
/// connected clients.
///
/// The final snapshot rides [`SessionEvent::TurnAppended`] — the same
/// delivery the turn already used when it was appended mid-stream.
/// `TurnFinalized` (a second, redundant repair snapshot repeating an already
/// delivered turn) was removed from the protocol entirely (v3);
/// `TurnAppended` is now the single authoritative turn delivery, so clients
/// render the final/error turn from it. The DB write inside `finalize_turn`
/// is required and unchanged.
pub(crate) fn finalize_and_broadcast_turn(
    session: &mut SessionState,
    ctx: &RequestContext,
    current_turn_id: u32,
) -> io::Result<()> {
    session.finalize_turn(&ctx.db, ctx.session_id, current_turn_id)?;
    if let Some(turn) = session.turns.get(&current_turn_id) {
        let _ = ctx
            .cmd_tx
            .send(SessionCommand::Broadcast(DaemonMessage::Session {
                session_id: Some(ctx.session_id),
                event: SessionEvent::TurnAppended {
                    turn_id: current_turn_id,
                    turn: turn_for_client(turn),
                },
            }));
    }
    Ok(())
}

pub(crate) fn finish_tool_call(
    request_id: u32,
    session: &mut SessionState,
    tool_call: &ChatToolCall,
    output: &mut ToolOutput,
    ctx: &RequestContext,
    turn_id: u32,
) {
    let is_error = output.is_error;
    let content = output.content.clone();

    // The five per-record fields come from `output` (content, is_error,
    // invocation_description, image_ref); only `name` is taken from the tool
    // call, since `ToolOutput` does not carry it.
    session.update_tool_result(turn_id, &tool_call.id, tool_call.name.clone(), output);

    broadcast_turn_appended(&ctx.cmd_tx, session, ctx.session_id, turn_id);

    let event = if is_error {
        DaemonMessage::Session {
            session_id: Some(ctx.session_id),
            event: SessionEvent::ToolCallFailed {
                request_id,
                call_id: tool_call.id.clone(),
                tool_name: tool_call.name.clone(),
                error: content,
            },
        }
    } else {
        DaemonMessage::Session {
            session_id: Some(ctx.session_id),
            event: SessionEvent::ToolCallFinished {
                request_id,
                call_id: tool_call.id.clone(),
                tool_name: tool_call.name.clone(),
            },
        }
    };
    if let Err(e) = ctx.cmd_tx.send(SessionCommand::Broadcast(event)) {
        warn!(%request_id, error = %e, "failed to broadcast tool call finished/failed event");
    }
}

/// Record a completed tool in the session and the next-call accumulator.
///
/// Shared by the serial and concurrent completion paths so both record a
/// result identically: emit any produced image, fill the tool's seeded
/// placeholder result in place (see [`SessionState::update_tool_result`]),
/// and collect the output for the provider's next request.
#[expect(clippy::too_many_arguments)]
pub(crate) fn record_tool_completion(
    request_id: u32,
    session: &mut SessionState,
    tool_call: &ChatToolCall,
    output: &mut ToolOutput,
    image: Option<PreparedImage>,
    ctx: &RequestContext,
    current_turn_id: u32,
    tool_results: &mut Vec<ToolResultItem>,
    known_hint_paths: &mut Vec<PathBuf>,
    pending_hints: &mut Vec<String>,
) {
    // Escape Unicode format characters (bidi overrides, ZWSP, …) before the
    // content enters the transcript — the session record AND the next-call
    // accumulator both derive from `output.content`, so this single point
    // covers every tool at once. ESC/ANSI, newlines, and tabs pass through
    // untouched (shell/VM colors survive); only the text-reordering chars
    // that could spoof the model are escaped. The terminal is defended
    // separately by the TUI's render filter.
    //
    // Escaping *expands* (a Cf char becomes `\u{202e}`), so content that was
    // byte-capped at the source as raw output (shell/VM/series) could exceed
    // the budget after sanitizing; re-applying the cap after the escape keeps
    // the transcript within MAX_TOOL_OUTPUT_BYTES for every tool. The cap is
    // idempotent — already-capped content passes through unchanged.
    output.content = truncate_tool_output(&sanitize_transcript(&output.content));

    if let Some(image) = image {
        emit_image(
            &ctx.cmd_tx,
            image,
            Some(tool_call.id.clone()),
            session,
            ctx.session_id,
            current_turn_id,
        );
    }

    finish_tool_call(request_id, session, tool_call, output, ctx, current_turn_id);
    collect_tool_result(CollectToolResultParams {
        tool_results,
        session,
        tool_call,
        output,
        known_hint_paths,
        pending_hints,
    });
}

/// Wrap a raw tool-execution channel message into the caller-facing
/// `(ToolOutput, bool)` pair, recording the per-tool execution metric once
/// for every outcome.
///
/// `invocation_description` is carried onto the error/panic outputs so a
/// failed execution renders with the same invocation context the success
/// path provides — shared by the serial path and the concurrent wait-loop.
///
/// `cancelled` is the sticky-cancel flag to report alongside the output: it is
/// `true` only when the message was drained *after* a cancel was observed
/// (the tool's real outcome is recorded, but the request still stops).
pub(crate) fn tool_result_from_channel(
    tool_name: &str,
    exec_start: std::time::Instant,
    msg: Result<Result<ToolOutput, ToolError>, crossbeam_channel::RecvError>,
    invocation_description: &str,
    cancelled: bool,
) -> (ToolOutput, bool) {
    match msg {
        Ok(Ok(output)) => {
            crate::metrics::record_tool_execution(
                tool_name,
                exec_start.elapsed().as_secs_f64(),
                output.is_error,
            );
            (output, cancelled)
        }
        Ok(Err(e)) => {
            crate::metrics::record_tool_execution(
                tool_name,
                exec_start.elapsed().as_secs_f64(),
                true,
            );
            (
                ToolOutput {
                    content: e.to_string(),
                    is_error: true,
                    invocation_description: invocation_description.to_string(),
                    ..Default::default()
                },
                cancelled,
            )
        }
        // The execution thread died without sending a final message.
        Err(_) => {
            crate::metrics::record_tool_execution(
                tool_name,
                exec_start.elapsed().as_secs_f64(),
                true,
            );
            (
                ToolOutput {
                    content: "tool execution thread panicked".to_string(),
                    is_error: true,
                    invocation_description: invocation_description.to_string(),
                    ..Default::default()
                },
                cancelled,
            )
        }
    }
}

/// Shared stop-arm handling for the serial and concurrent waits: a result
/// that queued in the same instant the stop signal fired is a real result and
/// must not be discarded, so it is drained first; only when nothing queued is
/// the stop message synthesized.  A disconnected exec channel means the
/// execution thread died — that is reported as a panic, not as the stop
/// message (the stop may be unrelated to the death), matching the `Result`
/// arm's mapping.
///
/// Returns the tool output and the sticky-cancel flag: `true` only for a
/// genuine request cancel in the serial path (a timeout or per-tool kill is
/// not a request cancel).  The caller sets the cooperative
/// `ToolContext.cancelled` flag before calling.
pub(crate) fn drain_queued_or_synthesize(
    tool_name: &str,
    exec_start: std::time::Instant,
    invocation_description: &str,
    exec_rx: &crossbeam_channel::Receiver<Result<ToolOutput, ToolError>>,
    stop_message: String,
    sticky_cancelled: bool,
) -> (ToolOutput, bool) {
    match exec_rx.try_recv() {
        // A result that completed in the same instant the stop fired is a
        // real result — record it as-is, carrying the sticky flag when the
        // stop was a genuine request cancel.
        Ok(msg) => tool_result_from_channel(
            tool_name,
            exec_start,
            Ok(msg),
            invocation_description,
            sticky_cancelled,
        ),
        // The tool is still running past its budget / after the stop signal.
        Err(crossbeam_channel::TryRecvError::Empty) => {
            crate::metrics::record_tool_execution(
                tool_name,
                exec_start.elapsed().as_secs_f64(),
                true,
            );
            (
                ToolOutput {
                    content: stop_message,
                    is_error: true,
                    invocation_description: invocation_description.to_string(),
                    ..Default::default()
                },
                sticky_cancelled,
            )
        }
        // The execution thread died (panicked) in the same instant — report
        // the real cause rather than the stop message.
        Err(crossbeam_channel::TryRecvError::Disconnected) => {
            crate::metrics::record_tool_execution(
                tool_name,
                exec_start.elapsed().as_secs_f64(),
                true,
            );
            (
                ToolOutput {
                    content: "tool execution thread panicked".to_string(),
                    is_error: true,
                    invocation_description: invocation_description.to_string(),
                    ..Default::default()
                },
                sticky_cancelled,
            )
        }
    }
}

#[expect(clippy::too_many_arguments)]
pub(crate) fn execute_tool_with_timeout(
    tool_call: &ChatToolCall,
    x_credentials: Option<&ServiceCredential>,
    working_dir: Option<&Path>,
    timeout_dur: Duration,
    request_id: u32,
    session_id: u64,
    session: &mut SessionState,
    cancel_rx: &crossbeam_channel::Receiver<()>,
    ctx: &RequestContext,
    invocation_description: &str,
) -> (ToolOutput, bool, Option<PreparedImage>) {
    let format = match &tool_call.caller {
        Some(caller) if caller.kind == "program" => ToolOutputFormat::Json,
        _ => ToolOutputFormat::Text,
    };
    // Capture start time for tool execution metrics.
    let exec_start = std::time::Instant::now();

    // Cooperative cancellation flag shared with the tool's context: set when
    // this wait observes a cancel (or the deadline expires), so a tool
    // that consults `ToolContext.cancelled` can stop early.  The concurrent
    // path mirrors this with `cancel_flag` in the collector; the serial path
    // owns its flag here because it builds the tool context per call.
    let cancel_flag = Arc::new(AtomicBool::new(false));
    let tool_ctx = crate::tools::context::ToolContext {
        session_id: ctx.session_id,
        db: Arc::clone(&ctx.db),
        daemon_tx: ctx.daemon_tx.clone(),
        active_tool_groups: session.config.active_tool_groups.clone(),
        reasoning_effort: session.config.reasoning_effort.clone(),
        selected_model: session.config.selected_model.clone(),
        working_dir: working_dir.map(|p| p.to_path_buf()),
        cancelled: Arc::clone(&cancel_flag),
        account_name: session.config.account_name.clone(),
    };

    // Shared channel wiring: forwarding thread + execution thread + the
    // channels between them (see [`spawn_tool_execution`]).  The wait below
    // consumes `result_rx`/`image_rx` and the drop guard stops the forwarder
    // via `kill_tx` when this function returns for any reason.
    let SpawnedToolExecution {
        exec_rx: result_rx,
        kill_tx,
        image_rx,
        _forwarder,
    } = spawn_tool_execution(
        tool_call,
        format,
        Arc::clone(&ctx.tool_registry),
        x_credentials.cloned(),
        working_dir.map(|p| p.to_path_buf()),
        tool_ctx,
        ctx.cmd_tx.clone(),
        session_id,
        request_id,
    );

    // Drop guard: when the main loop exits (for any reason), signal the
    // forwarder to stop so it doesn't orphan waiting on output_rx.
    struct KillGuard(crossbeam_channel::Sender<()>);
    impl Drop for KillGuard {
        fn drop(&mut self) {
            let _ = self.0.send(());
        }
    }
    let _kill_guard = KillGuard(kill_tx);

    // Event-driven wait: `select_biased!` waits on the cancellation channel,
    // the tool's result channel, and an exact timer for the remaining budget
    // simultaneously — no polling interval, timeouts fire precisely.  The
    // cancel arm is listed first because an *already-queued* cancel is
    // selected deterministically (the biased fast path scans the arms in
    // order); a cancel that arrives mid-block merely *tends* to beat a
    // simultaneously-ready result, and both outcomes are handled correctly
    // below (the request still stops; a completed result is still drained).
    // Every arm terminates the wait, so no loop is needed; a zero `remaining`
    // makes the timer fire immediately, covering a deadline that already
    // passed without a separate pre-check (which could itself miss a result
    // queued in the same instant).
    let deadline = std::time::Instant::now() + timeout_dur;
    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
    let (output, cancelled) = crossbeam_channel::select_biased! {
        recv(cancel_rx) -> _ => {
            // Bias for cancel: the request stops the instant Escape is
            // pressed, even if the tool's result was queued in the same
            // instant.  But a result that actually completed isn't discarded —
            // drain it so the transcript records the tool's real outcome and
            // Phase 3 mirrors any config change it made.  `true` is still
            // returned so the caller stops the request: the cancel signal was
            // consumed, so without the sticky flag a mid-tool cancel would be
            // silently swallowed.  The tool's context flag is set too, so a
            // tool that checks `ToolContext.cancelled` can stop early.
            cancel_flag.store(true, Ordering::Relaxed);
            drain_queued_or_synthesize(
                &tool_call.name,
                exec_start,
                invocation_description,
                &result_rx,
                format!("tool '{}' cancelled", tool_call.name),
                true,
            )
        }
        recv(result_rx) -> msg => {
            tool_result_from_channel(&tool_call.name, exec_start, msg, invocation_description, false)
        }
        // The exact deadline timer fired.  A result that queued in the same
        // instant is still a real result — drain it (non-blocking) before
        // reporting the timeout, closing the finish-vs-deadline race as far
        // as a deadline-based wait can.  The tool is still running past its
        // budget, so flag its context to let it stop if it can.
        recv(crossbeam_channel::after(remaining)) -> _ => {
            cancel_flag.store(true, Ordering::Relaxed);
            drain_queued_or_synthesize(
                &tool_call.name,
                exec_start,
                invocation_description,
                &result_rx,
                format!(
                    "tool '{}' timed out after {}s",
                    tool_call.name,
                    timeout_dur.as_secs()
                ),
                false,
            )
        }
    };
    // Drain any image the tool emitted during execution (the caller records
    // it on the turn alongside the output).
    let image = image_rx.try_recv().ok();
    (output, cancelled, image)
}
