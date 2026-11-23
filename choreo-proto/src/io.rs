use crate::ProtoError;
use crate::frame::{MAX_FRAME_SIZE, decode_frame, encode_frame};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::time::{Duration, Instant};

/// Environment variable that overrides the default unix-socket path; when set
/// (and non-empty) its value is returned verbatim by [`socket_path`], so the
/// daemon and every client resolve the same path from one place.
pub const SOCKET_PATH_ENV: &str = "CHOREOGRAPHR_SOCKET_PATH";

// The unix-socket stream type, resolved per platform: std's `UnixStream` on
// unix, the `uds_windows` named-pipe shim on Windows (std's Windows
// `UnixStream` is still unstable — feature `windows_unix_domain_sockets`,
// rust-lang/rust#150487). Re-exported here so every dial site in the
// workspace names the SAME type; the `#[cfg]` split lives in exactly one
// place instead of being re-inlined at each call site.
#[cfg(unix)]
pub use std::os::unix::net::UnixStream;
#[cfg(windows)]
pub use uds_windows::UnixStream;

/// The default Unix-socket path when `CHOREOGRAPHR_SOCKET_PATH` is unset.
///
/// The whole chain is owned by `choreo_shared::paths::default_socket_path`, and
/// this function only stringifies it: under a base dir
/// `{base}/run/choreographr.sock`; otherwise the XDG runtime directory,
/// `$XDG_RUNTIME_DIR/choreographr.sock` (the spec's home for sockets); and when
/// no runtime dir is available — macOS/Windows, or a bare environment — a
/// per-uid private directory under the PLATFORM temp dir
/// (`temp_dir()/choreographr-<uid>/choreographr.sock`, TMPDIR-aware, so
/// Android/Termux lands in the app's writable prefix tmp dir instead of a
/// hardcoded `/tmp` that yields EACCES).
///
/// Delegating rather than re-deriving the fallback here is deliberate: the
/// daemon and every client must resolve the SAME path, and a second copy of the
/// chain is how the two drift apart.
#[must_use]
pub fn default_socket_path() -> String {
    choreo_shared::paths::default_socket_path()
        .to_string_lossy()
        .into_owned()
}

/// Returns the configured socket path, honoring `CHOREOGRAPHR_SOCKET_PATH`.
#[must_use]
pub fn socket_path() -> String {
    socket_path_impl(|| std::env::var(SOCKET_PATH_ENV).ok())
}

pub(crate) fn socket_path_impl(get_env: impl Fn() -> Option<String>) -> String {
    get_env().unwrap_or_else(default_socket_path)
}

/// Dial the unix-domain socket at `path`, returning the connected stream.
///
/// This is the ONE cross-platform dial primitive: std's `UnixStream` on unix,
/// the `uds_windows` shim on Windows (see the [`UnixStream`] re-export).
/// Re-exporting both the type and the dial keeps every socket-probe site in
/// the workspace — the TUI autostart wait, the client connection path, and the
/// daemon's stale-socket probe — on the exact same mechanism, so their
/// classifications cannot drift.
///
/// # Errors
///
/// Propagates the dial error (e.g. `NotFound` when the socket file does not
/// exist, `PermissionDenied` on a path the user cannot reach).
pub fn connect_unix(path: &str) -> std::io::Result<UnixStream> {
    UnixStream::connect(path)
}

/// Whether something is currently accepting connections at the unix-domain
/// socket `path`: a successful dial proves a live listener. The boolean
/// convenience over [`connect_unix`] for callers that only need the yes/no
/// answer (the TUI's "wait for our spawned child" poll and the daemon's
/// stale-socket probe).
#[must_use]
pub fn socket_listening(path: &str) -> bool {
    connect_unix(path).is_ok()
}

