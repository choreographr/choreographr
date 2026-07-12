use super::{
    ToolError,
    shell_util::{format_shell_output, resolve_and_confine, setup_child, spawn_with_watchdog},
};
use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Deserialize)]
pub struct ExecArgs {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    pub workdir: Option<String>,
    pub timeout: Option<u64>,
}

pub(crate) struct Exec;

define_tool!(
    Exec,
    "exec",
    "Execute a program directly without a shell. The command is not parsed by a shell — no pipes, redirects, glob expansion, or environment variable interpolation. Prefer this over `sh` when you only need to run a single program with arguments (lower risk of shell-injection issues).",
    ExecArgs,
    execute_exec_tool,
    serde_json::json!({
        "type": "object",
        "properties": {
            "command": {
                "type": "string",
                "description": "Executable path or name (resolved against PATH if not absolute)"
            },
            "args": {
                "type": "array",
                "items": { "type": "string" },
                "description": "Arguments passed to the executable"
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
