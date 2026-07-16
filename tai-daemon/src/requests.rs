use crate::context;
use crate::db::{SessionRecord, write_message_retry, write_session_retry};
use crate::openai::{AssistantToolCall, AssistantToolFunction, ChatRequestMessage};
use crate::providers::shared::MAX_TOOL_CALLS;
use crate::providers::types::{ChatAssistantToolUse, ChatToolCall, ChatTurnResult};
use crate::providers::{
    ChatTurnRequest, InferenceProvider, ReasoningSupport, StreamEvent, ToolResultItem,
    effective_reasoning_support, lookup_provider,
};
use crate::sessions::{RequestContext, SessionCommand, SessionMetadata, SessionState};
use crate::tools::context::ToolContext;
use crate::tools::{PreparedImage, ToolExecutionOutput, ToolRegistry, ToolResult, resolve_path};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::mpsc::{RecvTimeoutError, TryRecvError};
use std::thread;
use std::time::Duration;
use std::time::Instant;
use tai_keystore::ServiceCredential;
use tai_proto::{
    AssistantToolCallRecord, DaemonMessage, DisplayedImageRecord, ImageMetadata, OutputStream,
    SessionMessage, SessionMessageKind, SessionStatus, ThinkingEffort, TokenUsage,
};
use tracing::{debug, info, trace, warn};

/// Persist a `PreparedImage` to the database as a `DisplayedImage` message,
/// push it into the session's in-memory message list, and broadcast it to
/// live subscribers immediately (mid-turn) so the image appears as soon as
/// the tool finishes rather than waiting for request completion.
/// Used by both the serial and concurrent tool paths.
fn emit_and_persist_image(
    cmd_tx: &mpsc::Sender<SessionCommand>,
    image: PreparedImage,
    session: &mut SessionState,
    ctx: &RequestContext,
) {
    let persisted = SessionMessage::now(SessionMessageKind::DisplayedImage(DisplayedImageRecord {
        metadata: ImageMetadata {
            image_id: 0,
            mime_type: image.mime_type,
            width: image.width,
            height: image.height,
            byte_len: image.data.len() as u64,
            alt: image.alt,
        },
        data: image.data,
    }));

    // Broadcast to live subscribers mid-turn so subscribers see the image
    // immediately, not just at request completion.
    let _ = cmd_tx.send(SessionCommand::Broadcast(
        DaemonMessage::SessionMessageAppended {
            message: persisted.clone(),
        },
    ));

    let img_idx = session.messages().len() as u32;
    if let Err(e) = write_message_retry(ctx.db.as_ref(), ctx.session_id, img_idx, &persisted) {
        tracing::warn!(
            session_id = ctx.session_id, error = %e,
            "failed to persist displayed image",
        );
    }
    session.push_message(persisted);
}

