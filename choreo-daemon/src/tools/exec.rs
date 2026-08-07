use super::{
    Tool, ToolExecError,
    context::ToolContext,
    shell_util::{format_shell_output, resolve_workdir, run_shell_streaming, spawn_with_watchdog},
};
use choreo_keystore::ServiceCredential;
use crossbeam_channel;
use schemars::JsonSchema;
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ExecArgs {
    /// Program to execute directly (not a shell command). This is not parsed
    /// by a shell — shell operators, globs, and variables won't work. Use the
    /// `sh`, `nushell`, or `fish` tools for shell commands.
    pub command: String,
    /// Literal arguments passed to the program (not shell-parsed)
    #[serde(default)]
    pub args: Vec<String>,
    /// Working directory for the command (relative to the session working directory, or absolute)
    pub workdir: Option<String>,
    /// Timeout in milliseconds (default 30000; capped by outer tool deadline)
    pub timeout: Option<u64>,
}

pub(crate) struct Exec;

impl Tool for Exec {
    type Args = ExecArgs;
    type Return = String;
    type Error = ToolExecError;

    fn name(&self) -> &'static str {
        "exec"
    }

    fn group(&self) -> &'static str {
        "shell"
    }

    fn description(&self) -> &'static str {
        "Execute a single program directly as a subprocess — no shell parsing. The command and each argument are passed literally; pipes, redirects, globs, environment variables, and command chaining are NOT supported. Use exec ONLY when you are certain the program exists and needs no shell features (e.g. `cargo build`, `python3 script.py`). For anything else — or when in doubt — use the `sh` tool (or `nushell`/`fish`) instead; sh is the default choice for running commands. The `command` is resolved against PATH; absolute paths and paths relative to the working directory are also accepted."
    }

    fn supports_streaming_output() -> bool {
        true
    }

    fn describe_invocation(&self, args: &Self::Args) -> String {
        let full_cmd: Vec<&str> = std::iter::once(&args.command)
            .chain(args.args.iter())
            .map(|s| s.as_str())
            .collect();
        let mut parts = vec![format!("Running command: `{}`.", full_cmd.join(" "))];
        if let Some(ref wd) = args.workdir {
            parts.push(format!(" Working directory: `{}`.", wd));
        }
        if let Some(timeout) = args.timeout {
            parts.push(format!(" Timeout: {}ms.", timeout));
        }
        parts.concat()
    }

    fn execute(
        &self,
        args: Self::Args,
        _x_credentials: Option<&ServiceCredential>,
        working_dir: Option<&Path>,
        _ctx: Option<&ToolContext>,
    ) -> Result<Self::Return, Self::Error> {
        // Guard the model-facing path (not the low-level execute_exec_tool,
        // which the integration tests exercise directly for the subprocess
        // plumbing): reject shell syntax and unknown programs with actionable
        // errors before anything is spawned.
        validate_exec_invocation(&args, working_dir)?;
        execute_exec_tool(&args, working_dir)
    }

    fn execute_streaming(
        &self,
        args: Self::Args,
        _x_credentials: Option<&ServiceCredential>,
        working_dir: Option<&Path>,
        output_tx: crossbeam_channel::Sender<Vec<u8>>,
        _ctx: Option<&ToolContext>,
    ) -> Result<Self::Return, Self::Error> {
        validate_exec_invocation(&args, working_dir)?;
        let program = &args.command;
        let prog_args = &args.args;
        let timeout_ms = args.timeout.unwrap_or(30000);
        let resolved = resolve_workdir(args.workdir.as_deref(), working_dir);

        let mut cmd = std::process::Command::new(program);
        cmd.args(prog_args)
            .current_dir(&resolved)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let display_cmd = if prog_args.is_empty() {
            program.to_string()
        } else {
            format!("{} {}", program, prog_args.join(" "))
        };

        run_shell_streaming(&mut cmd, &display_cmd, timeout_ms, output_tx)
    }

    fn return_string(ret: &Self::Return) -> String {
        ret.clone()
    }
}

/// Shell metacharacters that `exec` never interprets. Their presence in the
/// command or its arguments almost always means the caller wanted a shell
/// (pipes, redirects, globs, env expansion, command chaining) rather than
/// direct program execution. Rejecting up front — with a pointer to the shell
/// tools — lets the model learn the boundary from a single inline error
/// instead of a cryptic "No such file or directory" from the spawned process.
const SHELL_METACHARS: &[char] = &['|', '>', '<', '&', ';', '$', '`', '*', '?', '"', '\''];

