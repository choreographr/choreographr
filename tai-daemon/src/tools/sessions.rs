use crate::sessions::{list_sessions as daemon_list_sessions, session_by_id};
use crate::tools::{ToolExecutionOutput, ToolResult, truncate_tool_output};
use crate::DaemonState;
use tai_proto::SessionMessage;

pub(crate) async fn execute_list_sessions(state: &DaemonState) -> ToolExecutionOutput {
    let summaries = daemon_list_sessions(state).await;
    if summaries.is_empty() {
        return ToolExecutionOutput {
            result: ToolResult {
                content: "No sessions found.".to_string(),
                is_error: false,
            },
            image: None,
        };
    }
    let mut lines = Vec::with_capacity(summaries.len());
    for s in &summaries {
        let title = s.title.as_deref().unwrap_or("(untitled)");
        let model = s.selected_model.as_deref().unwrap_or("(no model)");
        let parent = s
            .parent_session_id
            .map(|id| id.to_string())
            .unwrap_or_else(|| "none".to_string());
        let cwd = s.cwd.as_deref().unwrap_or("(none)");
        lines.push(format!(
            "Session {}: \"{}\" | model: {} | messages: {} | parent: {} | cwd: {}",
            s.session_id, title, model, s.message_count, parent, cwd,
        ));
    }
    ToolExecutionOutput {
        result: ToolResult {
            content: truncate_tool_output(&lines.join("\n")),
            is_error: false,
        },
        image: None,
    }
}

pub(crate) async fn execute_get_session(
    state: &DaemonState,
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
            }
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
            }
        }
    };

    let Some(session) = session_by_id(state, session_id).await else {
        return ToolExecutionOutput {
            result: ToolResult {
                content: format!("Session {session_id} not found."),
                is_error: true,
            },
            image: None,
        };
    };

    let (title, messages) = {
        let guard = session.lock().await;
        let title = guard
            .title
            .clone()
            .unwrap_or_else(|| "(untitled)".to_string());
        let messages = guard.messages.clone();
        (title, messages)
    };

    if messages.is_empty() {
        return ToolExecutionOutput {
            result: ToolResult {
                content: format!("Session {session_id} (\"{title}\") has no messages."),
                is_error: false,
            },
            image: None,
        };
    }

    let mut output = format!("Session {session_id} (\"{title}\") messages:\n\n");
    for msg in &messages {
        output.push_str(&format_message(msg));
        output.push('\n');
    }

    ToolExecutionOutput {
        result: ToolResult {
            content: truncate_tool_output(&output),
            is_error: false,
        },
        image: None,
    }
}

fn format_message(msg: &SessionMessage) -> String {
    match msg {
        SessionMessage::SystemText { content } => {
            format!("[system] {content}")
        }
        SessionMessage::UserText { content } => {
            format!("[user] {content}")
        }
        SessionMessage::AssistantText { content } => {
            format!("[assistant] {content}")
        }
        SessionMessage::AssistantToolUse {
            content,
            tool_calls,
            ..
        } => {
            let calls = tool_calls
                .iter()
                .map(|call| format!("{}({})", call.name, call.arguments_json))
                .collect::<Vec<_>>()
                .join(", ");
            match content.as_deref().map(str::trim).filter(|c| !c.is_empty()) {
                Some(text) => format!("[tool-call] {calls} -- {text}"),
                None => format!("[tool-call] {calls}"),
            }
        }
        SessionMessage::ToolResult {
            name,
            content,
            is_error,
            ..
        } => {
            let status = if *is_error { "error" } else { "ok" };
            let preview = if content.len() > 500 {
                format!("{}...[truncated]", &content[..500])
            } else {
                content.clone()
            };
            format!("[tool-result:{status}] {name}: {preview}")
        }
    }
}

pub(crate) fn list_sessions_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {},
        "additionalProperties": false
    })
}

pub(crate) fn list_sessions_definition() -> crate::openai::ChatToolDefinition {
    crate::openai::ChatToolDefinition::function(
        "list_sessions",
        "List all sessions known to the daemon. Returns session ID, title, model, message count, parent session ID, and working directory for each session. Use this to discover what other sessions are doing before reading one with get_session.",
        list_sessions_schema(),
    )
}

pub(crate) fn get_session_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "session_id": {
                "type": "integer",
                "description": "The ID of the session to read messages from"
            }
        },
        "required": ["session_id"],
        "additionalProperties": false
    })
}

pub(crate) fn get_session_definition() -> crate::openai::ChatToolDefinition {
    crate::openai::ChatToolDefinition::function(
        "get_session",
        "Read the full message history of a session by its ID. Returns all messages (system, user, assistant, tool calls, tool results) with role labels. Use this after list_sessions to inspect the conversation in a specific session.",
        get_session_schema(),
    )
}
