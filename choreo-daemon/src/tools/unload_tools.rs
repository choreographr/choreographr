use crate::daemon::DaemonCommand;
use crate::tools::context::ToolContext;
use crate::tools::{AllowedCaller, Tool, ToolExecError};
use choreo_keystore::ServiceCredential;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::Path;
use std::sync::{Weak, mpsc};
use tracing::info;

// ── Args ───────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub(crate) struct UnloadToolsArgs {
    /// Tool groups to deactivate.
    pub(crate) groups: Vec<String>,
}

// ── Execute ────────────────────────────────────────────────────────────────

/// Apply an `unload_tools` request to the session's active group set,
/// returning a human-readable summary of what changed.  The "core" group is
/// protected and cannot be removed.
///
/// Pure function (no I/O, no channels) so it can be unit-tested directly and
/// reused by the session main loop, which holds the authoritative group set.
pub(crate) fn apply_unload_tools(
    active_tool_groups: &mut HashSet<String>,
    groups: &[String],
) -> String {
    let mut unloaded = Vec::new();
    let mut protected = Vec::new();
    for g in groups {
        if g == "core" {
            protected.push(g.clone());
        } else if active_tool_groups.remove(g) {
            unloaded.push(g.clone());
        }
    }

    let mut parts = Vec::new();
    if !unloaded.is_empty() {
        parts.push(format!(
            "Deactivated tool groups: {}",
            humfmt::list(&unloaded)
        ));
    }
    if !protected.is_empty() {
        parts.push("The 'core' group cannot be unloaded.".to_string());
    }
    if parts.is_empty() {
        parts.push("None of the specified groups were active.".to_string());
    }
    parts.join(" ")
}

fn execute_unload_tools(
    args: &UnloadToolsArgs,
    _working_dir: Option<&Path>,
    ctx: Option<&ToolContext>,
) -> Result<String, ToolExecError> {
    let ctx = ctx.ok_or_else(|| ToolExecError("no session context".into()))?;
    if args.groups.is_empty() {
        return Err(ToolExecError("missing required argument: groups".into()));
    }

    info!(
        session_id = ctx.session_id,
        groups = ?args.groups,
        "deactivating tool groups",
    );

    // Synchronous round-trip: the daemon forwards UnloadTools to the
    // session's main loop, which applies the change to the AUTHORITATIVE
    // active-group set (not this worker's throwaway copy) and replies with
    // a summary of what actually changed.  Blocking on the reply is safe:
    // the daemon replies immediately if the session is inactive, and the
    // session main loop is a dedicated message pump that always answers.
    let (reply, rx) = mpsc::channel();
    ctx.daemon_tx
        .send(DaemonCommand::UnloadTools {
            session_id: ctx.session_id,
            groups: args.groups.clone(),
            reply,
        })
        .map_err(|e| ToolExecError(format!("daemon communication failed: {e}")))?;
    let outcome = rx
        .recv()
        .map_err(|e| ToolExecError(format!("daemon did not respond: {e}")))?;
    outcome.map_err(ToolExecError)
}

pub fn describe_invocation(args: &UnloadToolsArgs) -> String {
    format!("Deactivating tool groups: {}.", args.groups.join(", "))
}

// ── Tool impl ──────────────────────────────────────────────────────────────

/// `unload_tools` tool.  Holds a weak reference to the registry so the JSON
/// Schema's `groups` enum can be derived from the live group catalog at
/// definition time (including dynamic MCP groups registered after startup),
/// mirroring how the pre-registry meta-tool definitions were built.
pub(crate) struct UnloadTools {
    registry: Weak<crate::tools::ToolRegistry>,
}

impl UnloadTools {
    pub fn new(registry: Weak<crate::tools::ToolRegistry>) -> Self {
        UnloadTools { registry }
    }

    /// Group names advertised in the schema (excluding "core", which is
    /// always active).  Falls back to an empty enum if the registry is
    /// gone — the model can still pass any valid group name.
    fn group_names(&self) -> Vec<String> {
        self.registry
            .upgrade()
            .map(|r| r.group_names())
            .unwrap_or_default()
    }
}

impl Tool for UnloadTools {
    type Args = UnloadToolsArgs;
    type Return = String;
    type Error = ToolExecError;

