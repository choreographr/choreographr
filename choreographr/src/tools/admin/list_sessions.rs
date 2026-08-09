use crate::daemon::DaemonCommand;
use crate::tools::context::ToolContext;
use crate::tools::{Tool, ToolExecError, truncate_tool_output};
use choreo_keystore::ServiceCredential;
use schemars::JsonSchema;
use serde::Deserialize;
use std::path::Path;

// ── Args structs ────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct ListSessionsArgs {}

// ── list_sessions ──────────────────────────────────────────────────────────

fn execute_list_sessions(
    _args: &ListSessionsArgs,
    _working_dir: Option<&Path>,
    ctx: Option<&ToolContext>,
) -> Result<String, ToolExecError> {
    let ctx = ctx.ok_or_else(|| ToolExecError("no session context".into()))?;
    let (reply, rx) = std::sync::mpsc::channel();
    ctx.daemon_tx
        .send(DaemonCommand::ListSessions { reply })
        .map_err(|e| ToolExecError(format!("daemon communication failed: {e}")))?;
    let sessions = rx
        .recv()
        .map_err(|_| ToolExecError("failed to list sessions".into()))?;
    if sessions.is_empty() {
        return Ok("No sessions found.".to_string());
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
            let working_dir = s.working_dir.as_deref().unwrap_or("(none)");
            format!(
                "Session {}: \"{}\" | model: {} | turns: {} | parent: {} | working_dir: {}",
                s.session_id, title, model, s.turn_count, parent, working_dir
            )
        })
        .collect();
    Ok(truncate_tool_output(&lines.join("\n")))
}

pub(crate) struct ListSessions;

impl Tool for ListSessions {
    type Args = ListSessionsArgs;
    type Return = String;
    type Error = ToolExecError;

    fn name(&self) -> &'static str {
        "list_sessions"
    }

    fn group(&self) -> &'static str {
        "core"
    }

    fn description(&self) -> &'static str {
        "List all sessions known to the daemon. Returns session ID, title, model, message count, parent session ID, and working directory for each session."
    }

    fn describe_invocation(&self, _args: &Self::Args) -> String {
        "Listing all sessions.".to_string()
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
        execute_list_sessions(&args, working_dir, ctx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::admin::tests::test_context;
    use std::sync::Arc;

    // -- list_sessions --------------------------------------------------------

    #[test]
    fn execute_list_sessions_empty() {
        let (ctx, _tx) = test_context();
        let result = execute_list_sessions(&ListSessionsArgs {}, None, Some(&ctx));
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "No sessions found.");
    }

    #[test]
    fn execute_list_sessions_no_context() {
        let result = execute_list_sessions(&ListSessionsArgs {}, None, None);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("no session context")
        );
    }

    #[test]
    fn execute_list_sessions_disconnected() {
        let (tx, _rx) = std::sync::mpsc::channel::<DaemonCommand>();
        // Drop the receiver so sends fail.
        drop(_rx);
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(redb::Database::create(dir.path().join("test.redb")).unwrap());
        let ctx = ToolContext::new(42, db, tx);
        let result = execute_list_sessions(&ListSessionsArgs {}, None, Some(&ctx));
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("daemon communication failed")
        );
    }
}
