use super::{
    ToolError,
    shell_util::{format_shell_output, resolve_and_confine, setup_child, spawn_with_watchdog},
};
use schemars::JsonSchema;
use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Shell {
    /// Bourne Again SHell
    Bash,
    /// Debian Almquist SHell
    Dash,
    /// Z SHell
    Zsh,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ShArgs {
    /// The shell command to execute (runs via `<shell> -c`)
    pub command: String,
    /// Which POSIX-compatible shell to use
    pub shell: Shell,
    /// Working directory for the command (relative to the session working directory, or absolute)
    pub workdir: Option<String>,
    /// Timeout in milliseconds (default 30000; capped by outer tool deadline)
    pub timeout: Option<u64>,
}

pub(crate) struct Sh;

define_tool!(
    Sh,
    "sh",
    "Execute a shell command using a POSIX-compatible shell (bash, dash, or zsh). Non-interactive only — commands that read from stdin will hang. The `shell` parameter must be explicitly specified.",
    ShArgs,
    execute_sh_tool,
    "shell"
);

pub fn execute_sh_tool(args: &ShArgs, working_dir: Option<&Path>) -> Result<String, ToolError> {
    let shell_str = match args.shell {
        Shell::Bash => "bash",
        Shell::Dash => "dash",
        Shell::Zsh => "zsh",
    };
    let command = &args.command;
    // The per-tool timeout (default 30s) governs shell execution.
    // The outer deadline in execute_tool_with_timeout (300s) is the
    // absolute safety net — no independent cap needed here.
    let timeout_ms = args.timeout.unwrap_or(30000);

    let resolved = resolve_and_confine(args.workdir.as_deref(), working_dir)?;

    let mut cmd = std::process::Command::new(shell_str);
    cmd.args(["-c", command])
        .current_dir(&resolved)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    setup_child(&mut cmd);

    let (output, was_killed) = spawn_with_watchdog(&mut cmd, timeout_ms)?;

    Ok(format_shell_output(
        command, &output, timeout_ms, was_killed,
    ))
}

#[cfg(test)]
mod tests {
    use crate::tools::Tool;

    #[test]
    fn sh_tool_has_valid_metadata() {
        let tool = super::Sh;
        assert!(!tool.name().is_empty());
        assert!(!tool.description().is_empty());
        let schema = tool.schema();
        assert!(schema.is_object());
    }
}
