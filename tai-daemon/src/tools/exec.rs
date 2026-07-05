use super::{
    ToolError, ToolResult, tool_ok,
    shell_util::{format_shell_output, resolve_and_confine, setup_child, spawn_with_watchdog},
};
use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Deserialize)]
struct ExecArgs {
    command: String,
    #[serde(default)]
    args: Vec<String>,
    workdir: Option<String>,
    timeout: Option<u64>,
}

define_tool_with_cwd!(
    Exec,
    "exec",
    "Execute a program directly without a shell. The command is not parsed by a shell — no pipes, redirects, glob expansion, or environment variable interpolation. Prefer this over `sh` when you only need to run a single program with arguments (lower risk of shell-injection issues).",
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

pub fn execute_exec_tool(arguments_json: &str, cwd: Option<&Path>) -> ToolResult {
    match execute_exec_inner(arguments_json, cwd) {
        Ok(content) => tool_ok(content),
        Err(error) => error.into(),
    }
}

fn execute_exec_inner(
    arguments_json: &str,
    cwd: Option<&Path>,
) -> Result<String, ToolError> {
    let args: ExecArgs = serde_json::from_str(arguments_json)?;

    let program = args.command;
    let prog_args = args.args;
    let timeout_ms = args.timeout.unwrap_or(30000).min(300000);

    let resolved = resolve_and_confine(args.workdir.as_deref(), cwd)?;

    let mut cmd = std::process::Command::new(&program);
    cmd.args(&prog_args)
        .current_dir(&resolved)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    setup_child(&mut cmd);

    let (output, was_killed) = spawn_with_watchdog(&mut cmd, timeout_ms)?;

    // Build a display string like "$ program arg1 arg2"
    let display_cmd = if prog_args.is_empty() {
        program.clone()
    } else {
        format!("{} {}", program, prog_args.join(" "))
    };

    Ok(format_shell_output(&display_cmd, &output, timeout_ms, was_killed))
}
