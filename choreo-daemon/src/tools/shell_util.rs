use super::{ToolExecError, truncate_tool_output};
use std::{
    io::Read,
    os::fd::{AsRawFd, OwnedFd},
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::mpsc,
    time::Duration,
};
use tracing::{debug, trace, warn};

/// Bounded wait used by the pipe-drain helpers: `poll(2)` in 100ms slices so a
/// stop signal (sent once the direct child is reaped) is observed quickly.
/// See `poll_readable` for why the drain must be bounded.
const DRAIN_POLL_MS: i32 = 100;

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
    // rustix wraps pidfd_open(2) (flags must be empty) and returns the fd as
    // an OwnedFd, so it is closed automatically when the watchdog thread
    // exits. `Pid::from_raw` yields None only for pid 0, which child.id()
    // never produces — kept defensive rather than unwrapping.
    let pid = rustix::process::Pid::from_raw(pid as i32)?;
    match rustix::process::pidfd_open(pid, rustix::process::PidfdFlags::empty()) {
        Ok(fd) => Some(fd),
        Err(e) => {
            // Expected on kernels < 5.3 and under pidfd-blocking seccomp policies,
            // and in a narrow race where the child already exited (ESRCH). Timeout
            // kills then degrade to the leader-checked PID path — that is by
            // design, so trace (not warn) keeps the common case quiet.
            trace!(pid = pid.as_raw_pid(), error = %e, "pidfd_open unavailable; timeout kills fall back to PID-based killpg");
            None
        }
    }
}

/// Non-Linux platforms have no pidfd(2); timeout kills stay PID-based there.
#[cfg(not(target_os = "linux"))]
fn open_pidfd(_pid: u32) -> Option<OwnedFd> {
    None
}

/// Kill a spawned child, preferring its whole process group so descendants
/// (e.g. a shell's `sleep`) are reaped too.
///
/// When a pidfd pins the child's identity (Linux), `pidfd_send_signal` is
/// used first: a successful SIGKILL proves the PID has not been recycled onto
/// an unrelated process, so the subsequent `killpg(pid)` sweep targets the
/// right group. If the pinned signal fails with `ESRCH` the child finished on
/// its own and nothing is reported as killed; any *other* failure (e.g.
/// EPERM/EINVAL/ENOSYS under seccomp) falls through to the leader-checked
/// PID-based path below rather than giving up on the timeout — the pidfd still
/// pins the identity, so the fallback remains safe.
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
#[cfg_attr(not(target_os = "linux"), allow(unused_variables))]
fn kill_child_tree(pid: u32, pidfd: Option<&OwnedFd>) -> bool {
    // Pinned-identity path — reachable only on Linux, where `open_pidfd` can
    // return `Some`.
    #[cfg(target_os = "linux")]
    if let Some(fd) = pidfd {
        // rustix::process::pidfd_send_signal fails with `Errno::SRCH` (ESRCH)
        // when the pinned process has exited — the property used to detect the
        // child finished on its own. A PID recycled onto an unrelated process
        // can never satisfy this, because the fd pins the original identity.
        match rustix::process::pidfd_send_signal(fd, rustix::process::Signal::KILL) {
            Ok(()) => {
                // SIGKILL the leader through the pinned fd, then sweep the
                // rest of the group by pgid (== pid, since setup_child made
                // the child a leader). The `killpg` is best-effort: if the
                // leader was the only member the group is already gone and it
                // returns ESRCH harmlessly.
                let _ = nix::sys::signal::killpg(
                    nix::unistd::Pid::from_raw(pid as i32),
                    nix::sys::signal::Signal::SIGKILL,
                );
                return true;
            }
            Err(rustix::io::Errno::SRCH) => {
                // The pinned process exited on its own before the timeout
                // landed — nothing to kill, so report "not killed".
                debug!(pid, "pidfd_send_signal: child already exited on its own");
                return false;
            }
            Err(e) => {
                // Non-ESRCH failure: don't give up on the timeout. Fall
                // through to the leader-checked killpg below; the pidfd still
                // pins the identity, so the PID-based signal cannot be
                // redirected at a recycled process.
                warn!(pid, error = %e, "pidfd_send_signal failed; falling back to leader-checked killpg");
            }
        }
    }

    // Legacy PID-based path (non-Linux, pidfd unavailable, or the pinned
    // signal failed with a non-ESRCH error above).
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

/// Wait up to `poll_ms` for `fd` to become readable or hit EOF, using
/// `poll(2)`. Returns `true` when a read should be attempted (data or EOF
/// pending); returns `false` when the stop channel was signalled or `poll`
/// failed, in which case the caller should stop draining.
///
/// The bounded poll is what makes the pipe drain safe: after the direct child
/// is reaped, a surviving grandchild (e.g. a backgrounded `sleep 10 &` that
/// inherited the stdout pipe) would otherwise keep a blocking `read(2)` in the
/// drain thread open until the grandchild exits on its own — turning a 500ms
/// tool timeout into a multi-second hang. Draining in poll slices lets the
/// stop signal (sent by the spawn helpers once `child.wait()` returns) cut the
/// wait short.
fn poll_readable(fd: nix::libc::c_int, stop_rx: &mpsc::Receiver<()>, poll_ms: i32) -> bool {
    let mut pfd = nix::libc::pollfd {
        fd,
        events: nix::libc::POLLIN,
        revents: 0,
    };
    loop {
        // poll(2): -1 on error, 0 on timeout, >0 when data or EOF is pending.
        let ret = unsafe { nix::libc::poll(&mut pfd, 1, poll_ms) };
        if ret < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue; // EINTR: retry the poll
            }
            debug!(error = %err, "poll on child pipe failed; stopping drain");
            return false;
        }
        if ret == 0 {
            // Nothing readable within the slice. If the direct child has been
            // reaped (stop signalled), stop rather than wait for a survivor.
            if stop_rx.try_recv().is_ok() {
                return false;
            }
            continue;
        }
        // Data available or EOF (POLLHUP|POLLIN) — attempt a read.
        return true;
    }
}