/// Whether a unix-socket DIAL failure means "nothing is listening at the
/// path" — the only condition a daemon autostart (or a stale-socket cleanup)
/// is allowed to act on:
///
/// - `NotFound` — the socket file does not exist at all (no daemon has ever
///   run, or the socket file was removed).
/// - `ConnectionRefused` — the socket file exists but nothing is bound to it
///   (a stale leftover from a crashed daemon, which the daemon unlinks at
///   startup).
///
/// Every OTHER dial failure (`PermissionDenied`, a wedged listener timing out
/// on the backlog, …) means a live-or-unknown daemon state that autostart
/// cannot fix, so the error must be surfaced verbatim instead of spawning a
/// second daemon.
///
/// Takes the WHOLE error (not its `kind`) so a caller cannot accidentally pass
/// an unrelated error's kind; the classification reads `error.kind()`
/// internally.
///
/// This predicate lives in `choreo-proto` (not in the client or the daemon)
/// because BOTH sides classify dials and they must never disagree: the TUI
/// connection path (`choreo-client-core`'s
/// `run_daemon_connection_with_autostart`) uses it to decide when to fire the
/// autostart hook, and the daemon's `remove_stale_socket` uses the same
/// connect-probe shape to decide whether an existing socket file is safe to
/// unlink. A platform whose dial error for "no listener" ever changes (e.g.
/// Windows returning an unexpected kind through `uds_windows`) then needs a
/// one-line fix here, not two divergent `matches!` on opposite sides of the
/// wire.
#[must_use]
pub fn dial_error_means_no_listener(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
    )
}

/// A socket that can bound how long a single blocking write syscall may take.
///
/// The setter is `&self` because a socket's write timeout is an OS-level
/// option (`SO_SNDTIMEO`, or the named-pipe equivalent on Windows), not Rust
/// state — a shared handle can re-arm it just as well as an owned one.
pub trait TimedWrite: Write {
    /// Arm the per-syscall write timeout (`SO_SNDTIMEO`), or clear it with
    /// `None`.
    ///
    /// # Errors
    ///
    /// Propagates the platform's rejection of the requested timeout (the
    /// `setsockopt`/`SetNamedPipeHandleState` failure).
    fn set_write_stall(&self, stall: Option<Duration>) -> std::io::Result<()>;
}

impl TimedWrite for std::net::TcpStream {
    fn set_write_stall(&self, stall: Option<Duration>) -> std::io::Result<()> {
        self.set_write_timeout(stall)
    }
}

// The unix-socket impl is per-platform for the same reason the [`UnixStream`]
// re-export is: std's Windows `UnixStream` is unstable, so the Windows build
// uses the `uds_windows` named-pipe shim. Both spell the setter
// `set_write_timeout`, so only the `#[cfg]` split differs.
#[cfg(unix)]
impl TimedWrite for UnixStream {
    fn set_write_stall(&self, stall: Option<Duration>) -> std::io::Result<()> {
        self.set_write_timeout(stall)
    }
}

#[cfg(windows)]
impl TimedWrite for UnixStream {
    fn set_write_stall(&self, stall: Option<Duration>) -> std::io::Result<()> {
        self.set_write_timeout(stall)
    }
}

/// The error a total-budget expiry returns: a `TimedOut` whose message names
/// the elapsed write deadline, so a caller logging the error never has to
/// guess which bound fired.
fn write_deadline_elapsed(pending: usize) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        format!("write deadline elapsed with {pending} byte(s) still to send"),
    )
}

/// The error a per-syscall stall expiry returns. Same `TimedOut` shape as
/// [`write_deadline_elapsed`], and named apart from it so a log line says
/// which of the two bounds actually fired.
fn write_stall_elapsed(pending: usize) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        format!("write stall elapsed with {pending} byte(s) still to send"),
    )
}

