use crate::context::{self, LoadedSkill, SkillMeta};
use crate::providers::InferenceProvider;
use crate::sessions::{AssistantResponse, RequestContext, SessionCommand, SessionState};
use crate::tools::context::ToolContext;
use crate::tools::load_tools::{LoadToolsArgs, apply_load_tools};
use crate::tools::set_working_dir::{SetWorkingDirArgs, resolve_working_dir_path};
use crate::tools::unload_tools::{UnloadToolsArgs, apply_unload_tools};
use crate::tools::{
    PreparedImage, STREAMING_CHANNEL_CAPACITY, ToolError, ToolOutput, ToolOutputFormat,
    ToolRegistry,
};
use choreo_ai_protocols::openai::{
    AssistantToolCall, AssistantToolFunction, ChatRequestMessage, ChatToolDefinition,
};
use choreo_ai_protocols::{
    ChatToolCall, ChatTurnRequest, ChatTurnResult, ReasoningPassback, StreamEvent, ToolResultItem,
    model_reasoning_capability, model_reasoning_passback,
};
use choreo_keystore::ServiceCredential;
use choreo_proto::{
    AssistantToolCallRecord, ContextConfig, DaemonMessage, DisplayedImageRecord, ImageMetadata,
    OutputStream, ReasoningArtifact, ReasoningProducer, SessionStatus, TokenUsage, Turn,
};
use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use std::time::Instant;
use tracing::{debug, info, trace, warn};

/// Broadcast a TurnAppended message to all session subscribers, if the
/// given turn_id exists in the session's turn map.
fn broadcast_turn_appended(
    cmd_tx: &mpsc::Sender<SessionCommand>,
    session: &SessionState,
    session_id: u64,
    turn_id: u32,
) {
    if let Some(turn) = session.turns.get(&turn_id)
        && let Err(e) = cmd_tx.send(SessionCommand::Broadcast(DaemonMessage::TurnAppended {
            session_id,
            turn_id,
            turn: turn.clone(),
        }))
    {
        warn!(%turn_id, error = %e, "failed to broadcast TurnAppended");
    }
}