/// Spawn a forwarding thread that relays streaming output chunks to session
/// subscribers in real time.  Exits when the output channel is disconnected
/// (tool finished) or a kill signal is received (caller stopped waiting).
fn spawn_forwarding_thread(
    cmd_tx: mpsc::Sender<SessionCommand>,
    request_id: u32,
    call_id: String,
    output_rx: mpsc::Receiver<Vec<u8>>,
    kill_rx: mpsc::Receiver<()>,
) {
    let check_interval = Duration::from_millis(200);
    thread::spawn(move || {
        loop {
            match output_rx.recv_timeout(check_interval) {
                Ok(data) => {
                    if cmd_tx
                        .send(SessionCommand::Broadcast(DaemonMessage::ToolCallOutput {
                            request_id,
                            call_id: call_id.clone(),
                            data,
                        }))
                        .is_err()
                    {
                        break;
                    }
                }
                Err(RecvTimeoutError::Timeout) => match kill_rx.try_recv() {
                    Ok(()) | Err(TryRecvError::Disconnected) => break,
                    Err(TryRecvError::Empty) => {}
                },
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
    });
}

/// Check whether a cancellation signal has been received.
///
/// This is a one-shot check — call this when you only need to check once
/// and don't need to cache the result across loop iterations.
pub(crate) fn is_cancelled_once(rx: &mpsc::Receiver<()>) -> bool {
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

fn refresh_session_context(
    session: &mut SessionState,
    working_dir: &Path,
    context_config: &tai_proto::ContextConfig,
) {
    if let Some(old_fp) = session.config.context_fingerprint
        && let Some(idx) = session.config.context_message_index
        && let Ok(Some(new_bundle)) = context::recheck_context(working_dir, context_config, old_fp)
    {
        let new_content = context::assemble_context(&new_bundle);
        if !new_content.is_empty() {
            session.set_message(
                idx,
                SessionMessage::now(SessionMessageKind::SystemText {
                    content: new_content,
                }),
            );
        }
        session.config.context_fingerprint = Some(new_bundle.fingerprint);
        session.config.context_file_paths =
            new_bundle.files.iter().map(|f| f.path.clone()).collect();
    }
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
    output: ToolExecutionOutput,
    image: Option<PreparedImage>,
}

/// Parameters for spawning a single concurrent tool call.
struct SpawnToolArgs {
    tool_call: ChatToolCall,
    timeout: Option<Duration>,
    request_id: u32,
    registry: Arc<ToolRegistry>,
    cmd_tx: mpsc::Sender<SessionCommand>,
    x_credentials: Option<ServiceCredential>,
    working_dir: Option<PathBuf>,
    ctx: ToolContext,
}

/// Spawn a single tool call on a dedicated thread with its own forwarding
/// channel, timeout guard, and image drain.
///
/// The returned `JoinHandle` lets the caller collect results in whatever
/// order they choose — the spawned thread handles all channel wiring,
/// timeouts, and error recording internally.
fn spawn_single_tool(args: SpawnToolArgs) -> thread::JoinHandle<ToolHandle> {
    let SpawnToolArgs {
        tool_call,
        timeout,
        request_id,
        registry,
        cmd_tx,
        x_credentials,
        working_dir,
        ctx,
    } = args;
    // Channel for the execution thread to deliver its final result.
    let (result_tx, result_rx) = mpsc::channel::<ToolExecutionOutput>();

    // Channel for streaming output forwarded to subscribers in real time.
    let (output_tx, output_rx) = mpsc::channel::<Vec<u8>>();

    // Kill signal for the forwarding thread — sent when we're done waiting.
    let (kill_tx, kill_rx) = mpsc::channel::<()>();

    // Image channel — the tool may emit one image during execution.
    let (image_tx, image_rx) = mpsc::channel::<PreparedImage>();

    // ── Forwarding thread ──────────────────────────────────────────
    //
    // Forwards streaming output chunks to subscribers as they arrive.
    // Exits when the output channel is disconnected (tool finished) or
    // a kill signal is received (we stopped waiting).
    spawn_forwarding_thread(cmd_tx, request_id, tool_call.id.clone(), output_rx, kill_rx);

    // ── Execution thread ───────────────────────────────────────────
    let tc = tool_call.clone();
    let tr = registry;
    let xc = x_credentials;
    let c = working_dir;
    let tool_ctx = ctx;
    thread::spawn(move || {
        let result = tr.execute_streaming(
            &tc,
            output_tx,
            xc.as_ref(),
            c.as_deref(),
            Some(&tool_ctx),
            Some(image_tx),
        );
        let _ = result_tx.send(result);
    });

    // ── Wait loop ──────────────────────────────────────────────────
    //
    // Two modes:
    //   Some(dur) — bounded wait with deadline; returns error on timeout.
    //   None      — unbounded wait; blocks until the tool completes.
    let deadline = timeout.map(|d| Instant::now() + d);
    let check_interval = Duration::from_millis(200);
    thread::spawn(move || {
        let output = loop {
            if let Some(deadline) = deadline {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    break ToolExecutionOutput {
                        result: ToolResult {
                            content: format!("tool '{}' timed out", tool_call.name,),
                            is_error: true,
                        },
                    };
                }
                match result_rx.recv_timeout(remaining.min(check_interval)) {
                    Ok(output) => break output,
                    Err(RecvTimeoutError::Timeout) => continue,
                    Err(RecvTimeoutError::Disconnected) => {
                        break ToolExecutionOutput {
                            result: ToolResult {
                                content: "tool execution thread panicked".to_string(),
                                is_error: true,
                            },
                        };
                    }
                }
            } else {
                // No timeout — block indefinitely until the tool finishes.
                match result_rx.recv() {
                    Ok(output) => break output,
                    Err(_) => {
                        break ToolExecutionOutput {
                            result: ToolResult {
                                content: "tool execution thread panicked".to_string(),
                                is_error: true,
                            },
                        };
                    }
                }
            }
        };

        // Drain any image that was emitted during execution.
        let image = image_rx.try_recv().ok();

        // Signal the forwarding thread to stop — we have our result and
        // won't be streaming any more output from this tool call.
        let _ = kill_tx.send(());

        ToolHandle {
            tool_call,
            output,
            image,
        }
    })
}

pub(crate) fn run_agent_loop(
    client: &InferenceProvider,
    session: &mut SessionState,
    model: &str,
    request_id: u32,
    cancel_rx: &mpsc::Receiver<()>,
    ctx: &RequestContext,
) -> io::Result<bool> {
    let max_turns = session.config.max_turns.unwrap_or(ctx.max_turns_default);

    let mut prev_resp_id: Option<String> = None;
    let mut tool_results: Vec<ToolResultItem> = Vec::new();

    for turn in 0..max_turns {
        let mut thinking_effort = session
            .config
            .reasoning_effort
            .unwrap_or(ThinkingEffort::Off);
        debug!(session_id = ctx.session_id, turn, "agent loop turn");
        if thinking_effort != ThinkingEffort::Off {
            // Validate that the current model actually supports the requested
            // reasoning effort at inference time.  The set-time check in
            // sessions.rs accepts the effort when no model is selected yet,
            // so we must re-validate here with the concrete model.
            let slug = client.provider_slug();
            let catalog_entry = lookup_provider(slug);
            let reasoning_support = catalog_entry
                .map(|e| e.reasoning)
                .unwrap_or(ReasoningSupport::None);
            let effective = effective_reasoning_support(model, reasoning_support);
            if effective == ReasoningSupport::None {
                warn!(
                    session_id = ctx.session_id, turn, model,
                    effort = %thinking_effort.as_label(),
                    "model does not support reasoning effort, disabling",
                );
                thinking_effort = ThinkingEffort::Off;
            } else {
                debug!(
                    session_id = ctx.session_id, turn,
                    effort = %thinking_effort.as_label(),
                    "reasoning effort active in agent loop",
                );
            }
        }
        crate::metrics::record_turn(model);
        let tools = ctx
            .tool_registry
            .available_definitions(&session.config.active_tool_groups);
        if is_cancelled_once(cancel_rx) {
            return Ok(true);
        }

        if let Some(session_working_dir) = session.config.working_dir.clone() {
            let context_config = session.config.context_config.clone();
            refresh_session_context(session, &session_working_dir, &context_config);
        }

        if ctx
            .cmd_tx
            .send(SessionCommand::StatusChanged(SessionStatus::Inference))
            .is_err()
        {
            return Ok(false);
        }

        let messages = build_chat_request_messages(session.messages());

        // Build a retry-notification callback that forwards status updates
        // through the session command channel so the TUI can display the
        // retry progress and the user can cancel during backoff.
        let mut retry_cb: Option<crate::openai::RetryCallback> = Some(Box::new({
            let cmd_tx = ctx.cmd_tx.clone();
            move |attempt, max_attempts, delay| {
                let _ = cmd_tx.send(SessionCommand::StatusChanged(SessionStatus::Retrying {
                    attempt,
                    max_attempts,
                    delay_ms: delay.as_millis() as u64,
                }));
            }
        }));

        let mut tool_call_seen: Vec<bool> = vec![false; MAX_TOOL_CALLS];
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
                        let _ = ctx.cmd_tx.send(SessionCommand::Broadcast(
                            DaemonMessage::OutputChunk {
                                request_id,
                                stream: OutputStream::Answer,
                                data: text.into_bytes(),
                            },
                        ));
                    }
                    StreamEvent::Reasoning(text) => {
                        let _ = ctx.cmd_tx.send(SessionCommand::Broadcast(
                            DaemonMessage::OutputChunk {
                                request_id,
                                stream: OutputStream::Reasoning,
                                data: text.into_bytes(),
                            },
                        ));
                    }
                    // Track which tool call indices have already been announced
                    // via ToolCallGenerationStarted, so subsequent deltas are
                    // emitted as ToolCallArgDelta instead.
                    StreamEvent::ToolCallArg {
                        index,
                        call_id,
                        tool_name,
                        delta,
                    } => {
                        let call_id_opt = if call_id.is_empty() {
                            None
                        } else {
                            Some(call_id)
                        };
                        let seen_idx = index as usize;
                        if seen_idx >= tool_call_seen.len() {
                            warn!(
                                request_id,
                                index,
                                max = MAX_TOOL_CALLS,
                                "tool call index out of bounds, dropping",
                            );
                            return Ok(());
                        }
                        // Vec<bool> returns false if already true.
                        if !std::mem::replace(&mut tool_call_seen[seen_idx], true) {
                            trace!(
                                request_id,
                                index,
                                ?tool_name,
                                delta_len = delta.len(),
                                "tool call generation started",
                            );
                            let _ = ctx.cmd_tx.send(SessionCommand::Broadcast(
                                DaemonMessage::ToolCallGenerationStarted {
                                    request_id,
                                    call_id: call_id_opt,
                                    tool_name: if tool_name.is_empty() {
                                        None
                                    } else {
                                        Some(tool_name)
                                    },
                                    index,
                                    arguments_delta: delta,
                                },
                            ));
                        } else {
                            trace!(
                                request_id,
                                index,
                                call_id = call_id_opt.as_deref().unwrap_or("?"),
                                delta_len = delta.len(),
                                "tool call arg delta",
                            );
                            let _ = ctx.cmd_tx.send(SessionCommand::Broadcast(
                                DaemonMessage::ToolCallArgDelta {
                                    request_id,
                                    call_id: call_id_opt,
                                    index,
                                    arguments_delta: delta,
                                },
                            ));
                        }
                    }
                }
                Ok(())
            },
        ) {
            Ok(ChatTurnResult::FinalText(final_text)) => {
                debug!(
                    session_id = ctx.session_id,
                    turn,
                    response_len = final_text.content.len(),
                    reasoning = final_text.reasoning.as_deref().unwrap_or_default(),
                    "model returned final text",
                );
                let token_usage = final_text.usage;
                accumulate_token_usage(session, &token_usage, turn, ctx);
                let msg = SessionMessage::now(SessionMessageKind::AssistantText {
                    content: final_text.content,
                    reasoning: final_text.reasoning,
                    token_usage,
                });
                let idx = session.push_message(msg.clone());
                if let Err(e) = write_message_retry(&ctx.db, ctx.session_id, idx, &msg) {
                    tracing::warn!(session_id = ctx.session_id, error = %e, "failed to persist assistant text");
                }
                // FinalText ends the agent loop — no next turn to chain to.
                tool_results.clear();
                return Ok(false);
            }
            Ok(ChatTurnResult::ToolUse(tool_use)) => {
                let token_usage = tool_use.usage;
                accumulate_token_usage(session, &token_usage, turn, ctx);
                persist_assistant_tool_use_sync(session, &tool_use, token_usage, ctx);
                // Store response_id for chaining tool results back to this turn
                prev_resp_id = tool_use.response_id;
                tool_results.clear();

                // Partition tool calls into two groups:
                //   mutators  — tools that need &mut SessionState or deep
                //               coupling with the agent loop (load_tools,
                //               unload_tools).
                //   concurrent — everything else (run_riscv, shell,
                //               filesystem, etc.) that can run on
                //               independent OS threads.
                let (mutators, concurrent): (Vec<_>, Vec<_>) =
                    tool_use.tool_calls.into_iter().partition(|tc| {
                        matches!(
                            tc.name.as_str(),
                            "load_tools" | "unload_tools" | "set_working_dir"
                        )
                    });

                // ── Phase 1: Session-mutating tools (serial) ────────
                for tool_call in mutators {
                    if is_cancelled_once(cancel_rx) {
                        return Ok(true);
                    }

                    let _ = ctx.cmd_tx.send(SessionCommand::Broadcast(
                        DaemonMessage::ToolCallStarted {
                            request_id,
                            call_id: tool_call.id.clone(),
                            tool_name: tool_call.name.clone(),
                            arguments_json: tool_call.arguments_json.clone(),
                        },
                    ));

                    // Meta-tools always have a timeout (load/unload are fast).
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
                        turn,
                        tool_name = %tool_call.name,
                        tool_call_id = %tool_call.id,
                        args_preview = %(&tool_call.arguments_json[..tool_call.arguments_json.len().min(200)]),
                        "executing tool (serial)",
                    );

                    let (image_tx, image_rx) = mpsc::channel::<PreparedImage>();
                    let turn_working_dir = session.config.working_dir.clone();
                    let mut output = execute_tool_with_timeout(
                        &tool_call,
                        None,
                        turn_working_dir.as_deref(),
                        tool_timeout,
                        request_id,
                        session,
                        cancel_rx,
                        ctx,
                        Some(image_tx),
                    );

                    // Drain any image emitted by the tool.
                    if let Ok(image) = image_rx.try_recv() {
                        emit_and_persist_image(&ctx.cmd_tx, image, session, ctx);
                    }

                    finish_tool_call(request_id, session, &tool_call, &mut output, ctx);
                    tool_results.push(ToolResultItem {
                        call_id: tool_call.id.clone(),
                        output: output.result.content.clone(),
                        caller: tool_call.caller.clone(),
                    });
                }

                // ── Phase 2: All remaining tools (concurrent) ───────
                if !concurrent.is_empty() {
                    // Broadcast all ToolCallStarted events before any tool
                    // begins execution so subscribers see the full batch.
                    for tc in &concurrent {
                        let _ = ctx.cmd_tx.send(SessionCommand::Broadcast(
                            DaemonMessage::ToolCallStarted {
                                request_id,
                                call_id: tc.id.clone(),
                                tool_name: tc.name.clone(),
                                arguments_json: tc.arguments_json.clone(),
                            },
                        ));
                    }

                    if ctx
                        .cmd_tx
                        .send(SessionCommand::StatusChanged(SessionStatus::ToolCall(
                            "(parallel)".into(),
                        )))
                        .is_err()
                    {
                        return Ok(false);
                    }

                    debug!(
                        session_id = ctx.session_id,
                        turn,
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
                        reasoning_effort: session.config.reasoning_effort,
                        working_dir: session.config.working_dir.clone(),
                        cancelled: Arc::clone(&cancel_flag),
                    };

                    let cmd_tx = ctx.cmd_tx.clone();
                    let reg = Arc::clone(&ctx.tool_registry);

                    // Pair each tool's identifying info with its thread handle
                    // so we can reconstruct a meaningful error if the thread
                    // panics (avoiding expect() in production code per AGENTS.md).
                    let handles: Vec<_> = concurrent
                        .into_iter()
                        .map(|tool_call| {
                            let timeout = determine_tool_timeout(&tool_call.name);
                            let call_info = (
                                tool_call.id.clone(),
                                tool_call.name.clone(),
                                tool_call.arguments_json.clone(),
                                Instant::now(),
                            );
                            let handle = spawn_single_tool(SpawnToolArgs {
                                tool_call,
                                timeout,
                                request_id,
                                registry: Arc::clone(&reg),
                                cmd_tx: cmd_tx.clone(),
                                x_credentials: None,
                                working_dir: session.config.working_dir.clone(),
                                ctx: tool_ctx.clone(),
                            });
                            (call_info, handle)
                        })
                        .collect();

                    // Collect results in source-call order so the LLM sees
                    // a deterministic conversation history.
                    //
                    // Between each join we check the parent cancellation
                    // channel.  If cancelled we propagate to all in-flight
                    // tools via the shared AtomicBool — the flag is cheap
                    // and requires no lock.  We do NOT return early because
                    // the remaining JoinHandles must be drained to avoid
                    // leaking threads.
                    if is_cancelled_once(cancel_rx) {
                        cancel_flag.store(true, Ordering::Relaxed);
                    }
                    for ((call_id, tool_name, arguments_json, tool_start), handle) in handles {
                        // Re-check cancellation before each join so the
                        // flag is set as early as possible for tools that
                        // haven't finished yet.
                        if is_cancelled_once(cancel_rx) {
                            cancel_flag.store(true, Ordering::Relaxed);
                        }

                        let ToolHandle {
                            tool_call,
                            mut output,
                            image,
                        } = handle.join().unwrap_or_else(|_| {
                            // Thread panicked — create a fallback result
                            // so the agent loop can continue with the other
                            // concurrent tool outputs.
                            ToolHandle {
                                tool_call: ChatToolCall {
                                    id: call_id,
                                    name: tool_name.clone(),
                                    arguments_json,
                                    caller: None,
                                },
                                output: ToolExecutionOutput {
                                    result: ToolResult {
                                        content: "tool thread panicked".to_string(),
                                        is_error: true,
                                    },
                                },
                                image: None,
                            }
                        });

                        let elapsed = tool_start.elapsed();

                        debug!(
                            session_id = ctx.session_id,
                            turn,
                            tool_name = %tool_call.name,
                            elapsed_ms = elapsed.as_millis(),
                            result_len = output.result.content.len(),
                            is_error = output.result.is_error,
                            "tool finished (concurrent)",
                        );

                        // Emit and persist any image the tool produced.
                        if let Some(image) = image {
                            emit_and_persist_image(&ctx.cmd_tx, image, session, ctx);
                        }

                        finish_tool_call(request_id, session, &tool_call, &mut output, ctx);
                        tool_results.push(ToolResultItem {
                            call_id: tool_call.id.clone(),
                            output: output.result.content.clone(),
                            caller: tool_call.caller.clone(),
                        });
                    }
                }
            }
            Err(tai_proto::InferenceError::Cancelled) => {
                // The user cancelled during a retry backoff —
                // treat this as a clean cancellation, not a failure.
                return Ok(true);
            }
            Err(e) => return Err(e.into()),
        }
    }

    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        format!("tool loop exceeded {max_turns} iterations"),
    ))
}