/// Write all of `data`, with the whole write bounded by `deadline`.
///
/// Before every `write` syscall this re-arms the socket's write stall to the
/// time **remaining** until `deadline` (`deadline.saturating_duration_since(
/// now)`), not to the caller's original budget. That is the difference from
/// arming a per-syscall timeout once: such a timeout bounds each individual
/// `write`, so pushing one message takes up to `number_of_syscalls × timeout`,
/// and how closely a single `write` honours its timeout depends on the kernel's
/// wakeup policy. Re-arming to the remaining time makes the total bounded by
/// `deadline` in our own arithmetic: once the deadline passes, `remaining` is
/// zero and the next iteration fails immediately instead of starting another
/// full-budget syscall.
///
/// `remaining` is therefore legitimately large on the first syscall (up to the
/// caller's whole budget) and shrinks as bytes are accepted — that shrinking
/// value is what keeps the total bounded.
///
/// An empty `data` slice returns `Ok(())` without arming or writing anything.
///
/// # Socket state
///
/// The function never clears the stall: on return the socket's write stall
/// stays armed at the last value it computed. Clearing it here would discard a
/// bound the caller deliberately chose, so a caller that later shares the
/// socket with its own untimed write must re-arm that write's policy itself.
///
/// # Errors
///
/// - [`std::io::ErrorKind::TimedOut`] when the deadline has already passed
///   (nothing is written) or when an armed write returns `WouldBlock`/
///   `TimedOut` — the socket is blocking and armed to the remaining budget, so
///   those kinds mean this deadline fired.
/// - [`std::io::ErrorKind::WriteZero`] when the writer accepts no bytes, which
///   mirrors [`Write::write_all`]'s treatment of a zero-length write.
/// - Any other error from the writer, propagated unchanged, as is the error
///   from arming the stall.
pub fn write_all_by<W: TimedWrite>(
    writer: &mut W,
    deadline: Instant,
    data: &[u8],
) -> std::io::Result<()> {
    write_bounded(writer, deadline, None, data)
}

/// Write all of `data`, bounded BOTH by one absolute `deadline` for the whole
/// write and by `stall` for any single syscall's wait for progress.
///
/// The two bounds protect against different failures, which is why a
/// long-lived connection needs both at once:
///
/// - `deadline` bounds the **total** time pushing one message may take. A large
///   message legitimately takes longer than a small one on a slow link, so a
///   fixed per-syscall timeout alone would either kill the large message or be
///   uselessly long for the small one.
/// - `stall` bounds the **detection latency** of a peer that has stopped
///   accepting bytes altogether. [`write_all_by`]'s single budget cannot do
///   that: it arms every syscall to the whole remaining budget, so a silent peer
///   is only noticed when the entire message budget expires — minutes, for a
///   large frame with a size-scaled deadline.
///
/// Each syscall is therefore armed to `min(deadline, Instant::now() + stall) -
/// now`, computed fresh before it runs. A peer that accepts nothing for `stall`
/// fails fast; a slow-but-alive peer keeps making progress until `deadline`.
///
/// An empty `data` slice returns `Ok(())` without arming or writing anything.
///
/// # Socket state
///
/// As [`write_all_by`], the socket's write stall is left armed at the last value
/// computed and never cleared.
///
/// # Errors
///
/// - [`std::io::ErrorKind::TimedOut`] when `deadline` has already passed
///   (nothing is written), when an armed syscall returns `WouldBlock`/
///   `TimedOut` — the message names whether the `stall` or the `deadline` fired.
/// - [`std::io::ErrorKind::WriteZero`] when the writer accepts no bytes, which
///   mirrors [`Write::write_all`]'s treatment of a zero-length write.
/// - Any other error from the writer, propagated unchanged, as is the error
///   from arming the stall.
pub fn write_all_with_stall<W: TimedWrite>(
    writer: &mut W,
    deadline: Instant,
    stall: Duration,
    data: &[u8],
) -> std::io::Result<()> {
    write_bounded(writer, deadline, Some(stall), data)
}

