use super::{ToolError, ToolResult, tool_ok, truncate_tool_output};
use serde::Deserialize;
use std::{
    os::unix::process::CommandExt,
    path::Path,
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

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
    })
);

pub fn execute_nu_tool(arguments_json: &str, cwd: Option<&std::path::Path>) -> ToolResult {
    match execute_nu_inner(arguments_json, cwd) {
        Ok(content) => tool_ok(content),
        Err(error) => error.into(),
    }
}

fn execute_nu_inner(
    arguments_json: &str,
    cwd: Option<&Path>,
) -> std::result::Result<String, ToolError> {
    let args: NuArgs = serde_json::from_str(arguments_json)?;

    let command = args.command;
    let workdir_str = args.workdir.unwrap_or_else(|| ".".to_string());
    let timeout_ms = args.timeout.unwrap_or(30000).min(300000);

    let resolved = super::resolve_path(&workdir_str, cwd);

    if let Some(session_cwd) = cwd {
        check_path_confinement(&resolved, session_cwd)?;
    }

    let mut cmd = std::process::Command::new("nu");
    cmd.args(["-c", &command])
        .current_dir(&resolved)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    for var in &[
        "LD_PRELOAD",
        "LD_LIBRARY_PATH",
        "LD_AUDIT",
        "LD_DEBUG",
        "PYTHONPATH",
        "PERL5LIB",
        "RUBYLIB",
        "DYLD_INSERT_LIBRARIES",
    ] {
        cmd.env_remove(var);
    }

    unsafe {
        cmd.pre_exec(|| {
            let limits = [
                (libc::RLIMIT_AS, 4 * 1024 * 1024 * 1024),
                (libc::RLIMIT_FSIZE, 100 * 1024 * 1024),
            ];
            for (resource, value) in limits {
                let lim = libc::rlimit {
                    rlim_cur: value,
                    rlim_max: value,
                };
                if libc::setrlimit(resource, &lim) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }

    let child = cmd.spawn()?;
    let pid = child.id();

    let was_killed = Arc::new(AtomicBool::new(false));
    let cancel = Arc::new(AtomicBool::new(false));
    let was_killed_clone = was_killed.clone();
    let cancel_clone = cancel.clone();

    let watchdog = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(timeout_ms));
        if !cancel_clone.load(Ordering::SeqCst) {
            was_killed_clone.store(true, Ordering::SeqCst);
            unsafe {
                libc::kill(pid as i32, libc::SIGKILL);
            }
        }
    });

    let output = child.wait_with_output()?;
    cancel.store(true, Ordering::SeqCst);
    let _ = watchdog.join();

    if was_killed.load(Ordering::SeqCst) {
        return Ok(truncate_tool_output(&format!(
            "$ {command}\n\n[command timed out after {timeout_ms}ms]\n\nExit code: -1"
        )));
    }

    let mut combined = output.stdout;
    combined.extend_from_slice(&output.stderr);
    let combined_str = String::from_utf8_lossy(&combined);
    let exit_code = output.status.code().unwrap_or(-1);

    Ok(truncate_tool_output(&format!(
        "$ {command}\n{combined_str}\n\nExit code: {exit_code}"
    )))
}

fn check_path_confinement(
    resolved: &Path,
    session_cwd: &Path,
) -> std::result::Result<(), ToolError> {
    let resolved_canonical = std::fs::canonicalize(resolved)
        .map_err(|e| ToolError::Other(format!("cannot resolve workdir path: {e}")))?;
    let cwd_canonical = std::fs::canonicalize(session_cwd)
        .map_err(|e| ToolError::Other(format!("cannot resolve session cwd: {e}")))?;
    if !resolved_canonical.starts_with(&cwd_canonical) {
        return Err(ToolError::Other(format!(
            "workdir '{}' is outside the session working directory '{}'",
            resolved.display(),
            cwd_canonical.display()
        )));
    }
    Ok(())
}
