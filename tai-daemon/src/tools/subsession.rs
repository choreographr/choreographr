use crate::daemon::DaemonCommand;
use crate::tools::context::ToolContext;
use crate::tools::{Tool, ToolError};
use serde::Deserialize;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::sync::mpsc::RecvTimeoutError;
use std::time::Duration;
use tai_proto::SessionMessage;

/// JSON schema for spawn_subsession arguments — reused by the Tool impl
/// and the LLM tool-definition builder in available_definitions.
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
            },
            "categories": {
                "type": "array",
                "items": {
                    "type": "string"
                },
                "description": "Optional tool categories to activate. Inherits from parent session if not set."
            }
        },
        "required": ["prompt"],
        "additionalProperties": false
    })
}

#[derive(Debug, Deserialize)]
pub struct SpawnSubsessionArgs {
    pub prompt: String,
    pub title: Option<String>,
    pub max_turns: Option<u32>,
    pub categories: Option<Vec<String>>,
}

pub struct SpawnSubsession;

impl Tool for SpawnSubsession {
    type Args = SpawnSubsessionArgs;
    type Return = String;

    fn name(&self) -> &'static str {
        "spawn_subsession"
    }

    fn group(&self) -> &'static str {
        "core"
    }

    fn description(&self) -> &'static str {
        "Spawn a sub-session to autonomously work on a task. The sub-session inherits the parent session's working directory and runs its own tool-calling loop."
    }

    fn schema(&self) -> serde_json::Value {
        spawn_subsession_schema()
    }

    fn execute(
        &self,
        args: Self::Args,
        _x_credentials: Option<&tai_keystore::ServiceCredential>,
        cwd: Option<&Path>,
        ctx: Option<&ToolContext>,
    ) -> Result<String, ToolError> {
        let ctx = ctx.ok_or_else(|| ToolError::Other("no session context".into()))?;

        // Determine child CWD: prefer tool-level cwd, fall back to session cwd
        let child_cwd = cwd.or(ctx.cwd.as_deref()).map(|p| p.to_path_buf());

        // Inherit or override tool groups
        let categories = args
            .categories
            .unwrap_or_else(|| ctx.active_tool_groups.iter().cloned().collect());

        // Create child session via the daemon command loop
        let (reply_tx, reply_rx) = mpsc::channel();
        ctx.daemon_tx
            .send(DaemonCommand::CreateSession {
                title: args.title,
                parent_session_id: Some(ctx.session_id),
                cwd: child_cwd.clone(),
                max_turns: args.max_turns,
                reasoning_effort: ctx.reasoning_effort,
                context_config: None,
                account_name: None,
                active_tool_groups: categories,
                reply: reply_tx,
            })
            .map_err(|e| ToolError::Other(format!("daemon communication failed: {e}")))?;

        let (child_id, child_tx) = match reply_rx.recv() {
            Ok(Ok(pair)) => pair,
            Ok(Err(e)) => {
                return Err(ToolError::Other(format!(
                    "failed to create sub-session: {e}"
                )));
            }
            Err(_) => return Err(ToolError::Other("daemon disconnected".into())),
        };

        // Push the prompt as the child's first message
        let _ = child_tx.send(crate::sessions::SessionCommand::AppendMessage {
            message: SessionMessage::SystemText {
                content: args.prompt,
            },
        });

        // Run the child session and wait for its result.
        // Poll the parent cancellation flag every 200ms so we can abort
        // early when the parent session is cancelled.
        let (result_tx, result_rx) = mpsc::channel();
        let _ = child_tx.send(crate::sessions::SessionCommand::RunChildInput {
            request_id: 1,
            reply: result_tx,
        });

        let check_interval = Duration::from_millis(200);
        loop {
            if ctx.cancelled.load(Ordering::Relaxed) {
                let _ = child_tx.send(crate::sessions::SessionCommand::Cancel { request_id: 1 });
                // Brief drain so the child can clean up its resources.
                let _ = result_rx.recv_timeout(Duration::from_secs(5)).ok();
                return Err(ToolError::Other("parent session cancelled".into()));
            }

            match result_rx.recv_timeout(check_interval) {
                Ok(Ok(child_result)) => {
                    return Ok(format!(
                        "sub-session {child_id} result:\n{}",
                        child_result.output
                    ));
                }
                Ok(Err(e)) => {
                    return Err(ToolError::Other(format!("child session error: {e}")));
                }
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(ToolError::Other(format!(
                        "sub-session {child_id} exited unexpectedly"
                    )));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawn_subsession_schema_has_required_prompt() {
        let schema = spawn_subsession_schema();
        let obj = schema.as_object().expect("schema should be an object");
        let required = obj
            .get("required")
            .and_then(|v| v.as_array())
            .expect("schema should have required array");
        assert!(
            required.iter().any(|v| v == "prompt"),
            "prompt should be in required: {required:?}",
        );
        let props = obj
            .get("properties")
            .and_then(|v| v.as_object())
            .expect("schema should have properties");
        assert!(
            props.contains_key("prompt"),
            "prompt should be in properties",
        );
        assert!(
            props["prompt"]["type"] == "string",
            "prompt should be string type",
        );
    }

    #[test]
    fn spawn_subsession_args_deserializes() {
        let json = r#"{"prompt": "do something"}"#;
        let args: SpawnSubsessionArgs = serde_json::from_str(json).expect("should deserialize");
        assert_eq!(args.prompt, "do something");
        assert!(args.title.is_none());
        assert!(args.max_turns.is_none());
        assert!(args.categories.is_none());
    }

    #[test]
    fn spawn_subsession_args_all_fields() {
        let json = r#"{
            "prompt": "work",
            "title": "my sub",
            "max_turns": 10,
            "categories": ["core", "shell"]
        }"#;
        let args: SpawnSubsessionArgs =
            serde_json::from_str(json).expect("should deserialize full payload");
        assert_eq!(args.prompt, "work");
        assert_eq!(args.title.as_deref(), Some("my sub"));
        assert_eq!(args.max_turns, Some(10));
        assert_eq!(args.categories, Some(vec!["core".into(), "shell".into()]));
    }

    #[test]
    fn spawn_subsession_args_missing_prompt_fails() {
        let json = r#"{"title": "no prompt"}"#;
        let result: Result<SpawnSubsessionArgs, _> = serde_json::from_str(json);
        assert!(result.is_err(), "missing prompt should fail: {result:?}",);
    }
}