/// Persist a `PreparedImage` to the session's current active turn and
/// broadcast it to live subscribers immediately (mid-turn) so the image
/// appears as soon as the tool finishes rather than waiting for request
/// completion.  Used by both the serial and concurrent tool paths.
fn emit_image(
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
fn spawn_forwarding_thread(
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
                            .send(SessionCommand::Broadcast(DaemonMessage::ToolResultChunk {
                                session_id,
                                request_id,
                                call_id: call_id.clone(),
                                data,
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
                                        DaemonMessage::ToolResultChunk {
                                            session_id,
                                            request_id,
                                            call_id: call_id.clone(),
                                            data,
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
fn accumulate_token_usage(
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

/// Broadcast the session's accumulated token usage to all subscribers so the
/// UI can update its final token display at the end of a turn.
fn broadcast_token_usage(session_id: u64, ctx: &RequestContext, session: &SessionState) {
    let _ = ctx
        .cmd_tx
        .send(SessionCommand::Broadcast(DaemonMessage::TokenUsageUpdate {
            session_id,
            token_usage: session.config.accumulated_usage,
            last_prompt_tokens: session.config.last_prompt_tokens,
        }));
}

/// Resolve the execution timeout for a tool by name.
///
/// Returns `None` for sub-sessions (run indefinitely) and `Some(duration)`
/// for all other tools so that hanging tools are eventually killed.
fn determine_tool_timeout(name: &str) -> Option<Duration> {
    if name == "spawn_subsession" {
        // Sub-sessions run their own agent loop which may need many
        // turns across multiple LLM calls — no wall-clock timeout.
        None
    } else if matches!(name, "sh" | "nushell" | "fish" | "exec") {
        // Shell commands may involve compilation, tests, or long-running
        // processes that need more time than the default.
        Some(Duration::from_secs(300))
    } else {
        Some(Duration::from_secs(60))
    }
}

/// Aggregated result of a single concurrent tool execution, including any
/// image the tool emitted through its streaming channel.
struct ToolHandle {
    tool_call: ChatToolCall,
    output: ToolOutput,
    image: Option<PreparedImage>,
    /// When the wait-loop thread started (≈ dispatch time), carried on the
    /// handle so the collector can log per-tool elapsed independent of the
    /// batch's arrival order.
    started_at: Instant,
}

/// Parameters for spawning a single concurrent tool call.
struct SpawnToolArgs {
    tool_call: ChatToolCall,
    timeout: Option<Duration>,
    request_id: u32,
    session_id: u64,
    registry: Arc<ToolRegistry>,
    cmd_tx: mpsc::Sender<SessionCommand>,
    x_credentials: Option<ServiceCredential>,
    working_dir: Option<PathBuf>,
    ctx: ToolContext,
    invocation_description: String,
    /// Dispatch-time instant, threaded through the wait-loop thread onto the
    /// handle so the collector logs per-tool elapsed from dispatch (not from
    /// whenever the wait-loop thread happened to start), and reused by the
    /// panic-synthesis fallback so both paths agree on the timestamp.
    started_at: Instant,
    /// Shared batch channel: the wait-loop thread delivers its final
    /// `ToolHandle` here the moment the tool completes, so the caller can
    /// collect results in completion order without joining.
    result_tx: crossbeam_channel::Sender<ToolHandle>,
}

/// Dispatch-order metadata for a single concurrent tool call, retained so a
/// wait-loop thread that dies before delivering its result can be
/// synthesized with the tool's real name, arguments, description, and start
/// time — matching what the old join-based path reconstructed.
struct CallInfo {
    call_id: String,
    tool_name: String,
    arguments_json: String,
    invocation_description: String,
    started_at: Instant,
    /// Collector-side sender of the per-tool kill channel.  Held for the
    /// whole batch drain and sent to on cancel, so a still-running wait-loop
    /// stops its forwarder, sets the cooperative `ToolContext.cancelled`
    /// flag, and delivers a "cancelled" result instead of waiting for the
    /// tool to finish.  Dropping this sender (the batch drain ended) is
    /// itself a stop signal to any wait-loop still blocked on the channel.
    kill_tx: crossbeam_channel::Sender<()>,
}

/// The subset of dispatched calls whose results were never delivered.
///
/// Handles arrive in *completion* order, not dispatch order, so the missing
/// set must be computed by `call_id` — skipping the first N entries by index
/// would misattribute the fallback to the wrong tools whenever a fast tool
/// finished before a slower one that died.
fn missing_calls<'a>(
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
fn panic_tool_handle(info: &CallInfo) -> ToolHandle {
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
struct SpawnedToolExecution {
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
fn spawn_tool_execution(
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
fn spawn_single_tool(args: SpawnToolArgs) -> crossbeam_channel::Sender<()> {
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

/// Resolve the effective reasoning effort for a turn, disabling it if the
/// model/provider combination does not support it.
fn resolve_reasoning_effort(
    client: &InferenceProvider,
    model: &str,
    session_id: u64,
    turn_iter: u32,
    configured_effort: &str,
) -> String {
    if configured_effort == "off" {
        return configured_effort.to_string();
    }
    let slug = client.provider_slug();
    let capability = model_reasoning_capability(slug, model);
    if capability.available_effort_levels.is_empty() {
        warn!(
            session_id, turn = turn_iter, model,
            effort = %configured_effort,
            "model does not support reasoning, disabling",
        );
        "off".to_string()
    } else if !capability
        .available_effort_levels
        .iter()
        .any(|l| l == configured_effort)
    {
        warn!(
            session_id, turn = turn_iter, model,
            effort = %configured_effort,
            valid = ?capability.available_effort_levels,
            "reasoning effort '{}' not in model's capability set, disabling",
            configured_effort,
        );
        "off".to_string()
    } else {
        configured_effort.to_string()
    }
}

/// Estimate the input tokens contributed by a reasoning artifact's payload.
///
/// The artifact is opaque bytes owned by the producing adapter; the daemon
/// never interprets it. Most current payloads (chat `reasoning_content`
/// strings, Anthropic thinking-block JSON, Gemini signatures) are UTF-8 text,
/// so count them with the encoding when decodable; otherwise fall back to a
/// bytes/4 heuristic. Replayed reasoning is billed as input on keep-all
/// models, so under-counting here would mislead the context-window display.
fn reasoning_artifact_tokens(enc: &tiktoken::CoreBpe, artifact: &ReasoningArtifact) -> u32 {
    let bytes = match artifact {
        // ChatReasoning is a struct variant (field tag + bytes); the field
        // tag only matters for re-emission targeting, not token estimation,
        // so bind just the payload.
        ReasoningArtifact::ChatReasoning { bytes: b, .. }
        | ReasoningArtifact::AnthropicThinking(b)
        | ReasoningArtifact::GoogleSignatures(b)
        | ReasoningArtifact::ResponsesItems(b) => b,
    };
    match std::str::from_utf8(bytes) {
        Ok(text) => enc.count(text) as u32,
        Err(_) => (bytes.len() / 4) as u32,
    }
}

/// Estimate the number of prompt tokens for the current request using
/// tiktoken.  Returns a (encoding, estimated_tokens) pair so the caller
/// can reuse the encoding for output-token counting during streaming.
fn estimate_prompt_tokens(
    model: &str,
    messages: &[ChatRequestMessage],
    tools: &[ChatToolDefinition],
) -> (Option<&'static tiktoken::CoreBpe>, u32) {
    let encoding =
        tiktoken::encoding_for_model(model).or_else(|| tiktoken::get_encoding("cl100k_base"));
    let estimated = match &encoding {
        Some(enc) => {
            // Reasoning artifacts are NOT excluded: since phase 4b the builder
            // attaches them to assistant messages under echo policies
            // (ToolLoop/AllTurns/Signature), and providers bill replayed
            // reasoning as input tokens (the round-trip payload is part of the
            // context on keep-all models). The legacy string fields
            // (reasoning_content/reasoning/reasoning_text) are still never
            // populated by the daemon, so only `reasoning_artifact` is counted.
            let content_tokens: u32 = messages
                .iter()
                .filter_map(|m| m.content.as_deref())
                .map(|text| enc.count(text) as u32)
                .sum();

            let tool_call_tokens: u32 = messages
                .iter()
                .filter_map(|m| m.tool_calls.as_ref())
                .flat_map(|calls| calls.iter())
                .map(|tc| {
                    enc.count(&tc.id) as u32
                        + enc.count(&tc.kind) as u32
                        + enc.count(&tc.function.name) as u32
                        + enc.count(&tc.function.arguments) as u32
                })
                .sum();

            let tool_def_tokens: u32 = tools
                .iter()
                .filter_map(|def| {
                    match serde_json::to_string(def) {
                        Ok(s) => Some(enc.count(&s) as u32),
                        Err(e) => {
                            warn!(error = %e, "failed to serialize tool definition for token estimation");
                            None
                        }
                    }
                })
                .sum();

            let artifact_tokens: u32 = messages
                .iter()
                .filter_map(|m| m.reasoning_artifact.as_ref())
                .map(|artifact| reasoning_artifact_tokens(enc, artifact))
                .sum();

            content_tokens + tool_call_tokens + tool_def_tokens + artifact_tokens
        }
        None => {
            tracing::warn!("no tiktoken encoding available for {model}");
            0
        }
    };
    (encoding, estimated)
}

fn extract_json_string(json: &str, key: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    v.get(key)?.as_str().map(|s| s.to_string())
}

struct SystemContentParams<'a> {
    working_dir: Option<&'a Path>,
    context_config: &'a ContextConfig,
    skills: &'a [SkillMeta],
    loaded_skill_bodies: &'a [LoadedSkill],
    tool_registry: &'a ToolRegistry,
    pending_hints: &'a [String],
    /// The session title, if one has been set, so the LLM can maintain
    /// awareness of the agreed-upon session purpose.
    session_title: Option<&'a str>,
}

fn build_system_content(
    params: SystemContentParams,
    context_cache: &mut Option<(u64, Arc<String>)>,
) -> Option<String> {
    let working_dir = match params.working_dir {
        Some(wd) => wd,
        None => {
            warn!("cannot build system content: no working directory on session");
            return None;
        }
    };
    let groups = params.tool_registry.groups();
    let base_prompt =
        context::build_base_prompt(params.skills, &groups, params.loaded_skill_bodies);
    let mut content = base_prompt;

    // Context files with fingerprint caching
    if let Ok(bundle) = context::discover_context(working_dir, params.context_config) {
        let context_str = match context_cache {
            Some((fp, cached)) if *fp == bundle.fingerprint => {
                debug!("context cache HIT (fp={})", fp);
                cached.as_str().to_string()
            }
            _ => {
                let s = context::assemble_context(&bundle);
                debug!(
                    "context cache MISS — rebuilt context ({} bytes from {} file(s))",
                    s.len(),
                    bundle.files.len()
                );
                *context_cache = Some((bundle.fingerprint, Arc::new(s.clone())));
                s
            }
        };
        if !context_str.is_empty() {
            content.push_str("\n\n");
            content.push_str(&context_str);
        }
    }

    // Inject the current session title so the LLM can see the agreed-upon
    // session purpose across turns without re-deriving it from conversation
    // history.  Only included when a title has been explicitly set.
    if let Some(title) = params.session_title
        && !title.is_empty()
    {
        content.push_str("\n\n## Current Session Title\n");
        content.push_str(title);
    }

    // Pending subdirectory hints
    if !params.pending_hints.is_empty() {
        content.push_str("\n\n## New context from project subdirectories\n");
        for hint in params.pending_hints {
            content.push('\n');
            content.push_str(hint);
        }
    }

    Some(content)
}

/// Detect a `load_skill` tool call and persist the loaded skill body into
/// the session's loaded_skill_bodies accumulator so it appears in subsequent
/// system prompts.
fn persist_loaded_skill(session: &mut SessionState, tool_name: &str, arguments_json: &str) {
    if tool_name != "load_skill" {
        return;
    }
    let Some(name) = extract_json_string(arguments_json, "name") else {
        warn!("load_skill tool call missing 'name' argument");
        return;
    };
    if session.loaded_skill_bodies.iter().any(|ls| ls.name == name) {
        debug!("skill '{}' already loaded, skipping", name);
        return;
    }
    let Some(ref working_dir) = session.config.working_dir else {
        warn!("cannot load skill '{}': no working directory", name);
        return;
    };
    if let Some(body) = context::load_skill_body(&name, working_dir) {
        info!("loaded skill body: '{}' ({} bytes)", name, body.len());
        session.loaded_skill_bodies.push(LoadedSkill { name, body });
    } else {
        warn!("skill '{}' not found or has empty body", name);
    }
}

/// Check whether a tool call touches a new subdirectory with an AGENTS.md /
/// CLAUDE.md file and, if so, collect the hint text and newly discovered paths.
fn check_subdirectory_hints(
    working_dir: Option<&Path>,
    tool_name: &str,
    arguments_json: &str,
    known_hint_paths: &mut Vec<PathBuf>,
    pending_hints: &mut Vec<String>,
) {
    if let Some((hint_text, new_paths)) =
        context::subdirectory_hints(tool_name, arguments_json, working_dir, known_hint_paths)
    {
        debug!(
            "subdirectory hints for '{}': {} new path(s)",
            tool_name,
            new_paths.len()
        );
        known_hint_paths.extend(new_paths);
        pending_hints.push(hint_text);
    }
}

struct CollectToolResultParams<'a> {
    tool_results: &'a mut Vec<ToolResultItem>,
    session: &'a mut SessionState,
    tool_call: &'a ChatToolCall,
    output: &'a ToolOutput,
    known_hint_paths: &'a mut Vec<PathBuf>,
    pending_hints: &'a mut Vec<String>,
}

/// Collect tool execution output into the result accumulator, persist any
/// `load_skill` call to the session, and check for new subdirectory hints.
/// Called after every tool execution in both the serial and concurrent phases.
fn collect_tool_result(params: CollectToolResultParams) {
    let CollectToolResultParams {
        tool_results,
        session,
        tool_call,
        output,
        known_hint_paths,
        pending_hints,
    } = params;
    trace!(
        "collecting tool result for call {} (tool: '{}')",
        tool_call.id, tool_call.name
    );
    tool_results.push(ToolResultItem {
        call_id: tool_call.id.clone(),
        output: output.content.clone(),
        caller: tool_call.caller.clone(),
    });
    persist_loaded_skill(session, &tool_call.name, &tool_call.arguments_json);
    check_subdirectory_hints(
        session.config.working_dir.as_deref(),
        &tool_call.name,
        &tool_call.arguments_json,
        known_hint_paths,
        pending_hints,
    );
}

/// Re-order tool results to match the model's original call order.
///
/// Concurrent completions are collected in arrival order (fast tools first),
/// so the accumulator fed to the provider on the next call is re-sorted to
/// match the assistant message's `tool_calls` array: some providers match
/// tool messages positionally, and the order should be deterministic. Items
/// whose `call_id` has no matching tool_call (e.g. a streaming stub created
/// before the start event arrived) sink to the end, keeping their relative
/// order (stable sort). The turn's own `tool_results` never need this — they
/// are seeded in call order and updated in place by `call_id`, so their
/// order is always the model's.
fn sort_by_call_order<T>(
    tool_calls: &[AssistantToolCallRecord],
    items: &mut [T],
    call_id_of: impl Fn(&T) -> &str,
) {
    let order: HashMap<&str, usize> = tool_calls
        .iter()
        .enumerate()
        .map(|(i, tc)| (tc.call_id.as_str(), i))
        .collect();
    if order.is_empty() {
        return;
    }
    items.sort_by_key(|item| order.get(call_id_of(item)).copied().unwrap_or(usize::MAX));
}

/// A successful session-config tool mutation, captured in Phase 1 and
/// applied to the worker's config copy in Phase 3.
///
/// The authoritative mutation is applied by the session main loop (via
/// DaemonCommand → SessionCommand routing); this worker copy must be updated
/// as well so the NEXT agent-loop iteration observes the change when it
/// rebuilds tool definitions, system content, and working-dir-relative file
/// operations.
enum PendingConfigChange {
    LoadTools(Vec<String>),
    UnloadTools(Vec<String>),
    /// The canonical path the tool resolved and the session main loop applied
    /// verbatim, taken from the tool's EXECUTED result (so no re-resolution
    /// and therefore no TOCTOU window).  `None` only when the executed result
    /// was unavailable AND re-resolution failed — the worker then skips the
    /// path update but still invalidates its skill cache so a stale cache
    /// never survives the request boundary.
    SetWorkingDir(Option<PathBuf>),
}

/// Whether `name` is one of the session-config tools that must run serially
/// and whose successful mutations are mirrored onto the worker config copy.
/// Single source of truth for the tool-name list used by the dispatch
/// partition and the mirror capture.
fn is_session_config_tool(name: &str) -> bool {
    matches!(name, "load_tools" | "unload_tools" | "set_working_dir")
}

/// Status label shown while a batch of concurrent tool calls executes.
///
/// Every non-session-config tool call lands in the concurrent dispatch
/// bucket, even a lone one — so the label must not claim parallelism for a
/// single call. Show the real tool name for `len == 1` and reserve
/// "(parallel)" for genuine multi-tool batches.
fn concurrent_tool_status_label(tools: &[ChatToolCall]) -> String {
    if tools.len() == 1 {
        tools[0].name.clone()
    } else {
        "(parallel)".into()
    }
}

/// Capture a successful session-config tool's mutation into a typed
/// [`PendingConfigChange`] for later application.  Called only for tools that
/// actually executed without error.
///
/// `base_working_dir` is the working directory in effect when the response
/// was planned — every `set_working_dir` call in the response resolved
/// against it, so the (rare) re-resolution fallback must too (chaining
/// relative resolutions against the mutated copy would diverge from the
/// canonical paths the tools sent to the main loop, which applies them
/// verbatim in call order).
fn pending_config_change(
    tool_call: &ChatToolCall,
    output: &ToolOutput,
    base_working_dir: Option<&Path>,
) -> Option<PendingConfigChange> {
    if !is_session_config_tool(&tool_call.name) {
        return None;
    }
    match tool_call.name.as_str() {
        "load_tools" => {
            let Ok(args) = serde_json::from_str::<LoadToolsArgs>(&tool_call.arguments_json) else {
                warn!(
                    tool_call_id = %tool_call.id,
                    "load_tools: could not parse args to mirror onto worker config",
                );
                return None;
            };
            Some(PendingConfigChange::LoadTools(args.groups))
        }
        "unload_tools" => {
            let Ok(args) = serde_json::from_str::<UnloadToolsArgs>(&tool_call.arguments_json)
            else {
                warn!(
                    tool_call_id = %tool_call.id,
                    "unload_tools: could not parse args to mirror onto worker config",
                );
                return None;
            };
            Some(PendingConfigChange::UnloadTools(args.groups))
        }
        "set_working_dir" => {
            // Prefer the canonical path from the tool's EXECUTED result: it
            // matches byte-for-byte what the session main loop applied, with
            // no re-resolution (and therefore no TOCTOU window in which the
            // directory could vanish between the tool's resolution and this
            // mirror).
            if let Some(path) = output
                .result_json
                .as_ref()
                .and_then(|v| v.get("path"))
                .and_then(|v| v.as_str())
            {
                return Some(PendingConfigChange::SetWorkingDir(Some(PathBuf::from(
                    path,
                ))));
            }
            // Fallback (practically unreachable — result_json is populated on
            // every successful execution): re-run the tool's own shared
            // resolution.  If even that fails, still return a None-path change
            // so the caller invalidates the worker's skill cache — a stale
            // cache must never survive the request boundary.
            let Ok(args) = serde_json::from_str::<SetWorkingDirArgs>(&tool_call.arguments_json)
            else {
                warn!(
                    tool_call_id = %tool_call.id,
                    "set_working_dir: could not parse args to mirror onto worker config",
                );
                return Some(PendingConfigChange::SetWorkingDir(None));
            };
            let path = resolve_working_dir_path(&args.path, base_working_dir).ok();
            Some(PendingConfigChange::SetWorkingDir(path))
        }
        _ => None,
    }
}

/// Apply a captured session-config mutation to the worker's config copy.
fn apply_pending_config_change(session: &mut SessionState, change: &PendingConfigChange) {
    match change {
        PendingConfigChange::LoadTools(groups) => {
            apply_load_tools(&mut session.config.active_tool_groups, groups);
            debug!(groups = ?groups, "mirrored load_tools onto worker session config");
        }
        PendingConfigChange::UnloadTools(groups) => {
            apply_unload_tools(&mut session.config.active_tool_groups, groups);
            debug!(groups = ?groups, "mirrored unload_tools onto worker session config");
        }
        PendingConfigChange::SetWorkingDir(path) => {
            if let Some(path) = path {
                session.config.working_dir = Some(path.clone());
            }
            // Always invalidate the skill cache: even when we could not
            // determine the new path, the authoritative state changed and a
            // stale cache would leak across the request boundary
            // (RequestFinished merges the worker's discovered_skills over the
            // main loop's invalidated None).  The main-loop handler does the
            // same for the authoritative state.
            session.discovered_skills = None;
            debug!(path = ?path, "mirrored set_working_dir onto worker session config");
        }
    }
}

/// Resolve the starting `previous_response_id` for a new agent-loop
/// invocation. ResponseId-policy providers (OpenAI/xAI Responses) chain
/// reasoning continuity across user turns, so the id persisted on the session
/// config after the last model call is restored here. Every other policy
/// resets to None: the id is meaningless outside Responses-style APIs and
/// must not leak into a request that does not understand it.
///
/// Restoration is additionally gated on provenance: the persisted id is
/// restored only when the current provider+model is the one that produced it
/// (same-model provenance as reasoning artifacts), so a stale id from a
/// different provider or model is never replayed.
fn initial_prev_resp_id(
    session: &SessionState,
    provider_slug: &str,
    model: &str,
) -> Option<String> {
    // A response id is service-bound: a stale id persisted under a different
    // provider (mid-session openai → xAI switch) would be rejected with a 400
    // if replayed, and it is model-bound for continuity purposes too — so
    // require an exact producer match, mirroring the artifact provenance check.
    let same_producer = session.config.last_response_id_producer.as_ref()
        == Some(&ReasoningProducer {
            provider_slug: provider_slug.to_string(),
            model: model.to_string(),
        });
    if same_producer
        && model_reasoning_passback(provider_slug, model) == ReasoningPassback::ResponseId
    {
        session.config.last_response_id.clone()
    } else {
        None
    }
}

/// Precondition guard for reasoning-echo policies (phase 4c).
///
/// Before sending a request whose passback policy requires echoing reasoning
/// (ToolLoop/AllTurns/Signature), check that every tool-involving turn in
/// history carries its artifact. A turn recorded before the artifact was
/// captured (e.g. a pre-migration session state) would otherwise be sent
/// without the reasoning payload the provider demands on tool-loop turns,
/// surfacing as a mysterious 400 — this turns that into a diagnosable log
/// line. Returns the number of missing artifacts (0 when clean) so tests can
/// exercise the path deterministically.
///
/// The guard deliberately does NOT disable thinking for the request: for
/// ToolLoop providers (DeepSeek/Kimi) the 400 comes from *history*, not the
/// current request's thinking setting, so flipping the effort to "off" would
/// not fix the failure and would silently change model behavior — the warn is
/// the honest signal. ResponseId/None policies skip the check entirely (no
/// artifact is expected on the wire).
fn warn_on_missing_reasoning_artifacts(
    session: &SessionState,
    session_id: u64,
    provider_slug: &str,
    model: &str,
) -> usize {
    let passback = model_reasoning_passback(provider_slug, model);
    if matches!(
        passback,
        ReasoningPassback::None | ReasoningPassback::ResponseId
    ) {
        return 0;
    }
    let mut missing = 0;
    for (turn_id, turn) in session.turns.iter() {
        if turn.undone || !turn_has_tool_involvement(turn) {
            continue;
        }
        if turn.reasoning_artifact.is_none() {
            missing += 1;
            warn!(
                session_id,
                turn_id,
                provider_slug,
                model,
                passback = ?passback,
                "reasoning artifact missing for tool-involving turn; provider may reject this request",
            );
        }
    }
    missing
}

pub(crate) fn run_agent_loop(
    client: &InferenceProvider,
    session: &mut SessionState,
    model: &str,
    request_id: u32,
    cancel_rx: &crossbeam_channel::Receiver<()>,
    ctx: &RequestContext,
    user_text: Option<String>,
) -> io::Result<bool> {
    let max_turns = ctx.max_turns;
    // `max_turns == 0` means *unlimited* — the loop runs until the model
    // produces a final answer, is cancelled, or hits an error.
    let limited = max_turns > 0;
    let provider_slug = client.provider_slug();

    // Phase 4c: ResponseId-policy providers chain reasoning continuity across
    // user turns via `previous_response_id`. The last response id is persisted
    // on the session config after every model call and restored here, so a new
    // user request continues the chain instead of resetting it. All other
    // policies reset to None — the id is meaningless outside Responses-style
    // APIs and must not leak across requests.
    let mut prev_resp_id = initial_prev_resp_id(session, provider_slug, model);
    let mut tool_results: Vec<ToolResultItem> = Vec::new();
    let mut known_hint_paths: Vec<PathBuf> = Vec::new();
    let mut pending_hints: Vec<String> = Vec::new();

    // Precondition guard (phase 4c): before sending a request whose passback
    // policy requires echoing reasoning, verify every tool-involving turn in
    // history carries its artifact. A turn recorded before the artifact was
    // captured (e.g. a pre-migration session) would otherwise produce a
    // mysterious 400 from the provider; surface it as a diagnosable warning.
    warn_on_missing_reasoning_artifacts(session, ctx.session_id, provider_slug, model);

    // Lazily cache discovered skills — they don't change during a session
    if session.discovered_skills.is_none()
        && let Some(ref wd) = session.config.working_dir
    {
        session.discovered_skills = Some(context::discover_skills(wd));
    }

    let mut turn_iter: u32 = 0;
    loop {
        // Enforce the iteration limit only when one is configured.
        // When `max_turns == 0` the loop is unbounded.
        if limited && turn_iter >= max_turns {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("tool loop exceeded {max_turns} iterations"),
            ));
        }
        debug!(
            session_id = ctx.session_id,
            turn = turn_iter,
            "agent loop turn"
        );
        let configured = session.config.reasoning_effort.as_deref().unwrap_or("off");
        let thinking_effort =
            resolve_reasoning_effort(client, model, ctx.session_id, turn_iter, configured);
        crate::metrics::record_turn(model);
        let tools = ctx
            .tool_registry
            .available_definitions(&session.config.active_tool_groups);
        if is_cancelled_once(cancel_rx) {
            return Ok(true);
        }

        // Start a new turn for this agent loop iteration.
        let turn_user_text = if turn_iter == 0 {
            user_text.clone()
        } else {
            None
        };
        let (current_turn_id, _) = session.start_turn(turn_user_text);
        broadcast_turn_appended(&ctx.cmd_tx, session, ctx.session_id, current_turn_id);
        if ctx
            .cmd_tx
            .send(SessionCommand::StatusChanged(SessionStatus::Inference))
            .is_err()
        {
            return Ok(false);
        }

        let system_content = {
            // Scope the immutable borrow on session so it ends before the
            // mutable borrows that follow (start_turn, set_assistant_response, etc.).
            let skills: &[SkillMeta] = session.discovered_skills.as_deref().unwrap_or_default();
            build_system_content(
                SystemContentParams {
                    working_dir: session.config.working_dir.as_deref(),
                    context_config: &session.config.context_config,
                    skills,
                    loaded_skill_bodies: &session.loaded_skill_bodies,
                    tool_registry: &ctx.tool_registry,
                    pending_hints: &pending_hints,
                    session_title: session.config.title.as_deref(),
                },
                &mut session.context_cache,
            )
        };
        pending_hints.clear();
        let messages =
            build_chat_request_messages(session, system_content.as_deref(), provider_slug, model);

        let (encoding, estimated_prompt_tokens) = estimate_prompt_tokens(model, &messages, &tools);

        let _ = ctx
            .cmd_tx
            .send(SessionCommand::Broadcast(DaemonMessage::Started {
                session_id: ctx.session_id,
                request_id,
                turn_id: current_turn_id,
                estimated_prompt_tokens,
            }));

        let mut retry_cb: Option<choreo_ai_protocols::openai::RetryCallback> = Some(Box::new({
            let cmd_tx = ctx.cmd_tx.clone();
            move |attempt, max_attempts, delay| {
                let _ = cmd_tx.send(SessionCommand::StatusChanged(SessionStatus::Retrying {
                    attempt,
                    max_attempts,
                    delay_ms: delay.as_millis() as u64,
                }));
            }
        }));

        // Running count of output tokens produced by the current turn.
        let mut output_token_count: u32 = 0;

        match client.chat_completion_turn_streaming(
            ChatTurnRequest {
                model,
                messages: &messages,
                tools: &tools,
                thinking_effort,
                on_retry: &mut retry_cb,
                cancel_rx: Some(cancel_rx),
                previous_response_id: prev_resp_id.as_deref(),
                tool_results: &tool_results,
                programmatic_tool_calling: client.supports_programmatic_tool_calling(model),
            },
            &mut |event| {
                match event {
                    StreamEvent::Answer(text) => {
                        if let Some(enc) = &encoding {
                            output_token_count += enc.count(&text) as u32;
                        }
                        let _ = ctx.cmd_tx.send(SessionCommand::Broadcast(
                            DaemonMessage::OutputChunk {
                                session_id: ctx.session_id,
                                request_id,
                                stream: OutputStream::Answer,
                                data: text.into_bytes(),
                            },
                        ));
                        // Let the UI update its live token display on every
                        // chunk so the count feels responsive.
                        let _ = ctx.cmd_tx.send(SessionCommand::Broadcast(
                            DaemonMessage::LiveOutputTokenCount {
                                session_id: ctx.session_id,
                                request_id,
                                output_tokens: output_token_count,
                            },
                        ));
                    }
                    StreamEvent::Reasoning(text) => {
                        if let Some(enc) = &encoding {
                            output_token_count += enc.count(&text) as u32;
                        }
                        let _ = ctx.cmd_tx.send(SessionCommand::Broadcast(
                            DaemonMessage::OutputChunk {
                                session_id: ctx.session_id,
                                request_id,
                                stream: OutputStream::Reasoning,
                                data: text.into_bytes(),
                            },
                        ));
                        let _ = ctx.cmd_tx.send(SessionCommand::Broadcast(
                            DaemonMessage::LiveOutputTokenCount {
                                session_id: ctx.session_id,
                                request_id,
                                output_tokens: output_token_count,
                            },
                        ));
                    }
                    // `StreamEvent` is #[non_exhaustive] — a future event kind
                    // this loop doesn't forward should be ignored, not crash
                    // the agent loop.
                    _ => {}
                }
                Ok(())
            },
        ) {
            Ok(ChatTurnResult::FinalText(final_text)) => {
                debug!(
                    session_id = ctx.session_id,
                    turn = turn_iter,
                    response_len = final_text.content.len(),
                    reasoning = final_text.reasoning.as_deref().unwrap_or_default(),
                    "model returned final text",
                );
                let token_usage = final_text.usage;
                accumulate_token_usage(session, &token_usage, turn_iter, ctx);
                broadcast_token_usage(ctx.session_id, ctx, session);
                // Write the reasoning artifact + producing model through to the
                // turn (phase 4c): the builder re-emits it on the next request
                // when the same model is still active and the passback policy
                // asks for it.
                // Record the producing provider+model once: it feeds both the
                // turn's provenance and the persisted response-id provenance.
                let producer = ReasoningProducer {
                    provider_slug: provider_slug.to_string(),
                    model: model.to_string(),
                };
                session.set_assistant_response(
                    current_turn_id,
                    AssistantResponse {
                        text: Some(final_text.content),
                        reasoning: final_text.reasoning,
                        token_usage,
                        reasoning_artifact: final_text.reasoning_artifact.clone(),
                        reasoning_producer: Some(producer.clone()),
                        ..Default::default()
                    },
                );
                // Persist the response id + its producing model so a
                // ResponseId-policy provider can chain the next user turn via
                // previous_response_id (restored at the top of the next loop
                // invocation only when the same provider+model is still
                // active — the id is service-bound and must not be replayed
                // into a different provider).
                session.config.last_response_id = final_text.response_id.clone();
                session.config.last_response_id_producer = Some(producer);
                finalize_and_broadcast_turn(session, ctx, current_turn_id)?;
                tool_results.clear();
                return Ok(false);
            }
            Ok(ChatTurnResult::ToolUse(tool_use)) => {
                let token_usage = tool_use.usage;
                accumulate_token_usage(session, &token_usage, turn_iter, ctx);
                broadcast_token_usage(ctx.session_id, ctx, session);
                // Build the call records once so the same ordered list seeds
                // both the assistant message's tool_calls and the placeholder
                // tool results (they must agree so the in-place updates below
                // match by call_id).
                let tool_call_records: Vec<AssistantToolCallRecord> = tool_use
                    .tool_calls
                    .iter()
                    .map(|tc| AssistantToolCallRecord {
                        call_id: tc.id.clone(),
                        name: tc.name.clone(),
                        arguments_json: tc.arguments_json.clone(),
                    })
                    .collect();
                // Record the producing provider+model once: it feeds both the
                // turn's provenance and the persisted response-id provenance.
                let producer = ReasoningProducer {
                    provider_slug: provider_slug.to_string(),
                    model: model.to_string(),
                };
                session.set_assistant_response(
                    current_turn_id,
                    AssistantResponse {
                        text: tool_use.content.clone(),
                        reasoning: tool_use.reasoning.clone(),
                        tool_calls: tool_call_records.clone(),
                        token_usage,
                        reasoning_artifact: tool_use.reasoning_artifact.clone(),
                        reasoning_producer: Some(producer.clone()),
                    },
                );
                // Seed one placeholder tool result per call, in the model's
                // call order, so the transcript renders every tool result in
                // that order at all times — each placeholder is filled in
                // place as its tool streams or finalizes.
                session.seed_tool_results(current_turn_id, &tool_call_records);
                broadcast_turn_appended(&ctx.cmd_tx, session, ctx.session_id, current_turn_id);
                // Store response_id for chaining tool results back to this
                // turn, and persist it (+ its producing model) on the session
                // config so ResponseId-policy providers can chain across user
                // turns (restored at the top of the next loop invocation only
                // when the same provider+model is still active).
                prev_resp_id = tool_use.response_id.clone();
                session.config.last_response_id = prev_resp_id.clone();
                session.config.last_response_id_producer = Some(producer);
                tool_results.clear();

                // Partition tool calls into serial and concurrent.
                // Session-config tools (load_tools, unload_tools,
                // set_working_dir) run serially even though they are now
                // registry tools: their mutations are applied by the session
                // main loop via daemon → session command routing, and serial
                // execution preserves the model's call order so e.g. a
                // load_tools followed by a set_working_dir lands in the
                // intended sequence.
                let (mutators, concurrent): (Vec<_>, Vec<_>) = tool_use
                    .tool_calls
                    .into_iter()
                    .partition(|tc| is_session_config_tool(&tc.name));

                // All session-config tools in this response resolve relative
                // paths against the working directory in effect when the
                // response was planned. Capture it once so the (rare) Phase 3
                // mirror fallback reproduces exactly the canonical paths the
                // tools sent to the main loop (which applies them verbatim, in
                // call order).
                let turn_base_working_dir = session.config.working_dir.clone();

                // Successful session-config mutations, in call order, to be
                // mirrored onto this worker's config copy once every tool in
                // the response has executed (see Phase 3 below).
                let mut pending_config_changes: Vec<PendingConfigChange> = Vec::new();

                // Sticky cancellation: a cancel observed during Phase 1 or
                // Phase 2 stops the request, but only AFTER Phase 3 has
                // mirrored the config changes from the tools that already
                // ran — the same ordering the no-cancel path uses.
                let mut cancelled = false;

                // call_ids whose results were actually recorded, so a
                // cancelled request can mark the never-executed placeholders
                // (see `SessionState::mark_unexecuted_tool_results`).
                let mut executed_tool_calls: HashSet<String> = HashSet::new();

                // ── Phase 1: Session-config tools (serial) ────────
                for tool_call in mutators.into_iter() {
                    if is_cancelled_once(cancel_rx) {
                        cancelled = true;
                        break;
                    }

                    if let Err(e) =
                        ctx.cmd_tx
                            .send(SessionCommand::Broadcast(DaemonMessage::ToolCallStarted {
                                session_id: ctx.session_id,
                                request_id,
                                call_id: tool_call.id.clone(),
                                tool_name: tool_call.name.clone(),
                                arguments_json: tool_call.arguments_json.clone(),
                            }))
                    {
                        warn!(%request_id, call_id = %tool_call.id, error = %e, "failed to broadcast ToolCallStarted");
                    }

                    let tool_timeout =
                        determine_tool_timeout(&tool_call.name).unwrap_or(Duration::from_secs(60));

                    if ctx
                        .cmd_tx
                        .send(SessionCommand::StatusChanged(SessionStatus::ToolCall(
                            tool_call.name.clone(),
                        )))
                        .is_err()
                    {
                        return Ok(false);
                    }

                    debug!(
                        session_id = ctx.session_id,
                        turn = turn_iter,
                        tool_name = %tool_call.name,
                        tool_call_id = %tool_call.id,
                        args_preview = %(&tool_call.arguments_json[..tool_call.arguments_json.len().min(200)]),
                        "executing tool (serial)",
                    );

                    // The invocation description is generated up front (as in
                    // the concurrent path) so the serial error/panic outputs
                    // carry it too — a timed-out or cancelled tool renders with
                    // the same invocation context the concurrent path shows.
                    let invocation_description = ctx.tool_registry.describe_invocation(&tool_call);
                    let turn_working_dir = session.config.working_dir.clone();
                    let (mut output, tool_cancelled, image) = execute_tool_with_timeout(
                        &tool_call,
                        None,
                        turn_working_dir.as_deref(),
                        tool_timeout,
                        request_id,
                        ctx.session_id,
                        session,
                        cancel_rx,
                        ctx,
                        &invocation_description,
                    );
                    if tool_cancelled {
                        // The wait observed a cancellation signal (consumed by
                        // its `select!`), so the request must stop after this
                        // tool's result is recorded below.
                        cancelled = true;
                    }

                    record_tool_completion(
                        request_id,
                        session,
                        &tool_call,
                        &mut output,
                        image,
                        ctx,
                        current_turn_id,
                        &mut tool_results,
                        &mut known_hint_paths,
                        &mut pending_hints,
                    );
                    executed_tool_calls.insert(tool_call.id.clone());

                    // Only mirror mutations that were actually accepted: an
                    // error (e.g. inactive session, daemon communication
                    // failure) means the authoritative state was NOT changed,
                    // so this worker must not pretend it was.
                    if !output.is_error
                        && let Some(change) = pending_config_change(
                            &tool_call,
                            &output,
                            turn_base_working_dir.as_deref(),
                        )
                    {
                        pending_config_changes.push(change);
                    }

                    if cancelled {
                        // Stop executing further serial tools; the concurrent
                        // batch is skipped and Phase 3 still runs below.
                        break;
                    }
                }

                // ── Phase 2: All remaining tools (concurrent) ───────
                if !cancelled && !concurrent.is_empty() {
                    for tc in concurrent.iter() {
                        if let Err(e) = ctx.cmd_tx.send(SessionCommand::Broadcast(
                            DaemonMessage::ToolCallStarted {
                                session_id: ctx.session_id,
                                request_id,
                                call_id: tc.id.clone(),
                                tool_name: tc.name.clone(),
                                arguments_json: tc.arguments_json.clone(),
                            },
                        )) {
                            warn!(%request_id, call_id = %tc.id, error = %e, "failed to broadcast ToolCallStarted");
                        }
                    }

                    if ctx
                        .cmd_tx
                        .send(SessionCommand::StatusChanged(SessionStatus::ToolCall(
                            concurrent_tool_status_label(&concurrent),
                        )))
                        .is_err()
                    {
                        return Ok(false);
                    }

                    debug!(
                        session_id = ctx.session_id,
                        turn = turn_iter,
                        count = concurrent.len(),
                        "dispatching {} tools concurrently",
                        concurrent.len(),
                    );

                    let cancel_flag = Arc::new(AtomicBool::new(false));

                    let tool_ctx = ToolContext {
                        session_id: ctx.session_id,
                        db: Arc::clone(&ctx.db),
                        daemon_tx: ctx.daemon_tx.clone(),
                        active_tool_groups: session.config.active_tool_groups.clone(),
                        reasoning_effort: session.config.reasoning_effort.clone(),
                        selected_model: session.config.selected_model.clone(),
                        working_dir: session.config.working_dir.clone(),
                        cancelled: Arc::clone(&cancel_flag),
                        account_name: session.config.account_name.clone(),
                    };

                    let cmd_tx = ctx.cmd_tx.clone();
                    let reg = Arc::clone(&ctx.tool_registry);

                    // Shared batch channel: every wait-loop thread delivers its
                    // final ToolHandle here the moment the tool completes
                    // (success, error, timeout, or panic). No joins — results
                    // arrive in *completion* order, so a fast tool broadcasts
                    // immediately instead of waiting for the slowest tool the
                    // model listed before it.
                    let (batch_tx, batch_rx) = crossbeam_channel::unbounded::<ToolHandle>();

                    // Dispatch-order metadata for every call, retained for the
                    // (rare) fallback synthesis below: rebuilding the results
                    // of wait-loop threads that died before delivering.
                    let mut call_infos: Vec<CallInfo> = Vec::with_capacity(concurrent.len());
                    for tool_call in concurrent.into_iter() {
                        let timeout = determine_tool_timeout(&tool_call.name);
                        let invocation_description = reg.describe_invocation(&tool_call);
                        // One dispatch-time instant for both the handle (delivered
                        // path) and the CallInfo (panic-synthesis path), so the
                        // collector's per-tool elapsed log is consistent either way.
                        let started_at = Instant::now();
                        let call_id = tool_call.id.clone();
                        let tool_name = tool_call.name.clone();
                        let arguments_json = tool_call.arguments_json.clone();
                        // A call counts as executed only once its result is
                        // actually recorded (`process_tool_handle`): the drain
                        // below can stop on a cancel, so a dispatched but
                        // unfinished call must still be swept as unexecuted.
                        // The spawn returns the collector-side kill sender;
                        // it is retained in the CallInfo for the whole batch
                        // drain so a mid-batch cancel can stop every still-
                        // running wait-loop promptly.
                        let kill_tx = spawn_single_tool(SpawnToolArgs {
                            tool_call,
                            timeout,
                            request_id,
                            session_id: ctx.session_id,
                            registry: Arc::clone(&reg),
                            cmd_tx: cmd_tx.clone(),
                            x_credentials: None,
                            working_dir: session.config.working_dir.clone(),
                            ctx: tool_ctx.clone(),
                            invocation_description: invocation_description.clone(),
                            started_at,
                            result_tx: batch_tx.clone(),
                        });
                        call_infos.push(CallInfo {
                            call_id,
                            tool_name,
                            arguments_json,
                            invocation_description,
                            started_at,
                            kill_tx,
                        });
                    }
                    // Drop our own sender: the batch channel disconnects only
                    // when every wait-loop thread has finished (sent or died),
                    // which is the receive loop's completion signal.
                    drop(batch_tx);

                    let batch_size = call_infos.len();

                    // Per-tool completion processing: broadcast the result the
                    // moment it arrives and accumulate it for the next model
                    // call. Extracted into a closure so the happy path and the
                    // panic-synthesis fallback share one implementation.
                    let mut process_tool_handle =
                        |ToolHandle {
                             tool_call,
                             mut output,
                             image,
                             started_at,
                         }: ToolHandle| {
                            let elapsed = started_at.elapsed();

                            debug!(
                                session_id = ctx.session_id,
                                turn = turn_iter,
                                tool_name = %tool_call.name,
                                elapsed_ms = elapsed.as_millis(),
                                result_len = output.content.len(),
                                is_error = output.is_error,
                                "tool finished (concurrent)",
                            );

                            record_tool_completion(
                                request_id,
                                session,
                                &tool_call,
                                &mut output,
                                image,
                                ctx,
                                current_turn_id,
                                &mut tool_results,
                                &mut known_hint_paths,
                                &mut pending_hints,
                            );
                            // The result is recorded now — this call_id must
                            // not be swept by the cancelled-turn placeholder
                            // sweep (`mark_unexecuted_tool_results`).
                            executed_tool_calls.insert(tool_call.id.clone());
                        };

                    // Which call_ids actually delivered, so the disconnected-
                    // channel fallback below synthesizes only the genuinely
                    // missing tools (handles arrive in completion order, NOT
                    // dispatch order).
                    let mut delivered: HashSet<String> = HashSet::with_capacity(batch_size);
                    while delivered.len() < batch_size {
                        // Block until a tool completes OR the request is
                        // cancelled.  `select_biased!` (cancel arm first) makes
                        // both waits event-driven: a cancel wakes this loop the
                        // instant it is sent, and a quiet batch costs nothing
                        // (no 200 ms ticks).  The bias is a preference, not a
                        // guarantee: an already-queued cancel is selected
                        // deterministically (the biased fast path scans arms in
                        // order), while a cancel that lands mid-block only
                        // *tends* to beat a simultaneously-ready result.  Both
                        // outcomes are handled correctly — a cancel always
                        // stops the batch, and any result queued at that
                        // instant is drained rather than discarded.  The cancel
                        // sender cannot disconnect while the worker runs (it
                        // is dropped only on RequestFinished), so a firing
                        // cancel arm always means "cancel".
                        let (cancelled_now, handle_msg) = crossbeam_channel::select_biased! {
                            recv(cancel_rx) -> _ => (true, None),
                            recv(batch_rx) -> msg => (false, Some(msg)),
                        };
                        if cancelled_now {
                            cancel_flag.store(true, Ordering::Relaxed);
                            cancelled = true;
                            // Bias for cancel: stop waiting for the slowest
                            // tool right now.  First, kill every still-running
                            // wait-loop so its forwarder stops streaming
                            // promptly, its cooperative
                            // `ToolContext.cancelled` flag is set, and it
                            // delivers a "cancelled" result instead of waiting
                            // for the tool (sends to wait-loops that already
                            // exited fail silently).  Then don't discard
                            // results that already landed in the same instant
                            // — drain them (non-blocking) so the transcript
                            // keeps the real output of tools that did
                            // complete.
                            for info in &call_infos {
                                let _ = info.kill_tx.send(());
                            }
                            while let Ok(handle) = batch_rx.try_recv() {
                                delivered.insert(handle.tool_call.id.clone());
                                process_tool_handle(handle);
                            }
                            // Every live wait-loop selects on its kill channel,
                            // so after the kill broadcast each one delivers its
                            // outcome (a real result that won the same-instant
                            // race, or a "cancelled" result) promptly.  Keep
                            // draining until all batch_size handles have
                            // arrived: this makes the transcript deterministic
                            // — a killed wait-loop's "cancelled" handle can no
                            // longer be dropped by a race with the placeholder
                            // sweep, so no unfinished call is swept that
                            // actually delivered an outcome.  The wait is
                            // bounded by thread scheduling, not by the slowest
                            // tool (its execution thread keeps running in the
                            // background either way, and its late result is
                            // discarded once the wait-loop exits).  A
                            // disconnect means a wait-loop thread died before
                            // delivering — synthesize its result, matching the
                            // normal batch-end path below.
                            while delivered.len() < batch_size {
                                match batch_rx.recv() {
                                    Ok(handle) => {
                                        delivered.insert(handle.tool_call.id.clone());
                                        process_tool_handle(handle);
                                    }
                                    Err(_) => {
                                        warn!(
                                            session_id = ctx.session_id,
                                            request_id,
                                            delivered = delivered.len(),
                                            expected = batch_size,
                                            "concurrent tool batch ended early after cancel; synthesizing missing tool results",
                                        );
                                        for info in missing_calls(&call_infos, &delivered) {
                                            process_tool_handle(panic_tool_handle(info));
                                        }
                                        break;
                                    }
                                }
                            }
                            break;
                        }
                        if let Some(msg) = handle_msg {
                            match msg {
                                Ok(handle) => {
                                    delivered.insert(handle.tool_call.id.clone());
                                    process_tool_handle(handle);
                                }
                                Err(_) => {
                                    // Every wait-loop thread has exited but fewer
                                    // handles arrived than expected: some thread
                                    // panicked before sending. Synthesize the same
                                    // "tool thread panicked" output the old
                                    // join-based path produced, for the missing
                                    // slots only (by call_id), so the turn still
                                    // records a result for every call.
                                    warn!(
                                        session_id = ctx.session_id,
                                        request_id,
                                        delivered = delivered.len(),
                                        expected = batch_size,
                                        "concurrent tool batch ended early; synthesizing missing tool results",
                                    );
                                    for info in missing_calls(&call_infos, &delivered) {
                                        process_tool_handle(panic_tool_handle(info));
                                    }
                                    break;
                                }
                            }
                        }
                    }

                    // ── Phase 2b: Normalize the next-call accumulator ──
                    //
                    // The receive loop above processed results in completion
                    // order so each broadcast hit the TUI the moment its tool
                    // finished (and streaming chunks flow even earlier). The
                    // turn's own tool_results never need re-ordering: they
                    // were seeded in call order before execution and updated
                    // in place by call_id, so the transcript is always in the
                    // model's order. The accumulator sent to the provider on
                    // the next agent-loop iteration should mirror the
                    // assistant message's tool_calls array, so re-sort it now
                    // the batch is complete — reusing `tool_call_records` (the
                    // same ordered list that seeded the placeholders and the
                    // assistant message) instead of re-reading the turn.
                    sort_by_call_order(&tool_call_records, &mut tool_results, |r| {
                        r.call_id.as_str()
                    });
                }

                // ── Phase 3: Mirror session-config changes onto the
                //    worker's config copy ────────────────────────────
                //
                // The authoritative mutations were applied by the session
                // main loop. The worker's throwaway copy must be updated too,
                // or the next loop iteration would keep building tool
                // definitions, system content, and file ops from the stale
                // pre-change state. This runs only after every tool in the
                // response has executed: the model planned all of them
                // against the state at the start of the turn (they are a
                // parallel batch), so applying the change earlier — e.g.
                // right after Phase 1 — would silently alter the semantics
                // of tools batched alongside the config change. The worker
                // copy is still discarded at request end, so the two copies
                // cannot drift across requests.
                for change in &pending_config_changes {
                    apply_pending_config_change(session, change);
                }

                // A cancel observed during tool execution stops the request
                // here — after Phase 3 has mirrored the config changes from
                // the tools that already ran, matching the no-cancel ordering.
                if cancelled {
                    // Tools that never ran still hold empty seeded placeholders;
                    // mark them so the transcript and the next provider request
                    // don't carry empty tool messages for calls that were never
                    // executed (the cancelled turn is not finalized, so it
                    // survives into the next request's history).
                    session.mark_unexecuted_tool_results(current_turn_id, &executed_tool_calls);
                    broadcast_turn_appended(&ctx.cmd_tx, session, ctx.session_id, current_turn_id);
                    return Ok(true);
                }
            }
            Ok(_) => {
                // A new ChatTurnResult variant (this enum is #[non_exhaustive])
                // is not handled here — fail loudly rather than silently
                // treating unknown output as success.
                warn!("provider returned an unhandled ChatTurnResult variant");
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "provider returned an unhandled turn result variant",
                ));
            }
            Err(choreo_proto::InferenceError::Cancelled) => {
                return Ok(true);
            }
            Err(e) => {
                // Finalize the turn so the session doesn't have an orphaned
                // open turn that confuses the LLM on the next request.
                if matches!(&e, choreo_proto::InferenceError::TruncatedToolCall { .. }) {
                    tracing::warn!(?e, "truncated tool call, finalizing turn gracefully");
                    session.set_assistant_response(
                        current_turn_id,
                        AssistantResponse {
                            text: Some(format!("[tool call truncated: {e}]")),
                            // No artifact or producer: the model never completed a
                            // response, so there is nothing to replay. Everything
                            // else (tool_calls, usage) stays at its default.
                            ..Default::default()
                        },
                    );
                    finalize_and_broadcast_turn(session, ctx, current_turn_id)?;
                    tool_results.clear();
                    return Ok(false);
                }
                return Err(e.into());
            }
        }

        // Advance the turn counter for the next iteration.
        turn_iter += 1;
    }
}

/// Persist the finalized turn to the database and broadcast the
/// [`DaemonMessage::TurnFinalized`] event to all connected clients.
fn finalize_and_broadcast_turn(
    session: &mut SessionState,
    ctx: &RequestContext,
    current_turn_id: u32,
) -> io::Result<()> {
    session.finalize_turn(&ctx.db, ctx.session_id, current_turn_id)?;
    if let Some(turn) = session.turns.get(&current_turn_id) {
        let _ = ctx
            .cmd_tx
            .send(SessionCommand::Broadcast(DaemonMessage::TurnFinalized {
                session_id: ctx.session_id,
                turn_id: current_turn_id,
                turn: turn.clone(),
            }));
    }
    Ok(())
}

fn finish_tool_call(
    request_id: u32,
    session: &mut SessionState,
    tool_call: &ChatToolCall,
    output: &mut ToolOutput,
    ctx: &RequestContext,
    turn_id: u32,
) {
    let is_error = output.is_error;
    let content = output.content.clone();
    let invocation_description = output.invocation_description.clone();

    session.update_tool_result(
        turn_id,
        &tool_call.id,
        tool_call.name.clone(),
        content.clone(),
        is_error,
        invocation_description,
    );

    broadcast_turn_appended(&ctx.cmd_tx, session, ctx.session_id, turn_id);

    let event = if is_error {
        DaemonMessage::ToolCallFailed {
            session_id: ctx.session_id,
            request_id,
            call_id: tool_call.id.clone(),
            tool_name: tool_call.name.clone(),
            error: content,
        }
    } else {
        DaemonMessage::ToolCallFinished {
            session_id: ctx.session_id,
            request_id,
            call_id: tool_call.id.clone(),
            tool_name: tool_call.name.clone(),
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
fn record_tool_completion(
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
fn tool_result_from_channel(
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
fn drain_queued_or_synthesize(
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
fn execute_tool_with_timeout(
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

/// Whether a turn is part of a tool loop: the assistant issued tool calls
/// (its message must echo reasoning on the next request for several
/// providers) or tool results are attached (it is mid-loop).
fn turn_has_tool_involvement(turn: &Turn) -> bool {
    !turn.tool_calls.is_empty() || !turn.tool_results.is_empty()
}

/// Build the provider request messages for a session, applying the reasoning
/// passback policy (phase 4b): the opaque artifact captured on a previous
/// turn is replayed only when BOTH gates pass — same-model provenance (the
/// artifact is model-bound; a turn produced by a different model must not
/// have its payload replayed) and the provider's `reasoning_passback` policy
/// for this request (`ToolLoop` → tool-involving turns only, `AllTurns` →
/// every turn, `Signature` → every turn, `ResponseId`/`None` → never via the
/// message).
///
/// Public for the daemon integration tests (the `test-utils` feature); the
/// production caller is `run_agent_loop`.
pub fn build_chat_request_messages(
    session: &SessionState,
    system_prompt: Option<&str>,
    provider_slug: &str,
    model: &str,
) -> Vec<ChatRequestMessage> {
    let mut messages = Vec::new();

    // Prepend system prompt and context if provided.
    if let Some(prompt) = system_prompt {
        messages.push(ChatRequestMessage::simple("system", prompt.to_string()));
    }

    // The passback policy decides *whether* the artifact is replayed; it is
    // constant for the whole request (the model does not change mid-loop).
    let passback = model_reasoning_passback(provider_slug, model);

    for turn in session.turns.values() {
        if turn.undone {
            continue;
        }
        // User message
        if let Some(text) = &turn.user_text {
            messages.push(ChatRequestMessage::simple("user", text.clone()));
        }
        // Assistant message (text or tool calls).
        //
        // Reasoning round-trip (phase 4b): the artifact is replayed only when
        // BOTH gates pass — (1) same-model provenance (artifacts are
        // model-bound; a turn produced by a different model must not have its
        // encrypted payload replayed into this request, matching pi's
        // isSameModel and Anthropic's strip-on-model-change rule) and (2) the
        // provider's passback policy for this request:
        //   ToolLoop  → only tool-involving turns (assistant tool_calls or
        //               tool results attached) — DeepSeek/Kimi reject a tool
        //               loop whose assistant message drops reasoning_content
        //   AllTurns  → every turn (Anthropic keep-all)
        //   Signature → every turn (Gemini encrypted thought signatures)
        //   ResponseId→ never via the message; continuity flows through
        //               previous_response_id (handled in the agent loop)
        //   None      → display-only providers, never replay
        // The three legacy string fields (reasoning_content/reasoning/
        // reasoning_text) stay None: the adapter re-emits the artifact in its
        // own wire format (phase 4a), so the daemon never interprets it.
        let same_model = turn.reasoning_producer.as_ref()
            == Some(&ReasoningProducer {
                provider_slug: provider_slug.to_string(),
                model: model.to_string(),
            });
        let include_artifact = same_model
            && match passback {
                ReasoningPassback::None => false,
                ReasoningPassback::ToolLoop => turn_has_tool_involvement(turn),
                ReasoningPassback::AllTurns => true,
                ReasoningPassback::Signature => true,
                ReasoningPassback::ResponseId => false,
            };
        let has_tool_calls = !turn.tool_calls.is_empty();
        if turn.assistant_text.is_some() || has_tool_calls {
            let tool_calls = if has_tool_calls {
                Some(
                    turn.tool_calls
                        .iter()
                        .map(|tc| AssistantToolCall {
                            id: tc.call_id.clone(),
                            kind: "function".to_string(),
                            function: AssistantToolFunction {
                                name: tc.name.clone(),
                                arguments: tc.arguments_json.clone(),
                            },
                        })
                        .collect(),
                )
            } else {
                None
            };
            messages.push(ChatRequestMessage {
                role: "assistant",
                content: turn.assistant_text.clone(),
                tool_call_id: None,
                tool_calls,
                reasoning_content: None,
                reasoning: None,
                reasoning_text: None,
                reasoning_artifact: include_artifact
                    .then(|| turn.reasoning_artifact.clone())
                    .flatten(),
            });
        }
        // Tool result messages
        for tr in &turn.tool_results {
            messages.push(ChatRequestMessage {
                role: "tool",
                content: Some(tr.content.clone()),
                tool_call_id: Some(tr.call_id.clone()),
                tool_calls: None,
                reasoning_content: None,
                reasoning: None,
                reasoning_text: None,
                reasoning_artifact: None,
            });
        }
    }
    messages
}

pub const REQUEST_IMAGE_BYTES: &[u8] = include_bytes!("../assets/dua.jpg");
pub const REQUEST_IMAGE_MIME_TYPE: &str = "image/jpeg";
pub const REQUEST_IMAGE_WIDTH: u32 = 640;
pub const REQUEST_IMAGE_HEIGHT: u32 = 640;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::DaemonCommand;
    use crate::providers::InferenceProvider;
    use crate::providers::test_util::make_test_provider;
    use crate::tools::context::ToolContext;
    use crate::tools::{Tool, ToolExecError, ToolRegistry};
    use choreo_ai_protocols::openai::AssistantToolCall;
    use choreo_proto::ChatReasoningField;
    use std::sync::mpsc;

    fn make_session_with_turns() -> SessionState {
        let mut session = SessionState::empty();
        let (tid0, _) = session.start_turn(Some("hello".into()));
        session.set_assistant_response(
            tid0,
            AssistantResponse {
                text: Some("hi".into()),
                ..Default::default()
            },
        );
        session
    }

    // Neutral provider/model for structure-only tests: the slug is not in the
    // catalog, so the passback policy resolves to None and no artifact is ever
    // attached — the exact behavior those tests assert.
    const TEST_PROVIDER: &str = "test-stub";
    const TEST_MODEL: &str = "test-model";

    #[test]
    fn build_chat_request_messages_empty() {
        let session = SessionState::empty();
        let result = build_chat_request_messages(&session, None, TEST_PROVIDER, TEST_MODEL);
        assert!(result.is_empty());
    }

    #[test]
    fn build_chat_request_messages_with_system_prompt() {
        let session = SessionState::empty();
        let result =
            build_chat_request_messages(&session, Some("system prompt"), TEST_PROVIDER, TEST_MODEL);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].role, "system");
        assert_eq!(result[0].content.as_deref(), Some("system prompt"));
    }

    #[test]
    fn build_chat_request_messages_user_and_assistant() {
        let session = make_session_with_turns();
        let result = build_chat_request_messages(&session, None, TEST_PROVIDER, TEST_MODEL);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].role, "user");
        assert_eq!(result[0].content.as_deref(), Some("hello"));
        assert_eq!(result[1].role, "assistant");
        assert_eq!(result[1].content.as_deref(), Some("hi"));
    }

    #[test]
    fn build_chat_request_messages_with_tool_calls() {
        let mut session = SessionState::empty();
        let (tid, _) = session.start_turn(Some("list files".into()));
        let records = vec![AssistantToolCallRecord {
            call_id: "call_1".into(),
            name: "ls".into(),
            arguments_json: r#"{"path": "."}"#.into(),
        }];
        session.set_assistant_response(
            tid,
            AssistantResponse {
                text: Some("thinking".into()),
                tool_calls: records.clone(),
                ..Default::default()
            },
        );
        // Placeholder results are seeded in call order; the finished tool
        // updates its slot in place.
        session.seed_tool_results(tid, &records);
        session.update_tool_result(
            tid,
            "call_1",
            "ls".into(),
            "file.txt".into(),
            false,
            String::new(),
        );

        let result = build_chat_request_messages(&session, None, TEST_PROVIDER, TEST_MODEL);
        assert_eq!(result.len(), 3);
        assert_eq!(result[0].role, "user");
        assert_eq!(result[1].role, "assistant");
        assert!(result[1].tool_calls.is_some());
        assert_eq!(result[2].role, "tool");
        assert_eq!(result[2].tool_call_id.as_deref(), Some("call_1"));
    }

    #[test]
    fn build_chat_request_messages_skips_undone_turns() {
        let mut session = SessionState::empty();
        let (tid0, _) = session.start_turn(Some("visible".into()));
        session.set_assistant_response(
            tid0,
            AssistantResponse {
                text: Some("ok".into()),
                ..Default::default()
            },
        );
        let (tid1, _) = session.start_turn(Some("hidden".into()));
        session.set_assistant_response(
            tid1,
            AssistantResponse {
                text: Some("nope".into()),
                ..Default::default()
            },
        );
        if let Some(turn) = session.turns.get_mut(&tid1) {
            turn.undone = true;
        }

        let result = build_chat_request_messages(&session, None, TEST_PROVIDER, TEST_MODEL);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].role, "user");
        assert_eq!(result[0].content.as_deref(), Some("visible"));
    }

    // -- Reasoning passback builder policy (phase 4b) -----------------------

    fn tool_call_record(call_id: &str, name: &str) -> AssistantToolCallRecord {
        AssistantToolCallRecord {
            call_id: call_id.into(),
            name: name.into(),
            arguments_json: "{}".into(),
        }
    }

    fn deepseek_producer() -> ReasoningProducer {
        ReasoningProducer {
            provider_slug: "deepseek".into(),
            model: "deepseek-v4-pro".into(),
        }
    }

    fn artifact(bytes: &[u8]) -> ReasoningArtifact {
        ReasoningArtifact::ChatReasoning {
            field: ChatReasoningField::ReasoningContent,
            bytes: bytes.to_vec(),
        }
    }

    /// Add a turn with an optional artifact/producer and optional assistant
    /// tool calls, returning its turn_id.
    fn add_turn(
        session: &mut SessionState,
        user_text: &str,
        assistant_text: &str,
        artifact: Option<ReasoningArtifact>,
        producer: Option<ReasoningProducer>,
        tool_calls: Vec<AssistantToolCallRecord>,
    ) -> u32 {
        let (tid, _) = session.start_turn(Some(user_text.to_string()));
        session.set_assistant_response(
            tid,
            AssistantResponse {
                text: Some(assistant_text.to_string()),
                tool_calls,
                reasoning_artifact: artifact,
                reasoning_producer: producer,
                ..Default::default()
            },
        );
        tid
    }

    fn assistant_messages(result: &[ChatRequestMessage]) -> Vec<&ChatRequestMessage> {
        result.iter().filter(|m| m.role == "assistant").collect()
    }

    #[test]
    fn builder_tool_loop_attaches_artifact_only_for_tool_involving_turns() {
        let mut session = SessionState::empty();
        // Plain text turn (no tool involvement) with an artifact: must NOT be
        // replayed under ToolLoop (DeepSeek/Kimi only require it on tool-loop
        // messages).
        add_turn(
            &mut session,
            "hello",
            "hi",
            Some(artifact(b"plain")),
            Some(deepseek_producer()),
            vec![],
        );
        // Tool-call turn with an artifact: must be replayed.
        add_turn(
            &mut session,
            "list files",
            "thinking...",
            Some(artifact(b"tool-thinking")),
            Some(deepseek_producer()),
            vec![tool_call_record("call_1", "ls")],
        );

        // deepseek-v4-pro carries an explicit `tool_loop` passback override.
        let result = build_chat_request_messages(&session, None, "deepseek", "deepseek-v4-pro");
        let assistants = assistant_messages(&result);
        assert_eq!(assistants.len(), 2);
        assert_eq!(
            assistants[0].reasoning_artifact, None,
            "plain text turn must not replay reasoning under ToolLoop",
        );
        assert_eq!(
            assistants[1].reasoning_artifact,
            Some(artifact(b"tool-thinking")),
            "tool-call turn must replay its artifact under ToolLoop",
        );
    }

    #[test]
    fn builder_tool_loop_attaches_artifact_for_tool_result_turns() {
        let mut session = SessionState::empty();
        // A turn carrying only tool RESULTS (no assistant tool_calls on the
        // message, e.g. a mid-loop state persisted under an earlier request)
        // is tool-involving too: the next request must still echo it.
        let tid = add_turn(
            &mut session,
            "run it",
            "running",
            Some(artifact(b"mid-loop")),
            Some(deepseek_producer()),
            vec![],
        );
        session
            .turns
            .get_mut(&tid)
            .expect("turn exists")
            .tool_results
            .push(choreo_proto::ToolResultRecord {
                call_id: "call_1".into(),
                name: "sh".into(),
                content: "ok".into(),
                is_error: false,
                invocation_description: String::new(),
            });

        let result = build_chat_request_messages(&session, None, "deepseek", "deepseek-v4-pro");
        let assistants = assistant_messages(&result);
        assert_eq!(assistants.len(), 1);
        assert_eq!(
            assistants[0].reasoning_artifact,
            Some(artifact(b"mid-loop"))
        );
    }

    #[test]
    fn builder_all_turns_attaches_always() {
        let mut session = SessionState::empty();
        // Unknown model under the anthropic slug → protocol default AllTurns
        // (no explicit TOML override, unlike claude-sonnet-4-5 which is a
        // last-turn-only `tool_loop` model).
        let producer = ReasoningProducer {
            provider_slug: "anthropic".into(),
            model: "claude-unknown-model".into(),
        };
        add_turn(
            &mut session,
            "hello",
            "hi",
            Some(artifact(b"one")),
            Some(producer.clone()),
            vec![],
        );
        add_turn(
            &mut session,
            "again",
            "bye",
            Some(artifact(b"two")),
            Some(producer),
            vec![],
        );

        // Anthropic → AllTurns: every assistant message replays its artifact,
        // even non-tool turns.
        let result =
            build_chat_request_messages(&session, None, "anthropic", "claude-unknown-model");
        let assistants = assistant_messages(&result);
        assert_eq!(assistants.len(), 2);
        assert_eq!(assistants[0].reasoning_artifact, Some(artifact(b"one")));
        assert_eq!(assistants[1].reasoning_artifact, Some(artifact(b"two")));
    }

    #[test]
    fn builder_signature_policy_attaches_always() {
        let mut session = SessionState::empty();
        add_turn(
            &mut session,
            "hello",
            "hi",
            Some(ReasoningArtifact::GoogleSignatures(b"sig".to_vec())),
            Some(ReasoningProducer {
                provider_slug: "google".into(),
                model: "gemini-2.5-pro".into(),
            }),
            vec![],
        );

        // Google → Signature: every assistant message replays the signatures.
        let result = build_chat_request_messages(&session, None, "google", "gemini-2.5-pro");
        let assistants = assistant_messages(&result);
        assert_eq!(assistants.len(), 1);
        assert_eq!(
            assistants[0].reasoning_artifact,
            Some(ReasoningArtifact::GoogleSignatures(b"sig".to_vec())),
        );
    }

    #[test]
    fn builder_none_never_attaches() {
        let mut session = SessionState::empty();
        // A tool-involving turn WITH an artifact under a None-policy provider:
        // the artifact must never be replayed (display-only provider).
        add_turn(
            &mut session,
            "list",
            "thinking",
            Some(artifact(b"payload")),
            Some(ReasoningProducer {
                provider_slug: "unknown-provider".into(),
                model: "m".into(),
            }),
            vec![tool_call_record("call_1", "ls")],
        );

        let result = build_chat_request_messages(&session, None, "unknown-provider", "m");
        let assistants = assistant_messages(&result);
        assert_eq!(assistants.len(), 1);
        assert_eq!(assistants[0].reasoning_artifact, None);
    }

    #[test]
    fn builder_response_id_policy_never_attaches_via_message() {
        let mut session = SessionState::empty();
        add_turn(
            &mut session,
            "list",
            "thinking",
            Some(artifact(b"payload")),
            Some(ReasoningProducer {
                provider_slug: "openai".into(),
                model: "gpt-4".into(),
            }),
            vec![tool_call_record("call_1", "ls")],
        );

        // gpt-4 is a Responses model → ResponseId policy: continuity flows via
        // previous_response_id, so the message must NOT carry the artifact.
        let result = build_chat_request_messages(&session, None, "openai", "gpt-4");
        let assistants = assistant_messages(&result);
        assert_eq!(assistants.len(), 1);
        assert_eq!(assistants[0].reasoning_artifact, None);
    }

    #[test]
    fn builder_same_model_mismatch_drops_artifact() {
        let mut session = SessionState::empty();
        // Current-model turn (deepseek): artifact kept.
        add_turn(
            &mut session,
            "list",
            "thinking",
            Some(artifact(b"kept")),
            Some(deepseek_producer()),
            vec![tool_call_record("call_1", "ls")],
        );
        // Turn produced by a DIFFERENT model mid-session (e.g. the user
        // switched deepseek → claude): the artifact is model-bound and must be
        // dropped even though the turn is tool-involving (replaying an
        // encrypted ChatReasoning payload into an Anthropic request — or a
        // stale deepseek payload after switching back — would corrupt the
        // conversation).
        add_turn(
            &mut session,
            "old model turn",
            "old thinking",
            Some(artifact(b"dropped")),
            Some(ReasoningProducer {
                provider_slug: "anthropic".into(),
                model: "claude-sonnet-4-5".into(),
            }),
            vec![tool_call_record("call_2", "grep")],
        );

        let result = build_chat_request_messages(&session, None, "deepseek", "deepseek-v4-pro");
        let assistants = assistant_messages(&result);
        assert_eq!(assistants.len(), 2);
        assert_eq!(assistants[0].reasoning_artifact, Some(artifact(b"kept")));
        assert_eq!(
            assistants[1].reasoning_artifact, None,
            "artifact from a previous model must be dropped",
        );
    }

    #[test]
    fn builder_undone_turn_artifact_is_skipped() {
        let mut session = SessionState::empty();
        // Visible, tool-involving turn: its artifact must be replayed under
        // ToolLoop.
        add_turn(
            &mut session,
            "visible",
            "ok",
            Some(artifact(b"kept")),
            Some(deepseek_producer()),
            vec![tool_call_record("call_0", "pwd")],
        );
        let undone_tid = add_turn(
            &mut session,
            "hidden",
            "nope",
            Some(artifact(b"dropped")),
            Some(deepseek_producer()),
            vec![tool_call_record("call_1", "ls")],
        );
        session
            .turns
            .get_mut(&undone_tid)
            .expect("turn exists")
            .undone = true;

        let result = build_chat_request_messages(&session, None, "deepseek", "deepseek-v4-pro");
        let assistants = assistant_messages(&result);
        assert_eq!(assistants.len(), 1, "undone turn must be skipped entirely");
        assert_eq!(assistants[0].reasoning_artifact, Some(artifact(b"kept")));
    }

    // -- prev_resp_id persistence (phase 4c) --------------------------------

    #[test]
    fn initial_prev_resp_id_response_policy_restores_persisted_id() {
        let mut session = SessionState::empty();
        session.config.last_response_id = Some("resp_123".into());
        session.config.last_response_id_producer = Some(ReasoningProducer {
            provider_slug: "openai".into(),
            model: "gpt-4".into(),
        });
        // gpt-4 is an OpenAI Responses model → ResponseId policy AND the
        // persisted id was produced by the same provider+model: the id must
        // be restored to chain reasoning continuity across user turns.
        assert_eq!(
            initial_prev_resp_id(&session, "openai", "gpt-4").as_deref(),
            Some("resp_123"),
        );
    }

    #[test]
    fn initial_prev_resp_id_other_policies_reset_to_none() {
        let mut session = SessionState::empty();
        session.config.last_response_id = Some("resp_123".into());
        session.config.last_response_id_producer = Some(ReasoningProducer {
            provider_slug: "deepseek".into(),
            model: "deepseek-v4-pro".into(),
        });
        // DeepSeek chat → ToolLoop policy: a stale id must NOT leak into a
        // request that does not understand previous_response_id — even when
        // the provenance matches.
        assert_eq!(
            initial_prev_resp_id(&session, "deepseek", "deepseek-v4-pro"),
            None,
        );
        // Unknown provider → None policy.
        assert_eq!(
            initial_prev_resp_id(&session, "unknown-provider", "m"),
            None
        );
    }

    #[test]
    fn initial_prev_resp_id_drops_stale_id_from_other_producer() {
        // The persisted id was produced by a DIFFERENT provider+model (e.g. a
        // mid-session openai → xAI switch): restoring it would replay a stale
        // previous_response_id into a service that does not recognize it →
        // provider 400. Provenance must gate the restore, exactly like
        // reasoning artifacts.
        let mut session = SessionState::empty();
        session.config.last_response_id = Some("resp_openai".into());
        session.config.last_response_id_producer = Some(ReasoningProducer {
            provider_slug: "openai".into(),
            model: "gpt-5.4".into(),
        });
        // Same provider, different model → dropped (model-bound provenance).
        assert_eq!(
            initial_prev_resp_id(&session, "openai", "gpt-4"),
            None,
            "id from gpt-5.4 must not be restored for gpt-4",
        );
        // Matching provider+model → restored.
        assert_eq!(
            initial_prev_resp_id(&session, "openai", "gpt-5.4").as_deref(),
            Some("resp_openai"),
        );
        // No producer recorded (fresh session) → no id is ever restored.
        let fresh = SessionState::empty();
        assert_eq!(initial_prev_resp_id(&fresh, "openai", "gpt-5.4"), None);
    }

    // -- Precondition guard (phase 4c) --------------------------------------

    #[test]
    fn guard_warns_when_tool_involving_turn_lacks_artifact() {
        let mut session = SessionState::empty();
        // Tool-involving turn WITHOUT an artifact (pre-migration session state).
        let (tid, _) = session.start_turn(Some("list".into()));
        let records = vec![tool_call_record("call_1", "ls")];
        session.set_assistant_response(
            tid,
            AssistantResponse {
                text: Some("thinking".into()),
                tool_calls: records.clone(),
                ..Default::default()
            },
        );
        session.seed_tool_results(tid, &records);
        // Tool-involving turn WITH an artifact: clean.
        add_turn(
            &mut session,
            "again",
            "thinking2",
            Some(artifact(b"ok")),
            Some(deepseek_producer()),
            vec![tool_call_record("call_2", "sh")],
        );

        let missing =
            warn_on_missing_reasoning_artifacts(&session, 7, "deepseek", "deepseek-v4-pro");
        assert_eq!(
            missing, 1,
            "only the artifact-less tool turn should be flagged",
        );
    }

    #[test]
    fn guard_clean_when_all_tool_turns_have_artifacts() {
        let mut session = SessionState::empty();
        add_turn(
            &mut session,
            "list",
            "thinking",
            Some(artifact(b"ok")),
            Some(deepseek_producer()),
            vec![tool_call_record("call_1", "ls")],
        );
        // Non-tool turns never need an artifact.
        add_turn(
            &mut session,
            "plain",
            "hi",
            None,
            Some(deepseek_producer()),
            vec![],
        );
        assert_eq!(
            warn_on_missing_reasoning_artifacts(&session, 7, "deepseek", "deepseek-v4-pro"),
            0,
        );
    }

    #[test]
    fn guard_skipped_for_non_echo_policies() {
        let mut session = SessionState::empty();
        let (tid, _) = session.start_turn(Some("list".into()));
        let records = vec![tool_call_record("call_1", "ls")];
        session.set_assistant_response(
            tid,
            AssistantResponse {
                text: Some("thinking".into()),
                tool_calls: records.clone(),
                ..Default::default()
            },
        );
        session.seed_tool_results(tid, &records);
        // ResponseId policy: artifacts flow via previous_response_id, so the
        // guard must not flag the missing message artifact.
        assert_eq!(
            warn_on_missing_reasoning_artifacts(&session, 7, "openai", "gpt-4"),
            0,
        );
    }

    // -- Concurrent tool status label tests --------------------------------

    #[test]
    fn concurrent_status_label_single_tool_uses_real_name() {
        // A lone non-config tool call still dispatches through the concurrent
        // bucket, so the status must show its real name, not "(parallel)".
        let label = concurrent_tool_status_label(&[config_change_call("sh", "{}")]);
        assert_eq!(label, "sh");
    }

    #[test]
    fn concurrent_status_label_multi_tool_batch_is_parallel() {
        let label = concurrent_tool_status_label(&[
            config_change_call("sh", "{}"),
            config_change_call("grep", "{}"),
        ]);
        assert_eq!(label, "(parallel)");
    }

    #[test]
    fn concurrent_status_label_empty_batch_is_parallel() {
        // Defensive: the caller guards with `!concurrent.is_empty()` before
        // sending, but the label should still be well-defined if reached.
        let label = concurrent_tool_status_label(&[]);
        assert_eq!(label, "(parallel)");
    }

    // -- Cancellation helper tests -----------------------------------------

    #[test]
    fn is_cancelled_once_no_signal() {
        let (_tx, rx) = crossbeam_channel::unbounded::<()>();
        assert!(!is_cancelled_once(&rx));
    }

    #[test]
    fn is_cancelled_once_with_signal() {
        let (tx, rx) = crossbeam_channel::unbounded::<()>();
        tx.send(()).unwrap();
        assert!(is_cancelled_once(&rx));
    }

    // -- pending/apply config-change tests ---------------------------------

    fn config_change_call(name: &str, arguments_json: &str) -> ChatToolCall {
        ChatToolCall {
            id: "call_1".into(),
            name: name.into(),
            arguments_json: arguments_json.into(),
            caller: None,
        }
    }

    /// A successful tool output with the given structured result (or `None`
    /// for tools whose result_json wasn't captured).
    fn ok_output(result_json: Option<serde_json::Value>) -> ToolOutput {
        ToolOutput {
            content: String::new(),
            is_error: false,
            invocation_description: String::new(),
            result_json,
        }
    }

    #[test]
    fn pending_load_tools_captures_groups_and_applies() {
        let tool_call = config_change_call("load_tools", r#"{"groups": ["shell", "x"]}"#);
        let change = pending_config_change(&tool_call, &ok_output(None), None)
            .expect("load_tools should produce a change");
        assert!(matches!(change, PendingConfigChange::LoadTools(ref g) if g == &["shell", "x"]));

        let mut session = SessionState::empty();
        session.config.active_tool_groups = ["core".into(), "git".into()].into_iter().collect();
        apply_pending_config_change(&mut session, &change);
        assert!(session.config.active_tool_groups.contains("shell"));
        assert!(session.config.active_tool_groups.contains("x"));
        assert!(session.config.active_tool_groups.contains("core"));
    }

    #[test]
    fn pending_unload_tools_captures_groups_and_applies() {
        let tool_call = config_change_call("unload_tools", r#"{"groups": ["shell"]}"#);
        let change = pending_config_change(&tool_call, &ok_output(None), None)
            .expect("unload_tools should produce a change");
        assert!(matches!(change, PendingConfigChange::UnloadTools(ref g) if g == &["shell"]));

        let mut session = SessionState::empty();
        session.config.active_tool_groups = ["core".into(), "shell".into()].into_iter().collect();
        apply_pending_config_change(&mut session, &change);
        assert!(!session.config.active_tool_groups.contains("shell"));
        assert!(session.config.active_tool_groups.contains("core"));
    }

    #[test]
    fn pending_set_working_dir_mirrors_executed_result() {
        // The tool executed against a path that has since been deleted.  The
        // mirror must use the EXECUTED result (the canonical path the tool
        // computed and the main loop applied) — no re-resolution, so the
        // deleted directory cannot break the mirror (no TOCTOU).
        let tool_call = config_change_call("set_working_dir", r#"{"path": "sub"}"#);
        let output = ok_output(Some(serde_json::json!({ "path": "/real/canonical/sub" })));
        let change = pending_config_change(&tool_call, &output, None)
            .expect("set_working_dir should produce a change");
        assert!(matches!(
            change,
            PendingConfigChange::SetWorkingDir(Some(ref p)) if p == &PathBuf::from("/real/canonical/sub")
        ));

        let mut session = SessionState::empty();
        session.discovered_skills = Some(Vec::new());
        apply_pending_config_change(&mut session, &change);
        assert_eq!(
            session.config.working_dir.as_deref(),
            Some(PathBuf::from("/real/canonical/sub").as_path())
        );
        assert!(
            session.discovered_skills.is_none(),
            "skill cache must be invalidated by the mirror"
        );
    }

    #[test]
    fn pending_set_working_dir_falls_back_to_shared_resolution() {
        // result_json missing (shouldn't happen on success) — the mirror
        // falls back to the SAME resolution helper the tool uses.
        let base = tempfile::tempdir().unwrap();
        let sub = base.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        let tool_call = config_change_call("set_working_dir", r#"{"path": "sub"}"#);

        let change = pending_config_change(&tool_call, &ok_output(None), Some(base.path()))
            .expect("set_working_dir should produce a change");

        let mut session = SessionState::empty();
        apply_pending_config_change(&mut session, &change);
        assert_eq!(
            session.config.working_dir.as_deref(),
            Some(sub.canonicalize().unwrap().as_path())
        );
    }

    #[test]
    fn pending_set_working_dir_nonexistent_path_still_invalidates_skills() {
        // The tool succeeded (result_json present) but the path is now gone.
        // The mirror still applies the executed path verbatim — no TOCTOU.
        let tool_call = config_change_call("set_working_dir", r#"{"path": "gone"}"#);
        let output = ok_output(Some(serde_json::json!({ "path": "/gone/dir" })));
        let change = pending_config_change(&tool_call, &output, None)
            .expect("set_working_dir should produce a change");

        let mut session = SessionState::empty();
        session.discovered_skills = Some(Vec::new());
        apply_pending_config_change(&mut session, &change);
        assert_eq!(
            session.config.working_dir.as_deref(),
            Some(PathBuf::from("/gone/dir").as_path())
        );
        assert!(
            session.discovered_skills.is_none(),
            "skill cache must be invalidated even when the path is gone"
        );
    }

    #[test]
    fn pending_set_working_dir_unresolvable_fallback_still_invalidates_skills() {
        // result_json missing AND the fallback resolution fails (path does
        // not exist) — the worker skips the path update but MUST still
        // invalidate its skill cache so stale skills never leak across the
        // request boundary (RequestFinished merges discovered_skills over the
        // main loop's invalidated None).
        let base = tempfile::tempdir().unwrap();
        let tool_call = config_change_call("set_working_dir", r#"{"path": "does-not-exist"}"#);

        let change = pending_config_change(&tool_call, &ok_output(None), Some(base.path()))
            .expect("set_working_dir should still produce a change");
        assert!(matches!(change, PendingConfigChange::SetWorkingDir(None)));

        let mut session = SessionState::empty();
        session.discovered_skills = Some(Vec::new());
        apply_pending_config_change(&mut session, &change);
        assert!(session.config.working_dir.is_none());
        assert!(
            session.discovered_skills.is_none(),
            "skill cache must be invalidated even when no path could be resolved"
        );
    }

    #[test]
    fn pending_unknown_tool_is_noop() {
        let tool_call = config_change_call("read_file", r#"{"path": "x"}"#);
        assert!(pending_config_change(&tool_call, &ok_output(None), None).is_none());
    }

    #[test]
    fn pending_unparseable_args_is_noop() {
        let tool_call = config_change_call("load_tools", "not json");
        assert!(pending_config_change(&tool_call, &ok_output(None), None).is_none());
    }

    // -- broadcast_turn_appended tests -----------------------------------

    #[test]
    fn broadcast_turn_appended_sends_when_turn_exists() {
        let (tx, rx) = mpsc::channel::<SessionCommand>();
        let mut session = SessionState::empty();
        let (turn_id, _) = session.start_turn(Some("hello".into()));

        broadcast_turn_appended(&tx, &session, 0, turn_id);

        match rx.try_recv() {
            Ok(SessionCommand::Broadcast(DaemonMessage::TurnAppended { turn_id: id, .. })) => {
                assert_eq!(id, turn_id);
            }
            Ok(_) => panic!("expected TurnAppended broadcast, got different command"),
            Err(e) => panic!("expected TurnAppended broadcast, got error: {e}"),
        }
    }

    #[test]
    fn broadcast_turn_appended_no_turn_no_broadcast() {
        let (tx, rx) = mpsc::channel::<SessionCommand>();
        let session = SessionState::empty();

        broadcast_turn_appended(&tx, &session, 0, 999);

        assert!(rx.try_recv().is_err(), "expected no message");
    }

    #[test]
    fn broadcast_turn_appended_disconnected_receiver_no_panic() {
        let (tx, rx) = mpsc::channel::<SessionCommand>();
        let mut session = SessionState::empty();
        let (turn_id, _) = session.start_turn(Some("hello".into()));
        drop(rx);

        // Disconnected receiver should not panic — warn! is logged instead.
        broadcast_turn_appended(&tx, &session, 0, turn_id);
    }

    // -- resolve_reasoning_effort tests ------------------------------------

    #[test]
    fn resolve_reasoning_effort_off_returns_off() {
        let provider = make_test_provider();
        let result = resolve_reasoning_effort(&provider, "o3-mini", 1, 0, "off");
        assert_eq!(result, "off");
    }

    #[test]
    fn resolve_reasoning_effort_unknown_provider_disables() {
        let provider = make_test_provider();
        let result = resolve_reasoning_effort(&provider, "o3-mini", 1, 0, "low");
        // "test-stub" slug is not in the catalog, so reasoning is unsupported.
        assert_eq!(result, "off");
    }

    #[test]
    fn resolve_reasoning_effort_openai_supported_model_preserves() {
        let config = choreo_ai_protocols::openai::ServiceConfig::default();
        let client =
            choreo_ai_protocols::openai::OpenAiClient::new(config, "test-key".into()).unwrap();
        let provider = InferenceProvider::from_openai(client);

        let result = resolve_reasoning_effort(&provider, "o3-mini", 1, 0, "high");
        assert_eq!(result, "high");
    }

    #[test]
    fn resolve_reasoning_effort_openai_unsupported_model_disables() {
        let config = choreo_ai_protocols::openai::ServiceConfig::default();
        let client =
            choreo_ai_protocols::openai::OpenAiClient::new(config, "test-key".into()).unwrap();
        let provider = InferenceProvider::from_openai(client);

        let result = resolve_reasoning_effort(&provider, "gpt-4.1", 1, 0, "medium");
        assert_eq!(result, "off");
    }

    // -- estimate_prompt_tokens tests ------------------------------------

    #[test]
    fn estimate_prompt_tokens_empty() {
        let (encoding, estimated) = estimate_prompt_tokens("gpt-4", &[], &[]);
        assert!(encoding.is_some());
        assert_eq!(estimated, 0);
    }

    #[test]
    fn estimate_prompt_tokens_counts_content() {
        let messages = vec![
            ChatRequestMessage::simple("user", "hello world".into()),
            ChatRequestMessage::simple("assistant", "hi there".into()),
        ];
        let (_, estimated) = estimate_prompt_tokens("gpt-4", &messages, &[]);
        assert!(
            estimated > 0,
            "expected positive token count, got {estimated}"
        );
    }

    #[test]
    fn estimate_prompt_tokens_does_not_count_reasoning_content() {
        let base_messages = vec![
            ChatRequestMessage::simple("user", "hello".into()),
            ChatRequestMessage::simple("assistant", "visible".into()),
        ];
        let mut with_reasoning = base_messages.clone();
        with_reasoning[1].reasoning_content = Some("thinking deep...".into());

        let (_, base_est) = estimate_prompt_tokens("gpt-4", &base_messages, &[]);
        let (_, reason_est) = estimate_prompt_tokens("gpt-4", &with_reasoning, &[]);
        assert_eq!(
            base_est, reason_est,
            "legacy reasoning_content string field is never populated by the daemon and must not count"
        );
    }

    #[test]
    fn estimate_prompt_tokens_counts_reasoning_artifact() {
        let base_messages = vec![
            ChatRequestMessage::simple("user", "hello".into()),
            ChatRequestMessage::simple("assistant", "visible".into()),
        ];
        let mut with_artifact = base_messages.clone();
        with_artifact[1].reasoning_artifact = Some(ReasoningArtifact::ChatReasoning {
            field: ChatReasoningField::ReasoningContent,
            bytes: "thinking deep...".into(),
        });

        let (_, base_est) = estimate_prompt_tokens("gpt-4", &base_messages, &[]);
        let (_, artifact_est) = estimate_prompt_tokens("gpt-4", &with_artifact, &[]);
        assert!(
            artifact_est > base_est,
            "replayed reasoning artifact should count as input: {artifact_est} <= {base_est}",
        );
    }

    #[test]
    fn estimate_prompt_tokens_counts_tool_call_metadata() {
        let messages = vec![ChatRequestMessage {
            role: "assistant",
            content: None,
            tool_calls: Some(vec![AssistantToolCall {
                id: "call_abc".into(),
                kind: "function".into(),
                function: AssistantToolFunction {
                    name: "read_file".into(),
                    arguments: r#"{"path": "/etc/hosts"}"#.into(),
                },
            }]),
            tool_call_id: None,
            reasoning_content: None,
            reasoning: None,
            reasoning_text: None,
            reasoning_artifact: None,
        }];
        let (_, estimated) = estimate_prompt_tokens("gpt-4", &messages, &[]);
        assert!(
            estimated > 0,
            "expected positive token count, got {estimated}"
        );
    }

    #[test]
    fn estimate_prompt_tokens_includes_tool_defs() {
        let tools = vec![ChatToolDefinition::function(
            "read_file",
            "Read a file from disk",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"}
                }
            }),
        )];
        let messages = vec![ChatRequestMessage::simple("user", "read file".into())];
        let (_, with_tools) = estimate_prompt_tokens("gpt-4", &messages, &tools);
        let (_, without_tools) = estimate_prompt_tokens("gpt-4", &messages, &[]);
        assert!(
            with_tools > without_tools,
            "tool defs should increase token count: {with_tools} <= {without_tools}",
        );
    }

    #[test]
    fn estimate_prompt_tokens_unknown_model_falls_back() {
        let messages = vec![ChatRequestMessage::simple("user", "hello".into())];
        let (encoding, estimated) =
            estimate_prompt_tokens("nonexistent-model-9000", &messages, &[]);
        assert!(encoding.is_some(), "should fall back to cl100k_base");
        assert!(estimated > 0);
    }

    // -- execute_tool_with_timeout tests -----------------------------------

    struct FastTestTool;

    impl Tool for FastTestTool {
        type Args = serde_json::Value;
        type Return = String;
        type Error = ToolExecError;

        fn name(&self) -> &'static str {
            "_test_fast"
        }
        fn group(&self) -> &'static str {
            "test"
        }
        fn description(&self) -> &'static str {
            "test tool that completes immediately"
        }
        fn describe_invocation(&self, _args: &Self::Args) -> String {
            format!("{}.", self.description())
        }
        fn return_string(ret: &Self::Return) -> String {
            ret.clone()
        }
        fn schema(&self) -> serde_json::Value {
            serde_json::json!({})
        }
        fn execute(
            &self,
            _args: Self::Args,
            _xc: Option<&ServiceCredential>,
            _working_dir: Option<&Path>,
            _ctx: Option<&ToolContext>,
        ) -> Result<Self::Return, Self::Error> {
            Ok("fast result".into())
        }
    }

    struct BlockingTestTool {
        proceed: std::sync::Mutex<Option<mpsc::Receiver<()>>>,
    }

    impl Tool for BlockingTestTool {
        type Args = serde_json::Value;
        type Return = String;
        type Error = ToolExecError;

        fn name(&self) -> &'static str {
            "_test_blocking"
        }
        fn group(&self) -> &'static str {
            "test"
        }
        fn description(&self) -> &'static str {
            "test tool that blocks until proceed"
        }
        fn describe_invocation(&self, _args: &Self::Args) -> String {
            format!("{}.", self.description())
        }
        fn return_string(ret: &Self::Return) -> String {
            ret.clone()
        }
        fn schema(&self) -> serde_json::Value {
            serde_json::json!({})
        }
        fn execute(
            &self,
            _args: Self::Args,
            _xc: Option<&ServiceCredential>,
            _working_dir: Option<&Path>,
            _ctx: Option<&ToolContext>,
        ) -> Result<Self::Return, Self::Error> {
            Ok("ignored".into())
        }
        fn execute_streaming(
            &self,
            _args: Self::Args,
            _xc: Option<&ServiceCredential>,
            _working_dir: Option<&Path>,
            _output_tx: crossbeam_channel::Sender<Vec<u8>>,
            _ctx: Option<&ToolContext>,
        ) -> Result<Self::Return, Self::Error> {
            if let Some(rx) = self.proceed.lock().unwrap().take() {
                let _ = rx.recv();
            }
            Ok("blocked tool done".into())
        }
    }

    fn run_exec_tool(
        tool: impl Tool + 'static,
        tool_name: &str,
        tool_args: &str,
        timeout_dur: Duration,
        cancel_rx: crossbeam_channel::Receiver<()>,
    ) -> (ToolOutput, bool, mpsc::Receiver<SessionCommand>) {
        let (daemon_tx, _daemon_rx) = mpsc::channel::<DaemonCommand>();
        let (cmd_tx, cmd_rx) = mpsc::channel::<SessionCommand>();

        let dir = tempfile::tempdir().expect("tempdir");
        let db = redb::Database::create(dir.path().join("test.redb")).expect("Database");

        let mut session = SessionState::empty();

        let mut registry = ToolRegistry::new();
        registry.register(tool);
        let registry = registry.build();

        let tool_call = ChatToolCall {
            id: "call_test".into(),
            name: tool_name.into(),
            arguments_json: tool_args.into(),
            caller: None,
        };

        let ctx = RequestContext {
            cmd_tx,
            session_id: 1,
            db: Arc::new(db),
            tool_registry: registry,
            daemon_tx,
            max_turns: 0,
        };
        let (result, cancelled, _image) = execute_tool_with_timeout(
            &tool_call,
            None,
            None,
            timeout_dur,
            1,
            1,
            &mut session,
            &cancel_rx,
            &ctx,
            "test invocation",
        );
        (result, cancelled, cmd_rx)
    }

    #[test]
    fn execute_tool_normal_completion() {
        let (_cancel_tx, cancel_rx) = crossbeam_channel::unbounded::<()>();
        let (result, cancelled, _cmd_rx) = run_exec_tool(
            FastTestTool,
            "_test_fast",
            "{}",
            Duration::from_secs(60),
            cancel_rx,
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
            cancel_rx,
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
            cancel_rx,
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

    struct StreamingTestTool;

    impl Tool for StreamingTestTool {
        type Args = serde_json::Value;
        type Return = String;
        type Error = ToolExecError;

        fn name(&self) -> &'static str {
            "_test_streaming"
        }
        fn group(&self) -> &'static str {
            "test"
        }
        fn description(&self) -> &'static str {
            "test tool that sends streaming output"
        }
        fn describe_invocation(&self, _args: &Self::Args) -> String {
            format!("{}.", self.description())
        }
        fn return_string(ret: &Self::Return) -> String {
            ret.clone()
        }
        fn schema(&self) -> serde_json::Value {
            serde_json::json!({})
        }
        fn execute(
            &self,
            _args: Self::Args,
            _xc: Option<&ServiceCredential>,
            _working_dir: Option<&Path>,
            _ctx: Option<&ToolContext>,
        ) -> Result<Self::Return, Self::Error> {
            Ok("exec result".into())
        }
        fn supports_streaming_output() -> bool {
            true
        }

        fn execute_streaming(
            &self,
            _args: Self::Args,
            _xc: Option<&ServiceCredential>,
            _working_dir: Option<&Path>,
            output_tx: crossbeam_channel::Sender<Vec<u8>>,
            _ctx: Option<&ToolContext>,
        ) -> Result<Self::Return, Self::Error> {
            let _ = output_tx.send(b"streamed payload".to_vec());
            Ok("streaming done".into())
        }
    }

    #[test]
    fn execute_tool_forwards_streaming_output() {
        let (_cancel_tx, cancel_rx) = crossbeam_channel::unbounded::<()>();
        let (result, cancelled, cmd_rx) = run_exec_tool(
            StreamingTestTool,
            "_test_streaming",
            "{}",
            Duration::from_secs(60),
            cancel_rx,
        );

        assert!(!result.is_error, "expected success: {}", result.content);
        assert!(
            result.content.contains("streaming done"),
            "{}",
            result.content
        );
        assert!(!cancelled, "completion must not report a cancel");

        // First chunk is the description string (from execute_streaming_json)
        match cmd_rx.recv() {
            Ok(SessionCommand::Broadcast(DaemonMessage::ToolResultChunk { data, .. })) => {
                assert_eq!(data, b"test tool that sends streaming output.");
            }
            Ok(_other) => panic!("expected ToolResultChunk, got unexpected SessionCommand"),
            Err(e) => panic!("channel disconnected while waiting for streaming output: {e}"),
        }
        // Second chunk is the actual payload from the tool's execute_streaming
        match cmd_rx.recv() {
            Ok(SessionCommand::Broadcast(DaemonMessage::ToolResultChunk { data, .. })) => {
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
        let (cmd_tx, cmd_rx) = mpsc::channel::<SessionCommand>();
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
            Ok(SessionCommand::Broadcast(DaemonMessage::ToolResultChunk { data, .. })) => {
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
        let (cmd_tx, cmd_rx) = mpsc::channel::<SessionCommand>();
        let (output_tx, output_rx) = crossbeam_channel::unbounded::<Vec<u8>>();
        let (_kill_tx, kill_rx) = crossbeam_channel::unbounded::<()>();

        let handle = spawn_forwarding_thread(cmd_tx, 1, 1, "call_2".into(), output_rx, kill_rx);

        output_tx.send(b"last chunk".to_vec()).expect("send chunk");
        // Tool finished: dropping the output sender makes the forwarder's next
        // `recv(output_rx)` return Err (disconnect) → drain → exit.
        drop(output_tx);

        match cmd_rx.recv() {
            Ok(SessionCommand::Broadcast(DaemonMessage::ToolResultChunk { data, .. })) => {
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
        let (cmd_tx, _cmd_rx) = mpsc::channel::<SessionCommand>();
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
        let (cmd_tx, cmd_rx) = mpsc::channel::<SessionCommand>();
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
            Ok(SessionCommand::Broadcast(DaemonMessage::ToolResultChunk { data, .. })) => {
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
        assert!(determine_tool_timeout("spawn_subsession").is_none());
    }

    #[test]
    fn determine_tool_timeout_shell_300() {
        for name in &["sh", "nushell", "fish", "exec"] {
            assert_eq!(
                determine_tool_timeout(name),
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
                determine_tool_timeout(name),
                Some(Duration::from_secs(60)),
                "tool {name} should have 60s timeout",
            );
        }
    }

    // -- spawn_single_tool tests ---------------------------------------

    /// Build a throwaway `ToolContext` and command channel for
    /// `spawn_single_tool` tests. Receivers are dropped, which is fine — no
    /// assertion inspects the daemon or session command streams here.
    fn spawn_test_ctx() -> (ToolContext, mpsc::Sender<SessionCommand>) {
        let (cmd_tx, _cmd_rx) = mpsc::channel::<SessionCommand>();
        let (_daemon_tx, _daemon_rx) = mpsc::channel::<DaemonCommand>();
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Arc::new(redb::Database::create(dir.path().join("test.redb")).expect("Database"));
        let ctx = ToolContext {
            session_id: 1,
            db,
            daemon_tx: _daemon_tx,
            active_tool_groups: std::collections::HashSet::new(),
            reasoning_effort: None,
            selected_model: None,
            working_dir: None,
            cancelled: Arc::new(AtomicBool::new(false)),
            account_name: None,
        };
        (ctx, cmd_tx)
    }

    fn run_spawn_single_tool(
        tool: impl Tool + 'static,
        tool_name: &str,
        tool_args: &str,
        timeout: Option<Duration>,
    ) -> ToolHandle {
        let (ctx, cmd_tx) = spawn_test_ctx();

        let mut registry = ToolRegistry::new();
        registry.register(tool);
        let registry = registry.build();

        let tool_call = ChatToolCall {
            id: "call_test".into(),
            name: tool_name.into(),
            arguments_json: tool_args.into(),
            caller: None,
        };

        let invocation_description = registry
            .describe_invocation_for(&tool_call.name, &tool_call.arguments_json)
            .unwrap_or_default();

        let (result_tx, result_rx) = crossbeam_channel::unbounded::<ToolHandle>();
        // Hold the kill sender for the duration of this wait — dropping it
        // would disconnect the kill channel and stop the wait-loop early.
        let _kill_tx = spawn_single_tool(SpawnToolArgs {
            tool_call,
            timeout,
            request_id: 1,
            session_id: 1,
            registry,
            cmd_tx,
            x_credentials: None,
            working_dir: None,
            ctx,
            invocation_description,
            started_at: Instant::now(),
            result_tx,
        });

        result_rx.recv().expect("tool did not deliver a result")
    }

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
            request_id: 1,
            session_id: 1,
            registry: Arc::clone(&registry),
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
            request_id: 1,
            session_id: 1,
            registry,
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
            request_id: 1,
            session_id: 1,
            registry,
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

    // -- sort_by_call_order tests -----------------------------------------

    #[test]
    fn sort_by_call_order_restores_model_order() {
        // Model issued calls a, b, c; the tools completed in the reverse order
        // (c first). The next-call accumulator must be restored to a, b, c so
        // tool messages mirror the assistant's tool_calls array.
        let tool_calls = vec![
            AssistantToolCallRecord {
                call_id: "a".into(),
                name: "read_file".into(),
                arguments_json: "{}".into(),
            },
            AssistantToolCallRecord {
                call_id: "b".into(),
                name: "grep".into(),
                arguments_json: "{}".into(),
            },
            AssistantToolCallRecord {
                call_id: "c".into(),
                name: "sh".into(),
                arguments_json: "{}".into(),
            },
        ];
        let mut items = vec![
            ToolResultItem {
                call_id: "c".into(),
                output: "c-out".into(),
                caller: None,
            },
            ToolResultItem {
                call_id: "a".into(),
                output: "a-out".into(),
                caller: None,
            },
            ToolResultItem {
                call_id: "b".into(),
                output: "b-out".into(),
                caller: None,
            },
        ];
        sort_by_call_order(&tool_calls, &mut items, |r| r.call_id.as_str());
        let order: Vec<_> = items.iter().map(|r| r.call_id.as_str()).collect();
        assert_eq!(order, vec!["a", "b", "c"]);
    }

    #[test]
    fn sort_by_call_order_sinks_unknown_call_ids() {
        // A streaming stub created before its start event arrived has no
        // matching tool_call; it must sink to the end, keeping relative order.
        let tool_calls = vec![AssistantToolCallRecord {
            call_id: "a".into(),
            name: "read_file".into(),
            arguments_json: "{}".into(),
        }];
        let mut items = vec![
            ToolResultItem {
                call_id: "ghost".into(),
                output: "g-out".into(),
                caller: None,
            },
            ToolResultItem {
                call_id: "a".into(),
                output: "a-out".into(),
                caller: None,
            },
        ];
        sort_by_call_order(&tool_calls, &mut items, |r| r.call_id.as_str());
        let order: Vec<_> = items.iter().map(|r| r.call_id.as_str()).collect();
        assert_eq!(order, vec!["a", "ghost"]);
    }

    // -- extract_json_string tests ------------------------------------------

    #[test]
    fn extract_json_string_gets_value() {
        let json = r#"{"name": "test-skill", "path": "src/main.rs"}"#;
        assert_eq!(
            extract_json_string(json, "name").as_deref(),
            Some("test-skill")
        );
        assert_eq!(
            extract_json_string(json, "path").as_deref(),
            Some("src/main.rs")
        );
    }

    #[test]
    fn extract_json_string_missing_key() {
        assert_eq!(extract_json_string(r#"{"other": "val"}"#, "name"), None);
    }

    #[test]
    fn extract_json_string_invalid_json() {
        assert_eq!(extract_json_string("not json", "name"), None);
    }

    // -- persist_loaded_skill tests -----------------------------------------

    #[test]
    fn persist_loaded_skill_adds_to_session() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join(".agents/skills/test-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "\
---\n\
name: test-skill\n\
description: A test skill\n\
---\n\
Hello, this is the skill body.\n\
---\n",
        )
        .unwrap();

        let mut session = SessionState::empty();
        session.config.working_dir = Some(dir.path().to_path_buf());
        assert!(session.loaded_skill_bodies.is_empty());

        persist_loaded_skill(&mut session, "load_skill", r#"{"name": "test-skill"}"#);

        assert_eq!(session.loaded_skill_bodies.len(), 1);
        assert_eq!(session.loaded_skill_bodies[0].name, "test-skill");
        assert!(session.loaded_skill_bodies[0].body.contains("skill body"));
    }

    #[test]
    fn persist_loaded_skill_skips_non_load_skill() {
        let mut session = SessionState::empty();
        persist_loaded_skill(&mut session, "read_file", r#"{"path": "Cargo.toml"}"#);
        assert!(session.loaded_skill_bodies.is_empty());
    }

    #[test]
    fn persist_loaded_skill_skips_missing_name() {
        let mut session = SessionState::empty();
        session.config.working_dir = Some(PathBuf::from("/tmp"));
        persist_loaded_skill(&mut session, "load_skill", r#"{}"#);
        assert!(session.loaded_skill_bodies.is_empty());
    }

    #[test]
    fn persist_loaded_skill_skips_without_working_dir() {
        let mut session = SessionState::empty();
        persist_loaded_skill(&mut session, "load_skill", r#"{"name": "test-skill"}"#);
        assert!(session.loaded_skill_bodies.is_empty());
    }

    // -- build_system_content tests -----------------------------------------

    fn setup_build_system_content_session() -> (SessionState, Arc<ToolRegistry>, tempfile::TempDir)
    {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "Project rules").unwrap();

        let mut registry = ToolRegistry::new();
        registry.register(FastTestTool);
        let registry = registry.build();

        let mut session = SessionState::empty();
        session.config.working_dir = Some(dir.path().to_path_buf());
        (session, registry, dir)
    }

    /// Call build_system_content with standard defaults derived from the
    /// session state and optional pending_hints overrides.
    fn test_build_content(
        session: &mut SessionState,
        registry: &ToolRegistry,
        pending_hints: &[String],
    ) -> Option<String> {
        build_system_content(
            SystemContentParams {
                working_dir: session.config.working_dir.as_deref(),
                context_config: &session.config.context_config,
                skills: &[],
                loaded_skill_bodies: &session.loaded_skill_bodies,
                tool_registry: registry,
                pending_hints,
                session_title: session.config.title.as_deref(),
            },
            &mut session.context_cache,
        )
    }

    #[test]
    fn build_system_content_with_working_dir() {
        let (mut session, registry, _dir) = setup_build_system_content_session();
        let content = test_build_content(&mut session, &registry, &[]);
        assert!(content.is_some());
        let content = content.unwrap();
        assert!(content.contains("Tool groups"));
        assert!(content.contains("core"));
        assert!(content.contains("Project rules"));
    }

    #[test]
    fn build_system_content_without_working_dir() {
        let mut session = SessionState::empty();
        let registry = ToolRegistry::new().build();
        let content = test_build_content(&mut session, &registry, &[]);
        assert!(content.is_none());
    }

    #[test]
    fn build_system_content_includes_loaded_skills() {
        let (mut session, registry, _dir) = setup_build_system_content_session();
        session.loaded_skill_bodies.push(LoadedSkill {
            name: "loaded-test".to_string(),
            body: "Loaded body text.".to_string(),
        });
        let content = test_build_content(&mut session, &registry, &[]);
        assert!(content.is_some());
        let content = content.unwrap();
        assert!(content.contains("Loaded skills"));
        assert!(content.contains("loaded-test"));
        assert!(content.contains("Loaded body text."));
    }

    #[test]
    fn build_system_content_populates_context_cache() {
        let (mut session, registry, _dir) = setup_build_system_content_session();
        assert!(session.context_cache.is_none());

        let _ = test_build_content(&mut session, &registry, &[]);
        assert!(
            session.context_cache.is_some(),
            "context_cache should be populated after first call"
        );
        let (fp, _) = session.context_cache.as_ref().unwrap();
        assert!(*fp > 0, "fingerprint should be non-zero");
    }

    #[test]
    fn build_system_content_includes_pending_hints() {
        let (mut session, registry, _dir) = setup_build_system_content_session();
        let pending_hints = vec!["Hint about subdirectory config.".to_string()];
        let content = test_build_content(&mut session, &registry, &pending_hints);
        assert!(content.is_some());
        let content = content.unwrap();
        assert!(content.contains("New context from project subdirectories"));
        assert!(content.contains("Hint about subdirectory config."));
    }

    #[test]
    fn build_system_content_includes_session_title() {
        let (mut session, registry, _dir) = setup_build_system_content_session();
        session.config.title = Some("Refactoring the database layer".into());
        let content = test_build_content(&mut session, &registry, &[]);
        assert!(content.is_some());
        let content = content.unwrap();
        assert!(content.contains("## Current Session Title"));
        assert!(content.contains("Refactoring the database layer"));
    }

    #[test]
    fn build_system_content_omits_empty_title() {
        let (mut session, registry, _dir) = setup_build_system_content_session();
        // Title is None by default — no "Current Session Title" section.
        let content = test_build_content(&mut session, &registry, &[]);
        assert!(content.is_some());
        let content = content.unwrap();
        assert!(!content.contains("## Current Session Title"));

        // Also omit when the title is an empty string.
        session.config.title = Some("".into());
        let content2 = test_build_content(&mut session, &registry, &[]);
        assert!(content2.is_some());
        let content2 = content2.unwrap();
        assert!(!content2.contains("## Current Session Title"));
    }
}
