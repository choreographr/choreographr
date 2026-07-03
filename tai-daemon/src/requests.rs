use crate::openai::{
    self, ChatAssistantToolUse, ChatRequestMessage, ChatTurnResult, CompletionChunkKind,
    OpenAiClient,
};
use crate::sessions::{append_message_and_persist, broadcast_to_session, SessionState};
use crate::tools::{available_tools, emit_prepared_image, execute_tool_call};
use std::{io, sync::Arc};
use tai_keystore::XCredentials;
use tai_proto::{
    AssistantToolCallRecord, DaemonMessage, ImageMetadata, MAX_IMAGE_CHUNK_SIZE, OutputStream,
    SessionMessage,
};
use tokio::sync::{Mutex, mpsc};

pub(crate) async fn execute_plain_request(
    client: &OpenAiClient,
    session: &Arc<Mutex<SessionState>>,
    session_id: u64,
    db: &Arc<redb::Database>,
    model: &str,
    request_id: u32,
) -> io::Result<()> {
    let prompt = {
        let guard = session.lock().await;
        build_prompt(&guard.messages)
    };
    let answer = Arc::new(Mutex::new(String::new()));
    let answer_clone = Arc::clone(&answer);
    client
        .completion_stream(model, &prompt, |kind, chunk| {
            let answer = Arc::clone(&answer_clone);
            let session_for_chunk = Arc::clone(session);
            async move {
                if matches!(kind, CompletionChunkKind::Answer) {
                    answer.lock().await.push_str(&chunk);
                }
                broadcast_to_session(
                    &session_for_chunk,
                    DaemonMessage::OutputChunk {
                        request_id,
                        stream: match kind {
                            CompletionChunkKind::Answer => OutputStream::Answer,
                            CompletionChunkKind::Reasoning => OutputStream::Reasoning,
                        },
                        data: chunk.into_bytes(),
                    },
                    None,
                )
                .await;
                Ok(())
            }
        })
        .await?;

    let final_answer = answer.lock().await.trim().to_string();
    if !final_answer.is_empty() {
        append_message_and_persist(
            session,
            db,
            session_id,
            SessionMessage::AssistantText {
                content: final_answer,
            },
        )
        .await;
    }
    Ok(())
}

pub(crate) async fn run_agent_loop(
    client: &OpenAiClient,
    session: &Arc<Mutex<SessionState>>,
    session_id: u64,
    db: &Arc<redb::Database>,
    model: &str,
    request_id: u32,
    x_credentials: Option<&XCredentials>,
    cwd: Option<&std::path::Path>,
    state: &crate::DaemonState,
) -> io::Result<()> {
    let tools = available_tools();
    let mut next_image_id = 1;

    let max_turns = {
        let guard = session.lock().await;
        guard.max_turns.unwrap_or_else(|| {
            state.blocking_lock().max_turns
        })
    };

    for _ in 0..max_turns {
        let messages = {
            let guard = session.lock().await;
            build_chat_request_messages(&guard.messages)
        };
        match client
            .chat_completion_turn(model, &messages, &tools)
            .await?
        {
            ChatTurnResult::FinalText(content) => {
                broadcast_to_session(
                    session,
                    DaemonMessage::OutputChunk {
                        request_id,
                        stream: OutputStream::Answer,
                        data: content.clone().into_bytes(),
                    },
                    None,
                )
                .await;
                append_message_and_persist(
                    session,
                    db,
                    session_id,
                    SessionMessage::AssistantText { content },
                )
                .await;
                return Ok(());
            }
            ChatTurnResult::ToolUse(tool_use) => {
                persist_assistant_tool_use(session, session_id, db, &tool_use).await;
                for tool_call in tool_use.tool_calls {
                    broadcast_to_session(
                        session,
                        DaemonMessage::ToolCallStarted {
                            request_id,
                            call_id: tool_call.id.clone(),
                            tool_name: tool_call.name.clone(),
                            arguments_json: tool_call.arguments_json.clone(),
                        },
                        None,
                    )
                    .await;

                    let output = if tool_call.name == "spawn_subsession" {
                        crate::tools::subsession::execute_spawn_subsession(
                            client, state, session, session_id, db, model,
                            &tool_call, x_credentials, cwd,
                        ).await
                    } else if tool_call.name == "list_sessions" {
                        crate::tools::sessions::execute_list_sessions(state).await
                    } else if tool_call.name == "get_session" {
                        crate::tools::sessions::execute_get_session(state, &tool_call.arguments_json).await
                    } else {
                        execute_tool_call(&tool_call, x_credentials, cwd).await
                    };

                    if let Some(image) = output.image {
                        emit_prepared_image(session, request_id, next_image_id, image).await;
                        next_image_id = next_image_id.wrapping_add(1);
                    }
                    append_message_and_persist(
                        session,
                        db,
                        session_id,
                        SessionMessage::ToolResult {
                            call_id: tool_call.id.clone(),
                            name: tool_call.name.clone(),
                            content: output.result.content.clone(),
                            is_error: output.result.is_error,
                        },
                    )
                    .await;
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
                    broadcast_to_session(session, event, None).await;
                }
            }
        }
    }

    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        format!("tool loop exceeded {max_turns} iterations"),
    ))
}

