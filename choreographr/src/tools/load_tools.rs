use crate::daemon::DaemonCommand;
use crate::tools::context::ToolContext;
use crate::tools::{AllowedCaller, Tool, ToolExecError, groups_enum_schema, unknown_group_names};
use choreo_keystore::ServiceCredential;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::Path;
use std::sync::{Weak, mpsc};
use tracing::info;

// ── Args ───────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub(crate) struct LoadToolsArgs {
    /// Tool groups to activate.
    pub(crate) groups: Vec<String>,
}

// ── Execute ────────────────────────────────────────────────────────────────

/// Apply a `load_tools` request to the session's active group set, returning
/// a human-readable summary of what changed.
///
/// Pure function (no I/O, no channels) so it can be unit-tested directly and
/// reused by the session main loop, which holds the authoritative group set.
pub(crate) fn apply_load_tools(
    active_tool_groups: &mut HashSet<String>,
    groups: &[String],
) -> String {
    let mut loaded = Vec::new();
    for g in groups {
        if active_tool_groups.insert(g.clone()) {
            loaded.push(g.clone());
        }
    }

    if loaded.is_empty() {
        "All specified groups were already active.".to_string()
    } else {
        format!("Activated tool groups: {}", humfmt::list(&loaded))
    }
}

fn execute_load_tools(
    args: &LoadToolsArgs,
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
        "activating tool groups",
    );

    // Synchronous round-trip: the daemon forwards LoadTools to the session's
    // main loop, which applies the change to the AUTHORITATIVE active-group
    // set (not this worker's throwaway copy) and replies with a summary of
    // what actually changed.  Blocking on the reply is safe: the daemon
    // replies immediately if the session is inactive, and the session main
    // loop is a dedicated message pump that always answers.
    let (reply, rx) = mpsc::channel();
    ctx.daemon_tx
        .send(DaemonCommand::LoadTools {
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

pub fn describe_invocation(args: &LoadToolsArgs) -> String {
    format!("Activating tool groups: {}.", args.groups.join(", "))
}

// ── Tool impl ──────────────────────────────────────────────────────────────

/// `load_tools` tool.  Holds a weak reference to the registry so the JSON
/// Schema's `groups` enum can be derived from the live group catalog at
/// definition time (including dynamic MCP groups registered after startup),
/// mirroring how the pre-registry meta-tool definitions were built.
pub(crate) struct LoadTools {
    registry: Weak<crate::tools::ToolRegistry>,
}

impl LoadTools {
    pub fn new(registry: Weak<crate::tools::ToolRegistry>) -> Self {
        LoadTools { registry }
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

impl Tool for LoadTools {
    type Args = LoadToolsArgs;
    type Return = String;
    type Error = ToolExecError;

    fn name(&self) -> &'static str {
        "load_tools"
    }

    fn group(&self) -> &'static str {
        "core"
    }

    fn description(&self) -> &'static str {
        "Activate one or more tool groups for use in this session. \
         Tools belonging to inactive groups will not be available. \
         The 'core' group is always active and cannot be unloaded."
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
        groups_enum_schema(self.group_names(), "Tool groups to activate")
    }

    fn execute(
        &self,
        args: Self::Args,
        _x_credentials: Option<&ServiceCredential>,
        working_dir: Option<&Path>,
        ctx: Option<&ToolContext>,
    ) -> Result<Self::Return, Self::Error> {
        // Reject unknown group names against the live catalog (the schema
        // enum is advisory — the model may pass anything).  Unknown groups
        // would otherwise be persisted into the session's active set and
        // reported as successfully activated.
        if let Some(known) = self.registry.upgrade().map(|r| r.known_group_names())
            && let Some(unknown) = unknown_group_names(&args.groups, &known)
        {
            return Err(ToolExecError(format!(
                "Unknown tool group(s): {}",
                unknown.join(", ")
            )));
        }
        execute_load_tools(&args, working_dir, ctx)
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

    /// Run execute_load_tools on a thread (it blocks waiting for the daemon's
    /// reply), intercept the DaemonCommand on the main thread, send the reply,
    /// and join.  Deterministic — no time-based waits: the tool blocks on the
    /// reply channel until this test sends it.  Takes owned args because the
    /// spawned thread must own everything it touches (`'static` bound).
    fn run_with_daemon_reply(
        args: LoadToolsArgs,
        reply: Result<String, String>,
    ) -> (Result<String, ToolExecError>, DaemonCommand) {
        let (ctx, _daemon_tx, daemon_rx) = test_context();
        let handle = std::thread::spawn(move || execute_load_tools(&args, None, Some(&ctx)));
        let cmd = daemon_rx.recv().unwrap();
        match &cmd {
            DaemonCommand::LoadTools { reply: tx, .. } => {
                tx.send(reply).unwrap();
            }
            other => panic!(
                "expected LoadTools, got {:?}",
                std::mem::discriminant(other)
            ),
        }
        (handle.join().unwrap(), cmd)
    }

    // -- apply_load_tools (pure logic) -------------------------------------

    #[test]
    fn apply_loads_new_groups() {
        let mut active: HashSet<String> = ["core".into(), "git".into()].into_iter().collect();
        let result = apply_load_tools(&mut active, &["shell".into(), "x".into()]);
        assert_eq!(result, "Activated tool groups: shell and x");
        assert!(active.contains("shell"));
        assert!(active.contains("x"));
        assert!(active.contains("core"));
    }

    #[test]
    fn apply_skips_already_active() {
        let mut active: HashSet<String> = ["core".into(), "git".into(), "shell".into()]
            .into_iter()
            .collect();
        let result = apply_load_tools(&mut active, &["shell".into()]);
        assert_eq!(result, "All specified groups were already active.");
    }

    // -- execute -----------------------------------------------------------

    #[test]
    fn execute_sends_daemon_command_and_returns_reply() {
        let args = LoadToolsArgs {
            groups: vec!["shell".into(), "x".into()],
        };
        let (result, cmd) =
            run_with_daemon_reply(args, Ok("Activated tool groups: shell and x".into()));
        assert_eq!(result.unwrap(), "Activated tool groups: shell and x");
        match cmd {
            DaemonCommand::LoadTools {
                session_id, groups, ..
            } => {
                assert_eq!(session_id, 42);
                assert_eq!(groups, vec!["shell", "x"]);
            }
            _ => panic!("expected LoadTools command"),
        }
    }

    #[test]
    fn execute_forwards_daemon_error() {
        let args = LoadToolsArgs {
            groups: vec!["shell".into()],
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
        let args = LoadToolsArgs {
            groups: vec!["shell".into()],
        };
        let result = execute_load_tools(&args, None, None);
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
        let args = LoadToolsArgs { groups: vec![] };
        let result = execute_load_tools(&args, None, Some(&ctx));
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
        let tool = LoadTools::new(Arc::downgrade(&registry));
        let schema = tool.schema();
        let items = schema["properties"]["groups"]["items"].as_object().unwrap();
        let enum_vals = items["enum"].as_array().unwrap();
        let names: Vec<&str> = enum_vals.iter().filter_map(|v| v.as_str()).collect();
        assert!(names.contains(&"git"), "enum should include git: {names:?}");
        assert!(
            !names.contains(&"core"),
            "core must not appear in load_tools enum: {names:?}"
        );
        let required = schema["required"].as_array().unwrap();
        assert!(required.iter().any(|v| v == "groups"));
    }

    #[test]
    fn tool_restricted_to_direct_callers() {
        let registry = ToolRegistry::new().build();
        let tool = LoadTools::new(Arc::downgrade(&registry));
        let callers = tool.allowed_callers();
        assert_eq!(callers, vec![AllowedCaller::Direct]);
        assert!(!callers.contains(&AllowedCaller::Programmatic));
    }

    #[test]
    fn execute_rejects_unknown_group() {
        let registry = ToolRegistry::new().build();
        let tool = LoadTools::new(Arc::downgrade(&registry));
        let args = LoadToolsArgs {
            groups: vec!["not-a-real-group".into()],
        };

        let result = tool.execute(args, None, None, None);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Unknown tool group(s): not-a-real-group")
        );
    }

    #[test]
    fn execute_accepts_core_and_known_groups() {
        let registry = ToolRegistry::new().build();
        let tool = LoadTools::new(Arc::downgrade(&registry));
        // "core" is always-on and valid input (a no-op), as are real groups
        // like "git" — validation must not reject either.
        let args = LoadToolsArgs {
            groups: vec!["core".into(), "git".into()],
        };
        // No context: validation passes first and the execution reports the
        // missing context, proving validation did not reject the names.
        let result = tool.execute(args, None, None, None);
        assert!(
            result
                .err()
                .map(|e| e.to_string())
                .is_some_and(|e| e.contains("no session context"))
        );
    }

    #[test]
    fn describe_invocation_includes_groups() {
        let args = LoadToolsArgs {
            groups: vec!["git".into(), "shell".into()],
        };
        let desc = describe_invocation(&args);
        assert_eq!(desc, "Activating tool groups: git, shell.");
    }

    #[test]
    fn execute_postcard_args_round_trip() {
        // Verify the args can be serialised/deserialised via postcard,
        // which is the wire format used by the VM execution path.
        let args = LoadToolsArgs {
            groups: vec!["git".into()],
        };
        let args_bytes = postcard::to_allocvec(&args).unwrap();
        let decoded: LoadToolsArgs = postcard::from_bytes(&args_bytes).unwrap();
        assert_eq!(decoded.groups, vec!["git"]);
    }
}