/// The single write loop behind [`write_all_by`] and [`write_all_with_stall`],
/// so the two-bounds policy cannot drift into two copies.
///
/// `stall` is the optional per-syscall cap: `None` arms every syscall to
/// whatever remains until `deadline` (the total-budget-only policy), `Some`
/// arms it to the tighter of the two. `deadline` bounds the write either way.
fn write_bounded<W: TimedWrite>(
    writer: &mut W,
    deadline: Instant,
    stall: Option<Duration>,
    data: &[u8],
) -> std::io::Result<()> {
    // `pending` is the not-yet-accepted tail of `data`; shrinking it by the
    // accepted byte count is what advances the write without ever indexing
    // (which could panic on a bogus return value).
    let mut pending = data;
    while !pending.is_empty() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            tracing::debug!(
                pending = pending.len(),
                "write deadline already elapsed; refusing to start another syscall"
            );
            return Err(write_deadline_elapsed(pending.len()));
        }

        // The budget for THIS syscall: the per-syscall stall when the caller
        // asked for one and it is tighter than the whole-message budget, the
        // time left until `deadline` otherwise. Recomputing it every iteration
        // is what keeps the total bounded by `deadline` — the arm shrinks as
        // bytes are accepted.
        let armed = stall.map_or(remaining, |stall| remaining.min(stall));
        writer.set_write_stall(Some(armed))?;

        match writer.write(pending) {
            // `Ok(0)` from a non-empty buffer means no forward progress is
            // possible: the same condition `Write::write_all` reports as
            // `WriteZero`.
            Ok(0) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WriteZero,
                    "failed to write whole buffer",
                ));
            }
            Ok(written) => {
                pending = match pending.split_at_checked(written) {
                    Some((_, rest)) => rest,
                    // A writer claiming more bytes than it was handed breaks
                    // the `Write` contract; treat it as corrupt rather than
                    // silently reporting success.
                    None => {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "writer reported more bytes than it was given",
                        ));
                    }
                };
            }
            // A signal interrupting the syscall says nothing about the
            // deadline, so retry the same write with a freshly computed budget.
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            // The socket is blocking and its stall was just armed to the
            // remaining budget, so either kind means that budget ran out.
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                tracing::debug!(
                    pending = pending.len(),
                    "armed write stall fired before the message was complete"
                );
                // Which bound burnt: with the whole-message budget still open,
                // the per-syscall stall is the one that fired (the writer
                // accepted nothing for `stall`). Once the deadline has passed
                // the two coincide, and the deadline is the honest name.
                let deadline_open = !deadline.saturating_duration_since(Instant::now()).is_zero();
                return Err(if stall.is_some() && deadline_open {
                    write_stall_elapsed(pending.len())
                } else {
                    write_deadline_elapsed(pending.len())
                });
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// Encode `message` and write the framed payload to `writer`.
///
/// # Errors
///
/// Fails with `ProtoError` when serialization fails or the underlying writer
/// rejects the frame bytes.
pub fn write_message<W, T>(writer: &mut W, message: &T) -> Result<(), ProtoError>
where
    W: Write,
    T: Serialize,
{
    let frame = encode_frame(message)?;
    writer.write_all(&frame)?;
    Ok(())
}

/// Read one length-prefixed frame from `reader` and deserialize it as `T`.
///
/// # Errors
///
/// Fails with `ProtoError` when the frame exceeds [`MAX_FRAME_SIZE`] or the
/// payload fails to deserialize into `T`.
pub fn read_message<R, T>(reader: &mut R) -> Result<T, ProtoError>
where
    R: Read,
    T: for<'de> Deserialize<'de>,
{
    let payload = read_payload(reader)?;
    decode_frame(&payload)
}

/// Read a single length-prefixed frame's raw payload bytes.
///
/// # Errors
///
/// Fails with `ProtoError` when the length header declares a payload larger
/// than [`MAX_FRAME_SIZE`] (`FrameTooLarge`), or when I/O on `reader` fails.
pub fn read_payload<R>(reader: &mut R) -> Result<Vec<u8>, ProtoError>
where
    R: Read,
{
    let mut len_buf = [0_u8; 4];
    reader.read_exact(&mut len_buf)?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_FRAME_SIZE {
        return Err(ProtoError::FrameTooLarge);
    }

    let mut payload = vec![0_u8; len];
    reader.read_exact(&mut payload)?;
    Ok(payload)
}
#[cfg(test)]
mod tests {
    use super::{TimedWrite, write_all_by, write_all_with_stall};
    use std::cell::RefCell;
    use std::io::{self, Write};
    use std::time::{Duration, Instant};

