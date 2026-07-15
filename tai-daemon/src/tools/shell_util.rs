use super::{ToolError, truncate_tool_output};
use std::{
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::mpsc,
    time::Duration,
};

/// Check whether a binary with the given name exists somewhere in PATH.
pub(crate) fn binary_exists(name: &str) -> bool {
    std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).any(|dir| dir.join(name).is_file()))
        .unwrap_or(false)
}

/// Resolve the working directory and enforce that it stays within the session working directory.
pub(crate) fn resolve_and_confine(
    workdir: Option<&str>,
    working_dir: Option<&Path>,
) -> Result<PathBuf, ToolError> {
    super::confine_path(workdir.unwrap_or("."), working_dir)
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
    use nix::sys::resource::{Resource, setrlimit};

    let limits = [
        (Resource::RLIMIT_AS, 4 * 1024 * 1024 * 1024),
        (Resource::RLIMIT_FSIZE, 100 * 1024 * 1024),
    ];
    for (resource, value) in limits {
        setrlimit(resource, value, value)?;
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

    let (done_tx, done_rx) = mpsc::channel::<()>();
    let (killed_tx, killed_rx) = mpsc::channel::<()>();

    let watchdog = std::thread::spawn(move || {
        if done_rx
            .recv_timeout(Duration::from_millis(timeout_ms))
            .is_err()
        {
            // Timeout expired before the main thread signalled completion — kill.
            // Only set was_killed when kill actually succeeds (ESRCH means the
            // child already exited normally, which can happen in a narrow race
            // where the timeout fires at the same instant the child finishes).
            if nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(pid as i32),
                nix::sys::signal::Signal::SIGKILL,
            )
            .is_ok()
            {
                let _ = killed_tx.send(());
            }
        }
    });

    let output = child.wait_with_output()?;
    let _ = done_tx.send(());
    let _ = watchdog.join();

    Ok((output, killed_rx.try_recv().is_ok()))
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;

    #[test]
    fn format_shell_output_was_killed_shows_timeout() {
        let output = Output {
            stdout: b"some output".to_vec(),
            stderr: b"".to_vec(),
            status: std::process::ExitStatus::from_raw(0),
        };
        let result = format_shell_output("sleep 10", &output, 5000, true);
        assert!(result.contains("timed out after 5000ms"));
        assert!(result.contains("Exit code: -1"));
    }

    #[test]
    fn format_shell_output_not_killed_shows_exit_code() {
        let output = Output {
            stdout: b"hello\nworld".to_vec(),
            stderr: b"".to_vec(),
            status: std::process::ExitStatus::from_raw(0),
        };
        let result = format_shell_output("echo hello", &output, 5000, false);
        assert!(!result.contains("timed out"));
        assert!(result.contains("hello"));
        assert!(result.contains("world"));
        assert!(result.contains("Exit code: 0"));
    }

    #[test]
    fn format_shell_output_includes_stderr() {
        // On Linux, the raw status encodes the exit code in bits 8-15.
        // `from_raw(1 << 8)` means exit code 1 (normal exit).
        let output = Output {
            stdout: b"stdout".to_vec(),
            stderr: b"stderr".to_vec(),
            status: std::process::ExitStatus::from_raw(1 << 8),
        };
        let result = format_shell_output("cmd", &output, 1000, false);
        assert!(result.contains("stdout"));
        assert!(result.contains("stderr"));
        assert!(result.contains("Exit code: 1"));
    }
}
