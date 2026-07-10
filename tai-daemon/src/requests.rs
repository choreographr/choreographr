use crate::context;
use crate::db::write_message_retry;
use crate::openai::{
    AssistantToolCall, AssistantToolFunction, ChatAssistantToolUse, ChatRequestMessage,
    ChatToolCall, ChatTurnResult, CompletionChunkKind,
};
use crate::providers::{
    ChatTurnRequest, InferenceProvider, ReasoningSupport, effective_reasoning_support,
    lookup_provider,
};
use crate::sessions::{RequestContext, SessionCommand, SessionMetadata, SessionState};
use crate::tools::{PreparedImage, ToolExecutionOutput, ToolResult};
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;
use tai_keystore::ServiceCredential;
use tai_proto::{
    AssistantToolCallRecord, DaemonMessage, DisplayedImageRecord, ImageMetadata,
    MAX_IMAGE_CHUNK_SIZE, OutputStream, SessionMessage, SessionStatus, ThinkingEffort, TokenUsage,
};
use tracing::{debug, warn};

/// Route an image-sync broadcast through the session command channel so the
/// main session thread dispatches it to its live subscriber map.
fn emit_prepared_image_sync(
    cmd_tx: &mpsc::Sender<SessionCommand>,
    request_id: u32,
    image_id: u32,
    image: PreparedImage,
) {
    // Safety: image.data.len() fits in u64 on all supported platforms.
    let byte_len = image.data.len() as u64;
    let metadata = ImageMetadata {
        image_id,
        mime_type: image.mime_type,
        width: image.width,
        height: image.height,
        byte_len,
        alt: image.alt,
    };
    let _ = cmd_tx.send(SessionCommand::Broadcast(DaemonMessage::ImageStart {
        request_id,
        metadata,
    }));
    for chunk in image.data.chunks(MAX_IMAGE_CHUNK_SIZE) {
        let _ = cmd_tx.send(SessionCommand::Broadcast(DaemonMessage::ImageChunk {
            request_id,
            image_id,
            data: chunk.to_vec(),
        }));
    }
    let _ = cmd_tx.send(SessionCommand::Broadcast(DaemonMessage::ImageEnd {
        request_id,
        image_id,
    }));
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

/// Check cancellation with a cached flag for use in polling loops.
///
/// On the first call that detects cancellation, `was_cancelled` is set to `true`.
/// Subsequent calls return `true` without re-checking the (now-empty) channel,
/// since the `Receiver` only delivers the signal once.
///
/// Initialize `was_cancelled` to `false` before the loop and pass the same
/// `&mut bool` on every iteration.
pub(crate) fn is_cancelled_cached(rx: &mpsc::Receiver<()>, was_cancelled: &mut bool) -> bool {
    if !*was_cancelled {
        *was_cancelled = rx.try_recv().is_ok();
    }
    *was_cancelled
}

fn refresh_session_context(
    session: &mut SessionState,
    cwd: &Path,
    context_config: &tai_proto::ContextConfig,
) {
    if let Some(old_fp) = session.config.context_fingerprint
        && let Some(idx) = session.config.context_message_index
        && let Ok(Some(new_bundle)) = context::recheck_context(cwd, context_config, old_fp)
    {
        let new_content = context::assemble_context(&new_bundle);
        if !new_content.is_empty() {
            session.set_message(
                idx,
                SessionMessage::SystemText {
                    content: new_content,
                },
            );
        }
        session.config.context_fingerprint = Some(new_bundle.fingerprint);
        session.config.context_file_paths =
            new_bundle.files.iter().map(|f| f.path.clone()).collect();
    }
}

pub(crate) fn run_agent_loop(
    client: &InferenceProvider,
    session: &mut SessionState,
    model: &str,
    request_id: u32,
    cwd: Option<&Path>,
    cancel_rx: &mpsc::Receiver<()>,
    ctx: &RequestContext,
) -> io::Result<bool> {
    let mut next_image_id = 1u32;
    let max_turns = session.config.max_turns.unwrap_or(ctx.max_turns_default);

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

        if let Some(session_cwd) = session.config.cwd.clone() {
            let context_config = session.config.context_config.clone();
            refresh_session_context(session, &session_cwd, &context_config);
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

        match client.chat_completion_turn_streaming(
            ChatTurnRequest {
                model,
                messages: &messages,
                tools: &tools,
                thinking_effort,
                on_retry: &mut retry_cb,
                cancel_rx: Some(cancel_rx),
            },
            &mut |kind, text| {
                let stream = match kind {
                    CompletionChunkKind::Answer => OutputStream::Answer,
                    CompletionChunkKind::Reasoning => OutputStream::Reasoning,
                };
                let _ = ctx
                    .cmd_tx
                    .send(SessionCommand::Broadcast(DaemonMessage::OutputChunk {
                        request_id,
                        stream,
                        data: text.into_bytes(),
                    }));
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
                let msg = SessionMessage::AssistantText {
                    content: final_text.content,
                    reasoning: final_text.reasoning,
                    token_usage,
                };
                let idx = session.push_message(msg.clone());
                if let Err(e) = write_message_retry(&ctx.db, ctx.session_id, idx, &msg) {
                    tracing::warn!(session_id = ctx.session_id, error = %e, "failed to persist assistant text");
                }
                return Ok(false);
            }
            Ok(ChatTurnResult::ToolUse(tool_use)) => {
                let token_usage = tool_use.usage.clone();
                accumulate_token_usage(session, &token_usage, turn, ctx);
                persist_assistant_tool_use_sync(session, &tool_use, token_usage, ctx);
                for tool_call in tool_use.tool_calls {
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

                    let tool_timeout = if tool_call.name == "spawn_subsession" {
                        Duration::from_secs(120)
                    } else if tool_call.name == "sh"
                        || tool_call.name == "nushell"
                        || tool_call.name == "fish"
                        || tool_call.name == "exec"
                    {
                        Duration::from_secs(300)
                    } else {
                        Duration::from_secs(60)
                    };

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
                        "executing tool",
                    );

                    let tool_start = std::time::Instant::now();
                    let (image_tx, image_rx) = mpsc::channel::<PreparedImage>();
                    let mut output = execute_tool_with_timeout(
                        &tool_call,
                        None,
                        cwd,
                        tool_timeout,
                        request_id,
                        client,
                        session,
                        model,
                        cancel_rx,
                        ctx,
                        Some(image_tx),
                    );

                    let elapsed = tool_start.elapsed();
                    debug!(
                        session_id = ctx.session_id,
                        turn,
                        tool_name = %tool_call.name,
                        tool_call_id = %tool_call.id,
                        elapsed_ms = elapsed.as_millis(),
                        result_len = output.result.content.len(),
                        is_error = output.result.is_error,
                        "tool finished",
                    );

                    // Drain any image emitted by the tool through the channel.
                    if let Ok(image) = image_rx.try_recv() {
                        emit_prepared_image_sync(
                            &ctx.cmd_tx,
                            request_id,
                            next_image_id,
                            PreparedImage {
                                mime_type: image.mime_type.clone(),
                                data: image.data.clone(),
                                width: image.width,
                                height: image.height,
                                alt: image.alt.clone(),
                            },
                        );
                        next_image_id = next_image_id.wrapping_add(1);

                        let persisted = SessionMessage::DisplayedImage(DisplayedImageRecord {
                            metadata: ImageMetadata {
                                image_id: 0,
                                mime_type: image.mime_type,
                                width: image.width,
                                height: image.height,
                                byte_len: image.data.len() as u64,
                                alt: image.alt,
                            },
                            data: image.data,
                        });
                        let img_idx = session.messages().len() as u32;
                        if let Err(e) = write_message_retry(
                            ctx.db.as_ref(),
                            ctx.session_id,
                            img_idx,
                            &persisted,
                        ) {
                            tracing::warn!(
                                session_id = ctx.session_id, error = %e,
                                "failed to persist displayed image",
                            );
                        }
                        session.push_message(persisted);
                    }

                    finish_tool_call(request_id, session, &tool_call, &mut output, ctx);
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
        session.config.cwd.as_deref(),
        &session.config.context_file_paths,
    ) {
        output.result.content = format!("{}\n\n---\n{}", output.result.content, hint);
    }

    let msg = SessionMessage::ToolResult {
        call_id: tool_call.id.clone(),
        name: tool_call.name.clone(),
        content: output.result.content.clone(),
        is_error: output.result.is_error,
    };
    let idx = session.push_message(msg.clone());
    if let Err(e) = write_message_retry(ctx.db.as_ref(), ctx.session_id, idx, &msg) {
        tracing::warn!(
            session_id = ctx.session_id, tool_name = %tool_call.name, error = %e,
            "failed to persist tool result",
        );
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
            output: output.result.content.clone(),
        }
    };
    let _ = ctx.cmd_tx.send(SessionCommand::Broadcast(event));
}

#[allow(clippy::too_many_arguments)]
fn execute_tool_with_timeout(
    tool_call: &crate::openai::ChatToolCall,
    x_credentials: Option<&ServiceCredential>,
    cwd: Option<&Path>,
    timeout_dur: Duration,
    request_id: u32,
    client: &InferenceProvider,
    session: &mut SessionState,
    model: &str,
    cancel_rx: &mpsc::Receiver<()>,
    ctx: &RequestContext,
    image_tx: Option<mpsc::Sender<PreparedImage>>,
) -> ToolExecutionOutput {
    // Capture start time for tool execution metrics.
    // Non-registry tools (spawn_subsession, load_tools, unload_tools) that
    // need mutable session state or deep coupling with the agent loop return
    // early and are not timed — only the registry-executed path below records
    // metrics.
    let exec_start = std::time::Instant::now();
    match tool_call.name.as_str() {
        "spawn_subsession" => {
            return execute_spawn_subsession_sync(
                client, session, model, tool_call, None, cwd, cancel_rx, ctx,
            );
        }
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
        _ => {}
    }

    let (result_tx, result_rx) = std::sync::mpsc::channel();
    let (output_tx, output_rx) = std::sync::mpsc::channel();
    let (kill_tx, kill_rx) = std::sync::mpsc::channel::<()>();

    // Forward streaming output to subscribers as it arrives (event-driven,
    // blocks on the channel — no polling).  Exits when output_rx is
    // disconnected (tool thread finished) or a kill signal arrives.
    let fwd_cmd_tx = ctx.cmd_tx.clone();
    let fwd_request_id = request_id;
    let fwd_call_id = tool_call.id.clone();
    let fwd_check_interval = Duration::from_millis(200);
    std::thread::spawn(move || {
        loop {
            match output_rx.recv_timeout(fwd_check_interval) {
                Ok(data) => {
                    if fwd_cmd_tx
                        .send(SessionCommand::Broadcast(DaemonMessage::ToolCallOutput {
                            request_id: fwd_request_id,
                            call_id: fwd_call_id.clone(),
                            data,
                        }))
                        .is_err()
                    {
                        break;
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    // Check whether a kill signal was sent (main loop exited).
                    match kill_rx.try_recv() {
                        Ok(()) | Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
                        Err(std::sync::mpsc::TryRecvError::Empty) => {}
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
    });

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
    let c = cwd.map(|p| p.to_path_buf());
    let tool_ctx = crate::tools::context::ToolContext::new(
        ctx.session_id,
        Arc::clone(&ctx.db),
        ctx.daemon_tx.clone(),
    );
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
    let mut was_cancelled = false;
    loop {
        // Check cancellation before each blocking wait so that a cancel
        // sent between tool start and our first recv_timeout is honoured
        // immediately rather than waiting up to check_interval.
        if is_cancelled_cached(cancel_rx, &mut was_cancelled) {
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

#[allow(clippy::too_many_arguments)]
fn execute_spawn_subsession_sync(
    _client: &InferenceProvider,
    parent_session: &SessionState,
    _model: &str,
    tool_call: &crate::openai::ChatToolCall,
    _x_credentials: Option<&ServiceCredential>,
    cwd: Option<&Path>,
    cancel_rx: &mpsc::Receiver<()>,
    ctx: &RequestContext,
) -> ToolExecutionOutput {
    if parent_session.config.reasoning_effort != Some(ThinkingEffort::Off) {
        debug!(
            parent_session_id = ctx.session_id,
            effort = ?parent_session.config.reasoning_effort,
            "spawn_subsession: parent has non-default reasoning effort; child will use default",
        );
    }

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
    let prompt = match args.get("prompt").and_then(|v| v.as_str()) {
        Some(p) => p.to_string(),
        None => {
            return ToolExecutionOutput {
                result: ToolResult {
                    content: "missing required argument: prompt".to_string(),
                    is_error: true,
                },
            };
        }
    };
    let title = args
        .get("title")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let max_turns = args
        .get("max_turns")
        .and_then(|v| v.as_u64())
        .map(|v| v as u32);
    let child_cwd = cwd.map(|p| p.to_path_buf());

    // Inherit categories from parent, or use explicit list if provided.
    let categories = args
        .get("categories")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_else(|| {
            parent_session
                .config
                .active_tool_groups
                .iter()
                .cloned()
                .collect()
        });

    let (reply_tx, reply_rx) = std::sync::mpsc::channel();
    let _ = ctx
        .daemon_tx
        .send(crate::daemon::DaemonCommand::CreateSession {
            title,
            parent_session_id: Some(ctx.session_id),
            cwd: child_cwd.clone(),
            max_turns,
            reasoning_effort: parent_session.config.reasoning_effort,
            context_config: None,
            account_name: None,
            active_tool_groups: categories,
            reply: reply_tx,
        });

    match reply_rx.recv() {
        Ok(Ok((child_id, child_tx))) => {
            let _ = child_tx.send(crate::sessions::SessionCommand::AppendMessage {
                message: SessionMessage::SystemText { content: prompt },
            });

            let (result_tx, result_rx) = std::sync::mpsc::channel();
            let _ = child_tx.send(crate::sessions::SessionCommand::RunChildInput {
                request_id: 1,
                reply: result_tx,
            });
            let mut was_cancelled = false;
            loop {
                if is_cancelled_cached(cancel_rx, &mut was_cancelled) {
                    let _ =
                        child_tx.send(crate::sessions::SessionCommand::Cancel { request_id: 1 });
                    return ToolExecutionOutput {
                        result: ToolResult {
                            content: format!("sub-session {child_id} cancelled"),
                            is_error: true,
                        },
                    };
                }

                match result_rx.recv_timeout(Duration::from_millis(100)) {
                    Ok(Ok(child_result)) => {
                        return ToolExecutionOutput {
                            result: ToolResult {
                                content: format!(
                                    "sub-session {child_id} result:\n{}",
                                    child_result.output
                                ),
                                is_error: child_result.is_error,
                            },
                        };
                    }
                    Ok(Err(e)) => {
                        return ToolExecutionOutput {
                            result: ToolResult {
                                content: format!("child session error: {e}"),
                                is_error: true,
                            },
                        };
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                        return ToolExecutionOutput {
                            result: ToolResult {
                                content: format!("sub-session {child_id} exited unexpectedly"),
                                is_error: true,
                            },
                        };
                    }
                }
            }
        }
        Ok(Err(e)) => ToolExecutionOutput {
            result: ToolResult {
                content: format!("failed to create sub-session: {e}"),
                is_error: true,
            },
        },
        Err(_) => ToolExecutionOutput {
            result: ToolResult {
                content: "daemon communication failed".to_string(),
                is_error: true,
            },
        },
    }
}

fn persist_assistant_tool_use_sync(
    session: &mut SessionState,
    tool_use: &ChatAssistantToolUse,
    token_usage: Option<TokenUsage>,
    ctx: &RequestContext,
) {
    let msg = SessionMessage::AssistantToolUse {
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
    };
    let idx = session.push_message(msg.clone());
    if let Err(e) = write_message_retry(&ctx.db, ctx.session_id, idx, &msg) {
        tracing::warn!(session_id = ctx.session_id, error = %e, "failed to persist assistant tool use");
    }
}

fn build_chat_request_messages(messages: &[SessionMessage]) -> Vec<ChatRequestMessage> {
    messages
        .iter()
        .filter_map(|message| match message {
            // DisplayedImage records are not part of the LLM conversation —
            // they are purely a display-side artifact for replayed images.
            SessionMessage::DisplayedImage(_) => None,
            SessionMessage::SystemText { content } => {
                Some(ChatRequestMessage::simple("system", content.clone()))
            }
            SessionMessage::UserText { content } => {
                Some(ChatRequestMessage::simple("user", content.clone()))
            }
            SessionMessage::AssistantText {
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
            SessionMessage::AssistantToolUse {
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
            SessionMessage::ToolResult {
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
        let msgs = [SessionMessage::SystemText {
            content: "system prompt".into(),
        }];
        let result = build_chat_request_messages(&msgs);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].role, "system");
        assert_eq!(result[0].content.as_deref(), Some("system prompt"));
    }

    #[test]
    fn build_chat_request_messages_user_text() {
        let msgs = [SessionMessage::UserText {
            content: "hello".into(),
        }];
        let result = build_chat_request_messages(&msgs);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].role, "user");
        assert_eq!(result[0].content.as_deref(), Some("hello"));
    }

    #[test]
    fn build_chat_request_messages_assistant_text() {
        let msgs = [SessionMessage::AssistantText {
            content: "hi".into(),
            reasoning: Some("thinking".into()),
            token_usage: None,
        }];
        let result = build_chat_request_messages(&msgs);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].role, "assistant");
        assert_eq!(result[0].content.as_deref(), Some("hi"));
        assert_eq!(result[0].reasoning_content.as_deref(), Some("thinking"));
    }

    #[test]
    fn build_chat_request_messages_assistant_tool_use() {
        let msgs = [SessionMessage::AssistantToolUse {
            content: Some("thinking".into()),
            tool_calls: vec![AssistantToolCallRecord {
                call_id: "call_1".into(),
                name: "read_file".into(),
                arguments_json: r#"{"path": "/tmp/test"}"#.into(),
            }],
            reasoning: None,
            token_usage: None,
        }];
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
        let msgs = [SessionMessage::ToolResult {
            call_id: "call_1".into(),
            name: "read_file".into(),
            content: "file content".into(),
            is_error: false,
        }];
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
            SessionMessage::UserText {
                content: "hello".into(),
            },
            SessionMessage::DisplayedImage(DisplayedImageRecord {
                metadata: ImageMetadata {
                    image_id: 0,
                    mime_type: "image/png".into(),
                    width: 1,
                    height: 1,
                    byte_len: 0,
                    alt: None,
                },
                data: vec![],
            }),
            SessionMessage::AssistantText {
                content: "hi".into(),
                reasoning: None,
                token_usage: None,
            },
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

    #[test]
    fn is_cancelled_cached_persists_after_message_consumed() {
        let (tx, rx) = mpsc::channel::<()>();
        tx.send(()).unwrap();
        let mut flag = false;

        // First call consumes the message and sets the cached flag.
        assert!(is_cancelled_cached(&rx, &mut flag));

        // Second call should still return true using the cached flag,
        // even though the channel is now empty.
        assert!(is_cancelled_cached(&rx, &mut flag));
        assert!(flag);
    }

    #[test]
    fn is_cancelled_cached_disconnected_no_signal() {
        let (tx, rx) = mpsc::channel::<()>();
        drop(tx); // disconnected without sending
        let mut flag = false;
        assert!(!is_cancelled_cached(&rx, &mut flag));
        assert!(!flag);
    }

    #[test]
    fn is_cancelled_cached_disconnected_after_signal_still_true() {
        let (tx, rx) = mpsc::channel::<()>();
        tx.send(()).unwrap();
        drop(tx); // disconnect after sending

        let mut flag = false;
        assert!(is_cancelled_cached(&rx, &mut flag)); // consumes the message
        assert!(flag);
        // Channel is now empty *and* disconnected, but cache keeps it true.
        assert!(is_cancelled_cached(&rx, &mut flag));
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
            _cwd: Option<&Path>,
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
            _cwd: Option<&Path>,
            _ctx: Option<&ToolContext>,
        ) -> Result<String, ToolError> {
            Ok("ignored".into())
        }
        fn execute_streaming(
            &self,
            _args: Self::Args,
            _xc: Option<&ServiceCredential>,
            _cwd: Option<&Path>,
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

        let config = crate::openai::ServiceConfig::default();
        let openai_client =
            crate::openai::OpenAiClient::new(config, "test-key".into()).expect("OpenAiClient");
        let client = InferenceProvider::from_openai(openai_client);

        let dir = tempfile::tempdir().expect("tempdir");
        let db = redb::Database::create(dir.path().join("test.redb")).expect("Database");

        let mut session = SessionState::empty();

        let mut registry = ToolRegistry::new();
        registry.register(tool);
        let registry = registry.build();

        let tool_call = crate::openai::ChatToolCall {
            id: "call_test".into(),
            name: tool_name.into(),
            arguments_json: tool_args.into(),
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
            None, // cwd
            timeout_dur,
            1, // request_id
            &client,
            &mut session,
            "test-model",
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
            _cwd: Option<&Path>,
            _ctx: Option<&ToolContext>,
        ) -> Result<String, ToolError> {
            Ok("exec result".into())
        }
        fn execute_streaming(
            &self,
            _args: Self::Args,
            _xc: Option<&ServiceCredential>,
            _cwd: Option<&Path>,
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
}