    /// What a [`FakeWriter`] does on each `write` call. Every variant is a pure
    /// function of the call index, so a test needs no sockets, timers, or
    /// sleeps to exercise a `write_all_by` branch.
    #[derive(Clone, Copy)]
    enum Behaviour {
        /// Accept the whole buffer in one call.
        AcceptAll,
        /// Accept exactly one byte per call, forcing one syscall per byte.
        AcceptOneByte,
        /// Accept nothing — the `Write::write_all` `WriteZero` condition.
        WriteZero,
        /// Fail with `kind` on the first call, then behave like `AcceptAll`.
        ErrorOnFirst(io::ErrorKind),
        /// Fail with `kind` on every call.
        AlwaysError(io::ErrorKind),
    }

    /// A `TimedWrite` fake that records the bytes it accepted, the stall it was
    /// asked to arm for each syscall, and how many syscalls happened.
    struct FakeWriter {
        behaviour: Behaviour,
        written: RefCell<Vec<u8>>,
        stalls: RefCell<Vec<Option<Duration>>>,
        calls: RefCell<usize>,
    }

    impl FakeWriter {
        fn new(behaviour: Behaviour) -> Self {
            Self {
                behaviour,
                written: RefCell::new(Vec::new()),
                stalls: RefCell::new(Vec::new()),
                calls: RefCell::new(0),
            }
        }

        fn calls(&self) -> usize {
            *self.calls.borrow()
        }

        fn written(&self) -> Vec<u8> {
            self.written.borrow().clone()
        }
    }

    impl Write for FakeWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            // Count before deciding, so `ErrorOnFirst` can key off the call
            // index rather than mutable state a test has to reach into.
            let call = {
                let mut calls = self.calls.borrow_mut();
                *calls += 1;
                *calls
            };
            match self.behaviour {
                Behaviour::AcceptAll => {
                    self.written.borrow_mut().extend_from_slice(buf);
                    Ok(buf.len())
                }
                Behaviour::AcceptOneByte => {
                    let accepted = buf.get(..1).unwrap_or(buf);
                    self.written.borrow_mut().extend_from_slice(accepted);
                    Ok(accepted.len())
                }
                Behaviour::WriteZero => Ok(0),
                Behaviour::ErrorOnFirst(kind) if call == 1 => {
                    Err(io::Error::new(kind, "fake first-call failure"))
                }
                Behaviour::ErrorOnFirst(_) => {
                    self.written.borrow_mut().extend_from_slice(buf);
                    Ok(buf.len())
                }
                Behaviour::AlwaysError(kind) => Err(io::Error::new(kind, "fake failure")),
            }
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl TimedWrite for FakeWriter {
        fn set_write_stall(&self, stall: Option<Duration>) -> io::Result<()> {
            // `&self` is all a real socket needs (SO_SNDTIMEO lives in the
            // kernel), so the recorder goes behind a `RefCell`.
            self.stalls.borrow_mut().push(stall);
            Ok(())
        }
    }

    /// A deadline comfortably in the future, so no iteration can find a zero
    /// remaining budget.
    fn future_deadline() -> Instant {
        Instant::now() + Duration::from_secs(30)
    }

    #[test]
    fn writes_every_byte_and_arms_one_stall_per_syscall() {
        let mut writer = FakeWriter::new(Behaviour::AcceptAll);
        write_all_by(&mut writer, future_deadline(), b"hello world").expect("write");

        assert_eq!(writer.written(), b"hello world".to_vec());
        assert_eq!(writer.calls(), 1);
        assert_eq!(writer.stalls.borrow().len(), 1);
    }

