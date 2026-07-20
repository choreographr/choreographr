use crate::daemon::DaemonCommand;
use crate::sessions::SessionCommand;
use crate::tools::context::ToolContext;
use crate::tools::{Tool, ToolExecError};
use schemars::JsonSchema;
use serde::Deserialize;
use std::path::Path;
use std::sync::mpsc;
use tai_keystore::ServiceCredential;
use tracing::{debug, error, info, warn};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SpawnSubsessionArgs {
    /// Task description for the sub-session to work on autonomously
    pub prompt: String,
    /// Optional title for the sub-session
    pub title: Option<String>,
    /// Optional maximum tool-calling iterations for this sub-session
    pub max_turns: Option<u32>,
    /// Optional tool categories to activate. Inherits from parent session if not set.
    pub categories: Option<Vec<String>>,
}

pub struct SpawnSubsession;

impl Tool for SpawnSubsession {
    type Args = SpawnSubsessionArgs;
    type Return = String;
    type Error = ToolExecError;

    fn name(&self) -> &'static str {
        "spawn_subsession"
    }

    fn group(&self) -> &'static str {
        "core"
    }

    fn description(&self) -> &'static str {
        "Spawn a sub-session to autonomously work on a task. The sub-session inherits the parent session's working directory and runs its own tool-calling loop."
    }

    fn return_string(ret: &Self::Return) -> String {
        ret.clone()
    }

    fn execute(
        &self,
        args: Self::Args,
        _x_credentials: Option<&ServiceCredential>,
        working_dir: Option<&Path>,
        ctx: Option<&ToolContext>,
    ) -> Result<Self::Return, Self::Error> {
        let ctx = ctx.ok_or_else(|| {
            warn!("spawn_subsession: no session context provided");
            ToolExecError("no session context".into())
        })?;

        let prompt_len = args.prompt.len();
        info!(
            session_id = ctx.session_id,
            prompt_len,
            title = args.title.as_deref().unwrap_or("(none)"),
            "spawn_subsession: creating child session"
        );

        // Determine child working_dir: prefer tool-level parameter, fall back to session context
        let child_working_dir = working_dir
            .or(ctx.working_dir.as_deref())
            .map(|p| p.to_path_buf());

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
                working_dir: child_working_dir.clone(),
                max_turns: args.max_turns,
                reasoning_effort: ctx.reasoning_effort,
                selected_model: ctx.selected_model.clone(),
                context_config: None,
                account_name: None,
                active_tool_groups: categories,
                reply: reply_tx,
            })
            .map_err(|e| {
                warn!(
                    session_id = ctx.session_id,
                    error = %e,
                    "spawn_subsession: daemon channel send failed"
                );
                ToolExecError(format!("daemon communication failed: {e}"))
            })?;

        let (child_id, child_tx) = match reply_rx.recv() {
            Ok(Ok(pair)) => {
                info!(
                    parent_id = ctx.session_id,
                    child_id = pair.0,
                    "spawn_subsession: child session created"
                );
                pair
            }
            Ok(Err(e)) => {
                warn!(
                    session_id = ctx.session_id,
                    error = %e,
                    "spawn_subsession: daemon rejected session creation"
                );
                return Err(ToolExecError(format!("failed to create sub-session: {e}")));
            }
            Err(_) => {
                error!(
                    session_id = ctx.session_id,
                    "spawn_subsession: daemon disconnected before CreateSession reply"
                );
                return Err(ToolExecError("daemon disconnected".into()));
            }
        };

        // Run the child session with the prompt and wait for its result.
        // Cancellation propagation is handled at the daemon level via
        // parent-child session tracking — no polling needed here.
        let (result_tx, result_rx) = mpsc::channel();
        if child_tx
            .send(SessionCommand::RunChildInput {
                request_id: 1,
                user_text: Some(args.prompt),
                reply: result_tx,
            })
            .is_err()
        {
            warn!(
                child_id,
                parent_id = ctx.session_id,
                "spawn_subsession: child session channel closed before RunChildInput"
            );
            return Err(ToolExecError(format!(
                "sub-session {child_id} exited unexpectedly"
            )));
        }

        match result_rx.recv() {
            Ok(Ok(child_result)) => {
                debug!(
                    child_id,
                    output_len = child_result.output.len(),
                    "spawn_subsession: child completed successfully"
                );
                Ok(format!(
                    "sub-session {child_id} result:\n{}",
                    child_result.output
                ))
            }
            Ok(Err(e)) => {
                warn!(
                    child_id,
                    error = %e,
                    "spawn_subsession: child returned error"
                );
                Err(ToolExecError(format!("child session error: {e}")))
            }
            Err(_) => {
                error!(
                    child_id,
                    parent_id = ctx.session_id,
                    "spawn_subsession: child exited without sending result"
                );
                Err(ToolExecError(format!(
                    "sub-session {child_id} exited unexpectedly"
                )))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawn_subsession_schema_has_required_prompt() {
        let schema = SpawnSubsession.schema();
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
