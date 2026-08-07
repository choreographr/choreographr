use super::{ToolExecError, truncate_tool_output};
use std::{
    io::{BufRead, BufReader, Read},
    os::fd::OwnedFd,
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::mpsc,
    time::Duration,
};
use tracing::warn;

/// Check whether a binary with the given name exists somewhere in PATH.
pub(crate) fn binary_exists(name: &str) -> bool {
    std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).any(|dir| dir.join(name).is_file()))
        .unwrap_or(false)
}

/// Resolve the working directory the child process should start in.
///
/// In-process path confinement was removed in favour of OS-level sandboxing
/// (Landlock on Linux, Seatbelt on macOS), so this only resolves the directory
/// — it no longer verifies that it stays inside the session working directory.
pub(crate) fn resolve_workdir(workdir: Option<&str>, working_dir: Option<&Path>) -> PathBuf {
    super::resolve_path(workdir.unwrap_or("."), working_dir)
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

/// Apply all child-process hardening (env sanitization + process-group
/// isolation).
///
/// Every shell-tool spawn funnels through this hook: `spawn_with_watchdog`
/// and `spawn_with_streaming` call it before spawning, so placing the child
/// in its own process group here covers `sh`, `fish`, `nu`, `exec`, and the
/// streaming variants in one place.
///
/// Resource limits are deliberately not applied here — the OS-level sandbox
/// (Landlock on Linux, Seatbelt on macOS) is the confinement boundary. A
/// `setrlimit` pre-exec hook was removed as dead code; see git history if
/// in-process rlimits are ever needed again.
fn setup_child(cmd: &mut Command) {
    sanitize_env(cmd);
    // Put the child in its own process group (pgid == child pid) so a
    // timeout watchdog can kill the entire tree via killpg(2) instead of
    // only the direct child. Without this, a timeout on
    // `fish -c "sleep 10"` SIGKILLs the fish wrapper but leaves the
    // orphaned `sleep` grandchild holding the stdout/stderr pipes — the
    // tool then blocks until the grandchild exits on its own, so a
    // "timed out" result takes 10s instead of the configured 500ms (and
    // in production, an LLM tool call would hang for the same duration).
    // `process_group(0)` is a safe API (the child calls setpgid(0, 0) before
    // exec), so no `unsafe` / pre_exec hook is needed here.
    cmd.process_group(0);
}

/// Open a pidfd pinning `pid`, so a later timeout kill can prove the PID
/// still refers to the child we spawned (see `kill_child_tree`). Linux-only
/// (pidfd(2), available since kernel 5.3); returns `None` when the syscall is
/// unavailable (older kernel, seccomp policy) or the child has already
/// exited — callers then fall back to the PID-based kill path.
#[cfg(target_os = "linux")]
fn open_pidfd(pid: u32) -> Option<OwnedFd> {
    use std::os::fd::FromRawFd;
    // pidfd_open(2): flags must be 0. The fd is wrapped in an OwnedFd so it is
    // closed automatically when the watchdog thread exits (see `spawn_watchdog`).
    let fd = unsafe { nix::libc::syscall(nix::libc::SYS_pidfd_open, pid as nix::libc::pid_t, 0) };
    if fd >= 0 {
        Some(unsafe { OwnedFd::from_raw_fd(fd as i32) })
    } else {
        None
    }
}

/// Non-Linux platforms have no pidfd(2); timeout kills stay PID-based there.
#[cfg(not(target_os = "linux"))]
fn open_pidfd(_pid: u32) -> Option<OwnedFd> {
    None
}

/// Send `sig` to the process pinned by `fd` (pidfd_send_signal(2)).
///
/// Fails with `ESRCH` when the pinned process no longer exists — which is the
/// property `kill_child_tree` relies on to detect that the child finished on
/// its own. A PID recycled onto an unrelated process can never satisfy this,
/// because the fd pins the original process identity.
#[cfg(target_os = "linux")]
fn pidfd_send_signal(fd: &OwnedFd, sig: nix::libc::c_int) -> nix::Result<()> {
    use std::os::fd::AsRawFd;
    // info = NULL delivers the signal with the default siginfo_t; flags must
    // be 0.
    let ret = unsafe {
        nix::libc::syscall(
            nix::libc::SYS_pidfd_send_signal,
            fd.as_raw_fd(),
            sig,
            std::ptr::null::<nix::libc::siginfo_t>(),
            0,
        )
    };
    if ret < 0 {
        Err(nix::errno::Errno::last())
    } else {
        Ok(())
    }
}

/// Kill a spawned child, preferring its whole process group so descendants
/// (e.g. a shell's `sleep`) are reaped too.
///
/// When a pidfd pins the child's identity (Linux), `pidfd_send_signal` is
/// used first: a successful SIGKILL proves the PID has not been recycled onto
/// an unrelated process, so the subsequent `killpg(pid)` sweep targets the
/// right group. If the pidfd signal fails, the pinned process no longer
/// exists (it finished on its own) and nothing is reported as killed.
///
/// Without a pidfd (macOS, or pidfd unavailable), `killpg(pid)` is only
/// attempted when the child is confirmed to be its group's leader
/// (`getpgid(pid) == pid`). That confirmation is what makes the syscall safe:
/// when the child is *not* a leader — the caller spawned without
/// `setup_child`, or the timeout landed in the fork→setpgid window —
/// `killpg(pid)` would signal whatever process group happens to have that id
/// (normally ESRCH, but a recycled PID could collide with an orphaned group
/// id and take down an unrelated tree). Non-leaders fall straight back to a
/// direct kill of the child itself.
fn kill_child_tree(pid: u32, _pidfd: Option<&OwnedFd>) -> bool {
    // Pinned-identity path — reachable only on Linux, where `open_pidfd` can
    // return `Some`.
    #[cfg(target_os = "linux")]
    if let Some(fd) = _pidfd {
        // SIGKILL the leader through the pinned fd, then sweep the rest of
        // the group by pgid (== pid, since setup_child made the child a
        // leader). The `killpg` is best-effort: if the leader was the only
        // member the group is already gone and it returns ESRCH harmlessly.
        if pidfd_send_signal(fd, nix::libc::SIGKILL).is_err() {
            return false;
        }
        let _ = nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(pid as i32),
            nix::sys::signal::Signal::SIGKILL,
        );
        return true;
    }

    // Legacy PID-based path (non-Linux, or pidfd unavailable).
    let pid = nix::unistd::Pid::from_raw(pid as i32);
    // True only while the child lives as its group's leader. Once it has
    // exited (even unreaped) getpgid can still succeed — matching kill(2),
    // which also "succeeds" on zombies — so the was_killed flag stays accurate
    // for the narrow finish-vs-timeout race exactly as before.
    let is_group_leader = nix::unistd::getpgid(Some(pid))
        .map(|pgid| pgid == pid)
        .unwrap_or(false);
    if is_group_leader && nix::sys::signal::killpg(pid, nix::sys::signal::Signal::SIGKILL).is_ok() {
        return true;
    }
    nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGKILL).is_ok()
}

