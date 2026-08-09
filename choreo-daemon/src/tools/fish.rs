use super::{
    Tool, ToolExecError,
    context::ToolContext,
    shell_util::{format_shell_output, resolve_workdir, run_shell_streaming, spawn_with_watchdog},
};
use choreo_keystore::ServiceCredential;
use crossbeam_channel;
use schemars::JsonSchema;
use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct FishArgs {
    /// The fish command to execute (runs via `fish -c`)
    pub command: String,
    /// Working directory for the command (relative to the session working directory, or absolute)
    pub workdir: Option<String>,
    /// Timeout in milliseconds (default 30000; capped by outer tool deadline)
    pub timeout: Option<u64>,
}

pub(crate) struct FishShell;

impl Tool for FishShell {
    type Args = FishArgs;
    type Return = String;
    type Error = ToolExecError;

    fn name(&self) -> &'static str {
        "fish"
    }

    fn group(&self) -> &'static str {
        "shell"
    }

    fn description(&self) -> &'static str {
        "Execute a fish shell command. Supports fish pipes, globs, and variables. Prefer this over `exec` when you need fish shell features. Non-interactive only — commands that read from stdin will hang. For running a single program without shell features, use `exec` instead."
    }

    fn supports_streaming_output() -> bool {
        true
    }

    fn describe_invocation(&self, args: &Self::Args) -> String {
        let mut parts = vec![format!("Running fish command: `{}`.", args.command)];
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
        execute_fish_tool(&args, working_dir)
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

        let mut cmd = std::process::Command::new("fish");
        cmd.args(["-c", command])
            .current_dir(&resolved)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        run_shell_streaming(&mut cmd, command, timeout_ms, output_tx)
    }

    fn return_string(ret: &Self::Return) -> String {
        ret.clone()
    }
}

pub fn execute_fish_tool(
    args: &FishArgs,
    working_dir: Option<&Path>,
) -> Result<String, ToolExecError> {
    let command = &args.command;
    let timeout_ms = args.timeout.unwrap_or(30000);

    let resolved = resolve_workdir(args.workdir.as_deref(), working_dir);

    let mut cmd = std::process::Command::new("fish");
    cmd.args(["-c", command])
        .current_dir(&resolved)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let (output, was_killed) = spawn_with_watchdog(&mut cmd, timeout_ms)?;

    Ok(format_shell_output(
        command, &output, timeout_ms, was_killed,
    ))
}

#[cfg(test)]
mod tests {
    use crate::tools::Tool;

    #[test]
    fn fish_tool_has_valid_metadata() {
        let tool = super::FishShell;
        assert!(!tool.name().is_empty());
        assert!(!tool.description().is_empty());
        let schema = tool.schema();
        assert!(schema.is_object());
    }
}
