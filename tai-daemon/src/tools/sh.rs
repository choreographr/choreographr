use super::{
    ToolError, ToolResult,
    shell_util::{
        binary_exists, format_shell_output, resolve_and_confine, setup_child, spawn_with_watchdog,
    },
    tool_ok,
};
use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Deserialize)]
struct ShArgs {
    command: String,
    shell: String,
    workdir: Option<String>,
    timeout: Option<u64>,
}

/// Build the JSON schema dynamically so the `shell` enum only lists
/// shells that are actually installed on the current system.
fn sh_schema() -> serde_json::Value {
    let shells: Vec<&str> = ["bash", "dash", "zsh"]
        .iter()
        .filter(|s| binary_exists(s))
        .copied()
        .collect();
    serde_json::json!({
        "type": "object",
        "properties": {
            "command": {
                "type": "string",
                "description": "The shell command to execute (runs via `<shell> -c`)"
            },
            "shell": {
                "type": "string",
                "enum": shells,
                "description": "Which POSIX-compatible shell to use. Must be one of the available variants on this system."
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
        "required": ["command", "shell"],
        "additionalProperties": false
    })
}

define_tool!(
    Sh,
    "sh",
    "Execute a shell command using a POSIX-compatible shell (bash, dash, or zsh). Non-interactive only — commands that read from stdin will hang. The `shell` parameter must be explicitly specified.",
    execute_sh_tool,
    sh_schema(),
    "shell"
);

pub fn execute_sh_tool(arguments_json: &str, cwd: Option<&Path>) -> ToolResult {
    match execute_sh_inner(arguments_json, cwd) {
        Ok(content) => tool_ok(content),
        Err(error) => error.into(),
    }
}

fn execute_sh_inner(arguments_json: &str, cwd: Option<&Path>) -> Result<String, ToolError> {
    let args: ShArgs = serde_json::from_str(arguments_json)?;

    let shell = args.shell;
    let command = args.command;
    let timeout_ms = args.timeout.unwrap_or(30000).min(300000);

    let resolved = resolve_and_confine(args.workdir.as_deref(), cwd)?;

    let mut cmd = std::process::Command::new(&shell);
    cmd.args(["-c", &command])
        .current_dir(&resolved)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    setup_child(&mut cmd);

    let (output, was_killed) = spawn_with_watchdog(&mut cmd, timeout_ms)?;

    Ok(format_shell_output(
        &command, &output, timeout_ms, was_killed,
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
