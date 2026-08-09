use crate::daemon::DaemonCommand;
use crate::tools::context::ToolContext;
use crate::tools::{Tool, ToolExecError};
use choreo_keystore::ServiceCredential;
use schemars::JsonSchema;
use serde::Deserialize;
use std::path::Path;

// ── Args structs ────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct GetSessionArgs {
    /// Session ID to inspect
    session_id: u64,
}

// ── get_session ────────────────────────────────────────────────────────────

fn execute_get_session(
    args: &GetSessionArgs,
    _working_dir: Option<&Path>,
    ctx: Option<&ToolContext>,
) -> Result<String, ToolExecError> {
    let ctx = ctx.ok_or_else(|| ToolExecError("no session context".into()))?;
    let (reply, rx) = std::sync::mpsc::channel();
    ctx.daemon_tx
        .send(DaemonCommand::GetSession {
            session_id: args.session_id,
            reply,
        })
        .map_err(|e| ToolExecError(format!("daemon communication failed: {e}")))?;
    match rx
        .recv()
        .map_err(|_| ToolExecError("failed to get session".into()))?
    {
        Some(summary) => Ok(format!(
            "Session {} ({}) has {} messages.",
            args.session_id,
            summary.title.as_deref().unwrap_or("untitled"),
            summary.turn_count
        )),
        None => Err(ToolExecError(format!(
            "Session {} not found.",
            args.session_id
        ))),
    }
}

pub(crate) struct GetSession;

impl Tool for GetSession {
    type Args = GetSessionArgs;
    type Return = String;
    type Error = ToolExecError;

    fn name(&self) -> &'static str {
        "get_session"
    }

    fn group(&self) -> &'static str {
        "core"
    }

    fn description(&self) -> &'static str {
        "Read the full message history of a session by its ID. Returns all messages (system, user, assistant, tool calls, tool results) with role labels."
    }

    fn describe_invocation(&self, args: &Self::Args) -> String {
        format!("Getting session {}.", args.session_id)
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
        execute_get_session(&args, working_dir, ctx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::admin::tests::test_context;
    use std::sync::Arc;

    // -- get_session ----------------------------------------------------------

    #[test]
    fn execute_get_session_found() {
        let (ctx, _tx) = test_context();
        let args = GetSessionArgs { session_id: 1 };
        let result = execute_get_session(&args, None, Some(&ctx));
        assert!(result.is_ok());
        let msg = result.unwrap();
        assert!(msg.contains("Session 1"));
        assert!(msg.contains("test"));
        assert!(msg.contains("5 messages"));
    }

    #[test]
    fn execute_get_session_not_found() {
        let (ctx, _tx) = test_context();
        let args = GetSessionArgs { session_id: 99 };
        let result = execute_get_session(&args, None, Some(&ctx));
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().to_string(), "Session 99 not found.");
    }

    #[test]
    fn execute_get_session_no_context() {
        let args = GetSessionArgs { session_id: 1 };
        let result = execute_get_session(&args, None, None);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("no session context")
        );
    }

    #[test]
    fn execute_get_session_disconnected() {
        let (tx, _rx) = std::sync::mpsc::channel::<DaemonCommand>();
        drop(_rx);
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(redb::Database::create(dir.path().join("test.redb")).unwrap());
        let ctx = ToolContext::new(42, db, tx);
        let args = GetSessionArgs { session_id: 1 };
        let result = execute_get_session(&args, None, Some(&ctx));
        assert!(result.is_err());
    }
}