/// Reject `command`/`args` containing shell metacharacters.
///
/// The command is checked first: a program name containing shell operators is
/// never legitimate (e.g. the model pasting an entire pipeline into
/// `command`). Args are checked per-argument so the error can name the exact
/// offending value. Both messages direct the caller to `sh`/`nushell`/`fish`,
/// which are the correct tools for shell features.
fn validate_no_shell_syntax(command: &str, args: &[String]) -> Result<(), ToolExecError> {
    if let Some(c) = command.chars().find(|c| SHELL_METACHARS.contains(c)) {
        return Err(ToolExecError(format!(
            "exec does not interpret shell syntax: found '{c}' in command \"{command}\". \
             Pipes, redirects, globs, environment variables, and command chaining need a \
             shell — use the `sh`, `nushell`, or `fish` tool instead."
        )));
    }
    for arg in args {
        if let Some(c) = arg.chars().find(|c| SHELL_METACHARS.contains(c)) {
            return Err(ToolExecError(format!(
                "exec does not interpret shell syntax: found '{c}' in argument \"{arg}\". \
                 Pipes, redirects, globs, environment variables, and command chaining need a \
                 shell — use the `sh`, `nushell`, or `fish` tool instead."
            )));
        }
    }
    Ok(())
}

/// Check whether `path` names an existing, executable regular file.
///
/// On Unix, "executable" means at least one execute bit is set — the same
/// criterion `execvp` effectively uses. On non-Unix platforms we fall back to
/// "is a file" since the executable-bit concept differs.
fn is_executable_file(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

/// Resolve `command` to an executable file the way the spawned process will.
///
/// Names containing a path separator are used directly — absolute, or relative
/// to `workdir` (the child resolves relative program paths against its current
/// directory). Bare names are searched on `PATH`. Returns `None` when nothing
/// executable was found, mirroring what `Command::new` + `execvp` would fail
/// with.
fn resolve_program(command: &str, workdir: &Path) -> Option<PathBuf> {
    if command.contains('/') {
        // Absolute or workdir-relative path — check it as-is. Command::new
        // passes the string straight to execvp, which resolves relative
        // program paths against the child's current directory (workdir).
        let candidate = if Path::new(command).is_absolute() {
            PathBuf::from(command)
        } else {
            workdir.join(command)
        };
        return is_executable_file(&candidate).then_some(candidate);
    }
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|dir| dir.join(command))
            .find(|p| is_executable_file(p))
    })
}

/// Pre-flight validation for the model-facing `exec` tool.
///
/// Combines the shell-syntax guard and the program-existence check into one
/// call the `Tool` impls share, so `execute` and `execute_streaming` never
/// drift. Pure filesystem introspection — nothing is spawned — so it is safe
/// to call before the subprocess machinery starts.
fn validate_exec_invocation(
    args: &ExecArgs,
    working_dir: Option<&Path>,
) -> Result<(), ToolExecError> {
    validate_no_shell_syntax(&args.command, &args.args)?;
    let workdir = resolve_workdir(args.workdir.as_deref(), working_dir);
    if resolve_program(&args.command, &workdir).is_none() {
        // Surface the searched PATH so the model can see exactly what was
        // tried, and give it two concrete recovery paths.
        let path_list = std::env::var_os("PATH")
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| "<unset>".to_string());
        return Err(ToolExecError(format!(
            "command not found: '{}'. Searched PATH: {path_list}. Verify the program is \
             installed, pass an absolute path, or use `sh` with `command -v <name>` to \
             discover where it lives.",
            args.command
        )));
    }
    Ok(())
}