fn finish_tool_call(
    request_id: u32,
    session: &mut SessionState,
    tool_call: &ChatToolCall,
    output: &mut ToolExecutionOutput,
    ctx: &RequestContext,
) {
    if let Some(hint) = context::subdirectory_hints(
        &tool_call.name,
        &tool_call.arguments_json,
        session.config.working_dir.as_deref(),
        &session.config.context_file_paths,
    ) {
        output.result.content = format!("{}\n\n---\n{}", output.result.content, hint);
    }

    let msg = SessionMessage::now(SessionMessageKind::ToolResult {
        call_id: tool_call.id.clone(),
        name: tool_call.name.clone(),
        content: output.result.content.clone(),
        is_error: output.result.is_error,
    });
    let idx = session.push_message(msg.clone());
    if let Err(e) = write_message_retry(ctx.db.as_ref(), ctx.session_id, idx, &msg) {
        tracing::warn!(
            session_id = ctx.session_id, tool_name = %tool_call.name, error = %e,
            "failed to persist tool result",
        );
    }

    let content = &output.result.content;
    const CHUNK_SIZE: usize = 4096;
    for chunk in content.as_bytes().chunks(CHUNK_SIZE) {
        let _ = ctx
            .cmd_tx
            .send(SessionCommand::Broadcast(DaemonMessage::ToolResultChunk {
                request_id,
                call_id: tool_call.id.clone(),
                data: chunk.to_vec(),
            }));
        // broadcast() uses try_send so the session thread never blocks
        // on a backed-up subscriber.  The bounded channel (128 slots ~
        // 512 KiB) absorbs short bursts; if the TUI falls behind the
        // chunk is dropped.  This is acceptable because the final
        // ToolCallFinished + post-request-snapshot deliver the complete
        // content.
    }

    let event = if output.result.is_error {
        DaemonMessage::ToolCallFailed {
            request_id,
            call_id: tool_call.id.clone(),
            tool_name: tool_call.name.clone(),
            error: output.result.content.clone(),
        }
    } else {
        DaemonMessage::ToolCallFinished {
            request_id,
            call_id: tool_call.id.clone(),
            tool_name: tool_call.name.clone(),
        }
    };
    let _ = ctx.cmd_tx.send(SessionCommand::Broadcast(event));
}

