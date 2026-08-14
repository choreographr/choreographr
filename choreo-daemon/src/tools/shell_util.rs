use super::{
    MAX_TOOL_OUTPUT_BYTES, STREAMING_CHANNEL_CAPACITY, ToolExecError, finish_tool_output_sanitized,
    sanitize_transcript,
};
use choreo_sanitize::{ByteBudget, TRUNCATION_SUFFIX};
use crossbeam_channel;
use std::{
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::mpsc,
    time::Duration,
};
// Unix-only process-control primitives: `std::os::fd` and
// `std::os::unix::process` do not exist on Windows (the Windows analogues are
// `std::os::windows::io::AsRawHandle` plus the Job Object FFI, see
// `ChildJob`).
#[cfg(unix)]
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
#[cfg(unix)]
use std::os::unix::process::CommandExt;
#[cfg(windows)]
use std::os::windows::io::AsRawHandle;
#[cfg(windows)]
use std::sync::Arc;
use tracing::{debug, warn};
// `trace` is used only by the Linux-only `open_pidfd`; gating the import
// keeps the non-Linux build warning-free.
#[cfg(target_os = "linux")]
use tracing::trace;

/// Bounded wait used by the Unix pipe-drain helpers: `poll(2)` in 100ms
/// slices so a stop signal (sent once the direct child is reaped) is observed
/// quickly. See `poll_readable` for why the drain must be bounded.
#[cfg(unix)]
const DRAIN_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// How long a drain is given to deliver its completion message after the
/// direct child is reaped before the caller gives up on it. A healthy drain
/// delivers in microseconds-to-milliseconds (EOF follows the child's exit);
/// the bound exists only for the wedged case — a surviving grandchild that
/// inherited a pipe write-end. Deliberately generous (1s, not the 100ms poll
/// slice) so a drain that is merely slow to be scheduled — or still consuming
/// a large buffered output — is never mistaken for wedged.
///
/// What happens past the bound is platform-specific (see `collect_capped_drain`
/// / `collect_line_drains`): on Unix the handle is dropped to detach the
/// thread; on Windows the Job Object is terminated to force EOF (killing the
/// survivor — the only way to close the write-end a blocking read waits on),
/// then [`DRAIN_DETACH_GRACE`] is given for delivery.
const DRAIN_COMPLETION_GRACE: Duration = Duration::from_secs(1);

/// Bound for joining a thread after it has been given every chance to finish:
/// a Windows drain AFTER the Job Object was terminated to force EOF
/// (terminating the job closes every descendant's pipe write-end, so a
/// blocked read unblocks in microseconds; the grace covers the
/// essentially-unreachable case where termination fails), or the stream
/// merger after both drains have disconnected its merge channel (a stalled
/// subscriber can keep it blocked in a send). Past the bound the handle is
/// dropped to detach the thread rather than hang the tool.
const DRAIN_DETACH_GRACE: Duration = Duration::from_secs(5);

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
    //
    // Windows has no process groups to put the child in; tree isolation
    // there is a Job Object (`ChildJob`), created after spawn because
    // `std::process::Command` has no pre-exec hook on Windows.
    #[cfg(unix)]
    cmd.process_group(0);
}

/// Open a pidfd pinning `pid`, so a later timeout kill can prove the PID
/// still refers to the child we spawned (see `kill_child_tree`). Linux-only
/// (pidfd(2), available since kernel 5.3); returns `None` when the syscall is
/// unavailable (older kernel, seccomp policy) or the child has already
/// exited — callers then fall back to the PID-based kill path.
#[cfg(unix)]
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

/// Non-Linux Unix platforms have no pidfd(2); timeout kills stay PID-based
/// there. Windows is excluded entirely — it has no pidfd and no PID-based
/// killpg either (tree kills go through the Job Object, see `ChildJob`).
#[cfg(unix)]
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
#[cfg(unix)]
#[cfg_attr(not(target_os = "linux"), allow(unused_variables))]
fn kill_child_tree(pid: u32, pidfd: Option<&OwnedFd>) -> bool {
    // `child.id()` is never 0, but rustix's `Pid::from_raw` returns an
    // `Option` and production code must not unwrap — keep the conversion
    // defensive rather than assuming.
    let Some(pid) = rustix::process::Pid::from_raw(pid as i32) else {
        debug!(raw_pid = pid, "refusing to signal pid 0");
        return false;
    };

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
                // the child a leader). The `kill_process_group` is
                // best-effort: if the leader was the only member the group is
                // already gone and it returns ESRCH harmlessly.
                let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
                return true;
            }
            Err(rustix::io::Errno::SRCH) => {
                // The pinned process exited on its own before the timeout
                // landed — nothing to kill, so report "not killed".
                debug!(
                    pid = pid.as_raw_pid(),
                    "pidfd_send_signal: child already exited on its own"
                );
                return false;
            }
            Err(e) => {
                // Non-ESRCH failure: don't give up on the timeout. Fall
                // through to the leader-checked killpg below; the pidfd still
                // pins the identity, so the PID-based signal cannot be
                // redirected at a recycled process.
                warn!(
                    pid = pid.as_raw_pid(),
                    error = %e,
                    "pidfd_send_signal failed; falling back to leader-checked killpg"
                );
            }
        }
    }

    // Legacy PID-based path (non-Linux, pidfd unavailable, or the pinned
    // signal failed with a non-ESRCH error above).
    //
    // True only while the child lives as its group's leader. Once it has
    // exited (even unreaped) getpgid can still succeed — matching kill(2),
    // which also "succeeds" on zombies — so the was_killed flag stays accurate
    // for the narrow finish-vs-timeout race exactly as before.
    let is_group_leader = rustix::process::getpgid(Some(pid))
        .map(|pgid| pgid == pid)
        .unwrap_or(false);
    if is_group_leader
        && rustix::process::kill_process_group(pid, rustix::process::Signal::KILL).is_ok()
    {
        return true;
    }
    rustix::process::kill_process(pid, rustix::process::Signal::KILL).is_ok()
}

/// Minimal FFI for the one process-wait primitive windows-sys does not expose:
/// `WaitForSingleObject` with a zero timeout is the standard non-blocking
/// "has this process exited?" probe (windows-sys 0.61 carries a thin kernel32
/// surface and omits it; kernel32 is already linked by std).
#[cfg(windows)]
mod winffi {
    #[link(name = "Kernel32")]
    unsafe extern "system" {
        pub(super) fn wait_for_single_object(
            handle: windows_sys::Win32::Foundation::HANDLE,
            timeout_ms: u32,
        ) -> u32;
    }
}

/// Windows process-tree isolation: the child is assigned to a Job Object so a
/// timeout (or the job handle dropping) can terminate the whole tree — the
/// equivalent of Unix process-group killpg(2). JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
/// ensures descendants cannot outlive the daemon's watch over them.
///
/// A HANDLE is an index into the kernel handle table, not a pointer into our
/// address space: the Job Object it names lives in kernel memory and every
/// operation on it (Assign/Terminate/Close) is thread-safe kernel-side. Sharing
/// the handle across the watchdog and drain threads (via `Arc<ChildJob>`) is
/// therefore sound; the `unsafe impl`s only let the value cross thread
/// boundaries — the handle itself is closed exactly once, by `Drop` on the
/// last owner. (Fifth sanctioned shared-state exception — documented in
/// AGENTS.md and ARCHITECTURE.md's `tools/shell_util.rs` row.)
#[cfg(windows)]
struct ChildJob(windows_sys::Win32::Foundation::HANDLE);

#[cfg(windows)]
unsafe impl Send for ChildJob {}
#[cfg(windows)]
unsafe impl Sync for ChildJob {}

#[cfg(windows)]
impl ChildJob {
    /// Create a Job Object with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` and assign
    /// `child` to it. The limit makes the job a process-tree kill switch: when
    /// the last job handle closes (or `terminate` is called), every process in
    /// the job is terminated — so a grandchild that inherited the pipes cannot
    /// outlive the daemon's watch over them.
    fn assign(child: &std::process::Child) -> std::io::Result<Self> {
        // SAFETY: four kernel32 FFI calls with well-formed arguments.
        // `CreateJobObjectW` with null attributes/name creates an anonymous
        // job (returns NULL on failure). `info` is a zeroed
        // JOBOBJECT_EXTENDED_LIMIT_INFORMATION (Default::default()) whose only
        // changed field enables KILL_ON_JOB_CLOSE; the length passed to
        // SetInformationJobObject matches the struct. `child.as_raw_handle()`
        // is the live process handle std keeps for the child, valid until
        // `child` is dropped. Every failure path closes the job handle before
        // returning, so no handle leaks.
        unsafe {
            let job = windows_sys::Win32::System::JobObjects::CreateJobObjectW(
                std::ptr::null(),
                std::ptr::null(),
            );
            if job.is_null() {
                return Err(std::io::Error::last_os_error());
            }
            let mut info: windows_sys::Win32::System::JobObjects::JOBOBJECT_EXTENDED_LIMIT_INFORMATION =
                Default::default();
            info.BasicLimitInformation.LimitFlags =
                windows_sys::Win32::System::JobObjects::JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if windows_sys::Win32::System::JobObjects::SetInformationJobObject(
                job,
                windows_sys::Win32::System::JobObjects::JobObjectExtendedLimitInformation,
                &info as *const _ as *const core::ffi::c_void,
                std::mem::size_of::<
                    windows_sys::Win32::System::JobObjects::JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
                >() as u32,
            ) == 0
            {
                let err = std::io::Error::last_os_error();
                windows_sys::Win32::Foundation::CloseHandle(job);
                return Err(err);
            }
            if windows_sys::Win32::System::JobObjects::AssignProcessToJobObject(
                job,
                child.as_raw_handle(),
            ) == 0
            {
                let err = std::io::Error::last_os_error();
                windows_sys::Win32::Foundation::CloseHandle(job);
                return Err(err);
            }
            Ok(ChildJob(job))
        }
    }