/// Spawn the watchdog thread that enforces `timeout_ms` on a child process.
///
/// The watchdog blocks on a channel receive with the given timeout. When the
/// child finishes, the main thread signals completion through `done_tx`,
/// waking the watchdog early — no polling required. If the timeout expires
/// first, the child's whole process tree is killed (see `kill_child_tree`),
/// and `killed_tx` is signalled so the caller can report `was_killed`. Shared
/// by the buffered and streaming spawn paths so their timeout semantics stay
/// identical.
///
/// `pidfd` pins the child's identity for the kill (see `open_pidfd`) and is
/// closed when the watchdog thread exits.
fn spawn_watchdog(
    timeout_ms: u64,
    pid: u32,
    pidfd: Option<OwnedFd>,
    done_rx: mpsc::Receiver<()>,
    killed_tx: mpsc::Sender<()>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        if done_rx
            .recv_timeout(Duration::from_millis(timeout_ms))
            .is_err()
            // Timeout expired before the main thread signalled completion —
            // kill the child's process tree (see `kill_child_tree`). Only set
            // was_killed when a kill actually lands (both killpg and kill
            // return ESRCH when everything already exited, which can happen in
            // a narrow race where the timeout fires at the same instant the
            // child finishes).
            && kill_child_tree(pid, pidfd.as_ref())
        {
            warn!(
                pid,
                timeout_ms, "shell tool timed out; killed child process group"
            );
            let _ = killed_tx.send(());
        }
    })
}

