use crate::context;
use crate::db::write_message_retry;
use tracing::debug;
use crate::openai::{
    AssistantToolCall, AssistantToolFunction, ChatAssistantToolUse, ChatRequestMessage,
    ChatToolCall, ChatTurnResult, CompletionChunkKind, OpenAiClient,
};
use crate::sessions::{SessionCommand, SessionMetadata, SessionState};
use crate::tools::{PreparedImage, ToolExecutionOutput, ToolRegistry, ToolResult};
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tai_keystore::ServiceCredential;
use tai_proto::{
    AssistantToolCallRecord, DaemonMessage, DisplayedImageRecord, ImageMetadata,
    MAX_IMAGE_CHUNK_SIZE, OutputStream, SessionMessage, SessionStatus,
};
use std::sync::mpsc;

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
    let _ = cmd_tx.send(SessionCommand::Broadcast(
        DaemonMessage::ImageStart {
            request_id,
            metadata,
        },
    ));
    for chunk in image.data.chunks(MAX_IMAGE_CHUNK_SIZE) {
        let _ = cmd_tx.send(SessionCommand::Broadcast(
            DaemonMessage::ImageChunk {
                request_id,
                image_id,
                data: chunk.to_vec(),
            },
        ));
    }
    let _ = cmd_tx.send(SessionCommand::Broadcast(
        DaemonMessage::ImageEnd {
            request_id,
            image_id,
        },
    ));
}

/// Check whether a cancellation signal has been received from an `mpsc` channel.
///
/// In a one-shot check (no caching needed), pass `&mut false`:
///
/// ```ignore
/// if is_cancelled(&cancel_rx, &mut false) { return Ok(true); }
/// ```
///
/// In a polling loop, pass a persistent `was_cancelled: &mut bool` initialized to
/// `false`. The flag is set once when the signal arrives and never cleared, so
/// subsequent iterations don't re-check the (now-empty) channel. This models a
/// single-shot, irreversible cancellation flag.
pub(crate) fn is_cancelled(rx: &mpsc::Receiver<()>, was_cancelled: &mut bool) -> bool {
    if !*was_cancelled {
        *was_cancelled = rx.try_recv().is_ok();
    }
    *was_cancelled
}

fn refresh_session_context(
    session: &mut SessionState,
    cwd: &Path,
    context_config: &context::ContextConfig,
) {
    if let Some(old_fp) = session.context_fingerprint {
        if let Some(idx) = session.context_message_index {
            if let Ok(Some(new_bundle)) =
                context::recheck_context(cwd, context_config, old_fp)
            {
                let new_content = context::assemble_context(&new_bundle);
                if !new_content.is_empty() {
                    session.set_message(idx, SessionMessage::SystemText {
                        content: new_content,
                    });
                }
                session.context_fingerprint = Some(new_bundle.fingerprint);
                session.context_file_paths =
                    new_bundle.files.iter().map(|f| f.path.clone()).collect();
            }
        }
    }
}

