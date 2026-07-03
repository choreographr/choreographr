use crate::openai::OpenAiClient;
use crate::sessions::{SessionState, append_message_and_persist, create_session_internal};
use crate::tools::{ToolExecutionOutput, ToolResult};
use crate::DaemonState;
use std::sync::Arc;
use tai_keystore::XCredentials;
use tai_proto::SessionMessage;
use tokio::sync::Mutex;

pub(crate) async fn execute_spawn_subsession(
    client: &OpenAiClient,
    state: &DaemonState,
    _parent_session: &Arc<Mutex<SessionState>>,
    parent_session_id: u64,
    db: &Arc<redb::Database>,
    model: &str,
    tool_call: &crate::openai::ChatToolCall,
    x_credentials: Option<&XCredentials>,
    cwd: Option<&std::path::Path>,
) -> ToolExecutionOutput {
    let args: serde_json::Value = match serde_json::from_str(&tool_call.arguments_json) {
        Ok(a) => a,
        Err(e) => return ToolExecutionOutput {
            result: ToolResult { content: format!("invalid arguments: {e}"), is_error: true },
            image: None,
        },
    };

    let prompt = match args.get("prompt").and_then(|v| v.as_str()) {
        Some(p) => p.to_string(),
        None => return ToolExecutionOutput {
            result: ToolResult { content: "missing required argument: prompt".to_string(), is_error: true },
            image: None,
        },
    };

    let title = args.get("title").and_then(|v| v.as_str()).map(|s| s.to_string());
    let max_turns = args.get("max_turns").and_then(|v| v.as_u64()).map(|v| v as u32);
    let child_cwd = cwd.map(|p| p.to_path_buf());

    let (child_session_id, child_session) = match create_session_internal(
        state,
        title,
        Some(parent_session_id),
        child_cwd,
        max_turns,
    ).await {
        Ok(s) => s,
        Err(e) => return ToolExecutionOutput {
            result: ToolResult { content: format!("failed to create sub-session: {e}"), is_error: true },
            image: None,
        },
    };

    append_message_and_persist(
        &child_session,
        db,
        child_session_id,
        SessionMessage::SystemText { content: prompt },
    ).await;

    let child_request_id = 1;

    let result = Box::pin(crate::requests::run_agent_loop(
        client,
        &child_session,
        child_session_id,
        db,
        model,
        child_request_id,
        x_credentials,
        cwd,
        state,
    )).await;

    match result {
        Ok(()) => {
            let messages = child_session.lock().await.messages.clone();
            let output = messages.iter()
                .filter_map(|m| match m {
                    SessionMessage::AssistantText { content } => Some(content.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            let result_text = if output.is_empty() {
                format!("sub-session {child_session_id} completed with no text output")
            } else {
                format!("sub-session {child_session_id} result:\n{output}")
            };
            ToolExecutionOutput {
                result: ToolResult { content: result_text, is_error: false },
                image: None,
            }
        }
        Err(e) => ToolExecutionOutput {
            result: ToolResult { content: format!("sub-session {child_session_id} failed: {e}"), is_error: true },
            image: None,
        },
    }
}

pub(crate) fn spawn_subsession_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "prompt": {
                "type": "string",
                "description": "Task description for the sub-session to work on autonomously"
            },
            "title": {
                "type": "string",
                "description": "Optional title for the sub-session"
            },
            "max_turns": {
                "type": "integer",
                "description": "Optional maximum tool-calling iterations for this sub-session. Inherits from parent if not set."
            }
        },
        "required": ["prompt"],
        "additionalProperties": false
    })
}

pub(crate) fn spawn_subsession_definition() -> crate::openai::ChatToolDefinition {
    crate::openai::ChatToolDefinition::function(
        "spawn_subsession",
        "Spawn a sub-session to autonomously work on a task. The sub-session inherits the parent session's working directory and runs its own tool-calling loop.",
        spawn_subsession_schema(),
    )
}