    #[test]
    fn empty_data_writes_nothing_and_arms_no_stall() {
        let mut writer = FakeWriter::new(Behaviour::AcceptAll);
        write_all_by(&mut writer, future_deadline(), b"").expect("empty write");

        assert_eq!(writer.written(), Vec::<u8>::new());
        assert_eq!(writer.calls(), 0);
        assert!(writer.stalls.borrow().is_empty());
    }

    #[test]
    fn each_syscall_is_rearmed_to_the_time_remaining() {
        // One byte per syscall makes the re-arming observable: a "arm once"
        // implementation would record a single stall, and one that re-armed
        // the caller's whole budget every time would record values equal to
        // the budget rather than shrinking ones.
        let mut writer = FakeWriter::new(Behaviour::AcceptOneByte);
        let start = Instant::now();
        let budget = Duration::from_secs(30);
        write_all_by(&mut writer, start + budget, b"abcde").expect("write");

        assert_eq!(writer.written(), b"abcde".to_vec());
        let stalls = writer.stalls.borrow();
        assert_eq!(stalls.len(), 5, "one stall armed per write syscall");

        // Every pause spent here only shrinks what a correct implementation
        // may arm, which is what the lower bound below pins down.
        let remaining_now = (start + budget).saturating_duration_since(Instant::now());
        let mut previous: Option<Duration> = None;
        for stall in stalls.iter() {
            let armed = stall.expect("every write arms a stall");
            assert!(!armed.is_zero(), "an armed stall must leave a budget");
            assert!(
                armed <= budget,
                "armed {armed:?} exceeds the {budget:?} budget"
            );
            // The cleared-or-shrinking budget is the property that makes the
            // total, not one syscall, bounded by the deadline.
            assert!(
                armed >= remaining_now,
                "armed {armed:?} is below the time still remaining ({remaining_now:?})"
            );
            if let Some(previous) = previous {
                assert!(
                    armed <= previous,
                    "stall grew from {previous:?} to {armed:?}; it must shrink"
                );
            }
            previous = Some(armed);
        }
    }

    #[test]
    fn past_deadline_is_timed_out_and_writes_nothing() {
        // A writer that would block proves the early return is what fired, not
        // the socket: no syscall is attempted at all.
        let mut writer = FakeWriter::new(Behaviour::AlwaysError(io::ErrorKind::WouldBlock));
        let deadline = Instant::now()
            .checked_sub(Duration::from_secs(1))
            .expect("instant one second ago");

        let error = write_all_by(&mut writer, deadline, b"payload").expect_err("deadline elapsed");

        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(
            error.to_string().contains("write deadline elapsed"),
            "message must name the deadline: {error}"
        );
        assert_eq!(writer.calls(), 0);
        assert_eq!(writer.written(), Vec::<u8>::new());
        assert!(writer.stalls.borrow().is_empty());
    }

    #[test]
    fn would_block_from_an_armed_write_is_timed_out() {
        let mut writer = FakeWriter::new(Behaviour::AlwaysError(io::ErrorKind::WouldBlock));

        let error =
            write_all_by(&mut writer, future_deadline(), b"payload").expect_err("timed out");

        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert_eq!(writer.written(), Vec::<u8>::new());
    }

    #[test]
    fn a_zero_length_write_is_write_zero() {
        let mut writer = FakeWriter::new(Behaviour::WriteZero);

        let error =
            write_all_by(&mut writer, future_deadline(), b"payload").expect_err("write zero");

        assert_eq!(error.kind(), io::ErrorKind::WriteZero);
        assert_eq!(writer.calls(), 1);
        assert_eq!(writer.written(), Vec::<u8>::new());
    }

    #[test]
    fn an_interrupted_write_is_retried() {
        let mut writer = FakeWriter::new(Behaviour::ErrorOnFirst(io::ErrorKind::Interrupted));

        write_all_by(&mut writer, future_deadline(), b"payload").expect("retry succeeds");

        assert_eq!(writer.written(), b"payload".to_vec());
        assert_eq!(writer.calls(), 2, "the interrupted syscall is retried once");
        assert_eq!(writer.stalls.borrow().len(), 2, "the retry is re-armed");
    }

