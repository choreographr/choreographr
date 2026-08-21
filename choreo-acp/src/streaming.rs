use std::sync::atomic::{AtomicU64, Ordering};

use choreo_proto::{DaemonMessage, SessionEvent};

use crate::acp_jsonrpc::{ContentBlock, SessionUpdateParams, SessionUpdateVariant};

/// Monotonically incrementing counter for ACP message IDs.  Replaces the
/// more expensive UUID generation that was used in earlier versions.
static NEXT_MSG_ID: AtomicU64 = AtomicU64::new(1);

fn next_message_id() -> String {
    let id = NEXT_MSG_ID.fetch_add(1, Ordering::Relaxed);
    format!("msg_{id}")
}

/// Translate a streaming `DaemonMessage` into one or more ACP session update
/// notifications.
///
/// Returns `None` for messages that don't produce ACP events (e.g.
/// non-streaming responses, responses for other requests).  Request-ID
/// filtering is the caller's responsibility — this function translates any
/// matching message type it understands.
///
/// Multiple updates are returned as a `Vec` because a single daemon message
/// can map to several ACP notifications (e.g. `Done` → usage update + status
/// update).
pub fn translate_message(
    msg: &DaemonMessage,
    session_acp_id: &str,
) -> Option<Vec<SessionUpdateParams>> {
    // Every session-scoped streaming event arrives wrapped in the `Session`
    // envelope (the origin `session_id` lives on the envelope, not on the
    // event). Unwrap it once; non-session messages — plain replies, control
    // messages — produce no ACP events here.
    let DaemonMessage::Session { event, .. } = msg else {
        return None;
    };
    match event {
        // ------------------------------------------------------------------
        // Output chunks → text content blocks appended to the assistant's
        // streaming message.  Both Answer and Reasoning streams produce
        // text content for the ACP protocol.
        // ------------------------------------------------------------------
        SessionEvent::OutputChunk {
            request_id: _,
            stream: _,
            data,
            ..
        } => {
            let text = String::from_utf8_lossy(data).to_string();
            let message_id = next_message_id();
            Some(vec![SessionUpdateParams {
                session_id: session_acp_id.to_string(),
                variant: SessionUpdateVariant::AgentMessageChunk {
                    message_id,
                    content: ContentBlock::Text { text },
                },
            }])
        }

        // ------------------------------------------------------------------
        // Tool call started → a new tool call block with "running" status
        // and the call arguments as content.
        // ------------------------------------------------------------------
        SessionEvent::ToolCallStarted {
            request_id: _,
            call_id,
            tool_name,
            arguments_json,
            ..
        } => {
            let kind = tool_kind_from_name(tool_name);
            Some(vec![SessionUpdateParams {
                session_id: session_acp_id.to_string(),
                variant: SessionUpdateVariant::ToolCall {
                    tool_call_id: call_id.clone(),
                    title: tool_name.clone(),
                    kind,
                    status: "running".into(),
                    content: vec![ContentBlock::Text {
                        text: arguments_json.clone(),
                    }],
                    locations: None,
                },
            }])
        }

        // ------------------------------------------------------------------
        // Tool result chunks → progress updates while the tool is running.
        // The old `ToolCallOutput` variant has been removed; `ToolResultChunk`
        // carries the opaque byte data that we surface as text.
        // ------------------------------------------------------------------
        SessionEvent::ToolResultChunk {
            request_id: _,
            call_id,
            data,
            ..
        } => {
            let text = String::from_utf8_lossy(data).to_string();
            Some(vec![SessionUpdateParams {
                session_id: session_acp_id.to_string(),
                variant: SessionUpdateVariant::ToolCallUpdate {
                    tool_call_id: call_id.clone(),
                    status: "running".into(),
                    content: Some(vec![ContentBlock::Text { text }]),
                },
            }])
        }

        // ------------------------------------------------------------------
        // Tool call finished → mark the tool call as "completed".
        // ------------------------------------------------------------------
        SessionEvent::ToolCallFinished {
            request_id: _,
            call_id,
            ..
        } => Some(vec![SessionUpdateParams {
            session_id: session_acp_id.to_string(),
            variant: SessionUpdateVariant::ToolCallUpdate {
                tool_call_id: call_id.clone(),
                status: "completed".into(),
                content: None,
            },
        }]),

        // ------------------------------------------------------------------
        // Tool call failed → mark the tool call as "failed" with the error.
        // ------------------------------------------------------------------
        SessionEvent::ToolCallFailed {
            request_id: _,
            call_id,
            tool_name: _,
            error,
            ..
        } => Some(vec![SessionUpdateParams {
            session_id: session_acp_id.to_string(),
            variant: SessionUpdateVariant::ToolCallUpdate {
                tool_call_id: call_id.clone(),
                status: "failed".into(),
                content: Some(vec![ContentBlock::Text {
                    text: error.clone(),
                }]),
            },
        }]),

        // ------------------------------------------------------------------
        // Done → emit a usage update (if token info is available) followed
        // by a status update signalling completion.
        // ------------------------------------------------------------------
        SessionEvent::Done {
            request_id: _,
            token_usage,
            ..
        } => {
            let mut updates = Vec::with_capacity(2);

            if let Some(usage) = token_usage {
                updates.push(SessionUpdateParams {
                    session_id: session_acp_id.to_string(),
                    variant: SessionUpdateVariant::UsageUpdate {
                        used_input_tokens: Some(usage.input_tokens),
                        used_output_tokens: Some(usage.output_tokens),
                        // TokenUsage does not carry a reasoning-token field.
                        used_reasoning_tokens: None,
                    },
                });
            }

            updates.push(SessionUpdateParams {
                session_id: session_acp_id.to_string(),
                variant: SessionUpdateVariant::StatusUpdate {
                    status: "completed".into(),
                },
            });

            Some(updates)
        }

        // ------------------------------------------------------------------
        // Failed → signal refusal / error.
        // ------------------------------------------------------------------
        SessionEvent::Failed { .. } => Some(vec![SessionUpdateParams {
            session_id: session_acp_id.to_string(),
            variant: SessionUpdateVariant::StatusUpdate {
                status: "refusal".into(),
            },
        }]),

        // ------------------------------------------------------------------
        // Cancelled → signal the session was cancelled.
        // ------------------------------------------------------------------
        SessionEvent::Cancelled { .. } => Some(vec![SessionUpdateParams {
            session_id: session_acp_id.to_string(),
            variant: SessionUpdateVariant::StatusUpdate {
                status: "cancelled".into(),
            },
        }]),

        // All other daemon messages (non-streaming responses, control
        // messages, etc.) produce no ACP events here.
        _ => None,
    }
}

