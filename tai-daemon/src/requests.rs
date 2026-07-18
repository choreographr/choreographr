use crate::context;
use crate::db::{SessionRecord, write_session_retry};
use crate::openai::{AssistantToolCall, AssistantToolFunction, ChatRequestMessage};
use crate::providers::types::{ChatToolCall, ChatTurnResult};
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
    SessionStatus, ThinkingEffort, TokenUsage,
};
use tracing::{debug, info, warn};

/// Persist a `PreparedImage` to the session's current active turn and
/// broadcast it to live subscribers immediately (mid-turn) so the image
/// appears as soon as the tool finishes rather than waiting for request
/// completion.  Used by both the serial and concurrent tool paths.
fn emit_image(
    cmd_tx: &mpsc::Sender<SessionCommand>,
    image: PreparedImage,
    tool_call_id: Option<String>,
    session: &mut SessionState,
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
    // Broadcast TurnAppended so subscribers see the image mid-turn.
    if let Some(turn) = session.turns.get(&turn_id) {
        let _ = cmd_tx.send(SessionCommand::Broadcast(DaemonMessage::TurnAppended {
            turn_id,
            turn: turn.clone(),
        }));
    }
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
                        .send(SessionCommand::Broadcast(DaemonMessage::ToolResultChunk {
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
    user_text: Option<String>,
) -> io::Result<bool> {
    let max_turns = session.config.max_turns.unwrap_or(ctx.max_turns_default);

    let mut prev_resp_id: Option<String> = None;
    let mut tool_results: Vec<ToolResultItem> = Vec::new();

    // Build system prompt and context at request time.
    let system_content = if let Some(ref working_dir) = session.config.working_dir {
        let skills = context::discover_skills(working_dir);
        let base_prompt = context::build_base_prompt(&skills, &ctx.tool_registry.groups());
        let mut content = base_prompt;
        if let Ok(bundle) = context::discover_context(working_dir, &session.config.context_config) {
            let context_str = context::assemble_context(&bundle);
            if !context_str.is_empty() {
                content.push_str("\n\n");
                content.push_str(&context_str);
            }
        }
        Some(content)
    } else {
        None
    };

    for turn_iter in 0..max_turns {
        let mut thinking_effort = session
            .config
            .reasoning_effort
            .unwrap_or(ThinkingEffort::Off);
        debug!(
            session_id = ctx.session_id,
            turn = turn_iter,
            "agent loop turn"
        );
        if thinking_effort != ThinkingEffort::Off {
            let slug = client.provider_slug();
            let catalog_entry = lookup_provider(slug);
            let reasoning_support = catalog_entry
                .map(|e| e.reasoning)
                .unwrap_or(ReasoningSupport::None);
            let effective = effective_reasoning_support(model, reasoning_support);
            if effective == ReasoningSupport::None {
                warn!(
                    session_id = ctx.session_id, turn = turn_iter, model,
                    effort = %thinking_effort.as_label(),
                    "model does not support reasoning effort, disabling",
                );
                thinking_effort = ThinkingEffort::Off;
            } else {
                debug!(
                    session_id = ctx.session_id, turn = turn_iter,
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

        // Start a new turn for this agent loop iteration.
        let turn_user_text = if turn_iter == 0 {
            user_text.clone()
        } else {
            None
        };
        let (current_turn_id, current_turn) = session.start_turn(turn_user_text);
        let _ = ctx
            .cmd_tx
            .send(SessionCommand::Broadcast(DaemonMessage::TurnAppended {
                turn_id: current_turn_id,
                turn: current_turn,
            }));
        let _ = ctx
            .cmd_tx
            .send(SessionCommand::Broadcast(DaemonMessage::Started {
                request_id,
                turn_id: current_turn_id,
            }));

        if ctx
            .cmd_tx
            .send(SessionCommand::StatusChanged(SessionStatus::Inference))
            .is_err()
        {
            return Ok(false);
        }

        let messages = build_chat_request_messages(session, system_content.as_deref());

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
                session.set_assistant_response(
                    current_turn_id,
                    Some(final_text.content),
                    final_text.reasoning,
                    Vec::new(),
                    token_usage,
                );
                // Persist and broadcast the finalized turn.
                session.finalize_turn(&ctx.db, current_turn_id)?;
                if let Some(turn) = session.turns.get(&current_turn_id) {
                    let _ =
                        ctx.cmd_tx
                            .send(SessionCommand::Broadcast(DaemonMessage::TurnFinalized {
                                turn_id: current_turn_id,
                                turn: turn.clone(),
                            }));
                }
                tool_results.clear();
                return Ok(false);
            }
            Ok(ChatTurnResult::ToolUse(tool_use)) => {
                let token_usage = tool_use.usage;
                accumulate_token_usage(session, &token_usage, turn_iter, ctx);
                session.set_assistant_response(
                    current_turn_id,
                    tool_use.content.clone(),
                    tool_use.reasoning.clone(),
                    tool_use
                        .tool_calls
                        .iter()
                        .map(|tc| AssistantToolCallRecord {
                            call_id: tc.id.clone(),
                            name: tc.name.clone(),
                            arguments_json: tc.arguments_json.clone(),
                        })
                        .collect(),
                    token_usage,
                );
                // Broadcast the updated turn with assistant tool use info.
                if let Some(turn) = session.turns.get(&current_turn_id) {
                    let _ =
                        ctx.cmd_tx
                            .send(SessionCommand::Broadcast(DaemonMessage::TurnAppended {
                                turn_id: current_turn_id,
                                turn: turn.clone(),
                            }));
                }
                // Store response_id for chaining tool results back to this turn
                prev_resp_id = tool_use.response_id;
                tool_results.clear();

                // Partition tool calls into mutators and concurrent.
                let (mutators, concurrent): (Vec<_>, Vec<_>) =
                    tool_use.tool_calls.into_iter().partition(|tc| {
                        matches!(
                            tc.name.as_str(),
                            "load_tools" | "unload_tools" | "set_working_dir"
                        )
                    });

                let _total_mutators = mutators.len();

                // ── Phase 1: Session-mutating tools (serial) ────────
                for tool_call in mutators.into_iter() {
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

                    if let Ok(image) = image_rx.try_recv() {
                        emit_image(
                            &ctx.cmd_tx,
                            image,
                            Some(tool_call.id.clone()),
                            session,
                            current_turn_id,
                        );
                    }

                    finish_tool_call(
                        request_id,
                        session,
                        &tool_call,
                        &mut output,
                        ctx,
                        current_turn_id,
                    );
                    tool_results.push(ToolResultItem {
                        call_id: tool_call.id.clone(),
                        output: output.result.content.clone(),
                        caller: tool_call.caller.clone(),
                    });
                }

                // ── Phase 2: All remaining tools (concurrent) ───────
                if !concurrent.is_empty() {
                    for tc in concurrent.iter() {
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
                        reasoning_effort: session.config.reasoning_effort,
                        selected_model: session.config.selected_model.clone(),
                        working_dir: session.config.working_dir.clone(),
                        cancelled: Arc::clone(&cancel_flag),
                    };

                    let cmd_tx = ctx.cmd_tx.clone();
                    let reg = Arc::clone(&ctx.tool_registry);

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

                    if is_cancelled_once(cancel_rx) {
                        cancel_flag.store(true, Ordering::Relaxed);
                    }
                    for ((call_id, tool_name, arguments_json, tool_start), handle) in
                        handles.into_iter()
                    {
                        if is_cancelled_once(cancel_rx) {
                            cancel_flag.store(true, Ordering::Relaxed);
                        }

                        let ToolHandle {
                            tool_call,
                            mut output,
                            image,
                        } = handle.join().unwrap_or_else(|_| ToolHandle {
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
                        });

                        let elapsed = tool_start.elapsed();

                        debug!(
                            session_id = ctx.session_id,
                            turn = turn_iter,
                            tool_name = %tool_call.name,
                            elapsed_ms = elapsed.as_millis(),
                            result_len = output.result.content.len(),
                            is_error = output.result.is_error,
                            "tool finished (concurrent)",
                        );

                        if let Some(image) = image {
                            emit_image(
                                &ctx.cmd_tx,
                                image,
                                Some(tool_call.id.clone()),
                                session,
                                current_turn_id,
                            );
                        }

                        finish_tool_call(
                            request_id,
                            session,
                            &tool_call,
                            &mut output,
                            ctx,
                            current_turn_id,
                        );
                        tool_results.push(ToolResultItem {
                            call_id: tool_call.id.clone(),
                            output: output.result.content.clone(),
                            caller: tool_call.caller.clone(),
                        });
                    }
                }
            }
            Err(tai_proto::InferenceError::Cancelled) => {
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
    turn_id: u32,
) {
    let content = output.result.content.clone();
    let is_error = output.result.is_error;

    session.add_tool_result(
        turn_id,
        tool_call.id.clone(),
        tool_call.name.clone(),
        content.clone(),
        is_error,
    );

    // Broadcast TurnAppended so subscribers see the updated turn.
    if let Some(turn) = session.turns.get(&turn_id) {
        let _ = ctx
            .cmd_tx
            .send(SessionCommand::Broadcast(DaemonMessage::TurnAppended {
                turn_id,
                turn: turn.clone(),
            }));
    }

    const CHUNK_SIZE: usize = 4096;
    for chunk in content.as_bytes().chunks(CHUNK_SIZE) {
        let _ = ctx
            .cmd_tx
            .send(SessionCommand::Broadcast(DaemonMessage::ToolResultChunk {
                request_id,
                call_id: tool_call.id.clone(),
                data: chunk.to_vec(),
            }));
    }

    let event = if is_error {
        DaemonMessage::ToolCallFailed {
            request_id,
            call_id: tool_call.id.clone(),
            tool_name: tool_call.name.clone(),
            error: content,
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
        selected_model: session.config.selected_model.clone(),
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

fn build_chat_request_messages(
    session: &SessionState,
    system_prompt: Option<&str>,
) -> Vec<ChatRequestMessage> {
    let mut messages = Vec::new();

    // Prepend system prompt and context if provided.
    if let Some(prompt) = system_prompt {
        messages.push(ChatRequestMessage::simple("system", prompt.to_string()));
    }

    for turn in session.turns.values() {
        if turn.undone {
            continue;
        }
        // User message
        if let Some(text) = &turn.user_text {
            messages.push(ChatRequestMessage::simple("user", text.clone()));
        }
        // Assistant message (text or tool calls)
        let has_tool_calls = !turn.tool_calls.is_empty();
        if turn.assistant_text.is_some() || has_tool_calls || turn.assistant_reasoning.is_some() {
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
                reasoning_content: turn.assistant_reasoning.clone(),
                reasoning: None,
                reasoning_text: None,
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
    use crate::tools::context::ToolContext;
    use crate::tools::{Tool, ToolError, ToolRegistry};
    use std::sync::mpsc;

    fn make_session_with_turns() -> SessionState {
        let mut session = SessionState::empty();
        let (tid0, _) = session.start_turn(Some("hello".into()));
        session.set_assistant_response(tid0, Some("hi".into()), None, vec![], None);
        session
    }

    #[test]
    fn build_chat_request_messages_empty() {
        let session = SessionState::empty();
        let result = build_chat_request_messages(&session, None);
        assert!(result.is_empty());
    }

    #[test]
    fn build_chat_request_messages_with_system_prompt() {
        let session = SessionState::empty();
        let result = build_chat_request_messages(&session, Some("system prompt"));
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].role, "system");
        assert_eq!(result[0].content.as_deref(), Some("system prompt"));
    }

    #[test]
    fn build_chat_request_messages_user_and_assistant() {
        let session = make_session_with_turns();
        let result = build_chat_request_messages(&session, None);
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
        session.set_assistant_response(
            tid,
            Some("thinking".into()),
            None,
            vec![AssistantToolCallRecord {
                call_id: "call_1".into(),
                name: "ls".into(),
                arguments_json: r#"{"path": "."}"#.into(),
            }],
            None,
        );
        session.add_tool_result(tid, "call_1".into(), "ls".into(), "file.txt".into(), false);

        let result = build_chat_request_messages(&session, None);
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
        session.set_assistant_response(tid0, Some("ok".into()), None, vec![], None);
        let (tid1, _) = session.start_turn(Some("hidden".into()));
        session.set_assistant_response(tid1, Some("nope".into()), None, vec![], None);
        if let Some(turn) = session.turns.get_mut(&tid1) {
            turn.undone = true;
        }

        let result = build_chat_request_messages(&session, None);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].role, "user");
        assert_eq!(result[0].content.as_deref(), Some("visible"));
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
            None,
            None,
            timeout_dur,
            1,
            &mut session,
            &cancel_rx,
            &ctx,
            None,
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

        drop(proceed_tx);
    }

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

        match cmd_rx.recv() {
            Ok(SessionCommand::Broadcast(DaemonMessage::ToolResultChunk { data, .. })) => {
                assert_eq!(data, b"streamed payload");
            }
            Ok(_other) => panic!("expected ToolResultChunk, got unexpected SessionCommand"),
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
            selected_model: None,
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
        assert!(handle.image.is_none(), "expected no image from fast tool");
    }

    #[test]
    fn spawn_single_tool_no_timeout_still_completes() {
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
}