    /// Terminate every process in the job. Returns `true` on success (the
    /// API returns nonzero even when the job already has no processes).
    ///
    /// This is the unconditional force-EOF/teardown variant — it is used on
    /// paths where the direct child is already known to be reaped (wedged
    /// drains, `wait`-failure teardown). The watchdog's timeout kill uses the
    /// aliveness-gated [`ProcessIsAlive`] wrapper instead so a child that
    /// finished on its own is not misreported as killed.
    fn terminate(&self) -> bool {
        // SAFETY: `self.0` is a valid job handle created by `assign`; it is
        // still open (Drop has not run) because `&self` borrows it.
        unsafe { windows_sys::Win32::System::JobObjects::TerminateJobObject(self.0, 1) != 0 }
    }
}

#[cfg(windows)]
impl Drop for ChildJob {
    fn drop(&mut self) {
        // SAFETY: `self.0` is a valid open job handle and this is the unique
        // CloseHandle for it (ChildJob has no Clone, so the handle cannot be
        // double-closed). KILL_ON_JOB_CLOSE then reaps any process still in
        // the job — the whole tree — when the last handle closes.
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

/// A copy of the direct child's process handle used to probe whether it is
/// still running. `WaitForSingleObject` with a zero timeout is the standard
/// non-blocking aliveness check; the handle is a copy of the std `Child`'s
/// handle and stays valid until the `Child` is dropped — the watchdog is
/// always joined before that.
#[cfg(windows)]
struct ProcessIsAlive(windows_sys::Win32::Foundation::HANDLE);

#[cfg(windows)]
unsafe impl Send for ProcessIsAlive {}

#[cfg(windows)]
impl ProcessIsAlive {
    fn from_child(child: &std::process::Child) -> Self {
        Self(child.as_raw_handle())
    }

    /// True while the process has not exited. `WaitForSingleObject(0)` returns
    /// WAIT_TIMEOUT (0x102) while the process runs and WAIT_OBJECT_0 once it
    /// has exited; the zero timeout never blocks.
    fn is_running(&self) -> bool {
        // SAFETY: `self.0` is a valid open process handle (a copy of the std
        // Child's handle) and the call is non-blocking (zero timeout).
        unsafe {
            winffi::wait_for_single_object(self.0, 0)
                == windows_sys::Win32::Foundation::WAIT_TIMEOUT
        }
    }
}

/// Collect a capped drain's buffer (Windows): bounded, channel-based wait.
/// A drain still silent at [`DRAIN_COMPLETION_GRACE`] is wedged on a pipe a
/// surviving grandchild holds open — the Job Object is terminated to force
/// EOF (killing the survivor: on Windows that is the only way to close the
/// write-end a blocking read is waiting on; the direct child is always
/// already reaped at this point, so only descendants are affected), then the
/// drain is given [`DRAIN_DETACH_GRACE`] to deliver. If it still does not
/// (termination failed), the handle is dropped to detach the thread rather
/// than hang the tool. `None` (never piped) returns `None`.
#[cfg(windows)]
fn collect_capped_drain(
    thread: Option<(std::thread::JoinHandle<()>, mpsc::Receiver<Vec<u8>>)>,
    job: &ChildJob,
) -> Option<Vec<u8>> {
    let (handle, rx) = thread?;
    let buf = match rx.recv_timeout(DRAIN_COMPLETION_GRACE) {
        Ok(buf) => buf,
        Err(mpsc::RecvTimeoutError::Timeout) => {
            warn!("shell tool drain wedged past the child's exit; terminating job to force EOF");
            job.terminate();
            match rx.recv_timeout(DRAIN_DETACH_GRACE) {
                Ok(buf) => buf,
                Err(_) => {
                    warn!("shell tool drain still blocked after job termination; detaching it");
                    drop(handle); // detach: the thread stays blocked in its read
                    return None;
                }
            }
        }
        // Disconnected: the drain panicked before delivering.
        Err(_) => Vec::new(),
    };
    // The message arrived, so the drain has finished: reap the thread.
    let _ = handle.join();
    Some(buf)
}

/// Collect a capped drain's buffer (Unix): the stop signal ends the drain at
/// its next poll slice, so the completion message arrives promptly; the
/// bounded `recv_timeout` waits for it and the handle is reaped. A drain
/// still silent at `grace` is wedged on a pipe a surviving grandchild keeps
/// producing — the stop signal is only checked when `poll` returns with no
/// data, so a producer never lets the drain see it. There is no way to force
/// EOF on Unix short of killing the survivor (which we have no handle to),
/// so the handle is dropped to detach the thread — it exits on its own once
/// the survivor does — rather than hang the tool.
#[cfg(unix)]
fn collect_capped_drain(
    thread: Option<(std::thread::JoinHandle<()>, mpsc::Receiver<Vec<u8>>)>,
    grace: Duration,
) -> Option<Vec<u8>> {
    let (handle, rx) = thread?;
    let buf = match rx.recv_timeout(grace) {
        Ok(buf) => buf,
        Err(mpsc::RecvTimeoutError::Timeout) => {
            warn!("shell tool drain did not deliver within {grace:?}; detaching it");
            drop(handle); // detach: the thread keeps draining until the survivor exits
            return None;
        }
        // Disconnected: the drain panicked before delivering.
        Err(_) => Vec::new(),
    };
    let _ = handle.join();
    Some(buf)
}

/// Wait for both streaming line drains (Windows): bounded, channel-based
/// wait; a drain still silent at the deadline is wedged on a survivor's pipe
/// — the Job Object is terminated to force EOF, then a grace period is given
/// before the handle is dropped to detach it (see [`collect_capped_drain`]).
#[cfg(windows)]
fn collect_line_drains(
    stdout: (std::thread::JoinHandle<()>, mpsc::Receiver<()>),
    stderr: (std::thread::JoinHandle<()>, mpsc::Receiver<()>),
    job: &ChildJob,
) {
    for (handle, rx, name) in [
        (stdout.0, stdout.1, "stdout"),
        (stderr.0, stderr.1, "stderr"),
    ] {
        match rx.recv_timeout(DRAIN_COMPLETION_GRACE) {
            Ok(()) => {
                let _ = handle.join();
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                warn!(
                    "shell tool {name} line drain wedged past the child's exit; terminating job to force EOF"
                );
                job.terminate();
                match rx.recv_timeout(DRAIN_DETACH_GRACE) {
                    Ok(()) => {
                        let _ = handle.join();
                    }
                    Err(_) => {
                        warn!(
                            "shell tool {name} line drain still blocked after job termination; detaching it"
                        );
                        drop(handle);
                    }
                }
            }
            // Disconnected: the drain panicked before delivering.
            Err(_) => {
                let _ = handle.join();
            }
        }
    }
}

/// Wait for both streaming line drains (Unix): the stop signal ends each
/// drain at its next poll slice, so the completion messages arrive promptly;
/// the bounded `recv_timeout` waits for them and the handles are reaped. A
/// drain still silent at `grace` is wedged (see [`collect_capped_drain`]):
/// its handle is dropped to detach it rather than hang the tool.
#[cfg(unix)]
fn collect_line_drains(
    stdout: (std::thread::JoinHandle<()>, mpsc::Receiver<()>),
    stderr: (std::thread::JoinHandle<()>, mpsc::Receiver<()>),
    grace: Duration,
) {
    for (handle, rx, name) in [
        (stdout.0, stdout.1, "stdout"),
        (stderr.0, stderr.1, "stderr"),
    ] {
        match rx.recv_timeout(grace) {
            Ok(()) => {
                let _ = handle.join();
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                warn!(
                    "shell tool {name} line drain did not deliver within {grace:?}; detaching it"
                );
                drop(handle);
            }
            // Disconnected: the drain panicked before delivering.
            Err(_) => {
                let _ = handle.join();
            }
        }
    }
}

/// Collect the stream merger's accumulated body: a bounded, channel-based
/// wait. The merger delivers its body right before exiting, once both drains
/// have disconnected the merge channel; `recv_timeout` is the wait (no
/// polling). The grace covers a merger wedged on a stalled subscriber (the
/// pre-existing plain join could hang the daemon thread forever in that
/// case) — past it the handle is dropped to detach rather than hang.
fn collect_merger_body(
    handle: std::thread::JoinHandle<()>,
    rx: mpsc::Receiver<Vec<u8>>,
) -> Vec<u8> {
    match rx.recv_timeout(DRAIN_DETACH_GRACE) {
        Ok(body) => {
            let _ = handle.join();
            body
        }
        Err(mpsc::RecvTimeoutError::Timeout) => {
            warn!("stream merger did not finish within {DRAIN_DETACH_GRACE:?}; detaching it");
            drop(handle);
            Vec::new()
        }
        // Disconnected: the merger panicked before delivering.
        Err(_) => {
            let _ = handle.join();
            Vec::new()
        }
    }
}

/// Spawn the watchdog thread that enforces `timeout_ms` on a child process.
///
/// The watchdog blocks on a channel receive with the given timeout. When the
/// child finishes, the main thread signals completion through `done_tx`,
/// waking the watchdog early — no polling required. If the timeout expires
/// first, the child's whole process tree is killed via `kill_tree` (killpg on
/// Unix, Job Object termination on Windows), and `killed_tx` is signalled so
/// the caller can report `was_killed`. Shared by the buffered and streaming
/// spawn paths so their timeout semantics stay identical.
///
/// `pid` is used only for the timeout log line; the kill itself is delegated
/// to the `kill_tree` closure (killpg + pidfd on Unix, Job Object termination
/// on Windows — see `kill_child_tree` / `ChildJob`), which reports whether a
/// kill actually landed. On Unix the closure owns the pidfd, so it is closed
/// when the watchdog thread exits.
fn spawn_watchdog(
    timeout_ms: u64,
    pid: u32,
    kill_tree: impl FnOnce() -> bool + Send + 'static,
    done_rx: mpsc::Receiver<()>,
    killed_tx: mpsc::Sender<()>,
    abort_tx: Option<crossbeam_channel::Sender<()>>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        if done_rx
            .recv_timeout(Duration::from_millis(timeout_ms))
            .is_err()
            // Timeout expired before the main thread signalled completion —
            // kill the child's process tree via the `kill_tree` closure
            // (killpg on Unix, Job Object termination on Windows). Only set
            // was_killed when a kill actually lands (the Unix killpg/kill and
            // the Windows aliveness probe both report false when everything
            // already exited, which can happen in a narrow race where the
            // timeout fires at the same instant the child finishes).
            && kill_tree()
        {
            warn!(
                pid,
                timeout_ms, "shell tool timed out; killed child process tree"
            );
            let _ = killed_tx.send(());
            if let Some(abort_tx) = abort_tx {
                // The streaming merger may be blocked on a full output
                // channel (a stalled subscriber); signal it to abandon so the
                // drain threads unblock and the timeout stays authoritative —
                // a slow consumer can never wedge the tool past its timeout.
                let _ = abort_tx.send(());
            }
        }
    })
}