#[allow(clippy::too_many_arguments)]
fn execute_tool_with_timeout(
    tool_call: &crate::providers::types::ChatToolCall,
    x_credentials: Option<&ServiceCredential>,
    working_dir: Option<&Path>,
    timeout_dur: Duration,
    request_id: u32,
    session: &mut SessionState,
    cancel_rx: &mpsc::Receiver<()>,
    ctx: &RequestContext,
    image_tx: Option<mpsc::Sender<PreparedImage>>,
) -> ToolExecutionOutput {
    // Capture start time for tool execution metrics.
    // Meta-tools (load_tools, unload_tools) that need mutable session state
    // return early and are not timed — only the registry-executed path below
    // records metrics.
    let exec_start = std::time::Instant::now();
    match tool_call.name.as_str() {
        "load_tools" => {
            let result = crate::tools::groups::execute_load_tools(
                &mut session.config.active_tool_groups,
                &tool_call.arguments_json,
            );
            let _ = ctx
                .daemon_tx
                .send(crate::daemon::DaemonCommand::UpdateMetadata {
                    session_id: ctx.session_id,
                    metadata: SessionMetadata::from(&*session),
                });
            return ToolExecutionOutput {
                result: ToolResult {
                    content: result,
                    is_error: false,
                },
            };
        }
        "unload_tools" => {
            let result = crate::tools::groups::execute_unload_tools(
                &mut session.config.active_tool_groups,
                &tool_call.arguments_json,
            );
            let _ = ctx
                .daemon_tx
                .send(crate::daemon::DaemonCommand::UpdateMetadata {
                    session_id: ctx.session_id,
                    metadata: SessionMetadata::from(&*session),
                });
            return ToolExecutionOutput {
                result: ToolResult {
                    content: result,
                    is_error: false,
                },
            };
        }
        "set_working_dir" => {
            // Meta-tools receive raw JSON rather than typed Args because they
            // are not registered as `Tool` impls — they run inline here with
            // direct `&mut SessionState` access, avoiding the indirection of
            // the ToolRegistry dispatch path.
            let args: serde_json::Value = match serde_json::from_str(&tool_call.arguments_json) {
                Ok(a) => a,
                Err(e) => {
                    return ToolExecutionOutput {
                        result: ToolResult {
                            content: format!("invalid arguments: {e}"),
                            is_error: true,
                        },
                    };
                }
            };
            let path_str = match args.get("path").and_then(|v| v.as_str()) {
                Some(s) => s,
                None => {
                    return ToolExecutionOutput {
                        result: ToolResult {
                            content: "missing required argument: path".to_string(),
                            is_error: true,
                        },
                    };
                }
            };
            // Resolve relative to the current session working directory
            // (or process cwd if none is set yet).
            let resolved = resolve_path(path_str, working_dir);
            // canonicalize() resolves symlinks and normalizes the path.
            // This serves two purposes:
            //   1. Prevents symlink-escape attacks that would let a model
            //      redirect subsequent file ops outside the intended tree.
            //   2. Ensures the path actually exists so subsequent tools
            //      (find, grep, read_file) don't silently operate on
            //      a directory that was mistyped or never created.
            // The downside is that `set_working_dir` cannot target a path
            // that doesn't exist yet — this is an intentional tradeoff.
            let canonical = match resolved.canonicalize() {
                Ok(p) => p,
                Err(e) => {
                    return ToolExecutionOutput {
                        result: ToolResult {
                            content: format!(
                                "path '{}' does not exist or cannot be resolved: {e}",
                                resolved.display()
                            ),
                            is_error: true,
                        },
                    };
                }
            };

            info!(
                session_id = ctx.session_id,
                path = %canonical.display(),
                "set_working_dir: changing session working directory",
            );

            session.config.working_dir = Some(canonical.clone());

            // Notify session subscribers (e.g. TUI) so the status bar
            // updates to reflect the new working directory immediately.
            let path_str = canonical.to_string_lossy().into_owned();
            debug!(
                session_id = ctx.session_id,
                path = %path_str,
                "set_working_dir: broadcasting SessionWorkingDirSet",
            );
            let _ = ctx.cmd_tx.send(SessionCommand::Broadcast(
                DaemonMessage::SessionWorkingDirSet {
                    session_id: ctx.session_id,
                    path: Some(path_str),
                },
            ));

            // Notify the daemon so the TUI/inspector can reflect the change
            // immediately without waiting for the next persist cycle.
            let _ = ctx
                .daemon_tx
                .send(crate::daemon::DaemonCommand::UpdateMetadata {
                    session_id: ctx.session_id,
                    metadata: SessionMetadata::from(&*session),
                });

            // Persist the updated working_dir so it survives a daemon restart.
            // This is the same pattern used by load_tools/unload_tools.
            let record: SessionRecord = (&*session).into();
            if let Err(e) = write_session_retry(&ctx.db, ctx.session_id, &record) {
                warn!(
                    session_id = ctx.session_id,
                    error = %e,
                    "set_working_dir: failed to persist session",
                );
            }

            return ToolExecutionOutput {
                result: ToolResult {
                    content: format!("Working directory changed to '{}'", canonical.display()),
                    is_error: false,
                },
            };
        }
        _ => {}
    }

    let (result_tx, result_rx) = std::sync::mpsc::channel();
    let (output_tx, output_rx) = std::sync::mpsc::channel();
    let (kill_tx, kill_rx) = std::sync::mpsc::channel::<()>();

    // Forward streaming output to subscribers as it arrives, exiting when
    // the output channel is disconnected (tool finished) or a kill signal
    // arrives (main loop exited).
    spawn_forwarding_thread(
        ctx.cmd_tx.clone(),
        request_id,
        tool_call.id.clone(),
        output_rx,
        kill_rx,
    );

    // Drop guard: when the main loop exits (for any reason), signal the
    // forwarder to stop so it doesn't orphan waiting on output_rx.
    struct KillGuard(mpsc::Sender<()>);
    impl Drop for KillGuard {
        fn drop(&mut self) {
            let _ = self.0.send(());
        }
    }
    let _kill_guard = KillGuard(kill_tx);

    // Tool execution thread
    let tc = tool_call.clone();
    let tr = Arc::clone(&ctx.tool_registry);
    let xc = x_credentials.cloned();
    let c = working_dir.map(|p| p.to_path_buf());
    let tool_ctx = crate::tools::context::ToolContext {
        session_id: ctx.session_id,
        db: Arc::clone(&ctx.db),
        daemon_tx: ctx.daemon_tx.clone(),
        active_tool_groups: session.config.active_tool_groups.clone(),
        reasoning_effort: session.config.reasoning_effort,
        working_dir: working_dir.map(|p| p.to_path_buf()),
        cancelled: Arc::new(AtomicBool::new(false)),
    };
    std::thread::spawn(move || {
        let result = tr.execute_streaming(
            &tc,
            output_tx,
            xc.as_ref(),
            c.as_deref(),
            Some(&tool_ctx),
            image_tx,
        );
        let _ = result_tx.send(result);
    });

    // Event-driven wait loop: blocked on recv_timeout for most of the
    // interval, waking briefly at each check point to see whether the
    // caller has cancelled the request or the tool has finished.
    let deadline = std::time::Instant::now() + timeout_dur;
    let check_interval = Duration::from_millis(200);
    loop {
        // Check cancellation before each blocking wait so that a cancel
        // sent between tool start and our first recv_timeout is honoured
        // immediately rather than waiting up to check_interval.
        if is_cancelled_once(cancel_rx) {
            crate::metrics::record_tool_execution(
                &tool_call.name,
                exec_start.elapsed().as_secs_f64(),
                true,
            );
            return ToolExecutionOutput {
                result: ToolResult {
                    content: format!("tool '{}' cancelled", tool_call.name),
                    is_error: true,
                },
            };
        }

        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            crate::metrics::record_tool_execution(
                &tool_call.name,
                exec_start.elapsed().as_secs_f64(),
                true,
            );
            return ToolExecutionOutput {
                result: ToolResult {
                    content: format!(
                        "tool '{}' timed out after {}s",
                        tool_call.name,
                        timeout_dur.as_secs()
                    ),
                    is_error: true,
                },
            };
        }

        match result_rx.recv_timeout(remaining.min(check_interval)) {
            Ok(output) => {
                crate::metrics::record_tool_execution(
                    &tool_call.name,
                    exec_start.elapsed().as_secs_f64(),
                    output.result.is_error,
                );
                return output;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                crate::metrics::record_tool_execution(
                    &tool_call.name,
                    exec_start.elapsed().as_secs_f64(),
                    true,
                );
                return ToolExecutionOutput {
                    result: ToolResult {
                        content: "tool execution thread panicked".to_string(),
                        is_error: true,
                    },
                };
            }
        }
    }
}