/// Read `reader` (whose raw fd must be `fd`) to EOF, or until `stop_rx` is
/// signalled, returning everything read. `on_data` is invoked with each chunk
/// (used for line-streaming). The returned buffer always contains the full
/// byte stream regardless of what `on_data` does with it.
///
/// The fd is put in non-blocking mode first so the drain loop can consume
/// every byte available per `poll` verdict (full throughput for large
/// outputs) without ever blocking on an empty pipe whose write end is still
/// open (a surviving grandchild). If `fcntl` fails — which never happens on a
/// freshly-created pipe — the code falls back to reading a single chunk per
/// poll verdict, which is safe but slower.
fn drain_fd<R: Read + AsRawFd>(
    mut reader: R,
    stop_rx: mpsc::Receiver<()>,
    poll_ms: i32,
    on_data: &mut dyn FnMut(&[u8]),
) -> Vec<u8> {
    let fd = reader.as_raw_fd();
    let nonblocking = set_nonblocking(fd);
    if !nonblocking {
        debug!(
            fd,
            "could not set child pipe non-blocking; draining one chunk per poll"
        );
    }
    let mut full: Vec<u8> = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        if !poll_readable(fd, &stop_rx, poll_ms) {
            break;
        }
        // Drain everything currently buffered (non-blocking reads loop until
        // EAGAIN). In the blocking-fd fallback, read at most once per poll
        // verdict so the read is always bounded by the poll result.
        loop {
            match reader.read(&mut buf) {
                Ok(0) => return full, // EOF: all pipe write ends are closed
                Ok(n) => {
                    on_data(&buf[..n]);
                    full.extend_from_slice(&buf[..n]);
                    if !nonblocking {
                        break; // blocking fallback: one read per poll
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break, // drained
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {
                    if !nonblocking {
                        break; // avoid a blocking re-read on the fallback path
                    }
                    continue;
                }
                Err(e) => {
                    debug!(error = %e, "read from child pipe failed; stopping drain");
                    return full;
                }
            }
        }
    }
    full
}

/// Put `fd` in non-blocking mode. Returns `false` if `fcntl` fails.
fn set_nonblocking(fd: nix::libc::c_int) -> bool {
    // SAFETY: fcntl is async-signal-safe and operates on the fd we own.
    let flags = unsafe { nix::libc::fcntl(fd, nix::libc::F_GETFL) };
    if flags < 0 {
        return false;
    }
    // SAFETY: F_SETFL with the OR'd O_NONBLOCK flag is the standard way to
    // make a pipe read end non-blocking; only the read end's own behaviour
    // changes (the child's writes to the other end are unaffected).
    unsafe { nix::libc::fcntl(fd, nix::libc::F_SETFL, flags | nix::libc::O_NONBLOCK) >= 0 }
}

/// Append `chunk` to `pending`, forwarding every complete line (terminated by
/// `\n`, with a trailing `\r` stripped so CRLF is folded into LF) to
/// `on_line`. Leftover bytes stay in `pending` for the next chunk, or a final
/// flush by the caller at EOF — matching `BufRead::lines()` semantics.
fn forward_complete_lines(chunk: &[u8], pending: &mut Vec<u8>, on_line: &mut dyn FnMut(Vec<u8>)) {
    for &b in chunk {
        // Fold CRLF into LF: drop a trailing `\r` when the next byte is `\n`
        // (matching `BufRead::lines()`, which strips both `\r\n` and `\n`).
        if b == b'\n' && pending.last() == Some(&b'\r') {
            pending.pop();
        }
        pending.push(b);
        if b == b'\n' {
            let line = std::mem::take(pending);
            on_line(line);
        }
    }
}

/// Spawn the command, run a watchdog thread to enforce the timeout,
/// and return the process output along with a flag indicating whether
/// the watchdog killed the process.
///
/// Applies child-process hardening (`setup_child`) before spawning, pins the
/// child's identity with a pidfd on Linux so a timeout kill can never be
/// redirected at a recycled PID (see `open_pidfd` / `kill_child_tree`), and
/// drains stdout/stderr in bounded background threads so a surviving
/// grandchild cannot hang the tool past the timeout (see `drain_fd`).
pub(crate) fn spawn_with_watchdog(
    cmd: &mut Command,
    timeout_ms: u64,
) -> Result<(Output, bool), ToolExecError> {
    setup_child(cmd);
    let mut child = cmd.spawn()?;
    let pid = child.id();
    let pidfd = open_pidfd(pid);

    let (done_tx, done_rx) = mpsc::channel::<()>();
    let (killed_tx, killed_rx) = mpsc::channel::<()>();
    let watchdog = spawn_watchdog(timeout_ms, pid, pidfd, done_rx, killed_tx);

    // Drain stdout/stderr concurrently so the child can never block on a full
    // pipe buffer (the classic pipe deadlock). Each drain is bounded by a stop
    // channel; a pipe that was not piped (None) is skipped, leaving the
    // Output field empty — matching the old wait_with_output behaviour.
    let (out_stop_tx, out_stop_rx) = mpsc::channel::<()>();
    let (err_stop_tx, err_stop_rx) = mpsc::channel::<()>();
    let stdout_thread = child
        .stdout
        .take()
        .map(|s| std::thread::spawn(move || drain_fd(s, out_stop_rx, DRAIN_POLL_MS, &mut |_| {})));
    let stderr_thread = child
        .stderr
        .take()
        .map(|s| std::thread::spawn(move || drain_fd(s, err_stop_rx, DRAIN_POLL_MS, &mut |_| {})));

    let status = child.wait()?;
    let _ = done_tx.send(());
    // The direct child is reaped. Stop the drainers: anything still holding a
    // pipe write end is a surviving grandchild or backgrounded process, and
    // waiting for its output could exceed the timeout we were enforcing.
    let _ = out_stop_tx.send(());
    let _ = err_stop_tx.send(());
    if let Err(e) = watchdog.join() {
        warn!("watchdog thread panicked: {:?}", e);
    }

    let stdout = stdout_thread
        .and_then(|t| t.join().ok())
        .unwrap_or_default();
    let stderr = stderr_thread
        .and_then(|t| t.join().ok())
        .unwrap_or_default();
    let was_killed = killed_rx.try_recv().is_ok();

    Ok((
        Output {
            stdout,
            stderr,
            status,
        },
        was_killed,
    ))
}

/// Spawn the command with piped stdout/stderr and stream stdout lines
/// through `output_tx` in real time as the process produces them.
/// Enforces a timeout via watchdog and returns the collected output
/// along with a was-killed flag.
///
/// Applies child-process hardening (`setup_child`) before spawning, pins the
/// child's identity with a pidfd on Linux so a timeout kill can never be
/// redirected at a recycled PID, and bounds both drains so a surviving
/// grandchild cannot hang the tool past the timeout. The caller must set
/// `Stdio::piped()` on both stdout and stderr before calling this.
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

    // Take stdout so wait() cannot grab it — we read it incrementally in a
    // background thread (see `drain_fd` for the bounded-drain rationale).
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| std::io::Error::other("stdout not piped"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| std::io::Error::other("stderr not piped"))?;

    let (out_stop_tx, out_stop_rx) = mpsc::channel::<()>();
    let (err_stop_tx, err_stop_rx) = mpsc::channel::<()>();

    // Thread: read stdout, forward each complete line (newline restored,
    // CRLF folded to LF) to output_tx, and accumulate the full output.
    let stdout_thread = std::thread::spawn(move || {
        let mut pending: Vec<u8> = Vec::new();
        let full = drain_fd(stdout, out_stop_rx, DRAIN_POLL_MS, &mut |chunk: &[u8]| {
            forward_complete_lines(chunk, &mut pending, &mut |line| {
                let _ = output_tx.send(line);
            });
        });
        // Flush any final unterminated line (matches `BufRead::lines()`
        // yielding a last line without a trailing newline). The bytes are
        // already included in `full` (drain_fd accumulates everything); this
        // only forwards the streamed copy.
        if !pending.is_empty() {
            let _ = output_tx.send(pending);
        }
        full
    });

    // Thread: drain stderr concurrently so the child can never block on a
    // full stderr pipe buffer.  Not streamed — just accumulated and returned
    // in the final Output struct.
    let stderr_thread =
        std::thread::spawn(move || drain_fd(stderr, err_stop_rx, DRAIN_POLL_MS, &mut |_| {}));

    // Watchdog thread: enforce timeout. Shared with the buffered path so the
    // two timeout semantics stay identical (see `spawn_watchdog`).
    let (done_tx, done_rx) = mpsc::channel::<()>();
    let (killed_tx, killed_rx) = mpsc::channel::<()>();
    let watchdog = spawn_watchdog(timeout_ms, pid, pidfd, done_rx, killed_tx);

    // Wait for the process to finish (both background threads drain the
    // pipes concurrently, preventing any blocking deadlock).
    let status = child.wait()?;
    let _ = done_tx.send(());
    // Stop the drainers once the direct child is reaped (see
    // `spawn_with_watchdog`).
    let _ = out_stop_tx.send(());
    let _ = err_stop_tx.send(());

    let stdout_buf = match stdout_thread.join() {
        Ok(buf) => buf,
        Err(e) => {
            warn!("stdout reader thread panicked: {:?}", e);
            Vec::new()
        }
    };
    let stderr_buf = stderr_thread.join().unwrap_or_default();
    if let Err(e) = watchdog.join() {
        warn!("watchdog thread panicked: {:?}", e);
    }
    let was_killed = killed_rx.try_recv().is_ok();

    Ok((
        Output {
            stdout: stdout_buf,
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
    use std::io::{BufRead, BufReader, Write};
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
        // reap the child via the direct-kill fallback instead. `exec` ensures
        // the shell replaces itself with `sleep`, so no orphan grandchild can
        // outlive the direct kill. Guards the fallback branch against silently
        // becoming dead code.
        let mut cmd = std::process::Command::new("sh");
        cmd.args(["-c", "exec sleep 30"]);
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
        // Wrap the child before the pidfd-unavailable early return below so it
        // is always reaped (a `sleep 30` would otherwise linger until the test
        // process exits).
        let mut _reap = ReapOnDrop(child);
        // pidfd_open can be unavailable (kernel < 5.3, seccomp policy) even
        // though the child is alive; production degrades to the PID-based
        // path in exactly that case, so the test skips rather than failing.
        let Some(pidfd) = open_pidfd(pid) else {
            eprintln!("pidfd_open unavailable; skipping pinned-pidfd test");
            return;
        };

        assert!(
            kill_child_tree(pid, Some(&pidfd)),
            "pinned pidfd kill must reap a leader child"
        );
        // The child must have been SIGKILLed, not exited cleanly.
        assert!(!_reap.0.wait().expect("wait on killed child").success());
    }

    #[test]
    fn drain_fd_reads_to_eof_without_stop_signal() {
        // A pipe whose write end is closed immediately: drain_fd must return
        // everything without needing a stop signal. Deterministic — the data
        // is buffered and EOF is signalled by drop, so no real time passes
        // (poll_ms = 0).
        let (reader, mut writer) = std::io::pipe().expect("pipe");
        writer.write_all(b"line1\nline2\n").expect("write");
        drop(writer); // EOF

        let (_stop_tx, stop_rx) = mpsc::channel::<()>();
        let got = drain_fd(reader, stop_rx, 0, &mut |_| {});
        assert_eq!(got, b"line1\nline2\n");
    }

    #[test]
    fn drain_fd_captures_output_larger_than_one_chunk() {
        // More than one 8 KiB drain chunk: the non-blocking inner loop must
        // consume every buffered byte before EOF without losing any. All the
        // data is written and the writer closed before the drain starts, so
        // the test is fully deterministic.
        let (reader, mut writer) = std::io::pipe().expect("pipe");
        let payload = vec![b'x'; 20 * 1024];
        writer.write_all(&payload).expect("write");
        drop(writer); // EOF

        let (_stop_tx, stop_rx) = mpsc::channel::<()>();
        let got = drain_fd(reader, stop_rx, 0, &mut |_| {});
        assert_eq!(got, payload);
    }

    #[test]
    fn drain_fd_stops_when_signalled_even_with_open_writer() {
        // A pipe whose write end stays open simulates a surviving grandchild
        // holding the shell's stdout pipe after the direct child is reaped.
        // Signalling the stop channel must end the drain promptly instead of
        // waiting for the writer to close, and already-buffered data is still
        // returned. poll_ms = 0 keeps the test free of real-time waits.
        let (reader, mut writer) = std::io::pipe().expect("pipe");
        writer.write_all(b"hello").expect("write");
        let (stop_tx, stop_rx) = mpsc::channel::<()>();
        stop_tx.send(()).expect("signal stop");

        let got = drain_fd(reader, stop_rx, 0, &mut |_| {});
        assert_eq!(
            got, b"hello",
            "buffered data must be drained before stopping"
        );
        drop(writer);
    }

    #[test]
    fn forward_complete_lines_splits_lines_and_folds_crlf() {
        let mut pending: Vec<u8> = Vec::new();
        let mut lines: Vec<Vec<u8>> = Vec::new();
        forward_complete_lines(b"a\r\nb", &mut pending, &mut |l| lines.push(l));
        // "a\r\n" is a complete line (CRLF folded to LF); "b" stays pending.
        assert_eq!(lines, vec![b"a\n".to_vec()]);
        assert_eq!(pending, b"b");

        forward_complete_lines(b"\nc", &mut pending, &mut |l| lines.push(l));
        assert_eq!(lines, vec![b"a\n".to_vec(), b"b\n".to_vec()]);
        assert_eq!(pending, b"c");

        // Final flush at EOF emits the unterminated remainder.
        if !pending.is_empty() {
            lines.push(std::mem::take(&mut pending));
        }
        assert_eq!(lines, vec![b"a\n".to_vec(), b"b\n".to_vec(), b"c".to_vec()]);
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
