use crate::daemon::DaemonCommand;
use crate::tools::context::ToolContext;
use crate::tools::{AllowedCaller, Tool, ToolExecError, resolve_path};
use choreo_keystore::ServiceCredential;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use tracing::info;

// ── Args ───────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub(crate) struct SetWorkingDirArgs {
    /// Absolute path or path relative to the current session working directory.
    pub(crate) path: String,
}

// ── Result ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SetWorkingDirResult {
    /// The canonical (symlink-resolved) absolute path of the new working directory.
    pub path: String,
}

// ── Resolution (shared with the worker-copy mirror) ────────────────────────

/// Resolve `path` relative to `working_dir` (with tilde expansion) and
/// canonicalize it, enforcing existence and symlink-escape checks.
///
/// Shared by the tool's own execution and by `requests.rs`'s worker-copy
/// mirror fallback, so the two can never drift on resolution rules.
pub(crate) fn resolve_working_dir_path(
    path: &str,
    working_dir: Option<&Path>,
) -> Result<PathBuf, ToolExecError> {
    // Resolve relative to the current session working directory (or process
    // cwd if none is set yet).  Tilde expansion (`~` → home directory) is
    // handled inside resolve_path.
    let resolved = resolve_path(path, working_dir);

    // canonicalize() resolves symlinks and normalizes the path.  This serves
    // two purposes:
    //   1. Prevents symlink-escape attacks that would let a model redirect
    //      subsequent file ops outside the intended tree.
    //   2. Ensures the path actually exists so subsequent tools (find, grep,
    //      read_file) don't silently operate on a directory that was
    //      mistyped or never created.
    // The downside is that set_working_dir cannot target a path that doesn't
    // exist yet — this is an intentional tradeoff.
    let canonical = resolved.canonicalize().map_err(|e| {
        ToolExecError(format!(
            "path '{}' does not exist or cannot be resolved: {e}",
            resolved.display()
        ))
    })?;
    Ok(canonical)
}

// ── Execute ────────────────────────────────────────────────────────────────

fn execute_set_working_dir(
    args: &SetWorkingDirArgs,
    working_dir: Option<&Path>,
    ctx: Option<&ToolContext>,
) -> Result<SetWorkingDirResult, ToolExecError> {
    let ctx = ctx.ok_or_else(|| ToolExecError("no session context".into()))?;
    let canonical = resolve_working_dir_path(&args.path, working_dir)?;

    info!(
        session_id = ctx.session_id,
        path = %canonical.display(),
        "setting session working directory",
    );

    // Route the change through the daemon, which forwards it to the session's
    // main loop for in-memory update, broadcast, and persistence.  The tool
    // must NOT mutate its own (worker-thread) copy of session state — that
    // copy is discarded when the request finishes, so changes made there
    // would silently revert on the next turn.
    //
    // Synchronous round-trip (like load_tools/unload_tools): the daemon
    // replies immediately if the session is inactive, and the session main
    // loop replies after applying the change — so a success return means the
    // authoritative state was actually updated, and a rejected round-trip is
    // surfaced to the model instead of silently succeeding.
    let (reply, rx) = mpsc::channel();
    ctx.daemon_tx
        .send(DaemonCommand::SetWorkingDir {
            session_id: ctx.session_id,
            path: canonical.clone(),
            reply,
        })
        .map_err(|e| ToolExecError(format!("daemon communication failed: {e}")))?;
    let outcome = rx
        .recv()
        .map_err(|e| ToolExecError(format!("daemon did not respond: {e}")))?;
    outcome.map_err(ToolExecError)?;

    Ok(SetWorkingDirResult {
        path: canonical.to_string_lossy().into_owned(),
    })
}

pub fn describe_invocation(args: &SetWorkingDirArgs) -> String {
    format!("Changing session working directory to '{}'.", args.path)
}

// ── Tool impl ──────────────────────────────────────────────────────────────