pub fn execute_exec_tool(
    args: &ExecArgs,
    working_dir: Option<&Path>,
) -> Result<String, ToolExecError> {
    let program = &args.command;
    let prog_args = &args.args;
    let timeout_ms = args.timeout.unwrap_or(30000);

    let resolved = resolve_workdir(args.workdir.as_deref(), working_dir);

    let mut cmd = std::process::Command::new(program);
    cmd.args(prog_args)
        .current_dir(&resolved)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let (output, was_killed) = spawn_with_watchdog(&mut cmd, timeout_ms)?;

    // Build a display string like "$ program arg1 arg2"
    let display_cmd = if prog_args.is_empty() {
        program.to_string()
    } else {
        format!("{} {}", program, prog_args.join(" "))
    };

    Ok(format_shell_output(
        &display_cmd,
        &output,
        timeout_ms,
        was_killed,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::Tool;

    #[test]
    fn exec_tool_has_valid_metadata() {
        let tool = super::Exec;
        assert!(!tool.name().is_empty());
        assert!(!tool.description().is_empty());
        let schema = tool.schema();
        assert!(schema.is_object());
    }

    #[test]
    fn describe_invocation_includes_command_and_args() {
        let tool = Exec;
        let args = ExecArgs {
            command: "cargo".into(),
            args: vec!["build".into(), "--release".into()],
            workdir: None,
            timeout: None,
        };
        let desc = tool.describe_invocation(&args);
        assert_eq!(desc, "Running command: `cargo build --release`.");
    }

    #[test]
    fn describe_invocation_includes_workdir() {
        let tool = Exec;
        let args = ExecArgs {
            command: "make".into(),
            args: vec![],
            workdir: Some("/home/user/project".into()),
            timeout: None,
        };
        let desc = tool.describe_invocation(&args);
        assert!(desc.contains("Running command: `make`."));
        assert!(desc.contains("Working directory: `/home/user/project`."));
    }

    #[test]
    fn describe_invocation_includes_timeout() {
        let tool = Exec;
        let args = ExecArgs {
            command: "sleep".into(),
            args: vec!["10".into()],
            workdir: None,
            timeout: Some(30000),
        };
        let desc = tool.describe_invocation(&args);
        assert!(desc.contains("Running command: `sleep 10`."));
        assert!(desc.contains("Timeout: 30000ms."));
    }

    // ── Shell-syntax guard ────────────────────────────────────────────

    #[test]
    fn shell_syntax_rejected_in_command() {
        // The classic failure: the model pastes a whole pipeline into `command`.
        let err = validate_no_shell_syntax("echo hello | grep world", &[])
            .unwrap_err()
            .to_string();
        assert!(err.contains("shell syntax"), "{err}");
        assert!(err.contains("`sh`"), "{err}");
    }

    #[test]
    fn shell_syntax_rejected_in_args() {
        // Redirects and chaining in arguments are also shell intent.
        let err = validate_no_shell_syntax("foo", &["a > out.txt".into()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("shell syntax"), "{err}");
        assert!(err.contains("`sh`"), "{err}");
    }

    #[test]
    fn shell_syntax_rejects_globs_and_quotes() {
        for bad in ["*.rs", "\"quoted\"", "a$VAR", "a'b'"] {
            let res = validate_no_shell_syntax("cmd", &[bad.into()]);
            assert!(res.is_err(), "expected '{bad}' to be rejected");
        }
    }

    #[test]
    fn clean_invocation_accepted() {
        assert!(validate_no_shell_syntax("cargo", &["build".into(), "--release".into()]).is_ok());
        assert!(validate_no_shell_syntax("git", &["log".into(), "--oneline".into()]).is_ok());
    }

    // ── Program-existence guard ───────────────────────────────────────

    #[test]
    fn unknown_program_rejected_with_actionable_error() {
        let args = ExecArgs {
            command: "definitely-not-a-real-binary-xyz".into(),
            args: vec![],
            workdir: None,
            timeout: None,
        };
        let err = validate_exec_invocation(&args, None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("command not found"), "{err}");
        assert!(err.contains("command -v"), "{err}");
        assert!(err.contains("PATH"), "{err}");
    }

    #[test]
    fn known_program_accepted() {
        // Existence check only — nothing is spawned in this unit test.
        let args = ExecArgs {
            command: "sh".into(),
            args: vec![],
            workdir: None,
            timeout: None,
        };
        assert!(validate_exec_invocation(&args, None).is_ok());
    }

    #[test]
    fn absolute_path_accepted() {
        // /bin/sh must exist and be executable on any Unix dev box.
        let args = ExecArgs {
            command: "/bin/sh".into(),
            args: vec![],
            workdir: None,
            timeout: None,
        };
        assert!(validate_exec_invocation(&args, None).is_ok());
    }
}