/// Wait up to `poll` for `fd` to become readable or hit EOF, using
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
#[cfg(unix)]
fn poll_readable(
    fd: rustix::fd::BorrowedFd<'_>,
    stop_rx: &mpsc::Receiver<()>,
    poll: Duration,
) -> bool {
    // Poll for readability/EOF on the pipe fd. The PollFd borrows the fd for
    // the duration of each call, so no raw-fd/unsafe handling is needed.
    let mut pfds = [rustix::event::PollFd::new(
        &fd,
        rustix::event::PollFlags::IN,
    )];
    // poll(2) takes a timespec; the drain slices come in as a Duration.
    // (`Timespec` is re-exported from rustix::event; rustix::timespec is private.)
    let timeout = rustix::event::Timespec {
        tv_sec: poll.as_secs() as i64,
        tv_nsec: poll.subsec_nanos() as i64,
    };
    loop {
        // rustix does not auto-retry EINTR, so the INTR branch loops back to
        // match the previous libc behaviour.
        match rustix::event::poll(&mut pfds, Some(&timeout)) {
            // Ok(0): nothing readable within the slice. If the direct child
            // has been reaped (stop signalled), stop rather than wait for a
            // survivor.
            Ok(0) => {
                if stop_rx.try_recv().is_ok() {
                    return false;
                }
                continue;
            }
            // Data available or EOF (POLLHUP|POLLIN) — attempt a read.
            Ok(_) => return true,
            // EINTR: retry the poll.
            Err(rustix::io::Errno::INTR) => continue,
            Err(e) => {
                debug!(error = %e, "poll on child pipe failed; stopping drain");
                return false;
            }
        }
    }
}

/// How much of the bytes read by [`drain_fd`] is accumulated into its
/// returned buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DrainAccumulate {
    /// Discard everything — the caller consumes the bytes via `on_data`
    /// (the streaming paths) and needs no second copy. Skips the cap
    /// accounting entirely.
    None,
    /// Keep the first `cap` bytes — the *first* bytes win, matching the
    /// byte-cap truncation the final tool result applies.
    Capped(usize),
}

/// Append `chunk` to `full`, honoring the accumulation cap: the *first*
/// `cap` bytes win, matching the byte-cap truncation the final tool result
/// applies (see [`DrainAccumulate::Capped`]). Shared by `drain_fd` and
/// `drain_reader` so the raw copy and the streamed copy can never disagree
/// on the cap.
fn accumulate_chunk(budget: &mut Option<ByteBudget>, full: &mut Vec<u8>, chunk: &[u8]) {
    match budget.as_mut() {
        Some(budget) => {
            let take = budget.fit(chunk.len());
            full.extend_from_slice(&chunk[..take]);
        }
        None => full.extend_from_slice(chunk),
    }
}

/// Read `reader` (whose raw fd must be `fd`) to EOF, or until `stop_rx` is
/// signalled, returning everything read. `on_data` is invoked with each chunk
/// (used for line-streaming). [`DrainAccumulate::Capped`] bounds the returned
/// buffer at `n` bytes — the *first* `n` bytes are kept, matching the
/// byte-cap truncation the final tool result applies — while `on_data` still
/// sees every chunk and the drain keeps consuming, so a child that
/// out-produces the cap can never deadlock on a full pipe.
///
/// The fd is put in non-blocking mode first so the drain loop can consume
/// every byte available per `poll` verdict (full throughput for large
/// outputs) without ever blocking on an empty pipe whose write end is still
/// open (a surviving grandchild). If `fcntl` fails — which never happens on a
/// freshly-created pipe — the code falls back to reading a single chunk per
/// poll verdict, which is safe but slower.
#[cfg(unix)]
fn drain_fd<R: Read + AsFd>(
    mut reader: R,
    stop_rx: mpsc::Receiver<()>,
    poll: Duration,
    on_data: &mut dyn FnMut(&[u8]),
    accumulate: DrainAccumulate,
) -> Vec<u8> {
    // Keep the raw fd around purely for the trace log below; all syscalls use
    // the BorrowedFd from `reader.as_fd()`, so there is no raw-fd unsafe.
    let fd = reader.as_fd().as_raw_fd();
    let nonblocking = set_nonblocking(reader.as_fd());
    if !nonblocking {
        debug!(
            fd,
            "could not set child pipe non-blocking; draining one chunk per poll"
        );
    }
    let mut full: Vec<u8> = Vec::new();
    // The accumulation cap is tracked by a shared ByteBudget (the same
    // "first N bytes" engine the streaming paths use) so the raw copy and
    // the streamed copy can never disagree on the cap. `DrainAccumulate::None`
    // skips the accounting entirely — the caller processes every chunk via
    // `on_data`.
    let mut budget = match accumulate {
        DrainAccumulate::None => None,
        DrainAccumulate::Capped(cap) => Some(ByteBudget::new(cap)),
    };
    let mut buf = [0u8; 8192];
    loop {
        if !poll_readable(reader.as_fd(), &stop_rx, poll) {
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
                    // The accumulation cap only bounds the *returned* copy:
                    // on_data still receives every chunk and the read loop
                    // keeps consuming, so a child that out-produces the cap
                    // never blocks on a full pipe. Without the cap, a
                    // `cat /dev/zero`-class command would buffer its entire
                    // output in daemon memory even though the final tool
                    // result is truncated at MAX_TOOL_OUTPUT_BYTES anyway.
                    accumulate_chunk(&mut budget, &mut full, &buf[..n]);
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
#[cfg(unix)]
fn set_nonblocking(fd: rustix::fd::BorrowedFd<'_>) -> bool {
    // F_GETFL then F_SETFL with O_NONBLOCK is the standard way to make a pipe
    // read end non-blocking; only the read end's own behaviour changes (the
    // child's writes to the other end are unaffected). rustix's typed fcntl
    // wrappers remove the two unsafe libc blocks the old code needed.
    match rustix::fs::fcntl_getfl(fd) {
        Ok(flags) => rustix::fs::fcntl_setfl(fd, flags | rustix::fs::OFlags::NONBLOCK).is_ok(),
        Err(_) => false,
    }
}

/// Windows pipe drain: anonymous pipes have no poll(2)/non-blocking mode, so
/// read to EOF in a dedicated thread. Boundedness comes from the Job Object:
/// terminating the job closes the write-ends (descendants die), so a blocked
/// read unblocks with EOF — no stop signal or poll needed.
///
/// Mirrors `drain_fd`'s contract: `on_data` observes every chunk, the returned
/// buffer obeys [`DrainAccumulate`] (first `cap` bytes win), and the child can
/// never deadlock on a full pipe because the drain keeps consuming.
#[cfg(windows)]
fn drain_reader<R: Read + Send>(
    mut reader: R,
    on_data: &mut dyn FnMut(&[u8]),
    accumulate: DrainAccumulate,
) -> Vec<u8> {
    let mut full: Vec<u8> = Vec::new();
    // The accumulation cap is tracked by a shared ByteBudget (the same
    // "first N bytes" engine the streaming paths use) so the raw copy and
    // the streamed copy can never disagree on the cap, exactly as in
    // `drain_fd`.
    let mut budget = match accumulate {
        DrainAccumulate::None => None,
        DrainAccumulate::Capped(cap) => Some(ByteBudget::new(cap)),
    };
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => return full, // EOF: all pipe write ends are closed
            Ok(n) => {
                on_data(&buf[..n]);
                // The accumulation cap only bounds the *returned* copy:
                // on_data still receives every chunk and the read loop keeps
                // consuming, so a child that out-produces the cap never
                // blocks on a full pipe.
                accumulate_chunk(&mut budget, &mut full, &buf[..n]);
            }
            // A blocking read can only be unblocked by the Job Object being
            // terminated (the write-ends close); EINTR is retried as usual.
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => {
                debug!(error = %e, "read from child pipe failed; stopping drain");
                return full;
            }
        }
    }
}

/// Maximum bytes buffered for an unterminated line before it is flushed
/// forward as a partial chunk. A pathological child that writes megabytes
/// without ever emitting a newline would otherwise grow `pending` without
/// bound (two pipes × unbounded per-line buffers). Real-world lines are far
/// smaller; the partial chunks are concatenated back together downstream, so
/// the byte stream is unchanged — only the chunking differs.
const MAX_PENDING_LINE_BYTES: usize = 16 * 1024;

/// Number of trailing bytes of `pending` that belong to an incomplete UTF-8
/// sequence — plus a trailing `\r` held back for CRLF folding — that must
/// NOT be flushed forward. Splitting only at char boundaries keeps the
/// client's per-chunk lossy decode in sync with the final record (which
/// decodes the whole stream at once): a char split across two chunks would
/// render as replacement chars in the live view but as the real character
/// in the record.
fn partial_tail_len(pending: &[u8]) -> usize {
    let mut i = pending.len();
    // A trailing CR is held back so a `\n` arriving in a later chunk can
    // still fold the CRLF (matching `BufRead::lines()`).
    let cr = usize::from(i > 0 && pending[i - 1] == b'\r');
    i -= cr;
    // Walk back over continuation bytes (0b10xxxxxx) to the sequence lead.
    let mut continuations = 0usize;
    while i > 0 && (0x80..=0xBF).contains(&pending[i - 1]) {
        i -= 1;
        continuations += 1;
    }
    if i > 0 {
        let lead = pending[i - 1];
        // Continuation bytes the lead declares; 0 for ASCII and for the
        // invalid leads 0xF8..=0xFF, which cannot start a valid sequence.
        let expected = match lead {
            0xC0..=0xDF => 1,
            0xE0..=0xEF => 2,
            0xF0..=0xF7 => 3,
            _ => 0,
        };
        if expected > continuations {
            // The lead declares more continuation bytes than are present —
            // the sequence is incomplete; hold the whole partial char back.
            return continuations + 1 + cr;
        }
    }
    // No lead found (orphan continuation bytes — not part of any char) or a
    // complete sequence: only the CR is held back.
    cr
}

/// Flush `pending` forward as a partial chunk — called when an unterminated
/// line exceeds [`MAX_PENDING_LINE_BYTES`] so a newline-less firehose cannot
/// balloon daemon memory. Holds back a trailing `\r` (CRLF folding) and any
/// partial UTF-8 char at the end ([`partial_tail_len`]), so the emitted
/// chunk ends on a char boundary and the client's per-chunk lossy decode
/// never renders a replacement char where the final record shows the real
/// character.
fn flush_partial_line(pending: &mut Vec<u8>, on_line: &mut dyn FnMut(Vec<u8>)) {
    // The partial tail is at most 5 bytes (3 continuation + lead + CR) and
    // the flush is only called when `pending` is at least the 16 KiB
    // threshold, so the split is always ≥ 1 — the flush always makes
    // progress and never emits an empty chunk.
    let split = pending.len() - partial_tail_len(pending);
    let line: Vec<u8> = pending.drain(..split).collect();
    on_line(line);
}

/// Append `chunk` to `pending`, forwarding every complete line (terminated by
/// `\n`, with a trailing `\r` stripped so CRLF is folded into LF) to
/// `on_line`. Leftover bytes stay in `pending` for the next chunk, or a final
/// flush by the caller at EOF — matching `BufRead::lines()` semantics.
///
/// An unterminated line that grows past [`MAX_PENDING_LINE_BYTES`] is flushed
/// as a partial chunk ([`flush_partial_line`]) instead of being buffered
/// forever, bounding daemon memory on newline-less output.
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
        } else if pending.len() >= MAX_PENDING_LINE_BYTES {
            // No newline in sight and the buffer is large: emit a partial
            // chunk so a megabyte-long line cannot grow memory without bound
            // (the drains and the bounded merge channel stay tight; the
            // merger concatenates the partials back into the byte stream).
            flush_partial_line(pending, on_line);
        }
    }
}