/// Map a tool name (as reported by the daemon) to a normalised ACP tool
/// kind string that the editor uses for display purposes.
fn tool_kind_from_name(name: &str) -> String {
    match name {
        "read" | "TextInput" | "read_file" | "grep" | "glob" => "read".into(),
        "edit" | "edit_file" | "write" | "write_file" | "create" | "patch" => "edit".into(),
        "bash" | "terminal" | "command" | "execute_command" | "run" => "terminal".into(),
        "web" | "web_fetch" | "web_search" | "browser" | "fetch" => "web_browsing".into(),
        _ => "custom".into(),
    }
}

/// Convenience constructor for a text `ContentBlock`.
pub fn text_block(text: String) -> ContentBlock {
    ContentBlock::Text { text }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sess() -> String {
        "sess_test".into()
    }

    // -- Helpers to build daemon messages for testing -- //

    fn output_chunk(stream: choreo_proto::OutputStream, data: &str) -> DaemonMessage {
        DaemonMessage::Session {
            session_id: None,
            event: SessionEvent::OutputChunk {
                request_id: 1,
                stream,
                data: data.as_bytes().to_vec(),
            },
        }
    }

    fn tool_call_started(call_id: &str, tool_name: &str, args: &str) -> DaemonMessage {
        DaemonMessage::Session {
            session_id: None,
            event: SessionEvent::ToolCallStarted {
                request_id: 1,
                call_id: call_id.into(),
                tool_name: tool_name.into(),
                arguments_json: args.into(),
                invocation_description: String::new(),
            },
        }
    }

    fn tool_result_chunk(call_id: &str, data: &str) -> DaemonMessage {
        DaemonMessage::Session {
            session_id: None,
            event: SessionEvent::ToolResultChunk {
                request_id: 1,
                call_id: call_id.into(),
                data: data.as_bytes().to_vec(),
            },
        }
    }

    fn tool_call_finished(call_id: &str, tool_name: &str) -> DaemonMessage {
        DaemonMessage::Session {
            session_id: None,
            event: SessionEvent::ToolCallFinished {
                request_id: 1,
                call_id: call_id.into(),
                tool_name: tool_name.into(),
            },
        }
    }

    fn tool_call_failed(call_id: &str, tool_name: &str, error: &str) -> DaemonMessage {
        DaemonMessage::Session {
            session_id: None,
            event: SessionEvent::ToolCallFailed {
                request_id: 1,
                call_id: call_id.into(),
                tool_name: tool_name.into(),
                error: error.into(),
            },
        }
    }

    fn done() -> DaemonMessage {
        DaemonMessage::Session {
            session_id: None,
            event: SessionEvent::Done {
                request_id: 1,
                token_usage: Some(choreo_proto::TokenUsage {
                    input_tokens: 10,
                    output_tokens: 20,
                    total_tokens: 30,
                }),
                last_prompt_tokens: None,
            },
        }
    }

    fn failed() -> DaemonMessage {
        DaemonMessage::Session {
            session_id: None,
            event: SessionEvent::Failed {
                request_id: 1,
                error: "model refused".into(),
            },
        }
    }

    // -- Tests -- //

    #[test]
    fn output_chunk_translates_to_agent_message_chunk() {
        let msg = output_chunk(choreo_proto::OutputStream::Answer, "Hello, world!");
        let result = translate_message(&msg, &sess()).unwrap();
        assert_eq!(result.len(), 1);
        let update = &result[0];
        assert_eq!(update.session_id, "sess_test");
        assert!(matches!(
            update.variant,
            SessionUpdateVariant::AgentMessageChunk { .. }
        ));
    }

    #[test]
    fn output_chunk_reasoning_also_translates() {
        let msg = output_chunk(choreo_proto::OutputStream::Reasoning, "thinking...");
        let result = translate_message(&msg, &sess());
        assert!(result.is_some());
    }

    #[test]
    fn tool_call_started_translates_to_tool_call() {
        let msg = tool_call_started("call_1", "bash", r#"{"cmd":"ls"}"#);
        let result = translate_message(&msg, &sess()).unwrap();
        assert_eq!(result.len(), 1);
        let update = &result[0];
        assert_eq!(update.session_id, "sess_test");
        match &update.variant {
            SessionUpdateVariant::ToolCall {
                tool_call_id,
                title,
                kind,
                status,
                ..
            } => {
                assert_eq!(tool_call_id, "call_1");
                assert_eq!(title, "bash");
                assert_eq!(kind, "terminal");
                assert_eq!(status, "running");
            }
            other => panic!("expected ToolCall, got {other:?}"),
        }
    }

    #[test]
    fn tool_result_chunk_translates_to_tool_call_update() {
        let msg = tool_result_chunk("call_1", "progress data");
        let result = translate_message(&msg, &sess()).unwrap();
        assert_eq!(result.len(), 1);
        match &result[0].variant {
            SessionUpdateVariant::ToolCallUpdate {
                tool_call_id,
                status,
                content,
            } => {
                assert_eq!(tool_call_id, "call_1");
                assert_eq!(status, "running");
                assert!(content.is_some());
            }
            other => panic!("expected ToolCallUpdate, got {other:?}"),
        }
    }

    #[test]
    fn tool_result_chunk_content_text() {
        let msg = tool_result_chunk("call_1", "result data");
        let result = translate_message(&msg, &sess()).unwrap();
        match &result[0].variant {
            SessionUpdateVariant::ToolCallUpdate { content, .. } => {
                let blocks = content.as_ref().unwrap();
                assert_eq!(blocks.len(), 1);
                if let ContentBlock::Text { text } = &blocks[0] {
                    assert_eq!(text, "result data");
                } else {
                    panic!("expected Text block");
                }
            }
            other => panic!("expected ToolCallUpdate, got {other:?}"),
        }
    }

    #[test]
    fn tool_call_finished_translates_to_completed() {
        let msg = tool_call_finished("call_1", "bash");
        let result = translate_message(&msg, &sess()).unwrap();
        match &result[0].variant {
            SessionUpdateVariant::ToolCallUpdate { status, .. } => {
                assert_eq!(status, "completed");
            }
            other => panic!("expected ToolCallUpdate, got {other:?}"),
        }
    }

    #[test]
    fn tool_call_failed_translates_to_failed() {
        let msg = tool_call_failed("call_1", "bash", "permission denied");
        let result = translate_message(&msg, &sess()).unwrap();
        match &result[0].variant {
            SessionUpdateVariant::ToolCallUpdate { status, .. } => {
                assert_eq!(status, "failed");
            }
            other => panic!("expected ToolCallUpdate, got {other:?}"),
        }
    }

    #[test]
    fn done_translates_to_usage_and_status() {
        let msg = done();
        let result = translate_message(&msg, &sess()).unwrap();
        assert_eq!(result.len(), 2);
        assert!(matches!(
            result[0].variant,
            SessionUpdateVariant::UsageUpdate { .. }
        ));
        match &result[1].variant {
            SessionUpdateVariant::StatusUpdate { status } => {
                assert_eq!(status, "completed");
            }
            other => panic!("expected StatusUpdate, got {other:?}"),
        }
    }

    #[test]
    fn done_translates_token_usage_correctly() {
        let msg = done();
        let result = translate_message(&msg, &sess()).unwrap();
        match &result[0].variant {
            SessionUpdateVariant::UsageUpdate {
                used_input_tokens,
                used_output_tokens,
                used_reasoning_tokens,
            } => {
                assert_eq!(*used_input_tokens, Some(10));
                assert_eq!(*used_output_tokens, Some(20));
                assert_eq!(*used_reasoning_tokens, None);
            }
            other => panic!("expected UsageUpdate, got {other:?}"),
        }
    }

    #[test]
    fn done_without_token_usage_omits_usage_update() {
        let msg = DaemonMessage::Session {
            session_id: None,
            event: SessionEvent::Done {
                request_id: 1,
                token_usage: None,
                last_prompt_tokens: None,
            },
        };
        let result = translate_message(&msg, &sess()).unwrap();
        // Only the status update should be present.
        assert_eq!(result.len(), 1);
        assert!(matches!(
            result[0].variant,
            SessionUpdateVariant::StatusUpdate { .. }
        ));
    }

    #[test]
    fn failed_translates_to_refusal() {
        let msg = failed();
        let result = translate_message(&msg, &sess()).unwrap();
        match &result[0].variant {
            SessionUpdateVariant::StatusUpdate { status } => {
                assert_eq!(status, "refusal");
            }
            other => panic!("expected StatusUpdate, got {other:?}"),
        }
    }

    #[test]
    fn non_streaming_message_returns_none() {
        let msgs = [
            DaemonMessage::Session {
                session_id: Some(1),
                event: SessionEvent::SessionCreated {
                    title: None,
                    parent_session_id: None,
                    working_dir: None,
                    account_name: None,
                    selected_model: None,
                    reasoning_effort: None,
                },
            },
            DaemonMessage::Sessions { sessions: vec![] },
            DaemonMessage::Models {
                models: vec![],
                selected_model: None,
            },
        ];
        for msg in &msgs {
            assert!(
                translate_message(msg, &sess()).is_none(),
                "expected None for {msg:?}"
            );
        }
    }

    #[test]
    fn tool_kind_maps_common_names() {
        // Read tools
        assert_eq!(tool_kind_from_name("read"), "read");
        assert_eq!(tool_kind_from_name("grep"), "read");
        assert_eq!(tool_kind_from_name("glob"), "read");

        // Edit tools
        assert_eq!(tool_kind_from_name("edit"), "edit");
        assert_eq!(tool_kind_from_name("write_file"), "edit");
        assert_eq!(tool_kind_from_name("create"), "edit");

        // Terminal tools
        assert_eq!(tool_kind_from_name("bash"), "terminal");
        assert_eq!(tool_kind_from_name("execute_command"), "terminal");
        assert_eq!(tool_kind_from_name("run"), "terminal");

        // Web tools
        assert_eq!(tool_kind_from_name("web_fetch"), "web_browsing");
        assert_eq!(tool_kind_from_name("browser"), "web_browsing");

        // Fallback
        assert_eq!(tool_kind_from_name("unknown_tool"), "custom");
        assert_eq!(tool_kind_from_name("docker"), "custom");
    }

    #[test]
    fn text_block_constructor() {
        let block = text_block("hello".into());
        match block {
            ContentBlock::Text { text } => assert_eq!(text, "hello"),
            _ => panic!("expected Text"),
        }
    }
}
