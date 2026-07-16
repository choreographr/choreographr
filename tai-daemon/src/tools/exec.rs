use super::{
    Tool, ToolError,
    context::ToolContext,
    shell_util::{
        format_shell_output, resolve_and_confine, run_shell_streaming, setup_child,
        spawn_with_watchdog,
    },
};
use schemars::JsonSchema;
use serde::Deserialize;
use std::path::Path;
use std::sync::mpsc;
use tai_keystore::ServiceCredential;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ExecArgs {
    /// Program or command to execute (first argument)
    pub command: String,
    /// Additional arguments passed to the program
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

    fn name(&self) -> &'static str {
        "exec"
    }

    fn group(&self) -> &'static str {
        "shell"
    }

    fn description(&self) -> &'static str {
        "Execute a program directly without a shell. The command is not parsed by a shell — no pipes, redirects, glob expansion, or environment variable interpolation. Prefer this over `sh` when you only need to run a single program with arguments (lower risk of shell-injection issues)."
    }

    fn execute(
        &self,
        args: Self::Args,
        _x_credentials: Option<&ServiceCredential>,
        working_dir: Option<&Path>,
        _ctx: Option<&ToolContext>,
    ) -> Result<String, ToolError> {
        execute_exec_tool(&args, working_dir)
    }

    fn execute_streaming(
        &self,
        args: Self::Args,
        _x_credentials: Option<&ServiceCredential>,
        working_dir: Option<&Path>,
        output_tx: mpsc::Sender<Vec<u8>>,
        _ctx: Option<&ToolContext>,
    ) -> Result<String, ToolError> {
        let program = &args.command;
        let prog_args = &args.args;
        let timeout_ms = args.timeout.unwrap_or(30000);
        let resolved = resolve_and_confine(args.workdir.as_deref(), working_dir)?;

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

    fn output_content(ret: &Self::Return) -> String {
        ret.clone()
    }
}

pub fn execute_exec_tool(args: &ExecArgs, working_dir: Option<&Path>) -> Result<String, ToolError> {
    let program = &args.command;
    let prog_args = &args.args;
    let timeout_ms = args.timeout.unwrap_or(30000);

    let resolved = resolve_and_confine(args.workdir.as_deref(), working_dir)?;

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
    use crate::tools::Tool;

    #[test]
    fn exec_tool_has_valid_metadata() {
        let tool = super::Exec;
        assert!(!tool.name().is_empty());
        assert!(!tool.description().is_empty());
        let schema = tool.schema();
        assert!(schema.is_object());
    }
}