/// Spawn a background thread that drains a capped stdout/stderr pipe into a
/// Vec, delivering the buffer over a completion channel (the message-passing
/// analogue of a join: the caller waits with `recv_timeout` instead of
/// polling `is_finished`, mirroring `spawn_watchdog`'s done-signal). The
/// `JoinHandle` is only reaped once the buffer has been delivered; on the
/// Windows give-up path it is dropped instead, detaching the (wedged) thread.
///
/// Unix: non-blocking poll slices, bounded by `stop_rx`.
/// Windows: blocking reads, bounded by the Job Object (see `drain_reader`).
#[cfg(unix)]
fn spawn_capped_drain<R: Read + AsFd + Send + 'static>(
    reader: R,
    stop_rx: mpsc::Receiver<()>,
) -> (std::thread::JoinHandle<()>, mpsc::Receiver<Vec<u8>>) {
    let (done_tx, done_rx) = mpsc::channel::<Vec<u8>>();
    let handle = std::thread::spawn(move || {
        let buf = drain_fd(
            reader,
            stop_rx,
            DRAIN_POLL_INTERVAL,
            &mut |_| {},
            DrainAccumulate::Capped(MAX_TOOL_OUTPUT_BYTES),
        );
        let _ = done_tx.send(buf);
    });
    (handle, done_rx)
}

#[cfg(windows)]
fn spawn_capped_drain<R: Read + Send + 'static>(
    reader: R,
    _stop_rx: mpsc::Receiver<()>, // Windows drains are bounded by the Job Object, not a stop signal
) -> (std::thread::JoinHandle<()>, mpsc::Receiver<Vec<u8>>) {
    let (done_tx, done_rx) = mpsc::channel::<Vec<u8>>();
    let handle = std::thread::spawn(move || {
        let buf = drain_reader(
            reader,
            &mut |_| {},
            DrainAccumulate::Capped(MAX_TOOL_OUTPUT_BYTES),
        );
        let _ = done_tx.send(buf);
    });
    (handle, done_rx)
}

