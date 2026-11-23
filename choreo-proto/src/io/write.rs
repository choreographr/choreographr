//! The deadline- and stall-bounded blocking write machinery shared by the
//! handshake and every data-plane writer.
//!
//! A blocking `write` under a single armed `SO_SNDTIMEO` bounds only that one
//! syscall; a caller that needs the WHOLE write bounded (a message, a handshake
//! frame) re-arms the stall to the time remaining on each iteration. That loop,
//! the [`TimedWrite`] capability that arms it, and the two bounds it honours
//! live here once so the handshake and the data plane cannot drift apart.

use std::io::Write;
use std::time::{Duration, Instant};

use super::UnixStream;

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
