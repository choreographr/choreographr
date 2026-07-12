use super::{
    ToolError,
    shell_util::{format_shell_output, resolve_and_confine, setup_child, spawn_with_watchdog},
};
use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Deserialize)]
pub struct NuArgs {
    pub command: String,
    pub workdir: Option<String>,
    pub timeout: Option<u64>,
}

pub(crate) struct NuShell;

define_tool!(
    NuShell,
    "nushell",
    "Execute a nushell command in the project directory. Returns combined stdout/stderr and exit code. Non-interactive only — commands that read from stdin will hang.",
    NuArgs,
    execute_nu_tool,
    serde_json::json!({
        "type": "object",
        "properties": {
            "command": {
                "type": "string",
                "description": "The nushell command to execute (runs via `nu -c`)"
            },
            "workdir": {
                "type": "string",
                "description": "Working directory for the command (relative to the session working directory, or absolute)"
            },
            "timeout": {
                "type": "integer",
                "description": "Timeout in milliseconds (default 30000; capped by outer tool deadline)",
                "default": 30000
            }
        },
        "required": ["command"],
        "additionalProperties": false
    }),
    "shell"
);

pub fn execute_nu_tool(args: &NuArgs, working_dir: Option<&Path>) -> Result<String, ToolError> {
    let command = &args.command;
    let timeout_ms = args.timeout.unwrap_or(30000);

    let resolved = resolve_and_confine(args.workdir.as_deref(), working_dir)?;

    let mut cmd = std::process::Command::new("nu");
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
    fn nushell_tool_has_valid_metadata() {
        let tool = super::NuShell;
        assert!(!tool.name().is_empty());
        assert!(!tool.description().is_empty());
        let schema = tool.schema();
        assert!(schema.is_object());
    }
}