/// Spawn the command, run a watchdog thread to enforce the timeout,
/// and return the process output along with a flag indicating whether
/// the watchdog killed the process.
///
/// Applies child-process hardening (`setup_child`) before spawning, isolates
/// the child's process tree (a pidfd on Linux so a timeout kill can never be
/// redirected at a recycled PID; a Job Object on Windows — see
/// `kill_child_tree` / `ChildJob`), and drains stdout/stderr in bounded
/// background threads so a surviving grandchild cannot hang the tool past the
/// timeout.
pub(crate) fn spawn_with_watchdog(
    cmd: &mut Command,
    timeout_ms: u64,
) -> Result<(Output, bool), ToolExecError> {
    setup_child(cmd);
    let mut child = cmd.spawn()?;
    let pid = child.id();

    // Platform-specific process-tree isolation, taken right after spawn.
    //
    // Unix: pin the child's identity with a pidfd (when available) so a
    // timeout kill can never be redirected at a recycled PID.
    #[cfg(unix)]
    let pidfd = open_pidfd(pid);
    // Windows: assign the child to a Job Object (the tree-kill switch). The
    // job is created AFTER spawn — `Command` has no pre-exec hook on
    // Windows — so there is a narrow window where the child runs ungoverned;
    // assignment failure kills the child before propagating. The watchdog
    // also keeps a copy of the child's process handle so it can tell "already
    // exited on its own" from "still running" when the timeout fires
    // (mirrors the Unix pidfd/ESRCH check — see `ProcessIsAlive`).
    #[cfg(windows)]
    let job = Arc::new(ChildJob::assign(&child).map_err(|e| {
        // Assignment failure leaves a live child running with no kill
        // switch; reap it before propagating the error.
        let _ = child.kill();
        let _ = child.wait();
        e
    })?);
    #[cfg(windows)]
    let watchdog_proc = ProcessIsAlive::from_child(&child);

    let (done_tx, done_rx) = mpsc::channel::<()>();
    let (killed_tx, killed_rx) = mpsc::channel::<()>();
    // The watchdog kills the tree on timeout: killpg on Unix, Job Object
    // termination on Windows — gated on the child still running so a timeout
    // that lands at the same instant the child exits on its own is not
    // misreported as a kill.
    #[cfg(unix)]
    let watchdog = spawn_watchdog(
        timeout_ms,
        pid,
        move || kill_child_tree(pid, pidfd.as_ref()),
        done_rx,
        killed_tx,
        None,
    );
    #[cfg(windows)]
    let watchdog = {
        let watchdog_job = Arc::clone(&job);
        spawn_watchdog(
            timeout_ms,
            pid,
            move || watchdog_proc.is_running() && watchdog_job.terminate(),
            done_rx,
            killed_tx,
            None,
        )
    };

    // Drain stdout/stderr concurrently so the child can never block on a full
    // pipe buffer (the classic pipe deadlock). Each drain is bounded (a stop
    // signal on Unix, the Job Object on Windows); a pipe that was not piped
    // (None) is skipped, leaving the Output field empty — matching the old
    // wait_with_output behaviour.
    let (out_stop_tx, out_stop_rx) = mpsc::channel::<()>();
    let (err_stop_tx, err_stop_rx) = mpsc::channel::<()>();
    let stdout_thread = child
        .stdout
        .take()
        .map(|s| spawn_capped_drain(s, out_stop_rx));
    let stderr_thread = child
        .stderr
        .take()
        .map(|s| spawn_capped_drain(s, err_stop_rx));

    let status = child.wait()?;
    let _ = done_tx.send(());
    // The direct child is reaped. Stop the drainers: anything still holding a
    // pipe write end is a surviving grandchild or backgrounded process, and
    // waiting for its output could exceed the timeout we were enforcing. (On
    // Windows these sends are no-ops — the drains ignore the stop channel;
    // the Job Object bounds them, see `collect_capped_drain`.)
    let _ = out_stop_tx.send(());
    let _ = err_stop_tx.send(());
    if let Err(e) = watchdog.join() {
        warn!("watchdog thread panicked: {:?}", e);
    }

    // Collect the drained output.
    //
    // Unix: the stop signal ends each drain at its next poll slice; the
    // completion message arrives promptly (blocking recv).
    //
    // Windows: the drains are blocking reads with no poll to interrupt — a
    // drain still silent at the deadline is wedged on a pipe a surviving
    // grandchild holds open, so `collect_capped_drain` terminates the Job
    // Object to force EOF (the analogue of the stop signals; unlike Unix it
    // kills the survivor, because only the job termination can close its
    // write-end — see `collect_capped_drain`).
    #[cfg(unix)]
    let stdout = collect_capped_drain(stdout_thread, DRAIN_COMPLETION_GRACE).unwrap_or_default();
    #[cfg(unix)]
    let stderr = collect_capped_drain(stderr_thread, DRAIN_COMPLETION_GRACE).unwrap_or_default();
    #[cfg(windows)]
    let stdout = collect_capped_drain(stdout_thread, &job).unwrap_or_default();
    #[cfg(windows)]
    let stderr = collect_capped_drain(stderr_thread, &job).unwrap_or_default();
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

/// Forward a bounded total of bytes through a streaming channel, appending
/// the shared `...[truncated]` byte-cap marker exactly once when the cap is
/// hit and dropping everything after. The bounded streaming *channel* only
/// bounds in-flight chunks (backpressure); this caps the *total*, so a
/// long-running command cannot push an unbounded live view to the client.
/// The final recorded result is separately capped by `format_shell_output`,
/// but the streamed view must not diverge from it — the stream budget
/// reserves the record framing (see [`RecordFraming`]) so that final cap is
/// a no-op. The cap accounting itself is the shared [`ByteBudget`] (the same
/// engine `drain_fd` and the VM's guest-WRITE path use), so all streaming
/// paths agree on the "first N bytes + one marker" contract.
struct StreamByteCap {
    budget: ByteBudget,
    tx: crossbeam_channel::Sender<Vec<u8>>,
    /// Signalled when the watchdog kills the child (timeout) — interrupts a
    /// send that is blocked on a full channel so the tool cannot be wedged
    /// past its timeout by a stalled subscriber.
    abort_rx: crossbeam_channel::Receiver<()>,
}

impl StreamByteCap {
    fn new(
        limit: usize,
        tx: crossbeam_channel::Sender<Vec<u8>>,
        abort_rx: crossbeam_channel::Receiver<()>,
    ) -> Self {
        Self {
            budget: ByteBudget::new(limit),
            tx,
            abort_rx,
        }
    }

    /// Forward `chunk` if the budget allows; once the cap is hit, emit the
    /// marker once and drop everything after. A chunk that would cross the
    /// cap is sent as a fitting prefix first (a partial final line is fine —
    /// the client lossy-decodes UTF-8 and the final result is truncated the
    /// same way), then the marker.
    ///
    /// Every forwarded byte — and the marker, when it fires — is appended to
    /// `out` (the recorded body) in exactly the order it is sent, so the
    /// accumulated copy stays byte-identical to the live view: the recorded
    /// result then contains precisely what the client saw streamed,
    /// truncation marker included.
    ///
    /// Returns `false` when forwarding must stop — the watchdog aborted the
    /// tool (timeout) or the client dropped the receiver — in which case the
    /// caller should stop accumulating. The accumulated copy only grows with
    /// bytes that were actually delivered (forward before accumulate), so the
    /// record never contains a chunk the client did not see; order in `out`
    /// still matches send order (prefix then marker).
    fn push(&mut self, chunk: &[u8], out: &mut Vec<u8>) -> bool {
        let n = self.budget.fit(chunk.len());
        // Forward before accumulating: a chunk whose send is aborted must not
        // land in the recorded body (the live view never showed it).
        if n > 0 && !self.forward(&chunk[..n]) {
            return false;
        }
        if n > 0 {
            out.extend_from_slice(&chunk[..n]);
        }
        if let Some(marker) = self.budget.take_marker() {
            // Same byte-cap marker `truncate_tool_output` appends, so the
            // live view reads exactly like the final (capped) result.
            if !self.forward(marker.as_bytes()) {
                return false;
            }
            out.extend_from_slice(marker.as_bytes());
        }
        true
    }

    /// Send `bytes` to the client. The fast path (`try_send`) never blocks
    /// and is never interrupted by the abort signal; when the bounded channel
    /// is full (backpressure from a stalled subscriber) the send blocks in a
    /// `select!` against the abort signal, so a timeout kill still unwedges
    /// the tool. A dropped receiver fails the send, which stops the stream
    /// the same way an abort does.
    fn forward(&self, bytes: &[u8]) -> bool {
        match self.tx.try_send(bytes.to_vec()) {
            Ok(()) => true,
            Err(_) => crossbeam_channel::select! {
                send(self.tx, bytes.to_vec()) -> res => res.is_ok(),
                recv(self.abort_rx) -> _ => false,
            },
        }
    }
}

/// Spawn a background thread that drains `reader` (a piped stdout or stderr
/// handle) to EOF — or until `stop_rx` is signalled — splitting the bytes
/// into complete lines (CRLF folded) and forwarding each to `merge_tx` in
/// arrival order. Any final unterminated remainder is flushed at EOF,
/// matching `BufRead::lines()` semantics. Shared by both drains in
/// `spawn_with_streaming` so the stdout/stderr handling cannot drift.
///
/// `reader` is fully drained (never blocking the child on a full pipe), but
/// the per-stream accumulation is [`DrainAccumulate::None`]: the merger
/// thread accumulates the body, so the drain's own buffer is kept empty
/// while `on_data` still observes every chunk.
///
/// The drain signals completion over a channel (returned alongside the
/// handle) — the message-passing analogue of a join, so the caller can wait
/// with `recv_timeout` instead of polling `is_finished` (see
/// [`spawn_capped_drain`]).
///
/// Unix: bounded by a stop signal (poll slices).
/// Windows: bounded by the Job Object (`stop_rx` is unused there).
#[cfg(unix)]
fn spawn_line_drain<R>(
    reader: R,
    stop_rx: mpsc::Receiver<()>,
    merge_tx: crossbeam_channel::Sender<Vec<u8>>,
) -> (std::thread::JoinHandle<()>, mpsc::Receiver<()>)
where
    R: Read + AsFd + Send + 'static,
{
    let (done_tx, done_rx) = mpsc::channel::<()>();
    let handle = std::thread::spawn(move || {
        let mut pending: Vec<u8> = Vec::new();
        drain_fd(
            reader,
            stop_rx,
            DRAIN_POLL_INTERVAL,
            &mut |chunk: &[u8]| {
                forward_complete_lines(chunk, &mut pending, &mut |line| {
                    let _ = merge_tx.send(line);
                });
            },
            DrainAccumulate::None,
        );
        // Flush any final unterminated line (matches `BufRead::lines()`
        // yielding a last line without a trailing newline).
        if !pending.is_empty() {
            let _ = merge_tx.send(pending);
        }
        let _ = done_tx.send(());
    });
    (handle, done_rx)
}

/// Windows twin of the Unix `spawn_line_drain` with the same body, except it
/// uses the blocking [`drain_reader`] and ignores `stop_rx`: the Job Object
/// bounds the drain (terminating it closes the write-ends), so no stop
/// signal is needed.
#[cfg(windows)]
fn spawn_line_drain<R>(
    reader: R,
    _stop_rx: mpsc::Receiver<()>,
    merge_tx: crossbeam_channel::Sender<Vec<u8>>,
) -> (std::thread::JoinHandle<()>, mpsc::Receiver<()>)
where
    R: Read + Send + 'static,
{
    let (done_tx, done_rx) = mpsc::channel::<()>();
    let handle = std::thread::spawn(move || {
        let mut pending: Vec<u8> = Vec::new();
        drain_reader(
            reader,
            &mut |chunk: &[u8]| {
                forward_complete_lines(chunk, &mut pending, &mut |line| {
                    let _ = merge_tx.send(line);
                });
            },
            DrainAccumulate::None,
        );
        // Flush any final unterminated line (matches `BufRead::lines()`
        // yielding a last line without a trailing newline).
        if !pending.is_empty() {
            let _ = merge_tx.send(pending);
        }
        let _ = done_tx.send(());
    });
    (handle, done_rx)
}

/// Reserved bytes of record framing that `format_shell_output` will add
/// around a streamed body — the `$ {cmd}\n` header, the worst-case exit-code
/// footer, and the truncation markers. `spawn_with_streaming` subtracts this
/// from the stream budget so the cap `finish_tool_output_sanitized` applies
/// at format time is a no-op: the recorded body then contains exactly the
/// bytes that were streamed, truncation marker included.
///
/// A newtype (rather than a raw `usize`) so a caller cannot pass an
/// arbitrary reservation: [`RecordFraming::shell`] derives the value from the
/// *same* display command that `run_shell_streaming` passes to
/// `format_shell_output`, so the reserved and the actual framing cannot
/// drift. Direct callers that format the output themselves use
/// [`RecordFraming::none`].
#[derive(Debug, Clone, Copy)]
pub struct RecordFraming(usize);

impl RecordFraming {
    /// No record framing: the stream caps at the full
    /// [`MAX_TOOL_OUTPUT_BYTES`] budget (the caller formats the output
    /// without a reserved frame).
    pub const fn none() -> Self {
        Self(0)
    }

    /// Framing for `format_shell_output`'s `$ {display_cmd}\n` record,
    /// computed from the same display command that will be formatted.
    pub fn shell(display_cmd: &str) -> Self {
        Self(shell_output_framing_reservation(display_cmd))
    }

    /// The number of framing bytes reserved inside the stream budget.
    fn bytes(self) -> usize {
        self.0
    }
}

/// Spawn the command with piped stdout/stderr and stream their lines through
/// `output_tx` in real time as the process produces them. Enforces a timeout
/// via watchdog and returns the collected output along with a was-killed
/// flag.
///
/// Applies child-process hardening (`setup_child`) before spawning, isolates
/// the child's process tree (a pidfd on Linux so a timeout kill can never be
/// redirected at a recycled PID; a Job Object on Windows — see
/// `kill_child_tree` / `ChildJob`), and bounds both drains so a surviving
/// grandchild cannot hang the tool past the timeout. The caller must set
/// `Stdio::piped()` on both stdout and stderr before calling this.
///
/// IMPORTANT: Both stdout and stderr are drained in background threads so
/// the child can never block on a full pipe buffer (the classic pipe
/// deadlock), and **both are streamed**: complete lines from either stream
/// are forwarded through the same byte budget in the order the child
/// produced them, and the returned `Output.stdout` is exactly that
/// interleaved body (`stderr` is empty) — so `format_shell_output`'s final
/// body is byte-identical to what the client saw live. A tool that writes
/// its progress to stderr (cargo, nextest, make, …) therefore shows a live
/// view instead of appearing all at once when it exits.
///
/// `framing` reserves `format_shell_output`'s record framing (the
/// `$ {cmd}\n` prefix, the exit-code footer, and the truncation marker)
/// *inside* the stream budget so the final cap is a no-op and the recorded
/// body contains exactly the streamed bytes, even when the stream is
/// truncated. `run_shell_streaming` computes it via
/// [`RecordFraming::shell`]; direct callers that format the output
/// themselves pass [`RecordFraming::none`].
///
/// On a timeout the watchdog kills the child and signals an abort channel;
/// the merger selects on it while blocked on a full output channel, so a
/// stalled subscriber can never wedge the tool past its timeout (the abort
/// drops the merge receiver, the drains' sends fail, and every thread joins
/// promptly).
pub fn spawn_with_streaming(
    cmd: &mut Command,
    timeout_ms: u64,
    framing: RecordFraming,
    output_tx: crossbeam_channel::Sender<Vec<u8>>,
) -> Result<(Output, bool), ToolExecError> {
    setup_child(cmd);
    let mut child = cmd.spawn()?;
    let pid = child.id();

    // Platform-specific process-tree isolation — see `spawn_with_watchdog`
    // for the full rationale (pidfd on Unix, Job Object on Windows).
    #[cfg(unix)]
    let pidfd = open_pidfd(pid);
    #[cfg(windows)]
    let job = Arc::new(ChildJob::assign(&child).map_err(|e| {
        let _ = child.kill();
        let _ = child.wait();
        e
    })?);
    #[cfg(windows)]
    let watchdog_proc = ProcessIsAlive::from_child(&child);

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

    // ── Merge channel ────────────────────────────────────────────────────
    //
    // Both drain threads forward complete lines here in arrival order; a
    // single consumer (the merger thread below) forwards them to the client
    // and accumulates the body. Merging through a channel — rather than
    // having the two drains write to a shared buffer — keeps the interleave
    // deterministic (FIFO channel order) without sharing mutable state
    // between threads. Bounded so a stalled client backpressures the child
    // (the merger blocks on `output_tx`, the channel fills, the drains
    // block, the child blocks on its pipe) instead of buffering unboundedly.
    let (merge_tx, merge_rx) = crossbeam_channel::bounded::<Vec<u8>>(STREAMING_CHANNEL_CAPACITY);

    // Both pipes are drained by the same `spawn_line_drain` helper: split
    // into complete lines (CRLF folded) and forwarded to the merge channel in
    // arrival order. `DrainAccumulate::None` keeps `drain_fd`'s own buffer
    // empty — the merger accumulates the body — while `on_data` still
    // consumes every byte (no pipe deadlock).
    let stdout_thread = spawn_line_drain(stdout, out_stop_rx, merge_tx.clone());
    let stderr_thread = spawn_line_drain(stderr, err_stop_rx, merge_tx.clone());

    // The main thread drops its sender so the merge channel disconnects the
    // moment BOTH drain threads finish — that disconnect is what terminates
    // the merger below. Without this drop, the merger would wait forever.
    drop(merge_tx);

    // Abort signal: the watchdog sends on this when it kills the child for a
    // timeout. The merger selects on it while blocked on a full output
    // channel, so a stalled subscriber can never wedge the tool past its
    // timeout — the abort drops the merge receiver, the drains' sends fail,
    // and every thread joins promptly.
    let (abort_tx, abort_rx) = crossbeam_channel::bounded::<()>(1);

    // Thread: consume merged lines in arrival order, forward each through the
    // shared byte cap (the live view), and accumulate the same capped bytes
    // into the body returned to the caller. One budget for both streams keeps
    // the streamed total and the recorded body capped at
    // MAX_TOOL_OUTPUT_BYTES with a single `...[truncated]` marker — the same
    // "first N bytes + one marker" contract as the client's live
    // accumulation, so the recorded result reads exactly like the stream.
    //
    // The budget is reduced by `framing` (the record framing
    // `format_shell_output` adds — header, footer, marker) so the final cap
    // is a no-op: the recorded body then contains exactly the streamed
    // bytes, truncation marker included, and the exit-code footer always
    // survives (including the transcript re-cap in `record_tool_completion`).
    //
    // The merger delivers its accumulated body over a completion channel so
    // the main thread can wait for it with `recv_timeout` (channel-based, no
    // polling) — see `collect_merger_body`.
    let (merger_done_tx, merger_done_rx) = mpsc::channel::<Vec<u8>>();
    let merger_thread = std::thread::spawn(move || {
        let mut stream_cap = StreamByteCap::new(
            MAX_TOOL_OUTPUT_BYTES.saturating_sub(framing.bytes()),
            output_tx,
            abort_rx,
        );
        let mut full: Vec<u8> = Vec::new();
        while let Ok(line) = merge_rx.recv() {
            // Escape Cf chars (the spoofing class) BEFORE the bytes enter
            // the stream budget, so the budget accounts for the escaped form
            // the client and the record both see. Sanitizing here — instead
            // of only at format time — makes the recorded body byte-identical
            // to the live view even for Cf-heavy output, which previously
            // expanded past the cap at `finish_tool_output` and got re-cut
            // (dropping streamed tail bytes and appending a second marker).
            // `from_utf8_lossy` is lossless here: the drains only emit
            // char-aligned chunks (see `flush_partial_line`), so no real
            // char is ever split across chunks. The named binding keeps the
            // lossy Cow alive for the duration of the escape.
            let lossy = String::from_utf8_lossy(&line);
            let escaped = sanitize_transcript(&lossy);
            if !stream_cap.push(escaped.as_bytes(), &mut full) {
                // Abort (timeout) or client gone: stop accumulating. The
                // record is discarded on the timeout path anyway.
                break;
            }
        }
        let _ = merger_done_tx.send(full);
    });

    // Watchdog thread: enforce timeout. Shared with the buffered path so the
    // two timeout semantics stay identical (see `spawn_watchdog`); the abort
    // sender lets the watchdog unwedge the merger when it kills the child.
    let (done_tx, done_rx) = mpsc::channel::<()>();
    let (killed_tx, killed_rx) = mpsc::channel::<()>();
    // The watchdog kills the tree on timeout (killpg on Unix, Job Object
    // termination on Windows — gated on the child still running) — see
    // `spawn_with_watchdog`; the abort sender lets it unwedge the merger.
    #[cfg(unix)]
    let watchdog = spawn_watchdog(
        timeout_ms,
        pid,
        move || kill_child_tree(pid, pidfd.as_ref()),
        done_rx,
        killed_tx,
        Some(abort_tx),
    );
    #[cfg(windows)]
    let watchdog = {
        let watchdog_job = Arc::clone(&job);
        spawn_watchdog(
            timeout_ms,
            pid,
            move || watchdog_proc.is_running() && watchdog_job.terminate(),
            done_rx,
            killed_tx,
            Some(abort_tx),
        )
    };

    // Wait for the process to finish (both background threads drain the
    // pipes concurrently, preventing any blocking deadlock).
    let status = match child.wait() {
        Ok(status) => status,
        Err(e) => {
            // `wait` failing is nearly unreachable (std retries EINTR), but a
            // silent thread leak here would be permanent: tear down every
            // thread before propagating. The dropped `done_tx` makes the
            // watchdog treat this as a timeout and kill the (possibly still
            // running) child, which also sends the abort that unwedges the
            // merger.
            //
            // Unix: the stop signals end the drainers at their next poll.
            // Windows: terminating the job closes every descendant's pipe
            // write-end, so the drains' blocking reads hit EOF, the merge
            // channel disconnects, and the merger ends too.
            #[cfg(unix)]
            {
                let _ = out_stop_tx.send(());
                let _ = err_stop_tx.send(());
                collect_line_drains(stdout_thread, stderr_thread, DRAIN_COMPLETION_GRACE);
            }
            #[cfg(windows)]
            {
                job.terminate();
                collect_line_drains(stdout_thread, stderr_thread, &job);
            }
            let _ = collect_merger_body(merger_thread, merger_done_rx);
            if let Err(e) = watchdog.join() {
                warn!("watchdog thread panicked: {:?}", e);
            }
            return Err(e.into());
        }
    };
    let _ = done_tx.send(());
    // Stop the drainers once the direct child is reaped (see
    // `spawn_with_watchdog`). On Windows these sends are no-ops — the drains
    // ignore the stop channel; the Job Object bounds them (see
    // `collect_line_drains`).
    let _ = out_stop_tx.send(());
    let _ = err_stop_tx.send(());

    // Collect the drain threads first so the merge channel disconnects, then
    // the merger — its `recv` loop ends on that disconnect and delivers the
    // fully accumulated interleaved body.
    //
    // Unix: the stop signal ends each drain at its next poll slice.
    // Windows: a drain still silent at the deadline is wedged on a pipe a
    // surviving grandchild holds open — `collect_line_drains` terminates the
    // Job Object (killing the survivor and closing its write-end → EOF)
    // before waiting for delivery; the analogue of the Unix stop signals.
    #[cfg(unix)]
    collect_line_drains(stdout_thread, stderr_thread, DRAIN_COMPLETION_GRACE);
    #[cfg(windows)]
    collect_line_drains(stdout_thread, stderr_thread, &job);
    let body = collect_merger_body(merger_thread, merger_done_rx);
    if let Err(e) = watchdog.join() {
        warn!("watchdog thread panicked: {:?}", e);
    }
    let was_killed = killed_rx.try_recv().is_ok();

    Ok((
        Output {
            // stdout carries the interleaved stdout+stderr body; stderr is
            // empty (see the doc comment). format_shell_output then produces
            // `$ cmd\n<body>\nExit code: N` with the body byte-identical to
            // what the client saw streamed.
            stdout: body,
            stderr: Vec::new(),
            status,
        },
        was_killed,
    ))
}

/// Bytes of record framing that `format_shell_output` adds around the
/// streamed body and that must therefore be reserved *inside* the stream
/// budget: the `$ {display_cmd}\n` header, the worst-case `\n\nExit code: N`
/// footer (an i32 exit code is at most 11 chars), and the `...[truncated]`
/// marker twice — once for the stream's own marker (which rides in the
/// recorded body) and once for the room `finish_tool_output` holds back for
/// its generic marker. Reserving them keeps `finish_tool_output`'s cap a
/// no-op, so the recorded result contains exactly the bytes that were
/// streamed — truncation marker included — and the exit-code footer always
/// survives (including the transcript re-cap in `record_tool_completion`).
///
/// The header length is measured *after* `sanitize_transcript`: the whole
/// record is escaped before the cap, so Cf chars in the display command
/// expand and must be reserved at their escaped size or the cap could still
/// re-cut the body.
fn shell_output_framing_reservation(display_cmd: &str) -> usize {
    let escaped_header_len = sanitize_transcript(&format!("$ {display_cmd}\n")).len();
    // `format_shell_output` renders the footer as `\n` + `\nExit code: {code}`;
    // i32::MIN ("-2147483648") is the longest possible exit code.
    const WORST_CASE_FOOTER_LEN: usize = "\n\nExit code: -2147483648".len();
    escaped_header_len + WORST_CASE_FOOTER_LEN + 2 * TRUNCATION_SUFFIX.len()
}

/// Convenience wrapper that combines `spawn_with_streaming` (which applies
/// `setup_child` hardening itself) and `format_shell_output` into a single
/// call — used by shell tool `execute_streaming` implementations to avoid
/// repeating the same spawn-and-format sequence across `sh`, `fish`, `nu`,
/// and `exec`.
///
/// Reserves the record framing (`$ {cmd}\n` + exit-code footer + truncation
/// marker) inside the stream budget so the recorded body is byte-identical
/// to the live view even when the output is truncated.
///
/// The caller must have set `Stdio::piped()` on both stdout and stderr.
pub fn run_shell_streaming(
    cmd: &mut Command,
    display_cmd: &str,
    timeout_ms: u64,
    output_tx: crossbeam_channel::Sender<Vec<u8>>,
) -> Result<String, ToolExecError> {
    // `RecordFraming::shell` derives the reservation from the SAME display
    // command passed to `format_shell_output` below, so the reserved and the
    // actual framing cannot drift.
    let (output, was_killed) = spawn_with_streaming(
        cmd,
        timeout_ms,
        RecordFraming::shell(display_cmd),
        output_tx,
    )?;
    Ok(format_shell_output(
        display_cmd,
        &output,
        timeout_ms,
        was_killed,
    ))
}

/// Format the tool output string for a shell-style command.
///
/// The body is sanitized for the transcript and capped with the exit code
/// reserved *inside* the budget (`finish_tool_output_sanitized`), so the
/// "Exit code" signal always survives — including the transcript re-cap in
/// `record_tool_completion`, which re-applies the cap after `sanitize_transcript`
/// (escaping expands Cf chars, so a raw near-cap body could otherwise push
/// past the budget and lose its tail).
pub(crate) fn format_shell_output(
    display_cmd: &str,
    output: &Output,
    timeout_ms: u64,
    was_killed: bool,
) -> String {
    if was_killed {
        return finish_tool_output_sanitized(
            &format!("$ {display_cmd}"),
            Some(format!(
                "\n[command timed out after {timeout_ms}ms]\n\nExit code: -1"
            )),
        );
    }
    // The streaming path folds stderr into `output.stdout` (see
    // `spawn_with_streaming`), so no copy is needed there; the buffered path
    // concatenates stdout then stderr into an owned buffer.
    let combined_str = if output.stderr.is_empty() {
        String::from_utf8_lossy(&output.stdout)
    } else {
        let mut combined = output.stdout.clone();
        combined.extend_from_slice(&output.stderr);
        std::borrow::Cow::Owned(String::from_utf8_lossy(&combined).into_owned())
    };
    let exit_code = output.status.code().unwrap_or(-1);
    finish_tool_output_sanitized(
        &format!("$ {display_cmd}\n{combined_str}"),
        Some(format!("\nExit code: {exit_code}")),
    )
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

        let pgid = rustix::process::getpgid(Some(
            rustix::process::Pid::from_raw(child_pid).expect("parsed child pid is nonzero"),
        ))
        .expect("getpgid on live child");
        assert_eq!(
            pgid.as_raw_pid(),
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
        let got = drain_fd(
            reader,
            stop_rx,
            Duration::ZERO,
            &mut |_| {},
            DrainAccumulate::None,
        );
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
        let got = drain_fd(
            reader,
            stop_rx,
            Duration::ZERO,
            &mut |_| {},
            DrainAccumulate::None,
        );
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

        let got = drain_fd(
            reader,
            stop_rx,
            Duration::ZERO,
            &mut |_| {},
            DrainAccumulate::None,
        );
        assert_eq!(
            got, b"hello",
            "buffered data must be drained before stopping"
        );
        drop(writer);
    }

    #[test]
    fn drain_fd_caps_accumulation_at_requested_limit() {
        // The accumulation cap must bound the returned buffer while still
        // consuming every byte (no pipe deadlock) and invoking on_data for
        // each chunk. Deterministic — all data is buffered and EOF is
        // signalled by drop before the drain starts (poll_ms = 0). The
        // payload must stay under the OS pipe buffer (16 KiB on macOS, 64 KiB
        // on Linux) or the blocking write_all would deadlock against a drain
        // that has not started yet.
        let (reader, mut writer) = std::io::pipe().expect("pipe");
        let payload = vec![b'x'; 12 * 1024];
        writer.write_all(&payload).expect("write");
        drop(writer); // EOF

        let (_stop_tx, stop_rx) = mpsc::channel::<()>();
        let mut seen = 0usize;
        let got = drain_fd(
            reader,
            stop_rx,
            Duration::ZERO,
            &mut |chunk: &[u8]| seen += chunk.len(),
            DrainAccumulate::Capped(8 * 1024),
        );
        assert_eq!(got.len(), 8 * 1024, "accumulation must stop at the cap");
        assert_eq!(
            seen,
            payload.len(),
            "on_data must still observe every byte past the cap"
        );
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
    fn forward_complete_lines_flushes_oversized_unterminated_lines() {
        // A child that never emits a newline must not balloon daemon memory:
        // the pending buffer is flushed forward in partial chunks (each at
        // most the threshold) as soon as it grows past MAX_PENDING_LINE_BYTES.
        // The byte stream is unchanged — only the chunking differs.
        let mut pending: Vec<u8> = Vec::new();
        let mut parts: Vec<Vec<u8>> = Vec::new();
        forward_complete_lines(
            &vec![b'x'; MAX_PENDING_LINE_BYTES * 3],
            &mut pending,
            &mut |l| parts.push(l),
        );
        let mut merged: Vec<u8> = parts.iter().flatten().copied().collect();
        merged.extend_from_slice(&pending);
        assert_eq!(
            merged.len(),
            MAX_PENDING_LINE_BYTES * 3,
            "no bytes lost across the partial flushes"
        );
        assert!(
            parts.iter().all(|p| p.len() <= MAX_PENDING_LINE_BYTES),
            "no partial chunk may exceed the threshold"
        );
        assert!(pending.is_empty(), "pure 'x' input leaves nothing pending");

        // A CRLF whose `\r` is flushed inside a partial chunk (the `\n`
        // arrives in a later chunk) must still fold: the flush holds the
        // trailing `\r` back so the fold can happen.
        let mut pending: Vec<u8> = vec![b'x'; MAX_PENDING_LINE_BYTES - 1];
        let mut parts = Vec::new();
        forward_complete_lines(b"\r", &mut pending, &mut |l| parts.push(l));
        assert_eq!(parts, vec![vec![b'x'; MAX_PENDING_LINE_BYTES - 1]]);
        assert_eq!(pending, b"\r", "trailing CR held back for CRLF folding");
        forward_complete_lines(b"\n", &mut pending, &mut |l| parts.push(l));
        assert_eq!(parts[1], b"\n", "the held-back CR folds with the LF");
        assert!(pending.is_empty());
    }

    #[test]
    fn forward_complete_lines_holds_back_partial_utf8_at_flush() {
        // A multi-byte char split across the flush boundary must be held back
        // whole: the emitted chunks stay valid UTF-8, so the client's
        // per-chunk lossy decode never renders a replacement char where the
        // final record (a whole-stream decode) shows the real character.
        // Deterministic — pure byte-buffer manipulation, no I/O.
        let mut pending: Vec<u8> = vec![b'x'; MAX_PENDING_LINE_BYTES - 2];
        let mut parts: Vec<Vec<u8>> = Vec::new();
        // Two of the three bytes of € (U+20AC: E2 82 AC) cross the threshold:
        // the flush must hold the partial char back instead of splitting it.
        forward_complete_lines(b"\xe2\x82", &mut pending, &mut |l| parts.push(l));
        assert_eq!(
            parts,
            vec![vec![b'x'; MAX_PENDING_LINE_BYTES - 2]],
            "the complete prefix is flushed; nothing past it"
        );
        assert_eq!(pending, b"\xe2\x82", "partial char held back whole");

        // The final byte completes the char; the EOF flush emits it whole.
        forward_complete_lines(b"\xac", &mut pending, &mut |l| parts.push(l));
        assert!(
            !pending.is_empty(),
            "the char is still pending (no newline)"
        );
        let joined: Vec<u8> = parts.concat();
        assert_eq!(
            joined.len(),
            MAX_PENDING_LINE_BYTES - 2,
            "no bytes lost across the flush"
        );
        // The pending € completes the stream; a whole-stream decode sees the
        // real char, never a replacement.
        let stream: Vec<u8> = joined
            .into_iter()
            .chain(std::mem::take(&mut pending))
            .collect();
        assert_eq!(
            String::from_utf8_lossy(&stream),
            format!("{}€", "x".repeat(MAX_PENDING_LINE_BYTES - 2)),
            "chunks must join back into the original valid UTF-8"
        );
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

    #[test]
    fn stream_byte_cap_accumulates_what_it_forwards() {
        // The forwarder must append every forwarded byte — and the marker,
        // when it fires — to the accumulated body in send order, so the
        // recorded result is byte-identical to the live view.
        let (tx, rx) = crossbeam_channel::unbounded::<Vec<u8>>();
        let (_abort_tx, abort_rx) = crossbeam_channel::bounded::<()>(1);
        let mut cap = StreamByteCap::new(10, tx, abort_rx);
        let mut out = Vec::new();

        assert!(cap.push(b"abcd", &mut out));
        assert!(cap.push(b"0123456789", &mut out));
        assert!(cap.push(b"xyz", &mut out));

        let streamed: Vec<u8> = rx.try_iter().flatten().collect();
        assert_eq!(
            out, streamed,
            "recorded body must equal the forwarded stream"
        );
        assert_eq!(out, b"abcd012345\n...[truncated]");
    }

    #[test]
    fn stream_byte_cap_abort_interrupts_a_blocked_send() {
        // A full output channel (stalled subscriber) must not wedge the tool:
        // the abort signal — sent by the watchdog on a timeout kill — wakes
        // the blocked send, and `push` reports false so the caller stops.
        // Deterministic: the channel is filled before the abort is queued, so
        // `push` blocks in `select!` and the queued abort wins (no time-based
        // waits).
        let (tx, rx) = crossbeam_channel::bounded::<Vec<u8>>(1);
        let (abort_tx, abort_rx) = crossbeam_channel::bounded::<()>(1);
        let mut cap = StreamByteCap::new(100, tx.clone(), abort_rx);
        let mut out = Vec::new();

        // Fill the channel: the next send must block.
        tx.try_send(b"full".to_vec()).expect("fill channel");
        abort_tx.send(()).expect("queue abort");
        assert!(
            !cap.push(b"blocked", &mut out),
            "abort must interrupt a blocked send"
        );
        // The client-side chunk stays queued; the aborted chunk was neither
        // delivered nor accumulated.
        assert_eq!(rx.try_recv().expect("queued chunk"), b"full");
        assert!(rx.try_recv().is_err(), "aborted send must not deliver");
        assert!(out.is_empty(), "aborted send must not be accumulated");
    }

    #[test]
    fn framing_reservation_covers_prefix_footer_and_markers() {
        // The reservation must cover the `$ {cmd}\n` prefix plus the longest
        // possible exit-code footer plus both truncation markers, so the
        // stream budget keeps `finish_tool_output` from re-cutting the
        // recorded body (the byte-identical guarantee under truncation).
        let prefix = "$ echo\n".len();
        let worst_footer = "\n\nExit code: -2147483648".len();
        assert_eq!(
            shell_output_framing_reservation("echo"),
            prefix + worst_footer + 2 * TRUNCATION_SUFFIX.len()
        );
        // Sanity: the framing is small relative to the budget.
        assert!(shell_output_framing_reservation("echo") < MAX_TOOL_OUTPUT_BYTES / 100);

        // A Cf char in the display command expands when escaped; the
        // reservation must cover the escaped header or the final cap could
        // still re-cut the body.
        assert!(
            shell_output_framing_reservation("echo\u{200b}")
                > shell_output_framing_reservation("echo"),
            "Cf chars in the display command must be reserved at escaped size"
        );
    }

    #[test]
    fn merged_streams_preserve_each_streams_line_order() {
        // Two pre-filled pipes drained concurrently into one merge channel
        // (the `spawn_with_streaming` pattern via the shared `spawn_line_drain`
        // helper): the merged sequence must be a valid interleaving — each
        // stream's lines keep their relative order no matter how the scheduler
        // interleaves the two drains. Deterministic: all data is buffered
        // before the drains start and EOF is signalled by dropping the
        // writers, so the drains return immediately (no real time passes).
        let (out_r, mut out_w) = std::io::pipe().expect("pipe");
        let (err_r, mut err_w) = std::io::pipe().expect("pipe");
        out_w.write_all(b"o1\no2\no3\n").expect("write stdout");
        err_w.write_all(b"e1\ne2\n").expect("write stderr");
        drop(out_w);
        drop(err_w);

        let (merge_tx, merge_rx) = crossbeam_channel::unbounded::<Vec<u8>>();
        let (_out_stop_tx, out_stop_rx) = mpsc::channel::<()>();
        let (_err_stop_tx, err_stop_rx) = mpsc::channel::<()>();
        let t1 = spawn_line_drain(out_r, out_stop_rx, merge_tx.clone());
        let t2 = spawn_line_drain(err_r, err_stop_rx, merge_tx.clone());
        // The drains deliver a completion message over their channel before
        // the thread exits (see `spawn_line_drain`); wait for it, then reap.
        t1.1.recv().expect("stdout drain completion");
        t2.1.recv().expect("stderr drain completion");
        t1.0.join().expect("stdout drain");
        t2.0.join().expect("stderr drain");
        drop(merge_tx);

        let merged: Vec<String> = merge_rx
            .try_iter()
            .map(|l| String::from_utf8_lossy(&l).into_owned())
            .collect();
        let outs: Vec<String> = merged
            .iter()
            .filter(|l| l.starts_with('o'))
            .cloned()
            .collect();
        let errs: Vec<String> = merged
            .iter()
            .filter(|l| l.starts_with('e'))
            .cloned()
            .collect();
        assert_eq!(
            outs,
            vec!["o1\n", "o2\n", "o3\n"],
            "stdout lines keep their relative order"
        );
        assert_eq!(errs, vec!["e1\n", "e2\n"], "stderr lines keep their order");
        assert_eq!(merged.len(), 5, "every line from both streams is merged");
    }

    #[test]
    fn collect_capped_drain_detaches_when_drain_never_delivers() {
        // A drain that never delivers (a surviving grandchild keeping the
        // pipe producing past the completion grace) must not hang the caller:
        // the wait is bounded, and past it the handle is dropped (the thread
        // detaches and exits on its own once the survivor does). Deterministic
        // — a zero grace fires the timeout immediately, no time passes.
        let (done_tx, done_rx) = mpsc::channel::<Vec<u8>>();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let handle = std::thread::spawn(move || {
            // Hold the sender alive forever to simulate a wedged drain; the
            // completion message is never sent. Block on the release channel
            // so the test can let the (detached) thread exit cleanly.
            let _done_tx = done_tx;
            let _ = release_rx.recv();
        });
        let got = collect_capped_drain(Some((handle, done_rx)), Duration::ZERO);
        assert!(
            got.is_none(),
            "a never-delivering drain must detach, not hang"
        );
        // Release the wedged drain so the detached thread exits promptly.
        release_tx.send(()).expect("release wedged drain");
    }

    #[test]
    fn collect_line_drains_detaches_when_drain_never_delivers() {
        // Same wedged-drain contract as the buffered path: line drains that
        // never deliver must be detached, not awaited forever.
        let (done_tx1, done_rx1) = mpsc::channel::<()>();
        let (rel1_tx, rel1_rx) = mpsc::channel::<()>();
        let h1 = std::thread::spawn(move || {
            let _done_tx1 = done_tx1;
            let _ = rel1_rx.recv();
        });
        let (done_tx2, done_rx2) = mpsc::channel::<()>();
        let (rel2_tx, rel2_rx) = mpsc::channel::<()>();
        let h2 = std::thread::spawn(move || {
            let _done_tx2 = done_tx2;
            let _ = rel2_rx.recv();
        });
        collect_line_drains((h1, done_rx1), (h2, done_rx2), Duration::ZERO);
        rel1_tx.send(()).expect("release stdout drain");
        rel2_tx.send(()).expect("release stderr drain");
    }
}
