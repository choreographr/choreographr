use super::{ToolError, truncate_tool_output};
use std::{
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::Duration,
};

/// Check whether a binary with the given name exists somewhere in PATH.
pub(crate) fn binary_exists(name: &str) -> bool {
    std::env::var_os("PATH")
        .map(|path| {
            std::env::split_paths(&path).any(|dir| dir.join(name).is_file())
        })
        .unwrap_or(false)
}

/// Resolve the working directory and enforce that it stays within the session CWD.
pub(crate) fn resolve_and_confine(
    workdir: Option<&str>,
    cwd: Option<&Path>,
) -> Result<PathBuf, ToolError> {
    let workdir_str = workdir.unwrap_or(".");
    let resolved = super::resolve_path(workdir_str, cwd);
    if let Some(session_cwd) = cwd {
        check_path_confinement(&resolved, session_cwd)?;
    }
    Ok(resolved)
}

/// Strip environment variables that could be used for code injection.
pub(crate) fn sanitize_env(cmd: &mut Command) {
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
}

/// Attach resource limits (AS, FSIZE) via a pre-exec hook.
/// Must be called inside an `unsafe { cmd.pre_exec(|| …) }` block.
pub(crate) fn apply_rlimits() -> Result<(), std::io::Error> {
    let limits = [
        (libc::RLIMIT_AS, 4 * 1024 * 1024 * 1024),
        (libc::RLIMIT_FSIZE, 100 * 1024 * 1024),
    ];
    for (resource, value) in limits {
        let lim = libc::rlimit {
            rlim_cur: value,
            rlim_max: value,
        };
        if unsafe { libc::setrlimit(resource, &lim) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Apply all child-process hardening (env sanitization + resource limits).
pub(crate) fn setup_child(cmd: &mut Command) {
    sanitize_env(cmd);
    unsafe {
        cmd.pre_exec(apply_rlimits);
    }
}

/// Spawn the command, run a watchdog thread to enforce the timeout,
/// and return the process output along with a flag indicating whether
/// the watchdog killed the process.
///
/// The watchdog blocks on a channel receive with the given timeout.
/// When the child finishes, the main thread sends a signal through the
/// channel, waking the watchdog early — no polling required.
pub(crate) fn spawn_with_watchdog(
    cmd: &mut Command,
    timeout_ms: u64,
) -> Result<(Output, bool), ToolError> {
    let child = cmd.spawn()?;
    let pid = child.id();

    let was_killed = Arc::new(AtomicBool::new(false));
    let wk = was_killed.clone();

    let (done_tx, done_rx) = mpsc::channel::<()>();

    let watchdog = std::thread::spawn(move || {
        if done_rx.recv_timeout(Duration::from_millis(timeout_ms)).is_err() {
            // Timeout expired before the main thread signalled completion — kill.
            wk.store(true, Ordering::SeqCst);
            unsafe {
                libc::kill(pid as i32, libc::SIGKILL);
            }
        }
    });

    let output = child.wait_with_output()?;
    let _ = done_tx.send(());
    let _ = watchdog.join();

    Ok((output, was_killed.load(Ordering::SeqCst)))
}

/// Format the tool output string for a shell-style command.
pub(crate) fn format_shell_output(
    display_cmd: &str,
    output: &Output,
    timeout_ms: u64,
    was_killed: bool,
) -> String {
    if was_killed {
        return truncate_tool_output(&format!(
            "$ {display_cmd}\n\n[command timed out after {timeout_ms}ms]\n\nExit code: -1"
        ));
    }
    let mut combined = output.stdout.clone();
    combined.extend_from_slice(&output.stderr);
    let combined_str = String::from_utf8_lossy(&combined);
    let exit_code = output.status.code().unwrap_or(-1);
    truncate_tool_output(&format!(
        "$ {display_cmd}\n{combined_str}\n\nExit code: {exit_code}"
    ))
}

fn check_path_confinement(
    resolved: &Path,
    session_cwd: &Path,
) -> Result<(), ToolError> {
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
