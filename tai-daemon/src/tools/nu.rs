use super::{
    ToolError, ToolResult, tool_ok,
    shell_util::{format_shell_output, resolve_and_confine, setup_child, spawn_with_watchdog},
};
use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Deserialize)]
struct NuArgs {
    command: String,
    workdir: Option<String>,
    timeout: Option<u64>,
}

define_tool_with_cwd!(
    NuShell,
    "nushell",
    "Execute a nushell command in the project directory. Returns combined stdout/stderr and exit code. Non-interactive only — commands that read from stdin will hang.",
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
                "description": "Working directory for the command (relative to session CWD, or absolute)"
            },
            "timeout": {
                "type": "integer",
                "description": "Timeout in milliseconds (default 30000, max 300000)",
                "default": 30000
            }
        },
        "required": ["command"],
        "additionalProperties": false
    }),
    "shell"
);

pub fn execute_nu_tool(arguments_json: &str, cwd: Option<&Path>) -> ToolResult {
    match execute_nu_inner(arguments_json, cwd) {
        Ok(content) => tool_ok(content),
        Err(error) => error.into(),
    }
}

fn execute_nu_inner(
    arguments_json: &str,
    cwd: Option<&Path>,
) -> Result<String, ToolError> {
    let args: NuArgs = serde_json::from_str(arguments_json)?;

    let command = args.command;
    let timeout_ms = args.timeout.unwrap_or(30000).min(300000);

    let resolved = resolve_and_confine(args.workdir.as_deref(), cwd)?;

    let mut cmd = std::process::Command::new("nu");
    cmd.args(["-c", &command])
        .current_dir(&resolved)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    setup_child(&mut cmd);

    let (output, was_killed) = spawn_with_watchdog(&mut cmd, timeout_ms)?;

    Ok(format_shell_output(&command, &output, timeout_ms, was_killed))
}