    #[test]
    fn unrelated_errors_propagate_unchanged() {
        let mut writer = FakeWriter::new(Behaviour::AlwaysError(io::ErrorKind::PermissionDenied));

        let error = write_all_by(&mut writer, future_deadline(), b"payload").expect_err("denied");

        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn a_short_stall_caps_every_syscall_below_the_whole_message_deadline() {
        // The deadline is 30 s away, so an implementation that armed the whole
        // remaining budget would record ~30 s per syscall; the short stall is
        // what each arm must be capped at instead.
        let stall = Duration::from_millis(50);
        let budget = Duration::from_secs(30);
        let mut writer = FakeWriter::new(Behaviour::AcceptOneByte);

        write_all_with_stall(&mut writer, future_deadline(), stall, b"abcde").expect("write");

        assert_eq!(writer.written(), b"abcde".to_vec());
        let stalls = writer.stalls.borrow();
        assert_eq!(stalls.len(), 5, "one stall armed per write syscall");
        for stall_armed in stalls.iter() {
            let armed = stall_armed.expect("every write arms a stall");
            assert!(!armed.is_zero(), "an armed stall must leave a budget");
            assert!(
                armed <= stall,
                "armed {armed:?} exceeds the {stall:?} stall (budget was {budget:?})"
            );
        }
    }

    #[test]
    fn a_long_stall_leaves_the_deadline_as_the_binding_bound() {
        // A stall far longer than the whole-message budget can never be the
        // binding bound: every arm must be at or under the time still remaining
        // until the deadline, and it must shrink as bytes are accepted.
        let stall = Duration::from_secs(30);
        let budget = Duration::from_millis(500);
        let start = Instant::now();
        let mut writer = FakeWriter::new(Behaviour::AcceptOneByte);

        write_all_with_stall(&mut writer, start + budget, stall, b"abcde").expect("write");

        assert_eq!(writer.written(), b"abcde".to_vec());
        let stalls = writer.stalls.borrow();
        assert_eq!(stalls.len(), 5, "one stall armed per write syscall");
        let mut previous: Option<Duration> = None;
        for stall_armed in stalls.iter() {
            let armed = stall_armed.expect("every write arms a stall");
            assert!(
                armed < stall,
                "armed {armed:?} reached the {stall:?} stall instead of the deadline"
            );
            assert!(
                armed <= budget,
                "armed {armed:?} exceeds the {budget:?} whole-message budget"
            );
            if let Some(previous) = previous {
                assert!(
                    armed <= previous,
                    "stall grew from {previous:?} to {armed:?}; it must shrink"
                );
            }
            previous = Some(armed);
        }
    }

    #[test]
    fn an_expired_stall_is_named_as_a_stall_not_a_deadline() {
        // A writer that never accepts anything, with the deadline still open:
        // the per-syscall stall is the bound that fired, and the error must say
        // so rather than blaming the whole-message budget.
        let mut writer = FakeWriter::new(Behaviour::AlwaysError(io::ErrorKind::WouldBlock));

        let error = write_all_with_stall(
            &mut writer,
            future_deadline(),
            Duration::from_millis(50),
            b"payload",
        )
        .expect_err("stalled out");

        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(
            error.to_string().contains("write stall elapsed"),
            "message must name the stall: {error}"
        );
    }

    #[test]
    fn a_stall_policy_still_honours_an_already_elapsed_deadline() {
        let mut writer = FakeWriter::new(Behaviour::AlwaysError(io::ErrorKind::WouldBlock));
        let deadline = Instant::now()
            .checked_sub(Duration::from_secs(1))
            .expect("instant one second ago");

        let error = write_all_with_stall(&mut writer, deadline, Duration::from_secs(1), b"payload")
            .expect_err("deadline elapsed");

        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(
            error.to_string().contains("write deadline elapsed"),
            "the total budget is the bound that fired: {error}"
        );
        assert_eq!(writer.calls(), 0);
    }
}
