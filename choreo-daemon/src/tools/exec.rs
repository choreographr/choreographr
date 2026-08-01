use super::{
    Tool, ToolExecError,
    context::ToolContext,
    shell_util::{
        format_shell_output, resolve_workdir, run_shell_streaming, setup_child, spawn_with_watchdog,
    },
};
use choreo_keystore::ServiceCredential;
use schemars::JsonSchema;
use serde::Deserialize;
use std::path::Path;
use std::sync::mpsc;

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
        "Execute a program directly — no shell parsing. The command is run as a subprocess without a shell, so pipes, redirects, glob expansion, and environment variable interpolation are NOT available. Do NOT use exec to run a shell interpreter — use the dedicated `sh`, `nushell`, or `fish` tools for shell commands. Prefer exec over shell tools when you only need to run a single program with arguments (lower risk of parsing issues)."
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
        execute_exec_tool(&args, working_dir)
    }

    fn execute_streaming(
        &self,
        args: Self::Args,
        _x_credentials: Option<&ServiceCredential>,
        working_dir: Option<&Path>,
        output_tx: mpsc::Sender<Vec<u8>>,
        _ctx: Option<&ToolContext>,
    ) -> Result<Self::Return, Self::Error> {
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

    setup_child(&mut cmd);

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
}