    fn name(&self) -> &'static str {
        "unload_tools"
    }

    fn group(&self) -> &'static str {
        "core"
    }

    fn description(&self) -> &'static str {
        "Deactivate one or more tool groups. Tools in deactivated \
         groups will no longer be available to call in this session. \
         The 'core' group cannot be unloaded."
    }

    fn describe_invocation(&self, args: &Self::Args) -> String {
        describe_invocation(args)
    }

    fn return_string(ret: &Self::Return) -> String {
        ret.clone()
    }

    // Session-config mutation: only the model (Direct) may change the
    // session's tool surface — not programmatic callers.
    fn allowed_callers(&self) -> Vec<AllowedCaller> {
        vec![AllowedCaller::Direct]
    }

    fn schema(&self) -> serde_json::Value {
        // Build the schema by hand (rather than deriving it from schemars)
        // so the `groups` enum reflects the live registry group catalog.
        let names = self.group_names();
        serde_json::json!({
            "type": "object",
            "properties": {
                "groups": {
                    "type": "array",
                    "items": {
                        "type": "string",
                        "enum": names
                    },
                    "description": "Tool groups to deactivate"
                }
            },
            "required": ["groups"]
        })
    }

    fn execute(
        &self,
        args: Self::Args,
        _x_credentials: Option<&ServiceCredential>,
        working_dir: Option<&Path>,
        ctx: Option<&ToolContext>,
    ) -> Result<Self::Return, Self::Error> {
        execute_unload_tools(&args, working_dir, ctx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::ToolRegistry;
    use crate::tools::context::ToolContext;
    use std::sync::Arc;

    /// Build a ToolContext with a mock daemon channel, plus the receiver so
    /// tests can intercept the DaemonCommand and reply to it.
    fn test_context() -> (
        ToolContext,
        std::sync::mpsc::Sender<DaemonCommand>,
        std::sync::mpsc::Receiver<DaemonCommand>,
    ) {
        let (daemon_tx, daemon_rx) = std::sync::mpsc::channel::<DaemonCommand>();
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.keep(); // Leak: prevent early cleanup of the temp directory.
        let db = Arc::new(redb::Database::create(db_path.join("test.redb")).unwrap());
        let ctx = ToolContext::new(42, db, daemon_tx.clone());
        (ctx, daemon_tx, daemon_rx)
    }

    /// Run execute_unload_tools on a thread (it blocks waiting for the
    /// daemon's reply), intercept the DaemonCommand on the main thread, send
    /// the reply, and join.  Deterministic — no time-based waits: the tool
    /// blocks on the reply channel until this test sends it.  Takes owned
    /// args because the spawned thread must own everything it touches
    /// (`'static` bound).
    fn run_with_daemon_reply(
        args: UnloadToolsArgs,
        reply: Result<String, String>,
    ) -> (Result<String, ToolExecError>, DaemonCommand) {
        let (ctx, _daemon_tx, daemon_rx) = test_context();
        let handle = std::thread::spawn(move || execute_unload_tools(&args, None, Some(&ctx)));
        let cmd = daemon_rx.recv().unwrap();
        match &cmd {
            DaemonCommand::UnloadTools { reply: tx, .. } => {
                tx.send(reply).unwrap();
            }
            other => panic!(
                "expected UnloadTools, got {:?}",
                std::mem::discriminant(other)
            ),
        }
        (handle.join().unwrap(), cmd)
    }

    // -- apply_unload_tools (pure logic) -----------------------------------

    #[test]
    fn apply_removes_groups() {
        let mut active: HashSet<String> = ["core".into(), "git".into(), "shell".into(), "x".into()]
            .into_iter()
            .collect();
        let result = apply_unload_tools(&mut active, &["x".into()]);
        assert_eq!(result, "Deactivated tool groups: x");
        assert!(!active.contains("x"));
        assert!(active.contains("core"));
        assert!(active.contains("git"));
    }

    #[test]
    fn apply_protects_core() {
        let mut active: HashSet<String> = ["core".into(), "git".into()].into_iter().collect();
        let result = apply_unload_tools(&mut active, &["core".into()]);
        assert_eq!(result, "The 'core' group cannot be unloaded.");
        assert!(active.contains("core"));
    }

    #[test]
    fn apply_skips_inactive() {
        let mut active: HashSet<String> = ["core".into()].into_iter().collect();
        let result = apply_unload_tools(&mut active, &["x".into(), "vm".into()]);
        assert_eq!(result, "None of the specified groups were active.");
    }

    #[test]
    fn apply_protected_and_unloaded() {
        let mut active: HashSet<String> = ["core".into(), "shell".into()].into_iter().collect();
        let result = apply_unload_tools(&mut active, &["core".into(), "shell".into()]);
        assert!(result.contains("Deactivated tool groups: shell"));
        assert!(result.contains("The 'core' group cannot be unloaded."));
        assert!(active.contains("core"));
        assert!(!active.contains("shell"));
    }

    // -- execute -----------------------------------------------------------

    #[test]
    fn execute_sends_daemon_command_and_returns_reply() {
        let args = UnloadToolsArgs {
            groups: vec!["x".into()],
        };
        let (result, cmd) = run_with_daemon_reply(args, Ok("Deactivated tool groups: x".into()));
        assert_eq!(result.unwrap(), "Deactivated tool groups: x");
        match cmd {
            DaemonCommand::UnloadTools {
                session_id, groups, ..
            } => {
                assert_eq!(session_id, 42);
                assert_eq!(groups, vec!["x"]);
            }
            _ => panic!("expected UnloadTools command"),
        }
    }

    #[test]
    fn execute_forwards_daemon_error() {
        let args = UnloadToolsArgs {
            groups: vec!["x".into()],
        };
        let (result, _cmd) = run_with_daemon_reply(args, Err("session is not active".into()));
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("session is not active")
        );
    }

    #[test]
    fn execute_no_context_returns_error() {
        let args = UnloadToolsArgs {
            groups: vec!["x".into()],
        };
        let result = execute_unload_tools(&args, None, None);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("no session context")
        );
    }

    #[test]
    fn execute_empty_groups_returns_error() {
        let (ctx, _daemon_tx, _daemon_rx) = test_context();
        let args = UnloadToolsArgs { groups: vec![] };
        let result = execute_unload_tools(&args, None, Some(&ctx));
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("missing required argument: groups")
        );
    }

    // -- schema ------------------------------------------------------------

    #[test]
    fn schema_has_groups_enum_excluding_core() {
        let registry = ToolRegistry::new().build();
        let tool = UnloadTools::new(Arc::downgrade(&registry));
        let schema = tool.schema();
        let items = schema["properties"]["groups"]["items"].as_object().unwrap();
        let enum_vals = items["enum"].as_array().unwrap();
        let names: Vec<&str> = enum_vals.iter().filter_map(|v| v.as_str()).collect();
        assert!(names.contains(&"git"), "enum should include git: {names:?}");
        assert!(
            !names.contains(&"core"),
            "core must not appear in unload_tools enum: {names:?}"
        );
        let required = schema["required"].as_array().unwrap();
        assert!(required.iter().any(|v| v == "groups"));
    }

    #[test]
    fn tool_restricted_to_direct_callers() {
        let registry = ToolRegistry::new().build();
        let tool = UnloadTools::new(Arc::downgrade(&registry));
        let callers = tool.allowed_callers();
        assert_eq!(callers, vec![AllowedCaller::Direct]);
        assert!(!callers.contains(&AllowedCaller::Programmatic));
    }

    #[test]
    fn describe_invocation_includes_groups() {
        let args = UnloadToolsArgs {
            groups: vec!["git".into(), "shell".into()],
        };
        let desc = describe_invocation(&args);
        assert_eq!(desc, "Deactivating tool groups: git, shell.");
    }

    #[test]
    fn execute_postcard_args_round_trip() {
        // Verify the args can be serialised/deserialised via postcard,
        // which is the wire format used by the VM execution path.
        let args = UnloadToolsArgs {
            groups: vec!["x".into()],
        };
        let args_bytes = postcard::to_allocvec(&args).unwrap();
        let decoded: UnloadToolsArgs = postcard::from_bytes(&args_bytes).unwrap();
        assert_eq!(decoded.groups, vec!["x"]);
    }
}