pub(crate) struct SetWorkingDir;

impl Tool for SetWorkingDir {
    type Args = SetWorkingDirArgs;
    type Return = SetWorkingDirResult;
    type Error = ToolExecError;

    fn name(&self) -> &'static str {
        "set_working_dir"
    }

    fn group(&self) -> &'static str {
        "core"
    }

    fn description(&self) -> &'static str {
        "Change the working directory for this session. All subsequent file operations, \
         shell commands, and context discovery (AGENTS.md, CLAUDE.md, skills) will \
         resolve relative to this new directory. The change takes effect on the next turn."
    }

    fn describe_invocation(&self, args: &Self::Args) -> String {
        describe_invocation(args)
    }

    fn return_string(ret: &Self::Return) -> String {
        format!("Working directory changed to '{}'.", ret.path)
    }

    // Session-config mutation: only the model (Direct) may redirect the
    // session's file operations — not programmatic callers, who could
    // otherwise silently move the working directory mid-task.
    fn allowed_callers(&self) -> Vec<AllowedCaller> {
        vec![AllowedCaller::Direct]
    }

    fn execute(
        &self,
        args: Self::Args,
        _x_credentials: Option<&ServiceCredential>,
        working_dir: Option<&Path>,
        ctx: Option<&ToolContext>,
    ) -> Result<Self::Return, Self::Error> {
        execute_set_working_dir(&args, working_dir, ctx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::context::ToolContext;
    use std::sync::Arc;

    /// Build a ToolContext with a mock daemon channel.
    /// Returns (context, sender, receiver) so the test can keep the
    /// receiver alive and verify messages.
    ///
    /// Uses `into_path()` on the tempdir to prevent the OS from removing
    /// the directory while the database (which holds an open file handle)
    /// is still in use. The directory leaks and is cleaned up by the OS
    /// temp-directory policy — acceptable for short-lived test helpers.
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

    /// Run execute_set_working_dir on a thread (it blocks waiting for the
    /// daemon's reply), intercept the DaemonCommand on the main thread, send
    /// the reply, and join.  Deterministic — no time-based waits: the tool
    /// blocks on the reply channel until this test sends it.  Takes owned
    /// args because the spawned thread must own everything it touches
    /// (`'static` bound).
    fn run_with_daemon_reply(
        args: SetWorkingDirArgs,
        working_dir: Option<PathBuf>,
        reply: Result<String, String>,
    ) -> (Result<SetWorkingDirResult, ToolExecError>, DaemonCommand) {
        let (ctx, _daemon_tx, daemon_rx) = test_context();
        let wd = working_dir.clone();
        let handle =
            std::thread::spawn(move || execute_set_working_dir(&args, wd.as_deref(), Some(&ctx)));
        let cmd = daemon_rx.recv().unwrap();
        match &cmd {
            DaemonCommand::SetWorkingDir { reply: tx, .. } => {
                tx.send(reply).unwrap();
            }
            other => panic!(
                "expected SetWorkingDir, got {:?}",
                std::mem::discriminant(other)
            ),
        }
        (handle.join().unwrap(), cmd)
    }

    #[test]
    fn execute_set_working_dir_sends_daemon_command() {
        let dir = tempfile::tempdir().unwrap();
        // canonicalize() resolves the `/var` → `/private/var` symlink on macOS,
        // so the expected path is the canonical form the tool reports.
        let canonical = dir.path().canonicalize().unwrap();
        let path = canonical.to_string_lossy().into_owned();
        let args = SetWorkingDirArgs { path: path.clone() };

        let (result, cmd) = run_with_daemon_reply(args, None, Ok(path.clone()));
        assert!(result.is_ok(), "expected ok: {:?}", result.err());
        assert_eq!(result.unwrap().path, path);

        // Verify a SetWorkingDir command was sent to the daemon.
        match cmd {
            DaemonCommand::SetWorkingDir {
                session_id,
                path: sent_path,
                ..
            } => {
                assert_eq!(session_id, 42);
                assert_eq!(sent_path.to_string_lossy(), path);
            }
            _ => panic!("expected SetWorkingDir command"),
        }
    }

    #[test]
    fn execute_resolves_relative_path_against_working_dir() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        let args = SetWorkingDirArgs { path: "sub".into() };

        let (result, _cmd) = run_with_daemon_reply(
            args,
            Some(dir.path().to_path_buf()),
            Ok(sub.to_string_lossy().into_owned()),
        );
        assert!(result.is_ok(), "expected ok: {:?}", result.err());
        assert_eq!(
            result.unwrap().path,
            sub.canonicalize().unwrap().to_string_lossy(),
            "relative path should resolve against working_dir and canonicalize"
        );
    }

    #[test]
    fn execute_expands_tilde() {
        let home = dirs::home_dir().expect("home dir should exist in test env");
        // `~` alone maps to the home directory itself, which always exists.
        let args = SetWorkingDirArgs { path: "~".into() };

        let (result, _cmd) =
            run_with_daemon_reply(args, None, Ok(home.to_string_lossy().into_owned()));
        assert!(result.is_ok(), "expected ok: {:?}", result.err());
        assert_eq!(result.unwrap().path, home.to_string_lossy());
    }

    #[test]
    fn execute_forwards_daemon_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_string_lossy().into_owned();
        let args = SetWorkingDirArgs { path };

        let (result, _cmd) = run_with_daemon_reply(args, None, Err("session is not active".into()));
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
        let args = SetWorkingDirArgs {
            path: "/tmp".into(),
        };
        let result = execute_set_working_dir(&args, None, None);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("no session context")
        );
    }

    #[test]
    fn execute_nonexistent_path_returns_error() {
        let (ctx, _daemon_tx, _daemon_rx) = test_context();
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist");
        let args = SetWorkingDirArgs {
            path: missing.to_string_lossy().into_owned(),
        };
        let result = execute_set_working_dir(&args, None, Some(&ctx));
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("does not exist or cannot be resolved")
        );
        // No command should have been sent for a failed resolution.
    }

    #[test]
    fn describe_invocation_includes_path() {
        let args = SetWorkingDirArgs {
            path: "/home/user/project".into(),
        };
        let desc = describe_invocation(&args);
        assert_eq!(
            desc,
            "Changing session working directory to '/home/user/project'."
        );
    }

    #[test]
    fn return_string_formats_correctly() {
        let result = SetWorkingDirResult {
            path: "/tmp".into(),
        };
        let s = SetWorkingDir::return_string(&result);
        assert_eq!(s, "Working directory changed to '/tmp'.");
    }

    #[test]
    fn tool_schema_has_required_path() {
        let tool = SetWorkingDir;
        let schema = tool.schema();
        let props = schema["properties"].as_object().unwrap();
        assert!(props.contains_key("path"));
        assert_eq!(props["path"]["type"], "string");
        let required = schema["required"].as_array().unwrap();
        assert!(required.iter().any(|v| v == "path"));
    }

    #[test]
    fn tool_restricted_to_direct_callers() {
        let tool = SetWorkingDir;
        let callers = tool.allowed_callers();
        assert_eq!(callers, vec![AllowedCaller::Direct]);
        assert!(!callers.contains(&AllowedCaller::Programmatic));
    }

    #[test]
    fn execute_postcard_args_round_trip() {
        // Verify the args can be serialised/deserialised via postcard,
        // which is the wire format used by the VM execution path.
        let args = SetWorkingDirArgs {
            path: "/tmp".into(),
        };
        let args_bytes = postcard::to_allocvec(&args).unwrap();
        let decoded: SetWorkingDirArgs = postcard::from_bytes(&args_bytes).unwrap();
        assert_eq!(decoded.path, "/tmp");
    }
}