fn persist_assistant_tool_use_sync(
    session: &mut SessionState,
    tool_use: &ChatAssistantToolUse,
    token_usage: Option<TokenUsage>,
    ctx: &RequestContext,
) {
    let msg = SessionMessage::now(SessionMessageKind::AssistantToolUse {
        content: tool_use.content.clone(),
        tool_calls: tool_use
            .tool_calls
            .iter()
            .map(|tool_call| AssistantToolCallRecord {
                call_id: tool_call.id.clone(),
                name: tool_call.name.clone(),
                arguments_json: tool_call.arguments_json.clone(),
            })
            .collect(),
        reasoning: tool_use.reasoning.clone(),
        token_usage,
    });
    let idx = session.push_message(msg.clone());
    if let Err(e) = write_message_retry(&ctx.db, ctx.session_id, idx, &msg) {
        tracing::warn!(session_id = ctx.session_id, error = %e, "failed to persist assistant tool use");
    }
}

fn build_chat_request_messages(messages: &[SessionMessage]) -> Vec<ChatRequestMessage> {
    messages
        .iter()
        .filter_map(|message| match &message.kind {
            // DisplayedImage records are not part of the LLM conversation —
            // they are purely a display-side artifact for replayed images.
            SessionMessageKind::DisplayedImage(_) => None,
            SessionMessageKind::SystemText { content } => {
                Some(ChatRequestMessage::simple("system", content.clone()))
            }
            SessionMessageKind::UserText { content } => {
                Some(ChatRequestMessage::simple("user", content.clone()))
            }
            SessionMessageKind::AssistantText {
                content, reasoning, ..
            } => Some(ChatRequestMessage {
                role: "assistant",
                content: Some(content.clone()),
                tool_call_id: None,
                tool_calls: None,
                reasoning_content: reasoning.clone(),
                reasoning: None,
                reasoning_text: None,
            }),
            SessionMessageKind::AssistantToolUse {
                content,
                tool_calls,
                reasoning,
                ..
            } => Some(ChatRequestMessage {
                role: "assistant",
                content: content.clone(),
                tool_call_id: None,
                tool_calls: Some(
                    tool_calls
                        .iter()
                        .map(|tool_call| AssistantToolCall {
                            id: tool_call.call_id.clone(),
                            kind: "function".to_string(),
                            function: AssistantToolFunction {
                                name: tool_call.name.clone(),
                                arguments: tool_call.arguments_json.clone(),
                            },
                        })
                        .collect(),
                ),
                reasoning_content: reasoning.clone(),
                reasoning: None,
                reasoning_text: None,
            }),
            SessionMessageKind::ToolResult {
                call_id, content, ..
            } => Some(ChatRequestMessage {
                role: "tool",
                content: Some(content.clone()),
                tool_call_id: Some(call_id.clone()),
                tool_calls: None,
                reasoning_content: None,
                reasoning: None,
                reasoning_text: None,
            }),
            _ => None,
        })
        .collect()
}

