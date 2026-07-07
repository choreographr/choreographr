use crate::context;
use crate::db::write_message_retry;
use tracing::debug;
use crate::openai::{
    AssistantToolCall, AssistantToolFunction, ChatAssistantToolUse, ChatRequestMessage,
    ChatTurnResult, OpenAiClient,
};
use crate::sessions::{SessionCommand, SessionState};
use crate::tools::{PreparedImage, ToolExecutionOutput, ToolRegistry, ToolResult};
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tai_keystore::XCredentials;
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

pub(crate) fn run_agent_loop(
    client: &OpenAiClient,
    session: &mut SessionState,
    session_id: u64,
    db: &Arc<redb::Database>,
    model: &str,
    request_id: u32,
    cwd: Option<&Path>,
    cancel: &AtomicBool,
    tool_registry: &Arc<ToolRegistry>,
    daemon_tx: &mpsc::Sender<crate::daemon::DaemonCommand>,
    max_turns_default: u32,
    cmd_tx: &std::sync::mpsc::Sender<SessionCommand>,
) -> io::Result<()> {
    let mut next_image_id = 1u32;
    let max_turns = session.max_turns.unwrap_or(max_turns_default);

    for turn in 0..max_turns {
        debug!(session_id, turn, "agent loop turn");
        let tools = tool_registry.available_definitions(&session.active_tool_groups);
        if cancel.load(Ordering::SeqCst) {
            return Ok(());
        }

        if let Some(ref session_cwd) = session.cwd {
            let context_config = client.config().context.clone();
            if let Some(old_fp) = session.context_fingerprint {
                if let Some(idx) = session.context_message_index {
                    if let Ok(Some(new_bundle)) =
                        context::recheck_context(session_cwd, &context_config, old_fp)
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

        if cmd_tx.send(SessionCommand::StatusChanged(SessionStatus::Inference)).is_err() {
            return Ok(());
        }

        let messages = build_chat_request_messages(session.messages());
        match client.chat_completion_turn(model, &messages, &tools)? {
            ChatTurnResult::FinalText(content) => {
                debug!(
                    session_id,
                    turn,
                    response_len = content.len(),
                    "model returned final text",
                );
                let _ = cmd_tx.send(SessionCommand::Broadcast(
                    DaemonMessage::OutputChunk {
                        request_id,
                        stream: OutputStream::Answer,
                        data: content.clone().into_bytes(),
                    },
                ));
                let msg = SessionMessage::AssistantText { content };
                let idx = session.push_message(msg.clone());
                if let Err(e) = write_message_retry(db, session_id, idx, &msg) {
                    tracing::warn!(session_id, error = %e, "failed to persist assistant text");
                }
                return Ok(());
            }
            ChatTurnResult::ToolUse(tool_use) => {
                persist_assistant_tool_use_sync(session, session_id, db, &tool_use);
                for tool_call in tool_use.tool_calls {
                    if cancel.load(Ordering::SeqCst) {
                        return Ok(());
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
                        return Ok(());
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
                        cancel,
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

                    if let Some(hint) = context::subdirectory_hints(
                        &tool_call.name,
                        &tool_call.arguments_json,
                        session.cwd.as_deref(),
                        &session.context_file_paths,
                    ) {
                        output.result.content =
                            format!("{}\n\n---\n{}", output.result.content, hint);
                    }

                    if let Some(image) = output.image {
                        let PreparedImage { mime_type, data, width, height, alt } = image;

                        // Broadcast to live subscribers first (needs a clone of
                        // data/mime_type/alt for chunking over the channel).
                        emit_prepared_image_sync(
                            cmd_tx,
                            request_id,
                            next_image_id,
                            PreparedImage {
                                mime_type: mime_type.clone(),
                                data: data.clone(),
                                width,
                                height,
                                alt: alt.clone(),
                            },
                        );
                        next_image_id = next_image_id.wrapping_add(1);

                        // Persist the image data so it can be replayed on session
                        // re-attach or after a daemon restart.  Written to the DB
                        // before pushing to the in-memory vec so that a crash between
                        // the two leaves a complete snapshot.
                        let persisted = SessionMessage::DisplayedImage(DisplayedImageRecord {
                            metadata: ImageMetadata {
                                image_id: 0, // unused for persisted images
                                mime_type,   // moved
                                width,
                                height,
                                // Safety: data.len() fits in u64 on all supported platforms.
                                byte_len: data.len() as u64,
                                alt, // moved
                            },
                            data, // moved — only one clone total (done above for broadcast)
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
            }
        }
    }

    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        format!("tool loop exceeded {max_turns} iterations"),
    ))
}

fn execute_tool_with_timeout(
    tool_registry: &Arc<ToolRegistry>,
    tool_call: &crate::openai::ChatToolCall,
    x_credentials: Option<&XCredentials>,
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
    cancel: &AtomicBool,
    cmd_tx: &mpsc::Sender<SessionCommand>,
) -> ToolExecutionOutput {
    if tool_call.name == "list_sessions" {
        return execute_list_sessions_sync(daemon_tx);
    }
    if tool_call.name == "get_session" {
        return execute_get_session_sync(daemon_tx, &tool_call.arguments_json);
    }
    if tool_call.name == "spawn_subsession" {
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
            cancel,
        );
    }
    if tool_call.name == "load_skill" {
        return execute_load_skill_sync(session, cwd, &tool_call.arguments_json);
    }
    if tool_call.name == "load_tools" {
        let result = crate::tools::groups::execute_load_tools(
            &mut session.active_tool_groups,
            &tool_call.arguments_json,
        );
        let _ = daemon_tx.send(crate::daemon::DaemonCommand::UpdateMetadata {
            session_id,
            metadata: session.to_metadata(),
        });
        return ToolExecutionOutput {
            result: ToolResult {
                content: result,
                is_error: false,
            },
            image: None,
        };
    }
    if tool_call.name == "unload_tools" {
        let result = crate::tools::groups::execute_unload_tools(
            &mut session.active_tool_groups,
            &tool_call.arguments_json,
        );
        let _ = daemon_tx.send(crate::daemon::DaemonCommand::UpdateMetadata {
            session_id,
            metadata: session.to_metadata(),
        });
        return ToolExecutionOutput {
            result: ToolResult {
                content: result,
                is_error: false,
            },
            image: None,
        };
    }

    let (result_tx, result_rx) = std::sync::mpsc::channel();
    let (output_tx, output_rx) = std::sync::mpsc::channel();
    let tc = tool_call.clone();
    let tr = Arc::clone(tool_registry);
    let xc = x_credentials.cloned();
    let c = cwd.map(|p| p.to_path_buf());
    std::thread::spawn(move || {
        let result = tr.execute_streaming(&tc, output_tx, xc.as_ref(), c.as_deref());
        let _ = result_tx.send(result);
    });

    let start = std::time::Instant::now();
    loop {
        if cancel.load(Ordering::SeqCst) {
            return ToolExecutionOutput {
                result: ToolResult {
                    content: format!("tool '{}' cancelled", tool_call.name),
                    is_error: true,
                },
                image: None,
            };
        }

        let elapsed = start.elapsed();
        if elapsed >= timeout_dur {
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

        // Drain any streaming output
        let drain_output = || {
            loop {
                match output_rx.try_recv() {
                    Ok(data) => {
                        let _ = cmd_tx.send(SessionCommand::Broadcast(DaemonMessage::ToolCallOutput {
                            request_id,
                            call_id: tool_call.id.clone(),
                            data,
                        }));
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => break,
                }
            }
        };
        drain_output();

        let poll = std::cmp::min(Duration::from_millis(100), timeout_dur - elapsed);
        match result_rx.recv_timeout(poll) {
            Ok(output) => {
                drain_output();
                return output;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
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
    _x_credentials: Option<&XCredentials>,
    cwd: Option<&Path>,
    _max_turns_default: u32,
    cancel: &AtomicBool,
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
            loop {
                if cancel.load(Ordering::SeqCst) {
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
        reasoning_content: tool_use.reasoning_content.clone(),
        reasoning: tool_use.reasoning.clone(),
        reasoning_text: tool_use.reasoning_text.clone(),
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
                reasoning_content,
                reasoning,
                reasoning_text,
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
                reasoning_content: reasoning_content.clone(),
                reasoning: reasoning.clone(),
                reasoning_text: reasoning_text.clone(),
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
            reasoning_content: None,
            reasoning: None,
            reasoning_text: None,
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
}
