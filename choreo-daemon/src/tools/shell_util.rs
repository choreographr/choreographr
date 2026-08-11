use super::{
    MAX_TOOL_OUTPUT_BYTES, STREAMING_CHANNEL_CAPACITY, ToolExecError, finish_tool_output_sanitized,
};
use choreo_sanitize::{ByteBudget, TRUNCATION_SUFFIX};
use crossbeam_channel;
use std::{
    io::Read,
    os::fd::{AsFd, AsRawFd, OwnedFd},
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
fn poll_readable(
    fd: rustix::fd::BorrowedFd<'_>,
    stop_rx: &mpsc::Receiver<()>,
    poll_ms: i32,
) -> bool {
    // Poll for readability/EOF on the pipe fd. The PollFd borrows the fd for
    // the duration of each call, so no raw-fd/unsafe handling is needed.
    let mut pfds = [rustix::event::PollFd::new(
        &fd,
        rustix::event::PollFlags::IN,
    )];
    // poll(2) takes a timespec; the drain slices come in as i32 milliseconds.
    // (`Timespec` is re-exported from rustix::event; rustix::timespec is private.)
    let timeout = rustix::event::Timespec {
        tv_sec: i64::from(poll_ms / 1000),
        tv_nsec: i64::from(poll_ms % 1000) * 1_000_000,
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

/// Read `reader` (whose raw fd must be `fd`) to EOF, or until `stop_rx` is
/// signalled, returning everything read. `on_data` is invoked with each chunk
/// (used for line-streaming). When `accumulate_cap` is `Some(n)`, the
/// returned buffer stops growing once it reaches `n` bytes — the *first* `n`
/// bytes are kept, matching the byte-cap truncation the final tool result
/// applies — while `on_data` still sees every chunk and the drain keeps
/// consuming, so a child that out-produces the cap can never deadlock on a
/// full pipe.
///
/// The fd is put in non-blocking mode first so the drain loop can consume
/// every byte available per `poll` verdict (full throughput for large
/// outputs) without ever blocking on an empty pipe whose write end is still
/// open (a surviving grandchild). If `fcntl` fails — which never happens on a
/// freshly-created pipe — the code falls back to reading a single chunk per
/// poll verdict, which is safe but slower.
fn drain_fd<R: Read + AsFd>(
    mut reader: R,
    stop_rx: mpsc::Receiver<()>,
    poll_ms: i32,
    on_data: &mut dyn FnMut(&[u8]),
    accumulate_cap: Option<usize>,
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
    // the streamed copy can never disagree on the cap.
    let mut budget = accumulate_cap.map(ByteBudget::new);
    let mut buf = [0u8; 8192];
    loop {
        if !poll_readable(reader.as_fd(), &stop_rx, poll_ms) {
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
                    match budget.as_mut() {
                        Some(budget) => {
                            let take = budget.fit(n);
                            full.extend_from_slice(&buf[..take]);
                        }
                        None => full.extend_from_slice(&buf[..n]),
                    }
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

/// Maximum bytes buffered for an unterminated line before it is flushed
/// forward as a partial chunk. A pathological child that writes megabytes
/// without ever emitting a newline would otherwise grow `pending` without
/// bound (two pipes × unbounded per-line buffers). Real-world lines are far
/// smaller; the partial chunks are concatenated back together downstream, so
/// the byte stream is unchanged — only the chunking differs.
const MAX_PENDING_LINE_BYTES: usize = 16 * 1024;

/// Flush `pending` forward as a partial chunk — called when an unterminated
/// line exceeds [`MAX_PENDING_LINE_BYTES`] so a newline-less firehose cannot
/// balloon daemon memory. Holds back a trailing `\r` so a `\n` arriving in a
/// later chunk can still fold the CRLF. The flush point is either the buffer
/// end or the held-back `\r` — both complete UTF-8 chars — so the client's
/// per-chunk lossy decode never renders a replacement char mid-line.
fn flush_partial_line(pending: &mut Vec<u8>, on_line: &mut dyn FnMut(Vec<u8>)) {
    // Splitting at `len` (or `len - 1` for the held-back CR) never lands
    // mid-char: the flushed prefix ends at a UTF-8 boundary by construction.
    let split = pending.len() - usize::from(pending.last() == Some(&b'\r'));
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
    let stdout_thread = child.stdout.take().map(|s| {
        std::thread::spawn(move || {
            drain_fd(
                s,
                out_stop_rx,
                DRAIN_POLL_MS,
                &mut |_| {},
                Some(MAX_TOOL_OUTPUT_BYTES),
            )
        })
    });
    let stderr_thread = child.stderr.take().map(|s| {
        std::thread::spawn(move || {
            drain_fd(
                s,
                err_stop_rx,
                DRAIN_POLL_MS,
                &mut |_| {},
                Some(MAX_TOOL_OUTPUT_BYTES),
            )
        })
    });

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

/// Forward a bounded total of bytes through a streaming channel, appending
/// the shared `...[truncated]` byte-cap marker exactly once when the cap is
/// hit and dropping everything after. The bounded streaming *channel* only
/// bounds in-flight chunks (backpressure); this caps the *total*, so a
/// long-running command cannot push an unbounded live view to the client.
/// The final recorded result is separately truncated by
/// `format_shell_output`, but the streamed view must not diverge from it.
/// The cap accounting itself is the shared [`ByteBudget`] (the same engine
/// `drain_fd` and the VM's guest-WRITE path use), so all streaming paths
/// agree on the "first N bytes + one marker" contract.
struct StreamByteCap {
    budget: ByteBudget,
    tx: crossbeam_channel::Sender<Vec<u8>>,
}

impl StreamByteCap {
    fn new(limit: usize, tx: crossbeam_channel::Sender<Vec<u8>>) -> Self {
        Self {
            budget: ByteBudget::new(limit),
            tx,
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
    fn push(&mut self, chunk: &[u8], out: &mut Vec<u8>) {
        let n = self.budget.fit(chunk.len());
        if n > 0 {
            out.extend_from_slice(&chunk[..n]);
            let _ = self.tx.send(chunk[..n].to_vec());
        }
        if let Some(marker) = self.budget.take_marker() {
            // Same byte-cap marker `truncate_tool_output` appends, so the
            // live view reads exactly like the final (capped) result.
            out.extend_from_slice(marker.as_bytes());
            let _ = self.tx.send(marker.as_bytes().to_vec());
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
/// the per-stream byte cap is 0: the merger thread accumulates the body, so
/// `drain_fd`'s own buffer is kept empty while `on_data` still observes
/// every chunk.
fn spawn_line_drain<R>(
    reader: R,
    stop_rx: mpsc::Receiver<()>,
    merge_tx: crossbeam_channel::Sender<Vec<u8>>,
) -> std::thread::JoinHandle<()>
where
    R: Read + AsFd + Send + 'static,
{
    std::thread::spawn(move || {
        let mut pending: Vec<u8> = Vec::new();
        drain_fd(
            reader,
            stop_rx,
            DRAIN_POLL_MS,
            &mut |chunk: &[u8]| {
                forward_complete_lines(chunk, &mut pending, &mut |line| {
                    let _ = merge_tx.send(line);
                });
            },
            Some(0),
        );
        // Flush any final unterminated line (matches `BufRead::lines()`
        // yielding a last line without a trailing newline).
        if !pending.is_empty() {
            let _ = merge_tx.send(pending);
        }
    })
}

/// Spawn the command with piped stdout/stderr and stream their lines through
/// `output_tx` in real time as the process produces them. Enforces a timeout
/// via watchdog and returns the collected output along with a was-killed
/// flag.
///
/// Applies child-process hardening (`setup_child`) before spawning, pins the
/// child's identity with a pidfd on Linux so a timeout kill can never be
/// redirected at a recycled PID, and bounds both drains so a surviving
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
/// `reservation` is the number of record-framing bytes `format_shell_output`
/// will add around the body (the `$ {cmd}\n` prefix, the exit-code footer,
/// and the truncation marker) — reserved *inside* the stream budget so the
/// final cap is a no-op and the recorded body contains exactly the streamed
/// bytes, even when the stream is truncated. Pass 0 to skip the reservation
/// (direct callers that do not format the output); `run_shell_streaming`
/// computes the right value via [`shell_output_framing_reservation`].
pub fn spawn_with_streaming(
    cmd: &mut Command,
    timeout_ms: u64,
    reservation: usize,
    output_tx: crossbeam_channel::Sender<Vec<u8>>,
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
    // arrival order. The per-stream accumulate cap is 0 because the merger
    // accumulates the body — `drain_fd` still consumes every byte (no pipe
    // deadlock) and calls `on_data` for each chunk.
    let stdout_thread = spawn_line_drain(stdout, out_stop_rx, merge_tx.clone());
    let stderr_thread = spawn_line_drain(stderr, err_stop_rx, merge_tx.clone());

    // The main thread drops its sender so the merge channel disconnects the
    // moment BOTH drain threads finish — that disconnect is what terminates
    // the merger below. Without this drop, the merger would wait forever.
    drop(merge_tx);

    // Thread: consume merged lines in arrival order, forward each through the
    // shared byte cap (the live view), and accumulate the same capped bytes
    // into the body returned to the caller. One budget for both streams keeps
    // the streamed total and the recorded body capped at
    // MAX_TOOL_OUTPUT_BYTES with a single `...[truncated]` marker — the same
    // "first N bytes + one marker" contract as the client's live
    // accumulation, so the recorded result reads exactly like the stream.
    //
    // The budget is reduced by `reservation` (the record framing
    // `format_shell_output` adds — header, footer, marker) so the final cap
    // is a no-op: the recorded body then contains exactly the streamed
    // bytes, truncation marker included, and the exit-code footer always
    // survives (including the transcript re-cap in `record_tool_completion`).
    let merger_thread = std::thread::spawn(move || {
        let mut stream_cap =
            StreamByteCap::new(MAX_TOOL_OUTPUT_BYTES.saturating_sub(reservation), output_tx);
        let mut full: Vec<u8> = Vec::new();
        while let Ok(line) = merge_rx.recv() {
            stream_cap.push(&line, &mut full);
        }
        full
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
    // Stop the drainers once the direct child is reaped (see
    // `spawn_with_watchdog`).
    let _ = out_stop_tx.send(());
    let _ = err_stop_tx.send(());

    // Join the drain threads first so the merge channel disconnects, then
    // the merger — its `recv` loop ends on that disconnect and returns the
    // fully accumulated interleaved body.
    if let Err(e) = stdout_thread.join() {
        warn!("stdout reader thread panicked: {:?}", e);
    }
    if let Err(e) = stderr_thread.join() {
        warn!("stderr reader thread panicked: {:?}", e);
    }
    let body = match merger_thread.join() {
        Ok(buf) => buf,
        Err(e) => {
            warn!("stream merger thread panicked: {:?}", e);
            Vec::new()
        }
    };
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
/// budget: the `$ {display_cmd}\n` prefix, the worst-case `\n\nExit code: N`
/// footer (an i32 exit code is at most 11 chars), and the `...[truncated]`
/// marker twice — once for the stream's own marker (which rides in the
/// recorded body) and once for the room `finish_tool_output` holds back for
/// its generic marker. Reserving them keeps `finish_tool_output`'s cap a
/// no-op, so the recorded result contains exactly the bytes that were
/// streamed — truncation marker included — and the exit-code footer always
/// survives (including the transcript re-cap in `record_tool_completion`).
fn shell_output_framing_reservation(display_cmd: &str) -> usize {
    // `format_shell_output` renders the footer as `\n` + `\nExit code: {code}`;
    // i32::MIN ("-2147483648") is the longest possible exit code.
    const WORST_CASE_FOOTER_LEN: usize = "\n\nExit code: -2147483648".len();
    format!("$ {display_cmd}\n").len() + WORST_CASE_FOOTER_LEN + 2 * TRUNCATION_SUFFIX.len()
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
    let reservation = shell_output_framing_reservation(display_cmd);
    let (output, was_killed) = spawn_with_streaming(cmd, timeout_ms, reservation, output_tx)?;
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
    let mut combined = output.stdout.clone();
    combined.extend_from_slice(&output.stderr);
    let combined_str = String::from_utf8_lossy(&combined);
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
        let got = drain_fd(reader, stop_rx, 0, &mut |_| {}, None);
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
        let got = drain_fd(reader, stop_rx, 0, &mut |_| {}, None);
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

        let got = drain_fd(reader, stop_rx, 0, &mut |_| {}, None);
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
            0,
            &mut |chunk: &[u8]| seen += chunk.len(),
            Some(8 * 1024),
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
        let mut cap = StreamByteCap::new(10, tx);
        let mut out = Vec::new();

        cap.push(b"abcd", &mut out);
        cap.push(b"0123456789", &mut out);
        cap.push(b"xyz", &mut out);

        let streamed: Vec<u8> = rx.try_iter().flatten().collect();
        assert_eq!(
            out, streamed,
            "recorded body must equal the forwarded stream"
        );
        assert_eq!(out, b"abcd012345\n...[truncated]");
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
        t1.join().expect("stdout drain");
        t2.join().expect("stderr drain");
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
}