/// Spawn the command, run a watchdog thread to enforce the timeout,
/// and return the process output along with a flag indicating whether
/// the watchdog killed the process.
///
/// Applies child-process hardening (`setup_child`) before spawning, and pins
/// the child's identity with a pidfd on Linux so a timeout kill can never be
/// redirected at a recycled PID (see `open_pidfd` / `kill_child_tree`).
pub(crate) fn spawn_with_watchdog(
    cmd: &mut Command,
    timeout_ms: u64,
) -> Result<(Output, bool), ToolExecError> {
    setup_child(cmd);
    let child = cmd.spawn()?;
    let pid = child.id();
    let pidfd = open_pidfd(pid);

    let (done_tx, done_rx) = mpsc::channel::<()>();
    let (killed_tx, killed_rx) = mpsc::channel::<()>();

    let watchdog = spawn_watchdog(timeout_ms, pid, pidfd, done_rx, killed_tx);

    let output = child.wait_with_output()?;
    let _ = done_tx.send(());
    let _ = watchdog.join();

    Ok((output, killed_rx.try_recv().is_ok()))
}

/// Spawn the command with piped stdout/stderr and stream stdout lines
/// through `output_tx` in real time as the process produces them.
/// Enforces a timeout via watchdog and returns the collected output
/// along with a was-killed flag.
///
/// Applies child-process hardening (`setup_child`) before spawning, and pins
/// the child's identity with a pidfd on Linux so a timeout kill can never be
/// redirected at a recycled PID. The caller must still set `Stdio::piped()`
/// on both stdout and stderr before calling this.
///
/// IMPORTANT: Both stdout and stderr are drained in background threads
/// so that the child process can never block on a full pipe buffer
/// (the classic pipe deadlock).  stderr is not streamed — it is only
/// accumulated and returned in the final `Output`.
pub fn spawn_with_streaming(
    cmd: &mut Command,
    timeout_ms: u64,
    output_tx: mpsc::Sender<Vec<u8>>,
) -> Result<(Output, bool), ToolExecError> {
    setup_child(cmd);
    let mut child = cmd.spawn()?;
    let pid = child.id();
    let pidfd = open_pidfd(pid);

    // Take stdout so wait_with_output cannot grab it — we read it
    // incrementally in a background thread.
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| std::io::Error::other("stdout not piped"))?;

    // Thread: read stdout line by line, forward each line (with its
    // newline restored) to output_tx, and accumulate the full output.
    let (full_buf_tx, full_buf_rx) = mpsc::channel::<Vec<u8>>();
    let stdout_thread = std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        let mut full_buf: Vec<u8> = Vec::new();
        for line in reader.lines() {
            let line = match line {
                Ok(l) => l,
                Err(e) => {
                    warn!(error = %e, "error reading stdout from child process");
                    break;
                }
            };
            let mut line_bytes: Vec<u8> = line.into_bytes();
            line_bytes.push(b'\n');
            let _ = output_tx.send(line_bytes.clone());
            full_buf.extend_from_slice(&line_bytes);
        }
        let _ = full_buf_tx.send(full_buf);
    });

    // Thread: drain stderr concurrently so the child can never block on
    // a full stderr pipe buffer (classic pipe deadlock).  Not streamed —
    // just accumulated and returned in the final Output struct.
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| std::io::Error::other("stderr not piped"))?;
    let stderr_thread = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr.read_to_end(&mut buf);
        buf
    });

    // Watchdog thread: enforce timeout. Shared with the buffered path so the
    // two timeout semantics stay identical (see `spawn_watchdog`).
    let (done_tx, done_rx) = mpsc::channel::<()>();
    let (killed_tx, killed_rx) = mpsc::channel::<()>();
    let watchdog = spawn_watchdog(timeout_ms, pid, pidfd, done_rx, killed_tx);

    // Wait for the process to finish (both background threads drain the
    // pipes concurrently, preventing any blocking deadlock).
    let status = child.wait()?;
    let _ = done_tx.send(());

    if let Err(e) = stdout_thread.join() {
        warn!("stdout reader thread panicked: {:?}", e);
    }
    let stderr_buf = stderr_thread.join().unwrap_or_default();
    if let Err(e) = watchdog.join() {
        warn!("watchdog thread panicked: {:?}", e);
    }
    let full_buf = full_buf_rx.recv().unwrap_or_default();
    let was_killed = killed_rx.try_recv().is_ok();

    Ok((
        Output {
            stdout: full_buf,
            stderr: stderr_buf,
            status,
        },
        was_killed,
    ))
}