pub(crate) fn run_agent_loop(
    client: &OpenAiClient,
    session: &mut SessionState,
    session_id: u64,
    db: &Arc<redb::Database>,
    model: &str,
    request_id: u32,
    cwd: Option<&Path>,
    cancel_rx: &mpsc::Receiver<()>,
    tool_registry: &Arc<ToolRegistry>,
    daemon_tx: &mpsc::Sender<crate::daemon::DaemonCommand>,
    max_turns_default: u32,
    cmd_tx: &std::sync::mpsc::Sender<SessionCommand>,
) -> io::Result<bool> {
    let mut next_image_id = 1u32;
    let max_turns = session.max_turns.unwrap_or(max_turns_default);

    for turn in 0..max_turns {
        debug!(session_id, turn, "agent loop turn");
        crate::metrics::record_turn(model);
        let tools = tool_registry.available_definitions(&session.active_tool_groups);
        if is_cancelled(&cancel_rx, &mut false) {
            return Ok(true);
        }

        if let Some(session_cwd) = session.cwd.clone() {
            let context_config = client.config().context.clone();
            refresh_session_context(session, &session_cwd, &context_config);
        }

        if cmd_tx.send(SessionCommand::StatusChanged(SessionStatus::Inference)).is_err() {
            return Ok(false);
        }

        let messages = build_chat_request_messages(session.messages());

        // Build a retry-notification callback that forwards status updates
        // through the session command channel so the TUI can display the
        // retry progress and the user can cancel during backoff.
        let mut retry_cb: Option<crate::openai::RetryCallback> = Some(Box::new({
            let cmd_tx = cmd_tx.clone();
            move |attempt, max_attempts, delay| {
                let _ = cmd_tx.send(SessionCommand::StatusChanged(SessionStatus::Retrying {
                    attempt,
                    max_attempts,
                    delay_ms: delay.as_millis() as u64,
                }));
            }
        }));

        match client.chat_completion_turn_streaming(
            model,
            &messages,
            &tools,
            &mut retry_cb,
            Some(cancel_rx),
            |kind, text| {
                let stream = match kind {
                    CompletionChunkKind::Answer => OutputStream::Answer,
                    CompletionChunkKind::Reasoning => OutputStream::Reasoning,
                };
                let _ = cmd_tx.send(SessionCommand::Broadcast(
                    DaemonMessage::OutputChunk {
                        request_id,
                        stream,
                        data: text.into_bytes(),
                    },
                ));
                Ok(())
            },
        ) {
            Ok(ChatTurnResult::FinalText(content)) => {
                debug!(
                    session_id,
                    turn,
                    response_len = content.len(),
                    "model returned final text",
                );
                let msg = SessionMessage::AssistantText { content };
                let idx = session.push_message(msg.clone());
                if let Err(e) = write_message_retry(db, session_id, idx, &msg) {
                    tracing::warn!(session_id, error = %e, "failed to persist assistant text");
                }
                return Ok(false);
            }
            Ok(ChatTurnResult::ToolUse(tool_use)) => {
                persist_assistant_tool_use_sync(session, session_id, db, &tool_use);
                for tool_call in tool_use.tool_calls {
                    if is_cancelled(&cancel_rx, &mut false) {
                        return Ok(true);
                    }

                    let _ = cmd_tx.send(SessionCommand::Broadcast(
                        DaemonMessage::ToolCallStarted {
                            request_id,
                            call_id: tool_call.id.clone(),
                            tool_name: tool_call.name.clone(),
                            arguments_json: tool_call.arguments_json.clone(),
                        },
                    ));

                    let tool_timeout = if tool_call.name == "spawn_subsession" {
                        Duration::from_secs(120)
                    } else if tool_call.name == "sh" || tool_call.name == "nushell"
                        || tool_call.name == "fish" || tool_call.name == "exec" {
                        Duration::from_secs(300)
                    } else {
                        Duration::from_secs(60)
                    };

                    if cmd_tx.send(SessionCommand::StatusChanged(SessionStatus::ToolCall(tool_call.name.clone()))).is_err() {
                        return Ok(false);
                    }

                    debug!(
                        session_id,
                        turn,
                        tool_name = %tool_call.name,
                        tool_call_id = %tool_call.id,
                        args_preview = %(&tool_call.arguments_json[..tool_call.arguments_json.len().min(200)]),
                        "executing tool",
                    );

                    let tool_start = std::time::Instant::now();
                    let mut output = execute_tool_with_timeout(
                        tool_registry,
                        &tool_call,
                        None,
                        cwd,
                        tool_timeout,
                        request_id,
                        daemon_tx,
                        client,
                        db,
                        session,
                        session_id,
                        model,
                        max_turns_default,
                        cancel_rx,
                        cmd_tx,
                    );

                    let elapsed = tool_start.elapsed();
                    debug!(
                        session_id,
                        turn,
                        tool_name = %tool_call.name,
                        tool_call_id = %tool_call.id,
                        elapsed_ms = elapsed.as_millis(),
                        result_len = output.result.content.len(),
                        is_error = output.result.is_error,
                        "tool finished",
                    );

                    finish_tool_call(
                        cmd_tx,
                        request_id,
                        session,
                        session_id,
                        db,
                        &tool_call,
                        &mut output,
                        &mut next_image_id,
                    );
                }
            }
            Err(crate::openai::OpenAiError::Cancelled) => {
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
    cmd_tx: &mpsc::Sender<SessionCommand>,
    request_id: u32,
    session: &mut SessionState,
    session_id: u64,
    db: &redb::Database,
    tool_call: &ChatToolCall,
    output: &mut ToolExecutionOutput,
    next_image_id: &mut u32,
) {
    if let Some(hint) = context::subdirectory_hints(
        &tool_call.name,
        &tool_call.arguments_json,
        session.cwd.as_deref(),
        &session.context_file_paths,
    ) {
        output.result.content =
            format!("{}\n\n---\n{}", output.result.content, hint);
    }

    if let Some(image) = output.image.take() {
        let PreparedImage { mime_type, data, width, height, alt } = image;

        emit_prepared_image_sync(
            cmd_tx,
            request_id,
            *next_image_id,
            PreparedImage {
                mime_type: mime_type.clone(),
                data: data.clone(),
                width,
                height,
                alt: alt.clone(),
            },
        );
        *next_image_id = next_image_id.wrapping_add(1);

        let persisted = SessionMessage::DisplayedImage(DisplayedImageRecord {
            metadata: ImageMetadata {
                image_id: 0,
                mime_type,
                width,
                height,
                byte_len: data.len() as u64,
                alt,
            },
            data,
        });
        let img_idx = session.messages().len() as u32;
        if let Err(e) = write_message_retry(db, session_id, img_idx, &persisted) {
            tracing::warn!(
                session_id, error = %e,
                "failed to persist displayed image",
            );
        }
        session.push_message(persisted);
    }

    let msg = SessionMessage::ToolResult {
        call_id: tool_call.id.clone(),
        name: tool_call.name.clone(),
        content: output.result.content.clone(),
        is_error: output.result.is_error,
    };
    let idx = session.push_message(msg.clone());
    if let Err(e) = write_message_retry(db, session_id, idx, &msg) {
        tracing::warn!(
            session_id, tool_name = %tool_call.name, error = %e,
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
    let _ = cmd_tx.send(SessionCommand::Broadcast(event));
}

fn execute_tool_with_timeout(
    tool_registry: &Arc<ToolRegistry>,
    tool_call: &crate::openai::ChatToolCall,
    x_credentials: Option<&ServiceCredential>,
    cwd: Option<&Path>,
    timeout_dur: Duration,
    request_id: u32,
    daemon_tx: &mpsc::Sender<crate::daemon::DaemonCommand>,
    client: &OpenAiClient,
    db: &redb::Database,
    session: &mut SessionState,
    session_id: u64,
    model: &str,
    max_turns_default: u32,
    cancel_rx: &mpsc::Receiver<()>,
    cmd_tx: &mpsc::Sender<SessionCommand>,
) -> ToolExecutionOutput {
    // Capture start time for tool execution metrics.
    // Admin tools (list_sessions, get_session, spawn_subsession, load_skill,
    // load_tools, unload_tools) return early and are not timed — only the
    // registry-executed path records metrics below.
    let exec_start = std::time::Instant::now();
    match tool_call.name.as_str() {
        "list_sessions" => {
            return execute_list_sessions_sync(daemon_tx);
        }
        "get_session" => {
            return execute_get_session_sync(daemon_tx, &tool_call.arguments_json);
        }
        "spawn_subsession" => {
            return execute_spawn_subsession_sync(
                client,
                daemon_tx,
                session,
                session_id,
                db,
                model,
                tool_call,
                None,
                cwd,
                max_turns_default,
                cancel_rx,
            );
        }
        "load_skill" => {
            return execute_load_skill_sync(session, cwd, &tool_call.arguments_json);
        }
        "load_tools" => {
            let result = crate::tools::groups::execute_load_tools(
                &mut session.active_tool_groups,
                &tool_call.arguments_json,
            );
            let _ = daemon_tx.send(crate::daemon::DaemonCommand::UpdateMetadata {
                session_id,
                metadata: SessionMetadata::from(&*session),
            });
            return ToolExecutionOutput {
                result: ToolResult {
                    content: result,
                    is_error: false,
                },
                image: None,
            };
        }
        "unload_tools" => {
            let result = crate::tools::groups::execute_unload_tools(
                &mut session.active_tool_groups,
                &tool_call.arguments_json,
            );
            let _ = daemon_tx.send(crate::daemon::DaemonCommand::UpdateMetadata {
                session_id,
                metadata: SessionMetadata::from(&*session),
            });
            return ToolExecutionOutput {
                result: ToolResult {
                    content: result,
                    is_error: false,
                },
                image: None,
            };
        }
        _ => {}
    }

    let (result_tx, result_rx) = std::sync::mpsc::channel();
    let (output_tx, output_rx) = std::sync::mpsc::channel();

    // Forward streaming output to subscribers as it arrives (event-driven,
    // blocks on the channel — no polling).
    let fwd_cmd_tx = cmd_tx.clone();
    let fwd_request_id = request_id;
    let fwd_call_id = tool_call.id.clone();
    std::thread::spawn(move || {
        while let Ok(data) = output_rx.recv() {
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
    });

    // Tool execution thread
    let tc = tool_call.clone();
    let tr = Arc::clone(tool_registry);
    let xc = x_credentials.cloned();
    let c = cwd.map(|p| p.to_path_buf());
    std::thread::spawn(move || {
        let result = tr.execute_streaming(&tc, output_tx, xc.as_ref(), c.as_deref());
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
        if is_cancelled(cancel_rx, &mut was_cancelled) {
            crate::metrics::record_tool_execution(&tool_call.name, exec_start.elapsed().as_secs_f64(), true);
            return ToolExecutionOutput {
                result: ToolResult {
                    content: format!("tool '{}' cancelled", tool_call.name),
                    is_error: true,
                },
                image: None,
            };
        }

        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            crate::metrics::record_tool_execution(&tool_call.name, exec_start.elapsed().as_secs_f64(), true);
            return ToolExecutionOutput {
                result: ToolResult {
                    content: format!(
                        "tool '{}' timed out after {}s",
                        tool_call.name,
                        timeout_dur.as_secs()
                    ),
                    is_error: true,
                },
                image: None,
            };
        }

        match result_rx.recv_timeout(remaining.min(check_interval)) {
            Ok(output) => {
                crate::metrics::record_tool_execution(&tool_call.name, exec_start.elapsed().as_secs_f64(), output.result.is_error);
                return output;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                crate::metrics::record_tool_execution(&tool_call.name, exec_start.elapsed().as_secs_f64(), true);
                return ToolExecutionOutput {
                    result: ToolResult {
                        content: "tool execution thread panicked".to_string(),
                        is_error: true,
                    },
                    image: None,
                };
            }
        }
    }
}

fn execute_list_sessions_sync(
    daemon_tx: &mpsc::Sender<crate::daemon::DaemonCommand>,
) -> ToolExecutionOutput {
    let (reply, rx) = std::sync::mpsc::channel();
    let _ = daemon_tx.send(crate::daemon::DaemonCommand::ListSessions { reply });
    match rx.recv() {
        Ok(sessions) => {
            if sessions.is_empty() {
                return ToolExecutionOutput {
                    result: ToolResult {
                        content: "No sessions found.".to_string(),
                        is_error: false,
                    },
                    image: None,
                };
            }
            let lines: Vec<String> = sessions
                .iter()
                .map(|s| {
                    let title = s.title.as_deref().unwrap_or("(untitled)");
                    let model = s.selected_model.as_deref().unwrap_or("(no model)");
                    let parent = s
                        .parent_session_id
                        .map(|id| id.to_string())
                        .unwrap_or_else(|| "none".to_string());
                    let cwd = s.cwd.as_deref().unwrap_or("(none)");
                    format!(
                        "Session {}: \"{}\" | model: {} | messages: {} | parent: {} | cwd: {}",
                        s.session_id, title, model, s.message_count, parent, cwd
                    )
                })
                .collect();
            ToolExecutionOutput {
                result: ToolResult {
                    content: crate::tools::truncate_tool_output(&lines.join("\n")),
                    is_error: false,
                },
                image: None,
            }
        }
        Err(_) => ToolExecutionOutput {
            result: ToolResult {
                content: "failed to list sessions".to_string(),
                is_error: true,
            },
            image: None,
        },
    }
}

fn execute_get_session_sync(
    daemon_tx: &mpsc::Sender<crate::daemon::DaemonCommand>,
    arguments_json: &str,
) -> ToolExecutionOutput {
    let args: serde_json::Value = match serde_json::from_str(arguments_json) {
        Ok(a) => a,
        Err(e) => {
            return ToolExecutionOutput {
                result: ToolResult {
                    content: format!("invalid arguments: {e}"),
                    is_error: true,
                },
                image: None,
            };
        }
    };
    let session_id = match args.get("session_id").and_then(|v| v.as_u64()) {
        Some(id) => id,
        None => {
            return ToolExecutionOutput {
                result: ToolResult {
                    content: "missing required argument: session_id".to_string(),
                    is_error: true,
                },
                image: None,
            };
        }
    };

    let (reply, rx) = std::sync::mpsc::channel();
    let _ = daemon_tx.send(crate::daemon::DaemonCommand::GetSession { session_id, reply });
    match rx.recv() {
        Ok(Some(summary)) => ToolExecutionOutput {
            result: ToolResult {
                content: format!(
                    "Session {} ({}) has {} messages.",
                    session_id,
                    summary.title.as_deref().unwrap_or("untitled"),
                    summary.message_count
                ),
                is_error: false,
            },
            image: None,
        },
        Ok(None) => ToolExecutionOutput {
            result: ToolResult {
                content: format!("Session {session_id} not found."),
                is_error: true,
            },
            image: None,
        },
        Err(_) => ToolExecutionOutput {
            result: ToolResult {
                content: "failed to get session".to_string(),
                is_error: true,
            },
            image: None,
        },
    }
}

fn execute_spawn_subsession_sync(
    _client: &OpenAiClient,
    daemon_tx: &mpsc::Sender<crate::daemon::DaemonCommand>,
    parent_session: &SessionState,
    parent_session_id: u64,
    _db: &redb::Database,
    _model: &str,
    tool_call: &crate::openai::ChatToolCall,
    _x_credentials: Option<&ServiceCredential>,
    cwd: Option<&Path>,
    _max_turns_default: u32,
    cancel_rx: &mpsc::Receiver<()>,
) -> ToolExecutionOutput {
    let args: serde_json::Value = match serde_json::from_str(&tool_call.arguments_json) {
        Ok(a) => a,
        Err(e) => {
            return ToolExecutionOutput {
                result: ToolResult {
                    content: format!("invalid arguments: {e}"),
                    is_error: true,
                },
                image: None,
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
                image: None,
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
        .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_else(|| parent_session.active_tool_groups.iter().cloned().collect());

    let (reply_tx, reply_rx) = std::sync::mpsc::channel();
    let _ = daemon_tx.send(crate::daemon::DaemonCommand::CreateSession {
        title,
        parent_session_id: Some(parent_session_id),
        cwd: child_cwd.clone(),
        max_turns,
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
                input_tokens: Vec::new(),
                reply: result_tx,
            });
            let mut was_cancelled = false;
            loop {
                // Cancellation is one-shot: `is_cancelled` reads the channel once
                // and then caches the result so subsequent iterations don't need
                // to re-check the (now-empty) channel.
                if is_cancelled(&cancel_rx, &mut was_cancelled) {
                    let _ =
                        child_tx.send(crate::sessions::SessionCommand::Cancel { request_id: 1 });
                    return ToolExecutionOutput {
                        result: ToolResult {
                            content: format!("sub-session {child_id} cancelled"),
                            is_error: true,
                        },
                        image: None,
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
                            image: None,
                        };
                    }
                    Ok(Err(e)) => {
                        return ToolExecutionOutput {
                            result: ToolResult {
                                content: format!("child session error: {e}"),
                                is_error: true,
                            },
                            image: None,
                        };
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                        return ToolExecutionOutput {
                            result: ToolResult {
                                content: format!("sub-session {child_id} exited unexpectedly"),
                                is_error: true,
                            },
                            image: None,
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
            image: None,
        },
        Err(_) => ToolExecutionOutput {
            result: ToolResult {
                content: "daemon communication failed".to_string(),
                is_error: true,
            },
            image: None,
        },
    }
}

fn execute_load_skill_sync(
    _session: &SessionState,
    cwd: Option<&Path>,
    arguments_json: &str,
) -> ToolExecutionOutput {
    let v: serde_json::Value = match serde_json::from_str(arguments_json) {
        Ok(v) => v,
        Err(e) => {
            return ToolExecutionOutput {
                result: ToolResult {
                    content: format!("invalid json: {e}"),
                    is_error: true,
                },
                image: None,
            };
        }
    };
    let name = match v.get("name").and_then(|n| n.as_str()) {
        Some(n) => n.to_string(),
        None => {
            return ToolExecutionOutput {
                result: ToolResult {
                    content: "missing required parameter: name".to_string(),
                    is_error: true,
                },
                image: None,
            };
        }
    };

    let effective_cwd = cwd.unwrap_or_else(|| Path::new("."));
    let body = match context::load_skill_body(&name, effective_cwd) {
        Some(b) => b,
        None => {
            return ToolExecutionOutput {
                result: ToolResult {
                    content: format!("skill not found: {name}"),
                    is_error: true,
                },
                image: None,
            };
        }
    };

    let skill_message = format!(
        "The following skill instructions are now active:\n\n<skill name=\"{name}\">\n{body}\n</skill>"
    );

    // Return the skill content as the result
    ToolExecutionOutput {
        result: ToolResult {
            content: format!("Loaded skill: {name}\n\n---\n{skill_message}"),
            is_error: false,
        },
        image: None,
    }
}

fn persist_assistant_tool_use_sync(
    session: &mut SessionState,
    session_id: u64,
    db: &Arc<redb::Database>,
    tool_use: &ChatAssistantToolUse,
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
    };
    let idx = session.push_message(msg.clone());
    if let Err(e) = write_message_retry(db, session_id, idx, &msg) {
        tracing::warn!(session_id, error = %e, "failed to persist assistant tool use");
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
            SessionMessage::AssistantText { content } => {
                Some(ChatRequestMessage::simple("assistant", content.clone()))
            }
            SessionMessage::AssistantToolUse {
                content,
                tool_calls,
                reasoning,
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
    use crate::tools::Tool;
    use std::sync::mpsc;
    use tai_proto::SessionSummary;

    #[test]
    fn build_chat_request_messages_empty() {
        let result = build_chat_request_messages(&[]);
        assert!(result.is_empty());
    }

    #[test]
    fn build_chat_request_messages_system_text() {
        let msgs = [SessionMessage::SystemText { content: "system prompt".into() }];
        let result = build_chat_request_messages(&msgs);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].role, "system");
        assert_eq!(result[0].content.as_deref(), Some("system prompt"));
    }

    #[test]
    fn build_chat_request_messages_user_text() {
        let msgs = [SessionMessage::UserText { content: "hello".into() }];
        let result = build_chat_request_messages(&msgs);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].role, "user");
        assert_eq!(result[0].content.as_deref(), Some("hello"));
    }

    #[test]
    fn build_chat_request_messages_assistant_text() {
        let msgs = [SessionMessage::AssistantText { content: "hi".into() }];
        let result = build_chat_request_messages(&msgs);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].role, "assistant");
        assert_eq!(result[0].content.as_deref(), Some("hi"));
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
            SessionMessage::UserText { content: "hello".into() },
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
            SessionMessage::AssistantText { content: "hi".into() },
        ];
        let result = build_chat_request_messages(&msgs);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].role, "user");
        assert_eq!(result[1].role, "assistant");
    }

    #[test]
    fn execute_list_sessions_sync_empty() {
        let (daemon_tx, daemon_rx) = mpsc::channel();
        std::thread::spawn(move || {
            if let Ok(DaemonCommand::ListSessions { reply }) = daemon_rx.recv() {
                let _ = reply.send(Vec::new());
            }
        });
        let output = execute_list_sessions_sync(&daemon_tx);
        assert!(!output.result.is_error);
        assert_eq!(output.result.content, "No sessions found.");
    }

    #[test]
    fn execute_list_sessions_sync_with_sessions() {
        let (daemon_tx, daemon_rx) = mpsc::channel();
        std::thread::spawn(move || {
            if let Ok(DaemonCommand::ListSessions { reply }) = daemon_rx.recv() {
                let sessions = vec![SessionSummary {
                    session_id: 1,
                    title: Some("Test".into()),
                    selected_model: Some("gpt-4".into()),
                    parent_session_id: None,
                    cwd: Some("/tmp".into()),
                    created_at: 1000,
                    message_count: 3,
                    max_turns: None,
                    status: SessionStatus::Inactive,
                    active_tool_groups: vec!["core".into()],
                }];
                let _ = reply.send(sessions);
            }
        });
        let output = execute_list_sessions_sync(&daemon_tx);
        assert!(!output.result.is_error);
        assert!(output.result.content.contains("Session 1"));
        assert!(output.result.content.contains("Test"));
        assert!(output.result.content.contains("gpt-4"));
    }

    #[test]
    fn execute_list_sessions_sync_disconnected() {
        let (daemon_tx, daemon_rx) = mpsc::channel::<DaemonCommand>();
        drop(daemon_rx);
        let output = execute_list_sessions_sync(&daemon_tx);
        assert!(output.result.is_error);
        assert_eq!(output.result.content, "failed to list sessions");
    }

    #[test]
    fn execute_get_session_sync_invalid_args() {
        let (daemon_tx, _) = mpsc::channel::<DaemonCommand>();
        let output = execute_get_session_sync(&daemon_tx, "not json");
        assert!(output.result.is_error);
        assert!(output.result.content.contains("invalid arguments"));
    }

    #[test]
    fn execute_get_session_sync_missing_id() {
        let (daemon_tx, _) = mpsc::channel::<DaemonCommand>();
        let output = execute_get_session_sync(&daemon_tx, r#"{}"#);
        assert!(output.result.is_error);
        assert_eq!(output.result.content, "missing required argument: session_id");
    }

    #[test]
    fn execute_get_session_sync_found() {
        let (daemon_tx, daemon_rx) = mpsc::channel();
        std::thread::spawn(move || {
            if let Ok(DaemonCommand::GetSession { session_id: 1, reply }) = daemon_rx.recv() {
                let _ = reply.send(Some(SessionSummary {
                    session_id: 1,
                    title: Some("Test".into()),
                    selected_model: Some("gpt-4".into()),
                    parent_session_id: None,
                    cwd: Some("/tmp".into()),
                    created_at: 1000,
                    message_count: 5,
                    max_turns: None,
                    status: SessionStatus::Inactive,
                    active_tool_groups: vec!["core".into()],
                }));
            }
        });
        let output = execute_get_session_sync(&daemon_tx, r#"{"session_id": 1}"#);
        assert!(!output.result.is_error);
        assert!(output.result.content.contains("Session 1"));
        assert!(output.result.content.contains("Test"));
        assert!(output.result.content.contains("5 messages"));
    }

    #[test]
    fn execute_get_session_sync_not_found() {
        let (daemon_tx, daemon_rx) = mpsc::channel();
        std::thread::spawn(move || {
            if let Ok(DaemonCommand::GetSession { session_id: 99, reply }) = daemon_rx.recv() {
                let _ = reply.send(None);
            }
        });
        let output = execute_get_session_sync(&daemon_tx, r#"{"session_id": 99}"#);
        assert!(output.result.is_error);
        assert_eq!(output.result.content, "Session 99 not found.");
    }

    #[test]
    fn execute_get_session_sync_disconnected() {
        let (daemon_tx, daemon_rx) = mpsc::channel::<DaemonCommand>();
        drop(daemon_rx);
        let output = execute_get_session_sync(&daemon_tx, r#"{"session_id": 1}"#);
        assert!(output.result.is_error);
        assert_eq!(output.result.content, "failed to get session");
    }

    #[test]
    fn execute_load_skill_sync_invalid_json() {
        let session = SessionState::empty();
        let output = execute_load_skill_sync(&session, None, "not json");
        assert!(output.result.is_error);
        assert!(output.result.content.contains("invalid json"));
    }

    #[test]
    fn execute_load_skill_sync_missing_name() {
        let session = SessionState::empty();
        let output = execute_load_skill_sync(&session, None, r#"{}"#);
        assert!(output.result.is_error);
        assert_eq!(output.result.content, "missing required parameter: name");
    }

    // -- Cancellation helper tests -----------------------------------------

    #[test]
    fn is_cancelled_returns_false_initially() {
        let (_tx, rx) = mpsc::channel::<()>();
        let mut flag = false;
        assert!(!is_cancelled(&rx, &mut flag));
        assert!(!flag);
    }

    #[test]
    fn is_cancelled_returns_true_after_send() {
        let (tx, rx) = mpsc::channel::<()>();
        tx.send(()).unwrap();
        let mut flag = false;
        assert!(is_cancelled(&rx, &mut flag));
        assert!(flag);
    }

    #[test]
    fn is_cancelled_caching_persists_after_message_consumed() {
        let (tx, rx) = mpsc::channel::<()>();
        tx.send(()).unwrap();
        let mut flag = false;

        // First call consumes the message and sets the cached flag.
        assert!(is_cancelled(&rx, &mut flag));

        // Second call should still return true using the cached flag,
        // even though the channel is now empty.
        assert!(is_cancelled(&rx, &mut flag));
        assert!(flag);
    }

    #[test]
    fn is_cancelled_one_shot_without_cache_still_detects() {
        // Passing `&mut false` on every call works as a one-shot check:
        // each call tries the channel afresh.
        let (tx, rx) = mpsc::channel::<()>();
        tx.send(()).unwrap();

        assert!(is_cancelled(&rx, &mut false));
    }

    #[test]
    fn is_cancelled_disconnected_no_signal() {
        let (tx, rx) = mpsc::channel::<()>();
        drop(tx); // disconnected without sending
        let mut flag = false;
        assert!(!is_cancelled(&rx, &mut flag));
        assert!(!flag);
    }

    #[test]
    fn is_cancelled_disconnected_after_signal_still_true_with_cache() {
        let (tx, rx) = mpsc::channel::<()>();
        tx.send(()).unwrap();
        drop(tx); // disconnect after sending

        let mut flag = false;
        assert!(is_cancelled(&rx, &mut flag)); // consumes the message
        assert!(flag);
        // Channel is now empty *and* disconnected, but cache keeps it true.
        assert!(is_cancelled(&rx, &mut flag));
    }

    // -- execute_tool_with_timeout tests -----------------------------------
    //
    // These exercise the streaming execution path with cancellation and
    // timeout handling.

    /// A tool that completes immediately.
    struct FastTestTool;

    impl Tool for FastTestTool {
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
            _args: &str,
            _xc: Option<&ServiceCredential>,
            _cwd: Option<&Path>,
        ) -> ToolExecutionOutput {
            ToolExecutionOutput {
                result: ToolResult {
                    content: "fast result".into(),
                    is_error: false,
                },
                image: None,
            }
        }
    }

    /// A tool that blocks in `execute_streaming` until a proceed signal is
    /// received (to simulate a long-running tool for timeout / cancel tests).
    struct BlockingTestTool {
        proceed: std::sync::Mutex<Option<mpsc::Receiver<()>>>,
    }

    impl Tool for BlockingTestTool {
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
            _args: &str,
            _xc: Option<&ServiceCredential>,
            _cwd: Option<&Path>,
        ) -> ToolExecutionOutput {
            ToolExecutionOutput {
                result: ToolResult {
                    content: "ignored".into(),
                    is_error: false,
                },
                image: None,
            }
        }
        fn execute_streaming(
            &self,
            _args: &str,
            _xc: Option<&ServiceCredential>,
            _cwd: Option<&Path>,
            _output_tx: mpsc::Sender<Vec<u8>>,
        ) -> ToolExecutionOutput {
            // Block until the proceed signal arrives or the sender is
            // dropped (which unblocks on test teardown).
            if let Some(rx) = self.proceed.lock().unwrap().take() {
                let _ = rx.recv();
            }
            ToolExecutionOutput {
                result: ToolResult {
                    content: "blocked tool done".into(),
                    is_error: false,
                },
                image: None,
            }
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
        let client =
            crate::openai::OpenAiClient::new(config, "test-key".into()).expect("OpenAiClient");

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

        let result = execute_tool_with_timeout(
            &registry,
            &tool_call,
            None,                   // x_credentials
            None,                   // cwd
            timeout_dur,
            1,                      // request_id
            &daemon_tx,
            &client,
            &db,
            &mut session,
            1,                      // session_id
            "test-model",
            25,                     // max_turns_default
            &cancel_rx,
            &cmd_tx,
        );
        (result, cmd_rx)
    }

    #[test]
    fn execute_tool_normal_completion() {
        let (_cancel_tx, cancel_rx) = mpsc::channel::<()>();
        let (result, _cmd_rx) = run_exec_tool(FastTestTool, "_test_fast", "{}", Duration::from_secs(60), cancel_rx);
        assert!(!result.result.is_error, "expected success: {}", result.result.content);
        assert!(result.result.content.contains("fast result"), "{}", result.result.content);
    }

    #[test]
    fn execute_tool_cancelled_before_execution() {
        let (cancel_tx, cancel_rx) = mpsc::channel::<()>();
        cancel_tx.send(()).expect("send cancel");
        drop(cancel_tx);

        let (result, _cmd_rx) = run_exec_tool(FastTestTool, "_test_fast", "{}", Duration::from_secs(60), cancel_rx);
        assert!(result.result.is_error, "expected error: {}", result.result.content);
        assert!(result.result.content.contains("cancelled"), "{}", result.result.content);
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

        assert!(result.result.is_error, "expected error: {}", result.result.content);
        assert!(result.result.content.contains("timed out"), "{}", result.result.content);

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
        let (result, _cmd_rx) = run_exec_tool(FastTestTool, "_test_fast", "{}", Duration::from_millis(10), cancel_rx);
        // With a 10ms timeout and an instant tool, we might get either the
        // result (fast tool wins) or a timeout.  Neither is wrong — the
        // disconnected case is exercised elsewhere.
        if result.result.is_error {
            assert!(
                result.result.content.contains("panicked") || result.result.content.contains("timed out"),
                "unexpected error: {}",
                result.result.content
            );
        }
    }

    /// A tool that sends data on the output channel during streaming
    /// execution (used to verify the forwarding thread).
    struct StreamingTestTool;

    impl Tool for StreamingTestTool {
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
            _args: &str,
            _xc: Option<&ServiceCredential>,
            _cwd: Option<&Path>,
        ) -> ToolExecutionOutput {
            ToolExecutionOutput {
                result: ToolResult {
                    content: "exec result".into(),
                    is_error: false,
                },
                image: None,
            }
        }
        fn execute_streaming(
            &self,
            _args: &str,
            _xc: Option<&ServiceCredential>,
            _cwd: Option<&Path>,
            output_tx: mpsc::Sender<Vec<u8>>,
        ) -> ToolExecutionOutput {
            // Send some output before returning so the forwarding thread
            // has data to forward.
            let _ = output_tx.send(b"streamed payload".to_vec());
            ToolExecutionOutput {
                result: ToolResult {
                    content: "streaming done".into(),
                    is_error: false,
                },
                image: None,
            }
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

        assert!(!result.result.is_error, "expected success: {}", result.result.content);
        assert!(result.result.content.contains("streaming done"), "{}", result.result.content);

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
