use super::{
    Tool, ToolExecError,
    context::ToolContext,
    shell_resolver::{self, ResolvedShell},
    shell_util::{format_shell_output, resolve_workdir, run_shell_streaming, spawn_with_watchdog},
};
use choreo_keystore::ServiceCredential;
use crossbeam_channel;
use schemars::JsonSchema;
use serde::Deserialize;
use std::path::Path;
use std::process::{Command, Stdio};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ShArgs {
    /// The shell command to execute (runs via `<shell> -c`)
    pub command: String,
    /// Working directory for the command (relative to the session working directory, or absolute)
    pub workdir: Option<String>,
    /// Timeout in milliseconds (default 30000; the daemon's outer deadline is raised to cover this when longer)
    pub timeout: Option<u64>,
}

/// The `sh` tool.
///
/// The shell is resolved ONCE at daemon startup (see [`shell_resolver`]) and
/// carried here: the model never picks a shell, so [`ShArgs`] has no `shell`
/// parameter. The tool is only registered when resolution succeeds, so `Sh`
/// always has a usable [`ResolvedShell`].
pub(crate) struct Sh {
    shell: ResolvedShell,
}

impl Sh {
    /// Resolve the shell this machine's `sh` tool should run under, or `None`
    /// when no suitable POSIX shell is installed (in which case the tool is not
    /// registered at all).
    pub(crate) fn resolve() -> Option<Sh> {
        shell_resolver::detect_default().map(|shell| Sh { shell })
    }
}

impl Tool for Sh {
    type Args = ShArgs;
    type Return = String;
    type Error = ToolExecError;

    fn name(&self) -> &'static str {
        "sh"
    }

    fn group(&self) -> &'static str {
        "shell"
    }

    fn description(&self) -> &str {
        // The description names the resolved shell (type + compatibility +
        // version) and never the filesystem path.
        &self.shell.description
    }

    fn supports_streaming_output() -> bool {
        true
    }

    fn describe_invocation(&self, args: &Self::Args) -> String {
        let mut parts = vec![format!("Running shell command: `{}`.", args.command)];
        if let Some(timeout) = args.timeout {
            parts.push(format!(" Timeout: {timeout}ms."));
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
        execute_sh_with(&self.shell, &args, working_dir)
    }

    fn execute_streaming(
        &self,
        args: Self::Args,
        _x_credentials: Option<&ServiceCredential>,
        working_dir: Option<&Path>,
        output_tx: crossbeam_channel::Sender<Vec<u8>>,
        _ctx: Option<&ToolContext>,
    ) -> Result<Self::Return, Self::Error> {
        let command = &args.command;
        let timeout_ms = args.timeout.unwrap_or(30000);
        let resolved = resolve_workdir(args.workdir.as_deref(), working_dir);

        let mut cmd = build_command(&self.shell, command);
        cmd.current_dir(&resolved)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        run_shell_streaming(&mut cmd, command, timeout_ms, output_tx)
    }

    fn return_string(ret: &Self::Return) -> String {
        ret.clone()
    }
}

/// Build the child `Command` that runs `command` under the resolved shell.
///
/// The single place that knows the shell's invocation shape, shared by the
/// buffered and streaming paths: `-c command` for every family, plus the
/// forced `argv[0]` when the resolved shell needs one (zsh → `sh`, busybox →
/// `ash`). `arg0` is a Unix-only primitive — this is a Unix-shell tool, so the
/// forcing is gated to Unix and a resolved shell on Windows (Git/WSL bash)
/// carries no `argv0`.
fn build_command(shell: &ResolvedShell, command: &str) -> Command {
    let mut cmd = Command::new(&shell.program);
    cmd.args(["-c", command]);
    #[cfg(unix)]
    if let Some(argv0) = &shell.argv0 {
        use std::os::unix::process::CommandExt as _;
        cmd.arg0(argv0);
    }
    cmd
}

/// Run `command` under `shell` and return its buffered output. The core of the
/// `sh` tool, shared with the public [`execute_sh_tool`] convenience entry point.
///
/// # Errors
///
/// Returns Err if the shell cannot be spawned, the command times out, or the
/// command exits non-zero.
pub(crate) fn execute_sh_with(
    shell: &ResolvedShell,
    args: &ShArgs,
    working_dir: Option<&Path>,
) -> Result<String, ToolExecError> {
    let command = &args.command;
    // The per-tool timeout (default 30s) governs shell execution.
    // The outer deadline in execute_tool_with_timeout (300s) is the
    // absolute safety net — no independent cap needed here.
    let timeout_ms = args.timeout.unwrap_or(30000);

    let resolved = resolve_workdir(args.workdir.as_deref(), working_dir);

    let mut cmd = build_command(shell, command);
    cmd.current_dir(&resolved)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let (output, was_killed) = spawn_with_watchdog(&mut cmd, timeout_ms)?;

    Ok(format_shell_output(
        command, &output, timeout_ms, was_killed,
    ))
}

/// Run `command` under the process's resolved POSIX shell and return its output.
///
/// A convenience entry point that resolves (and caches) the shell itself; the
/// `sh` tool uses [`execute_sh_with`] with the shell it already holds.
///
/// # Errors
///
/// Returns Err if no suitable POSIX shell is installed, the shell cannot be
/// spawned, the command times out, or the command exits non-zero.
pub fn execute_sh_tool(args: &ShArgs, working_dir: Option<&Path>) -> Result<String, ToolExecError> {
    let shell = shell_resolver::detect_default()
        .ok_or_else(|| ToolExecError("no suitable POSIX shell found on this system".into()))?;
    execute_sh_with(&shell, args, working_dir)
}

#[cfg(test)]
mod tests {
    use crate::tools::Tool;

    #[test]
    fn sh_tool_has_valid_metadata_when_a_shell_is_available() {
        // On a host with a POSIX shell the tool resolves and advertises a
        // non-empty description; on a host without one `resolve` is `None` and
        // the tool is simply not registered.
        if let Some(tool) = super::Sh::resolve() {
            assert_eq!(tool.name(), "sh");
            assert_ne!(tool.description(), "");
            assert!(tool.schema().is_object());
        }
    }

    #[test]
    fn args_schema_has_no_shell_parameter() {
        let schema =
            serde_json::to_value(schemars::schema_for!(super::ShArgs)).expect("serialize schema");
        let properties = &schema["properties"];
        assert!(properties.get("command").is_some());
        assert!(
            properties.get("shell").is_none(),
            "the model must not pick a shell"
        );
    }
}