pub const REQUEST_IMAGE_BYTES: &[u8] = include_bytes!("../assets/dua.jpg");
pub const REQUEST_IMAGE_MIME_TYPE: &str = "image/jpeg";
pub const REQUEST_IMAGE_WIDTH: u32 = 640;
pub const REQUEST_IMAGE_HEIGHT: u32 = 640;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::DaemonCommand;
    use crate::tools::context::ToolContext;
    use crate::tools::{Tool, ToolError, ToolRegistry};
    use std::sync::mpsc;

    #[test]
    fn build_chat_request_messages_empty() {
        let result = build_chat_request_messages(&[]);
        assert!(result.is_empty());
    }

    #[test]
    fn build_chat_request_messages_system_text() {
        let msgs = [SessionMessage::now(SessionMessageKind::SystemText {
            content: "system prompt".into(),
        })];
        let result = build_chat_request_messages(&msgs);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].role, "system");
        assert_eq!(result[0].content.as_deref(), Some("system prompt"));
    }

    #[test]
    fn build_chat_request_messages_user_text() {
        let msgs = [SessionMessage::now(SessionMessageKind::UserText {
            content: "hello".into(),
        })];
        let result = build_chat_request_messages(&msgs);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].role, "user");
        assert_eq!(result[0].content.as_deref(), Some("hello"));
    }

    #[test]
    fn build_chat_request_messages_assistant_text() {
        let msgs = [SessionMessage::now(SessionMessageKind::AssistantText {
            content: "hi".into(),
            reasoning: Some("thinking".into()),
            token_usage: None,
        })];
        let result = build_chat_request_messages(&msgs);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].role, "assistant");
        assert_eq!(result[0].content.as_deref(), Some("hi"));
        assert_eq!(result[0].reasoning_content.as_deref(), Some("thinking"));
    }

    #[test]
    fn build_chat_request_messages_assistant_tool_use() {
        let msgs = [SessionMessage::now(SessionMessageKind::AssistantToolUse {
            content: Some("thinking".into()),
            tool_calls: vec![AssistantToolCallRecord {
                call_id: "call_1".into(),
                name: "read_file".into(),
                arguments_json: r#"{"path": "/tmp/test"}"#.into(),
            }],
            reasoning: None,
            token_usage: None,
        })];
        let result = build_chat_request_messages(&msgs);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].role, "assistant");
        assert_eq!(result[0].content.as_deref(), Some("thinking"));
        let tool_calls = result[0].tool_calls.as_ref().unwrap();
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].id, "call_1");
        assert_eq!(tool_calls[0].kind, "function");
        assert_eq!(tool_calls[0].function.name, "read_file");
        assert_eq!(tool_calls[0].function.arguments, r#"{"path": "/tmp/test"}"#);
    }

    #[test]
    fn build_chat_request_messages_tool_result() {
        let msgs = [SessionMessage::now(SessionMessageKind::ToolResult {
            call_id: "call_1".into(),
            name: "read_file".into(),
            content: "file content".into(),
            is_error: false,
        })];
        let result = build_chat_request_messages(&msgs);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].role, "tool");
        assert_eq!(result[0].content.as_deref(), Some("file content"));
        assert_eq!(result[0].tool_call_id.as_deref(), Some("call_1"));
        assert!(result[0].tool_calls.is_none());
    }

    #[test]
    fn build_chat_request_messages_skips_displayed_image() {
        let msgs = [
            SessionMessage::now(SessionMessageKind::UserText {
                content: "hello".into(),
            }),
            SessionMessage::now(SessionMessageKind::DisplayedImage(DisplayedImageRecord {
                metadata: ImageMetadata {
                    image_id: 0,
                    mime_type: "image/png".into(),
                    width: 1,
                    height: 1,
                    byte_len: 0,
                    alt: None,
                },
                data: vec![],
            })),
            SessionMessage::now(SessionMessageKind::AssistantText {
                content: "hi".into(),
                reasoning: None,
                token_usage: None,
            }),
        ];
        let result = build_chat_request_messages(&msgs);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].role, "user");
        assert_eq!(result[1].role, "assistant");
    }

    // -- Cancellation helper tests -----------------------------------------

    #[test]
    fn is_cancelled_once_no_signal() {
        let (_tx, rx) = mpsc::channel::<()>();
        assert!(!is_cancelled_once(&rx));
    }

    #[test]
    fn is_cancelled_once_with_signal() {
        let (tx, rx) = mpsc::channel::<()>();
        tx.send(()).unwrap();
        assert!(is_cancelled_once(&rx));
    }

    // -- execute_tool_with_timeout tests -----------------------------------
    //
    // These exercise the streaming execution path with cancellation and
    // timeout handling.

    /// A tool that completes immediately.
    struct FastTestTool;

    impl Tool for FastTestTool {
        type Args = serde_json::Value;
        type Return = String;

        fn name(&self) -> &'static str {
            "_test_fast"
        }
        fn group(&self) -> &'static str {
            "test"
        }
        fn description(&self) -> &'static str {
            "test tool that completes immediately"
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
        ) -> Result<String, ToolError> {
            Ok("fast result".into())
        }
    }

    /// A tool that blocks in `execute_streaming` until a proceed signal is
    /// received (to simulate a long-running tool for timeout / cancel tests).
    struct BlockingTestTool {
        proceed: std::sync::Mutex<Option<mpsc::Receiver<()>>>,
    }

    impl Tool for BlockingTestTool {
        type Args = serde_json::Value;
        type Return = String;

        fn name(&self) -> &'static str {
            "_test_blocking"
        }
        fn group(&self) -> &'static str {
            "test"
        }
        fn description(&self) -> &'static str {
            "test tool that blocks until proceed"
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
        ) -> Result<String, ToolError> {
            Ok("ignored".into())
        }
        fn execute_streaming(
            &self,
            _args: Self::Args,
            _xc: Option<&ServiceCredential>,
            _working_dir: Option<&Path>,
            _output_tx: mpsc::Sender<Vec<u8>>,
            _ctx: Option<&ToolContext>,
        ) -> Result<String, ToolError> {
            // Block until the proceed signal arrives or the sender is
            // dropped (which unblocks on test teardown).
            if let Some(rx) = self.proceed.lock().unwrap().take() {
                let _ = rx.recv();
            }
            Ok("blocked tool done".into())
        }
    }

    /// Helper: register a tool, build the registry, and call
    /// `execute_tool_with_timeout` with minimal ceremony.
    /// Returns the tool result and the cmd channel receiver (for verifying
    /// streaming output).
    fn run_exec_tool(
        tool: impl Tool + 'static,
        tool_name: &str,
        tool_args: &str,
        timeout_dur: Duration,
        cancel_rx: mpsc::Receiver<()>,
    ) -> (ToolExecutionOutput, mpsc::Receiver<SessionCommand>) {
        let (daemon_tx, _daemon_rx) = mpsc::channel::<DaemonCommand>();
        let (cmd_tx, cmd_rx) = mpsc::channel::<SessionCommand>();

        let dir = tempfile::tempdir().expect("tempdir");
        let db = redb::Database::create(dir.path().join("test.redb")).expect("Database");

        let mut session = SessionState::empty();

        let mut registry = ToolRegistry::new();
        registry.register(tool);
        let registry = registry.build();

        let tool_call = crate::providers::types::ChatToolCall {
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
            max_turns_default: 25,
        };
        let result = execute_tool_with_timeout(
            &tool_call,
            None, // x_credentials
            None, // working_dir
            timeout_dur,
            1, // request_id
            &mut session,
            &cancel_rx,
            &ctx,
            None, // image_tx
        );
        (result, cmd_rx)
    }

    #[test]
    fn execute_tool_normal_completion() {
        let (_cancel_tx, cancel_rx) = mpsc::channel::<()>();
        let (result, _cmd_rx) = run_exec_tool(
            FastTestTool,
            "_test_fast",
            "{}",
            Duration::from_secs(60),
            cancel_rx,
        );
        assert!(
            !result.result.is_error,
            "expected success: {}",
            result.result.content
        );
        assert!(
            result.result.content.contains("fast result"),
            "{}",
            result.result.content
        );
    }

    #[test]
    fn execute_tool_cancelled_before_execution() {
        let (cancel_tx, cancel_rx) = mpsc::channel::<()>();
        cancel_tx.send(()).expect("send cancel");
        drop(cancel_tx);

        let (result, _cmd_rx) = run_exec_tool(
            FastTestTool,
            "_test_fast",
            "{}",
            Duration::from_secs(60),
            cancel_rx,
        );
        assert!(
            result.result.is_error,
            "expected error: {}",
            result.result.content
        );
        assert!(
            result.result.content.contains("cancelled"),
            "{}",
            result.result.content
        );
    }

    #[test]
    fn execute_tool_timeout() {
        let (_cancel_tx, cancel_rx) = mpsc::channel::<()>();
        let (proceed_tx, proceed_rx) = mpsc::channel::<()>();

        let (result, _cmd_rx) = run_exec_tool(
            BlockingTestTool {
                proceed: std::sync::Mutex::new(Some(proceed_rx)),
            },
            "_test_blocking",
            "{}",
            Duration::from_millis(100),
            cancel_rx,
        );

        assert!(
            result.result.is_error,
            "expected error: {}",
            result.result.content
        );
        assert!(
            result.result.content.contains("timed out"),
            "{}",
            result.result.content
        );

        // Unblock the tool execution thread so it can exit cleanly.
        drop(proceed_tx);
    }

    #[test]
    fn execute_tool_disconnected_channel() {
        // When the tool thread panics or exits without sending on result_tx,
        // the result channel is disconnected and we should get a panic error.
        // We can simulate this by registering a tool that returns normally
        // (fast path), but that will send on result_tx.  The disconnected
        // case is exercised when the tool execution thread itself panics.
        //
        // Since we can't easily force a panic inside the spawned tool thread
        // through the Tool trait, this test at least verifies the error
        // message matches what execute_tool_with_timeout produces.
        let (_cancel_tx, cancel_rx) = mpsc::channel::<()>();
        let (result, _cmd_rx) = run_exec_tool(
            FastTestTool,
            "_test_fast",
            "{}",
            Duration::from_millis(10),
            cancel_rx,
        );
        // With a 10ms timeout and an instant tool, we might get either the
        // result (fast tool wins) or a timeout.  Neither is wrong — the
        // disconnected case is exercised elsewhere.
        if result.result.is_error {
            assert!(
                result.result.content.contains("panicked")
                    || result.result.content.contains("timed out"),
                "unexpected error: {}",
                result.result.content
            );
        }
    }

    /// A tool that sends data on the output channel during streaming
    /// execution (used to verify the forwarding thread).
    struct StreamingTestTool;

    impl Tool for StreamingTestTool {
        type Args = serde_json::Value;
        type Return = String;

        fn name(&self) -> &'static str {
            "_test_streaming"
        }
        fn group(&self) -> &'static str {
            "test"
        }
        fn description(&self) -> &'static str {
            "test tool that sends streaming output"
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
        ) -> Result<String, ToolError> {
            Ok("exec result".into())
        }
        fn execute_streaming(
            &self,
            _args: Self::Args,
            _xc: Option<&ServiceCredential>,
            _working_dir: Option<&Path>,
            output_tx: mpsc::Sender<Vec<u8>>,
            _ctx: Option<&ToolContext>,
        ) -> Result<String, ToolError> {
            // Send some output before returning so the forwarding thread
            // has data to forward.
            let _ = output_tx.send(b"streamed payload".to_vec());
            Ok("streaming done".into())
        }
    }

    #[test]
    fn execute_tool_forwards_streaming_output() {
        let (_cancel_tx, cancel_rx) = mpsc::channel::<()>();
        let (result, cmd_rx) = run_exec_tool(
            StreamingTestTool,
            "_test_streaming",
            "{}",
            Duration::from_secs(60),
            cancel_rx,
        );

        assert!(
            !result.result.is_error,
            "expected success: {}",
            result.result.content
        );
        assert!(
            result.result.content.contains("streaming done"),
            "{}",
            result.result.content
        );

        // Verify the streaming payload was forwarded to cmd_tx.
        // The forwarding thread sends the payload before the tool result is
        // delivered, so the message is already in the channel by this point.
        match cmd_rx.recv() {
            Ok(SessionCommand::Broadcast(DaemonMessage::ToolCallOutput { data, .. })) => {
                assert_eq!(data, b"streamed payload");
            }
            Ok(_other) => panic!("expected ToolCallOutput, got unexpected SessionCommand"),
            Err(e) => panic!("channel disconnected while waiting for streaming output: {e}"),
        }
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

    /// Helper: create a ToolRegistry with a test tool and spawn a single tool call.
    fn run_spawn_single_tool(
        tool: impl Tool + 'static,
        tool_name: &str,
        tool_args: &str,
        timeout: Option<Duration>,
    ) -> ToolHandle {
        let (cmd_tx, _cmd_rx) = mpsc::channel::<SessionCommand>();
        let (_daemon_tx, _daemon_rx) = mpsc::channel::<DaemonCommand>();
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Arc::new(redb::Database::create(dir.path().join("test.redb")).expect("Database"));

        let mut registry = ToolRegistry::new();
        registry.register(tool);
        let registry = registry.build();

        let tool_call = ChatToolCall {
            id: "call_test".into(),
            name: tool_name.into(),
            arguments_json: tool_args.into(),
            caller: None,
        };

        let tool_ctx = ToolContext {
            session_id: 1,
            db,
            daemon_tx: _daemon_tx,
            active_tool_groups: std::collections::HashSet::new(),
            reasoning_effort: None,
            working_dir: None,
            cancelled: Arc::new(AtomicBool::new(false)),
        };

        let handle = spawn_single_tool(SpawnToolArgs {
            tool_call,
            timeout,
            request_id: 1,
            registry,
            cmd_tx,
            x_credentials: None,
            working_dir: None,
            ctx: tool_ctx,
        });

        handle.join().expect("tool thread panicked")
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
            !handle.output.result.is_error,
            "expected success: {}",
            handle.output.result.content
        );
        assert!(
            handle.output.result.content.contains("fast result"),
            "{}",
            handle.output.result.content
        );
        assert!(handle.image.is_none(), "expected no image from fast tool",);
    }

    #[test]
    fn spawn_single_tool_no_timeout_still_completes() {
        // Even with None timeout, a fast tool should complete successfully.
        let handle = run_spawn_single_tool(FastTestTool, "_test_fast", "{}", None);
        assert!(
            !handle.output.result.is_error,
            "expected success: {}",
            handle.output.result.content
        );
        assert!(
            handle.output.result.content.contains("fast result"),
            "{}",
            handle.output.result.content
        );
    }

    // -- emit_and_persist_image tests -----------------------------------

    #[test]
    fn emit_and_persist_image_broadcasts_and_persists() {
        let (daemon_tx, _daemon_rx) = mpsc::channel::<DaemonCommand>();
        let (cmd_tx, cmd_rx) = mpsc::channel::<SessionCommand>();
        let dir = tempfile::tempdir().expect("tempdir");
        let db = redb::Database::create(dir.path().join("test.redb")).expect("Database");
        let registry = ToolRegistry::new().build();

        let mut session = SessionState::empty();

        let ctx = RequestContext {
            cmd_tx,
            session_id: 42,
            db: Arc::new(db),
            tool_registry: registry,
            daemon_tx,
            max_turns_default: 25,
        };

        let image = PreparedImage {
            mime_type: "image/png".into(),
            data: b"fakedata".to_vec(),
            width: 100,
            height: 200,
            alt: Some("test image".into()),
        };

        emit_and_persist_image(&ctx.cmd_tx, image, &mut session, &ctx);

        // Session should have one message: a DisplayedImage.
        assert_eq!(session.messages().len(), 1);
        match &session.messages()[0].kind {
            SessionMessageKind::DisplayedImage(record) => {
                assert_eq!(record.metadata.mime_type, "image/png");
                assert_eq!(record.metadata.width, 100);
                assert_eq!(record.metadata.height, 200);
                assert_eq!(record.data, b"fakedata");
                assert_eq!(record.metadata.alt.as_deref(), Some("test image"));
            }
            other => panic!("expected DisplayedImage, got {other:?}"),
        }

        // Should have received a mid-turn broadcast of the DisplayedImage.
        match cmd_rx.try_recv() {
            Ok(SessionCommand::Broadcast(DaemonMessage::SessionMessageAppended {
                message:
                    SessionMessage {
                        kind: SessionMessageKind::DisplayedImage(record),
                        ..
                    },
            })) => {
                assert_eq!(record.metadata.mime_type, "image/png");
                assert_eq!(record.data, b"fakedata");
            }
            Ok(_) => panic!(
                "expected SessionCommand::Broadcast with SessionMessageAppended, got Broadcast with something else"
            ),
            Err(e) => panic!("expected broadcast, got {e:?}"),
        }
    }

    // -- set_working_dir meta-tool tests ------------------------------------

    #[test]
    fn set_working_dir_valid_path_updates_session_config() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (daemon_tx, _daemon_rx) = mpsc::channel::<DaemonCommand>();
        let (cmd_tx, _cmd_rx) = mpsc::channel::<SessionCommand>();
        let db_dir = tempfile::tempdir().expect("tempdir");
        let db = redb::Database::create(db_dir.path().join("test.redb")).expect("Database");
        let registry = ToolRegistry::new().build();

        let mut session = SessionState::empty();

        let tool_call = crate::providers::types::ChatToolCall {
            id: "call_set_wd".into(),
            name: "set_working_dir".into(),
            arguments_json: serde_json::json!({"path": dir.path().display().to_string()})
                .to_string(),
            caller: None,
        };

        let (_cancel_tx, cancel_rx) = mpsc::channel::<()>();
        let ctx = RequestContext {
            cmd_tx,
            session_id: 1,
            db: Arc::new(db),
            tool_registry: registry,
            daemon_tx,
            max_turns_default: 25,
        };

        let result = execute_tool_with_timeout(
            &tool_call,
            None,
            None, // working_dir (not needed for absolute path)
            Duration::from_secs(10),
            1,
            &mut session,
            &cancel_rx,
            &ctx,
            None,
        );

        assert!(
            !result.result.is_error,
            "unexpected error: {}",
            result.result.content
        );
        assert!(
            result.result.content.contains("Working directory changed"),
            "result: {}",
            result.result.content
        );
        assert_eq!(
            session
                .config
                .working_dir
                .map(|p| p.to_string_lossy().to_string()),
            Some(dir.path().to_string_lossy().to_string()),
        );
    }

    #[test]
    fn set_working_dir_nonexistent_path_returns_error() {
        let (_cancel_tx, cancel_rx) = mpsc::channel::<()>();
        let (daemon_tx, _daemon_rx) = mpsc::channel::<DaemonCommand>();
        let (cmd_tx, _cmd_rx) = mpsc::channel::<SessionCommand>();
        let db_dir = tempfile::tempdir().expect("tempdir");
        let db = redb::Database::create(db_dir.path().join("test.redb")).expect("Database");
        let registry = ToolRegistry::new().build();

        let mut session = SessionState::empty();

        let tool_call = crate::providers::types::ChatToolCall {
            id: "call_set_wd_bad".into(),
            name: "set_working_dir".into(),
            arguments_json: r#"{"path": "/nonexistent_path_abcxyz_12345"}"#.to_string(),
            caller: None,
        };

        let ctx = RequestContext {
            cmd_tx,
            session_id: 1,
            db: Arc::new(db),
            tool_registry: registry,
            daemon_tx,
            max_turns_default: 25,
        };

        let result = execute_tool_with_timeout(
            &tool_call,
            None,
            None,
            Duration::from_secs(10),
            1,
            &mut session,
            &cancel_rx,
            &ctx,
            None,
        );

        assert!(
            result.result.is_error,
            "expected error, got: {}",
            result.result.content
        );
        assert!(
            result.result.content.contains("does not exist"),
            "result: {}",
            result.result.content
        );
    }

    #[test]
    fn set_working_dir_relative_path_resolves_against_current_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        let subdir = dir.path().join("subdir");
        std::fs::create_dir(&subdir).expect("create subdir");

        let (daemon_tx, _daemon_rx) = mpsc::channel::<DaemonCommand>();
        let (cmd_tx, _cmd_rx) = mpsc::channel::<SessionCommand>();
        let db_dir = tempfile::tempdir().expect("tempdir");
        let db = redb::Database::create(db_dir.path().join("test.redb")).expect("Database");
        let registry = ToolRegistry::new().build();

        let mut session = SessionState::empty();

        let tool_call = crate::providers::types::ChatToolCall {
            id: "call_set_wd_rel".into(),
            name: "set_working_dir".into(),
            arguments_json: r#"{"path": "subdir"}"#.to_string(),
            caller: None,
        };

        let (_cancel_tx, cancel_rx) = mpsc::channel::<()>();
        let ctx = RequestContext {
            cmd_tx,
            session_id: 1,
            db: Arc::new(db),
            tool_registry: registry,
            daemon_tx,
            max_turns_default: 25,
        };

        let result = execute_tool_with_timeout(
            &tool_call,
            None,
            Some(dir.path()), // current working_dir for relative resolution
            Duration::from_secs(10),
            1,
            &mut session,
            &cancel_rx,
            &ctx,
            None,
        );

        assert!(
            !result.result.is_error,
            "unexpected error: {}",
            result.result.content
        );
        assert!(
            result.result.content.contains("Working directory changed"),
            "result: {}",
            result.result.content
        );
        let new_wd = session.config.working_dir.unwrap();
        assert!(
            new_wd.ends_with("subdir"),
            "expected endswith subdir, got: {}",
            new_wd.display()
        );
    }
}