pub(crate) async fn execute_chat_tool_request(
    client: &OpenAiClient,
    session: &Arc<Mutex<SessionState>>,
    session_id: u64,
    db: &Arc<redb::Database>,
    model: &str,
    request_id: u32,
    x_credentials: Option<XCredentials>,
    cwd: Option<std::path::PathBuf>,
    state: &crate::DaemonState,
) -> io::Result<()> {
    run_agent_loop(
        client, session, session_id, db, model, request_id,
        x_credentials.as_ref(), cwd.as_deref(), state,
    ).await
}

pub(crate) async fn persist_assistant_tool_use(
    session: &Arc<Mutex<SessionState>>,
    session_id: u64,
    db: &Arc<redb::Database>,
    tool_use: &ChatAssistantToolUse,
) {
    append_message_and_persist(
        session,
        db,
        session_id,
        SessionMessage::AssistantToolUse {
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
        },
    )
    .await;
}

pub(crate) fn build_chat_request_messages(messages: &[SessionMessage]) -> Vec<ChatRequestMessage> {
    messages
        .iter()
        .map(|message| match message {
            SessionMessage::SystemText { content } => {
                ChatRequestMessage::simple("system", content.clone())
            }
            SessionMessage::UserText { content } => {
                ChatRequestMessage::simple("user", content.clone())
            }
            SessionMessage::AssistantText { content } => {
                ChatRequestMessage::simple("assistant", content.clone())
            },
            SessionMessage::AssistantToolUse {
                content,
                tool_calls,
                reasoning_content,
                reasoning,
                reasoning_text,
            } => ChatRequestMessage {
                role: "assistant",
                content: content.clone(),
                tool_call_id: None,
                tool_calls: Some(
                    tool_calls
                        .iter()
                        .map(|tool_call| openai::AssistantToolCall {
                            id: tool_call.call_id.clone(),
                            kind: "function".to_string(),
                            function: openai::AssistantToolFunction {
                                name: tool_call.name.clone(),
                                arguments: tool_call.arguments_json.clone(),
                            },
                        })
                        .collect(),
                ),
                reasoning_content: reasoning_content.clone(),
                reasoning: reasoning.clone(),
                reasoning_text: reasoning_text.clone(),
            },
            SessionMessage::ToolResult {
                call_id, content, ..
            } => ChatRequestMessage {
                role: "tool",
                content: Some(content.clone()),
                tool_call_id: Some(call_id.clone()),
                tool_calls: None,
                reasoning_content: None,
                reasoning: None,
                reasoning_text: None,
            },
        })
        .collect()
}

pub(crate) fn build_prompt(messages: &[SessionMessage]) -> String {
    let mut prompt = String::new();
    for message in messages {
        let line = message.render_line();
        if !prompt.is_empty() {
            prompt.push_str("\n\n");
        }
        prompt.push_str(line.trim());
    }
    prompt
}

pub const REQUEST_IMAGE_BYTES: &[u8] = include_bytes!("../assets/dua.jpg");
pub const REQUEST_IMAGE_MIME_TYPE: &str = "image/jpeg";
pub const REQUEST_IMAGE_WIDTH: u32 = 640;
pub const REQUEST_IMAGE_HEIGHT: u32 = 640;

pub(crate) async fn emit_demo_image(
    tx: &mpsc::Sender<DaemonMessage>,
    request_id: u32,
    image_id: u32,
) -> Result<(), mpsc::error::SendError<DaemonMessage>> {
    let metadata = ImageMetadata {
        image_id,
        mime_type: REQUEST_IMAGE_MIME_TYPE.to_string(),
        width: REQUEST_IMAGE_WIDTH,
        height: REQUEST_IMAGE_HEIGHT,
        byte_len: REQUEST_IMAGE_BYTES.len() as u64,
        alt: Some("dua".to_string()),
    };
    tx.send(DaemonMessage::ImageStart {
        request_id,
        metadata,
    })
    .await?;
    for data in REQUEST_IMAGE_BYTES.chunks(MAX_IMAGE_CHUNK_SIZE) {
        tx.send(DaemonMessage::ImageChunk {
            request_id,
            image_id,
            data: data.to_vec(),
        })
        .await?;
    }
    tx.send(DaemonMessage::ImageEnd {
        request_id,
        image_id,
    })
    .await
}