/// Convenience wrapper that combines `spawn_with_streaming` (which applies
/// `setup_child` hardening itself) and `format_shell_output` into a single
/// call — used by shell tool `execute_streaming` implementations to avoid
/// repeating the same 3-line pattern across `sh`, `fish`, `nu`, and `exec`.
///
/// The caller must have set `Stdio::piped()` on both stdout and stderr.
pub fn run_shell_streaming(
    cmd: &mut Command,
    display_cmd: &str,
    timeout_ms: u64,
    output_tx: mpsc::Sender<Vec<u8>>,
) -> Result<String, ToolExecError> {
    let (output, was_killed) = spawn_with_streaming(cmd, timeout_ms, output_tx)?;
    Ok(format_shell_output(
        display_cmd,
        &output,
        timeout_ms,
        was_killed,
    ))
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
    use std::io::BufRead;
    use std::os::unix::process::ExitStatusExt;
    use std::process::Stdio;

    /// Reap a spawned test child on any exit path — including a panicking
    /// assertion. `setup_child` deliberately places children in their own
    /// process group, which also puts them *outside* the test runner's cleanup
    /// scope: a leaked busy-loop `sh` would otherwise spin at 100% CPU forever
    /// (and nextest cannot reach it because it lives in a different group).
    struct ReapOnDrop(std::process::Child);

    impl Drop for ReapOnDrop {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[test]
    fn setup_child_places_child_in_its_own_process_group() {
        // `sh -c 'echo $$; …'` reports its own PID, then busy-spins forever
        // so the parent can inspect the process group before killing it.
        // Deterministic — no sleeps. Without `process_group(0)`, the child
        // inherits the parent's group (pgid == parent pid) and the assertion
        // fails; the own-group property is what lets a timeout watchdog reap
        // grandchildren via killpg(2).
        let mut cmd = std::process::Command::new("sh");
        cmd.args(["-c", "echo $$; while :; do :; done"])
            .stdout(Stdio::piped());
        setup_child(&mut cmd);

        let mut child = cmd.spawn().expect("spawn child");
        let stdout = child.stdout.take().expect("take stdout");
        // Ensure the busy-loop child is killed even if an assertion below
        // panics — it lives in its own group, so nothing else can clean it up.
        let _reap = ReapOnDrop(child);

        let mut reader = BufReader::new(stdout);
        let mut pid_line = String::new();
        reader.read_line(&mut pid_line).expect("read child pid");
        let child_pid = pid_line.trim().parse::<i32>().expect("parse child pid");

        let pgid = nix::unistd::getpgid(Some(nix::unistd::Pid::from_raw(child_pid)))
            .expect("getpgid on live child");
        assert_eq!(
            pgid.as_raw(),
            child_pid,
            "child must be leader of its own process group"
        );
    }

    #[test]
    fn kill_child_tree_falls_back_to_direct_kill_when_not_group_leader() {
        // Without setup_child the child shares our process group, so it is not
        // a group leader and kill_child_tree must skip killpg (there is no
        // group to target by this pid — and a recycled PID could otherwise
        // collide with an orphaned group id and signal an unrelated tree) and
        // reap the child via the direct-kill fallback instead. Guards the
        // fallback branch against silently becoming dead code.
        let mut cmd = std::process::Command::new("sh");
        cmd.args(["-c", "sleep 30"]);
        let child = cmd.spawn().expect("spawn child");
        let pid = child.id();
        let mut _reap = ReapOnDrop(child);

        assert!(
            kill_child_tree(pid, None),
            "direct-kill fallback must reap a non-leader child"
        );
        // The child must have been SIGKILLed, not exited cleanly.
        assert!(!_reap.0.wait().expect("wait on killed child").success());
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn kill_child_tree_kills_group_through_pinned_pidfd() {
        // With a pidfd pinning the child's identity, kill_child_tree must kill
        // the leader via pidfd_send_signal and then sweep the group. Guards
        // the Linux-only pidfd branch against silently becoming dead code.
        let mut cmd = std::process::Command::new("sh");
        cmd.args(["-c", "sleep 30"]).stdout(Stdio::piped());
        setup_child(&mut cmd);
        let child = cmd.spawn().expect("spawn child");
        let pid = child.id();
        let pidfd = open_pidfd(pid).expect("pidfd_open on live child");
        let mut _reap = ReapOnDrop(child);

        assert!(
            kill_child_tree(pid, Some(&pidfd)),
            "pinned pidfd kill must reap a leader child"
        );
        // The child must have been SIGKILLed, not exited cleanly.
        assert!(!_reap.0.wait().expect("wait on killed child").success());
    }

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
