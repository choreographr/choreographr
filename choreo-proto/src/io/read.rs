//! The deadline- and stall-bounded blocking read machinery shared by the
//! handshake and the data plane.
//!
//! The mirror image of [`super::write`]: a blocking `read` under a single armed
//! `SO_RCVTIMEO` bounds only that one syscall, so a reader that must finish a
//! fixed-length buffer under a bound re-arms the timeout to the time remaining on
//! each iteration. That loop and the [`TimedRead`] capability that arms it live
//! here once, so no caller hand-rolls a second copy whose budget arithmetic could
//! drift.
//!
//! Two bounds are offered, and they are not interchangeable:
//!
//! * [`read_exact_by`] bounds the **whole** buffer with one absolute deadline. The
//!   handshake uses it: one budget covers its entire unauthenticated phase, so a
//!   peer that dribbles bytes to keep resetting a per-syscall timeout is still cut
//!   off.
//! * [`read_exact_with_stall`] bounds each **syscall** instead: no progress for
//!   `stall` fails the read. The data plane uses it, because it cannot know a
//!   message's total size — a length prefix reveals one fragment at a time — so
//!   there is no whole-message figure to scale a budget from, and because a slow
//!   but alive peer must be allowed to finish however long it takes.
//!
//! There is deliberately no `read_exact_with_budget`: a size-scaled whole-message
//! budget needs the total size, which only a writer has.

use std::io::{self, Read};
use std::net::TcpStream;
use std::time::{Duration, Instant};

use super::UnixStream;
use super::policy::MIN_SOCKET_STALL;

/// A socket that can bound how long a single blocking read syscall may take.
///
/// The setter is `&self` because a socket's read timeout is an OS-level option
/// (`SO_RCVTIMEO`, or the named-pipe equivalent on Windows), not Rust state — a
/// shared handle can re-arm it just as well as an owned one.
pub trait TimedRead: Read {
    /// Arm the per-syscall read timeout (`SO_RCVTIMEO`), or clear it with `None`.
    ///
    /// # Errors
    ///
    /// Propagates the platform's rejection of the requested timeout (the
    /// `setsockopt`/`SetNamedPipeHandleState` failure).
    fn set_read_stall(&self, stall: Option<Duration>) -> std::io::Result<()>;
}

impl TimedRead for TcpStream {
    fn set_read_stall(&self, stall: Option<Duration>) -> std::io::Result<()> {
        self.set_read_timeout(stall)
    }
}

// The unix-socket impl is per-platform for the same reason the [`UnixStream`]
// re-export is: std's Windows `UnixStream` is unstable, so the Windows build
// uses the `uds_windows` named-pipe shim. Both spell the setter
// `set_read_timeout`, so only the `#[cfg]` split differs.
#[cfg(unix)]
impl TimedRead for UnixStream {
    fn set_read_stall(&self, stall: Option<Duration>) -> std::io::Result<()> {
        self.set_read_timeout(stall)
    }
}

#[cfg(windows)]
impl TimedRead for UnixStream {
    fn set_read_stall(&self, stall: Option<Duration>) -> std::io::Result<()> {
        self.set_read_timeout(stall)
    }
}

/// The error a whole-buffer budget expiry returns: a `TimedOut` whose message names
/// the bytes still outstanding, so a caller logging it never has to guess which
/// bound ended the read.
fn read_deadline_elapsed(pending: usize) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        format!("read deadline elapsed with {pending} byte(s) still to read"),
    )
}

/// The error a per-syscall stall expiry returns. Same `TimedOut` shape as
/// [`read_deadline_elapsed`], and named apart from it so a log line says which of
/// the two bounds actually fired.
fn read_stall_elapsed(pending: usize) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        format!("read stall elapsed with {pending} byte(s) still to read"),
    )
}

/// Fill `buf` completely, never taking longer than `deadline` in total.
///
/// Before every `read` syscall the socket's read timeout is re-armed to the time
/// **remaining** until `deadline`, not to the caller's original budget. That is the
/// difference from arming a per-syscall timeout once: such a timeout bounds each
/// individual `read`, so a peer that sends one byte per window keeps resetting it
/// and can hold the reader — a connection thread and a socket FD, before any
/// authentication — open indefinitely. Re-arming to the remaining time makes the
/// total bounded by our own arithmetic: once the deadline passes, `remaining` is
/// zero and the next iteration fails immediately instead of starting another
/// full-budget syscall.
///
/// Mirrors [`Read::read_exact`]'s contract otherwise: an empty `buf` completes
/// without arming anything, EOF before the buffer fills is
/// [`io::ErrorKind::UnexpectedEof`], and an interrupted syscall is retried.
///
/// # Socket state
///
/// This never clears the timeout: on return it stays armed at the last value
/// computed. A caller that later wants an unbounded read on the same socket must
/// clear or re-arm it itself (the data plane does exactly that between messages —
/// see `choreo-transport`'s receive loop).
///
/// `SO_RCVTIMEO` is sticky, so a syscall that would arm the value already armed
/// skips the `setsockopt`. A deadline pieces its bound out of the remaining time,
/// so it shrinks and re-arms essentially every iteration; only a bound that is
/// constant — [`read_exact_with_stall`]'s — collapses to a single arm.
///
/// # Errors
///
/// - [`io::ErrorKind::TimedOut`] when `deadline` has already passed (nothing is
///   read) or when an armed read returns `WouldBlock`/`TimedOut` — the socket is
///   blocking and armed to the remaining budget, so those kinds mean this deadline
///   fired.
/// - [`io::ErrorKind::UnexpectedEof`] when the peer closes before `buf` is full.
/// - Any other error from the reader, propagated unchanged, as is the error from
///   arming the stall.
pub fn read_exact_by<R: TimedRead>(
    reader: &mut R,
    deadline: Instant,
    buf: &mut [u8],
) -> io::Result<()> {
    read_exact_bounded(reader, ReadBound::Deadline(deadline), buf)
}

/// Fill `buf` completely, failing if any single read syscall goes `stall` without
/// progress.
///
/// This is the bound for a reader that cannot know its total length and must not
/// put a wall clock on a legitimate transfer: a peer that is merely slow keeps
/// making progress (each syscall completes within `stall`) however long the whole
/// buffer takes, while a peer that has stopped fails promptly. It is the read-side
/// counterpart of [`super::write_all_with_stall`]'s per-syscall bound, applied —
/// unlike the write side's budget — without any total-size figure.
///
/// Note what "no progress" means: a `read` that fills any part of the buffer
/// resets the clock for the next syscall. A peer pacing one byte per interval
/// therefore satisfies this bound forever, which is why a caller that must bound
/// *total* time as well uses [`read_exact_by`], and why the data plane pairs it
/// with TCP keepalive (a liveness check) rather than a clock.
///
/// `stall` is floored at [`super::MIN_SOCKET_STALL`] before it is armed. The floor
/// is silent: [`super::effective_stall`] is the seam that WARNS about a configured
/// value this low, and this is the backstop for a caller that never passes through
/// it.
///
/// Mirrors [`Read::read_exact`]'s contract otherwise, including an empty `buf`
/// completing without arming anything.
///
/// # Socket state
///
/// As [`read_exact_by`], the timeout is left armed at the last value computed. The
/// bound here is constant, so it is armed ONCE for the whole call rather than
/// before every syscall — `SO_RCVTIMEO` keeps applying to each subsequent read.
///
/// # Errors
///
/// - [`io::ErrorKind::TimedOut`] when a read accepts nothing for `stall`.
/// - [`io::ErrorKind::UnexpectedEof`] when the peer closes before `buf` is full.
/// - Any other error from the reader, propagated unchanged, as is the error from
///   arming the stall.
pub fn read_exact_with_stall<R: TimedRead>(
    reader: &mut R,
    stall: Duration,
    buf: &mut [u8],
) -> io::Result<()> {
    // A socket rejects a zero timeout instead of treating it as unbounded, so a
    // stall that arrives here below the floor is raised rather than armed as-is.
    read_exact_bounded(reader, ReadBound::Stall(stall.max(MIN_SOCKET_STALL)), buf)
}

/// The one time bound a fixed-length read runs under.
///
/// One variant, never both: a caller either knows a total deadline for the whole
/// buffer or cannot know it and bounds each syscall instead. Encoding that as one
/// value rather than two independent `Option`s means "no bound at all" — an
/// unbounded read, the resource leak these helpers exist to prevent — is not
/// expressible, and the error a fired bound produces cannot name the wrong one.
#[derive(Clone, Copy)]
enum ReadBound {
    /// The whole buffer must complete before this instant.
    Deadline(Instant),
    /// Each syscall may wait at most this long for progress.
    Stall(Duration),
}

/// The single bounded-read loop behind [`read_exact_by`] and
/// [`read_exact_with_stall`], so the arming arithmetic cannot drift into two
/// copies.
///
/// Which bound is in force is the [`ReadBound`] the caller handed over: a
/// `Deadline` re-arms to the shrinking remaining time (a total-time bound), a
/// `Stall` re-arms to the constant stall (a no-progress bound).
fn read_exact_bounded<R: TimedRead>(
    reader: &mut R,
    bound: ReadBound,
    buf: &mut [u8],
) -> io::Result<()> {
    // `filled` is the length of the satisfied prefix of `buf`; growing it is what
    // advances the read without ever indexing (which could panic on a bogus
    // cursor) — the tail is taken through a checked `get_mut`.
    let mut filled = 0;
    // The value last armed on the socket, so an unchanged bound skips a redundant
    // `setsockopt`: `SO_RCVTIMEO` is sticky, so it already applies to the next
    // syscall. Only a constant bound (a stall) makes that a single arm; a deadline
    // recomputes to a smaller value and re-arms.
    let mut armed_now: Option<Duration> = None;
    while filled < buf.len() {
        let pending = buf.len() - filled;
        let to_arm = match bound {
            ReadBound::Deadline(deadline) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    tracing::debug!(
                        pending,
                        "read deadline already elapsed; refusing to start another syscall"
                    );
                    return Err(read_deadline_elapsed(pending));
                }
                remaining
            }
            ReadBound::Stall(stall) => stall,
        };
        if armed_now != Some(to_arm) {
            reader.set_read_stall(Some(to_arm))?;
            armed_now = Some(to_arm);
        }

        let Some(tail) = buf.get_mut(filled..) else {
            // Unreachable: the loop condition keeps `filled < buf.len()`, so the
            // range is always in bounds. Reported rather than indexed so a future
            // refactor of the loop cannot turn this into a panic in a connection
            // thread.
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "read cursor escaped the buffer",
            ));
        };

        match reader.read(tail) {
            Ok(0) => {
                return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
            }
            Ok(read) => filled += read,
            // A signal interrupting the syscall says nothing about the bounds, so
            // retry with a freshly computed one.
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            // The socket is blocking and its stall was just armed, so either kind
            // means a bound ran out.
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                tracing::debug!(pending, "armed read bound fired before the buffer was full");
                // The bound IS the attribution: exactly one of the two is armed,
                // so exactly one can have fired.
                return Err(match bound {
                    ReadBound::Deadline(_) => read_deadline_elapsed(pending),
                    ReadBound::Stall(_) => read_stall_elapsed(pending),
                });
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{TimedRead, read_exact_by, read_exact_with_stall};
    use crate::io::policy::MIN_SOCKET_STALL;
    use std::cell::RefCell;
    use std::io::{self, Read};
    use std::time::{Duration, Instant};

    /// What a [`FakeReader`] does on each `read` call: a pure function of the call
    /// index, so a test needs no sockets, timers, or sleeps.
    #[derive(Clone, Copy)]
    enum Behaviour {
        /// Fill the whole buffer in one call.
        AcceptAll,
        /// Fill exactly one byte per call, forcing one syscall per byte.
        AcceptOneByte,
        /// Report EOF immediately — `read_exact`'s `UnexpectedEof` condition.
        Eof,
        /// Fail with `kind` on the first call, then behave like `AcceptAll`.
        ErrorOnFirst(io::ErrorKind),
        /// Fail with `kind` on every call.
        AlwaysError(io::ErrorKind),
    }

    /// A `TimedRead` fake that records the stall it was asked to arm for each
    /// syscall and how many syscalls happened.
    struct FakeReader {
        behaviour: Behaviour,
        stalls: RefCell<Vec<Option<Duration>>>,
        calls: RefCell<usize>,
    }

    impl FakeReader {
        fn new(behaviour: Behaviour) -> Self {
            Self {
                behaviour,
                stalls: RefCell::new(Vec::new()),
                calls: RefCell::new(0),
            }
        }

        fn calls(&self) -> usize {
            *self.calls.borrow()
        }
    }

    impl Read for FakeReader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let call = {
                let mut calls = self.calls.borrow_mut();
                *calls += 1;
                *calls
            };
            match self.behaviour {
                Behaviour::AcceptAll => {
                    buf.fill(0x5A);
                    Ok(buf.len())
                }
                Behaviour::AcceptOneByte => {
                    if let Some(first) = buf.first_mut() {
                        *first = 0x5A;
                        Ok(1)
                    } else {
                        Ok(0)
                    }
                }
                Behaviour::Eof => Ok(0),
                Behaviour::ErrorOnFirst(kind) if call == 1 => {
                    Err(io::Error::new(kind, "fake first-call failure"))
                }
                Behaviour::ErrorOnFirst(_) => {
                    buf.fill(0x5A);
                    Ok(buf.len())
                }
                Behaviour::AlwaysError(kind) => Err(io::Error::new(kind, "fake failure")),
            }
        }
    }

    impl TimedRead for FakeReader {
        fn set_read_stall(&self, stall: Option<Duration>) -> io::Result<()> {
            // `&self` is all a real socket needs (SO_RCVTIMEO lives in the
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

    /// The stall used by the no-progress tests: the value is recorded, never
    /// waited on, so any constant works.
    const STALL: Duration = Duration::from_secs(5);

    #[test]
    fn fills_the_whole_buffer_and_arms_one_stall_per_syscall() {
        let mut reader = FakeReader::new(Behaviour::AcceptAll);
        let mut buf = [0u8; 8];
        read_exact_by(&mut reader, future_deadline(), &mut buf).expect("read");

        assert_eq!(buf, [0x5A; 8]);
        assert_eq!(reader.calls(), 1);
        assert_eq!(reader.stalls.borrow().len(), 1);
    }

    #[test]
    fn an_empty_buffer_reads_nothing_and_arms_no_stall() {
        let mut reader = FakeReader::new(Behaviour::AcceptAll);
        read_exact_by(&mut reader, future_deadline(), &mut []).expect("empty read");

        assert_eq!(reader.calls(), 0);
        assert!(reader.stalls.borrow().is_empty());
    }

    /// One byte per syscall makes the re-arming observable: the arms must be
    /// shrinking budgets, not the caller's original one repeated. The EXACT count
    /// is not pinned, because consecutive `Instant::now()` reads may return the same
    /// value and an unchanged bound is deliberately not re-armed (`SO_RCVTIMEO` is
    /// sticky); the properties below are what the test exists for.
    #[test]
    fn each_syscall_is_rearmed_to_the_time_remaining() {
        let mut reader = FakeReader::new(Behaviour::AcceptOneByte);
        let start = Instant::now();
        let budget = Duration::from_secs(30);
        let mut buf = [0u8; 5];
        read_exact_by(&mut reader, start + budget, &mut buf).expect("read");

        assert_eq!(buf, [0x5A; 5]);
        let stalls = reader.stalls.borrow();
        assert!(
            (1..=5).contains(&stalls.len()),
            "at most one arm per read syscall, at least one for the call: {}",
            stalls.len()
        );

        // Every pause spent here only shrinks what a correct implementation may
        // arm, which is what the lower bound below pins down.
        let remaining_now = (start + budget).saturating_duration_since(Instant::now());
        let mut previous: Option<Duration> = None;
        for stall in stalls.iter() {
            let armed = stall.expect("every read arms a stall");
            assert!(!armed.is_zero(), "an armed stall must leave a budget");
            assert!(
                armed <= budget,
                "armed {armed:?} exceeds the {budget:?} budget"
            );
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
    fn past_deadline_is_timed_out_and_reads_nothing() {
        // A reader that would block proves the early return is what fired, not the
        // socket: no syscall is attempted at all.
        let mut reader = FakeReader::new(Behaviour::AlwaysError(io::ErrorKind::WouldBlock));
        let deadline = Instant::now()
            .checked_sub(Duration::from_secs(1))
            .expect("instant one second ago");

        let mut buf = [0u8; 4];
        let error = read_exact_by(&mut reader, deadline, &mut buf).expect_err("deadline elapsed");

        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(
            error.to_string().contains("read deadline elapsed"),
            "message must name the deadline: {error}"
        );
        assert_eq!(reader.calls(), 0);
        assert!(reader.stalls.borrow().is_empty());
    }

    #[test]
    fn eof_before_the_buffer_fills_is_unexpected_eof() {
        let mut reader = FakeReader::new(Behaviour::Eof);
        let mut buf = [0u8; 4];
        let error = read_exact_by(&mut reader, future_deadline(), &mut buf).expect_err("eof");

        assert_eq!(
            error.kind(),
            io::ErrorKind::UnexpectedEof,
            "EOF before the buffer fills must match read_exact's semantics"
        );
    }

    #[test]
    fn would_block_from_an_armed_read_is_timed_out() {
        let mut reader = FakeReader::new(Behaviour::AlwaysError(io::ErrorKind::WouldBlock));
        let mut buf = [0u8; 4];
        let error = read_exact_by(&mut reader, future_deadline(), &mut buf).expect_err("timed out");

        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(
            error.to_string().contains("read deadline elapsed"),
            "an armed timeout is still the deadline firing: {error}"
        );
    }

    #[test]
    fn an_interrupted_read_is_retried() {
        let mut reader = FakeReader::new(Behaviour::ErrorOnFirst(io::ErrorKind::Interrupted));
        let mut buf = [0u8; 4];
        read_exact_by(&mut reader, future_deadline(), &mut buf).expect("retry succeeds");

        assert_eq!(buf, [0x5A; 4]);
        assert_eq!(reader.calls(), 2, "the interrupted syscall is retried once");
        // The retry re-arms to the FRESHLY computed budget, which for a deadline
        // bound need not differ from the previous one — see
        // `each_syscall_is_rearmed_to_the_time_remaining`.
        assert!(
            (1..=2).contains(&reader.stalls.borrow().len()),
            "the retry is armed at least once"
        );
    }

    #[test]
    fn unrelated_errors_propagate_unchanged() {
        let mut reader = FakeReader::new(Behaviour::AlwaysError(io::ErrorKind::PermissionDenied));
        let mut buf = [0u8; 4];
        let error = read_exact_by(&mut reader, future_deadline(), &mut buf).expect_err("denied");

        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }

    // ── the stall-only bound ──

    /// Every syscall runs under the constant stall, and never under a shrinking
    /// deadline — there is no deadline to shrink. This is the bound the data plane
    /// runs under, where a legitimate transfer may take arbitrarily long. Because
    /// the bound never changes, it is armed exactly ONCE for the call and
    /// `SO_RCVTIMEO` carries it to every later syscall.
    #[test]
    fn a_constant_stall_bound_is_armed_once() {
        let mut reader = FakeReader::new(Behaviour::AcceptOneByte);
        let mut buf = [0u8; 4];
        read_exact_with_stall(&mut reader, STALL, &mut buf).expect("read");

        assert_eq!(buf, [0x5A; 4]);
        assert_eq!(reader.calls(), 4, "one read syscall per byte");
        let stalls = reader.stalls.borrow();
        assert_eq!(
            stalls.len(),
            1,
            "a constant bound is armed once, not per byte"
        );
        assert_eq!(
            stalls.first().copied().flatten(),
            Some(STALL),
            "the single arm is the caller's stall"
        );
    }

    /// A zero stall is raised before it reaches the socket: `std` rejects
    /// `Some(Duration::ZERO)` with `InvalidInput` rather than reading it as
    /// "unbounded", so passing one straight through would fail the first syscall.
    #[test]
    fn a_zero_stall_is_floored_before_it_reaches_the_socket() {
        let mut reader = FakeReader::new(Behaviour::AcceptOneByte);
        let mut buf = [0u8; 2];
        read_exact_with_stall(&mut reader, Duration::ZERO, &mut buf)
            .expect("the floor leaves a usable bound");

        assert_eq!(buf, [0x5A; 2]);
        let stalls = reader.stalls.borrow();
        assert_eq!(
            stalls.first().copied().flatten(),
            Some(MIN_SOCKET_STALL),
            "a zero stall must be raised to the socket minimum"
        );
    }

    /// A peer that accepts nothing for the stall fails the read, and the error
    /// names the **stall** rather than a deadline the caller never set — that
    /// distinction is what tells an operator whether a slow link or a wedged peer
    /// ended the connection.
    #[test]
    fn a_stalled_read_is_named_as_a_stall_not_a_deadline() {
        let mut reader = FakeReader::new(Behaviour::AlwaysError(io::ErrorKind::WouldBlock));
        let mut buf = [0u8; 4];

        let error = read_exact_with_stall(&mut reader, STALL, &mut buf).expect_err("stalled");

        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(
            error.to_string().contains("read stall elapsed"),
            "message must name the stall: {error}"
        );
        assert!(
            !error.to_string().contains("deadline"),
            "the stall-only bound must never blame a deadline: {error}"
        );
    }

    /// Progress resets the clock: a peer that fills one byte per syscall passes
    /// however small the stall is, because each syscall completes. That is the
    /// property that lets an arbitrarily slow link finish. The stall used is the
    /// smallest a socket accepts, so this is also the tightest legal bound.
    #[test]
    fn a_slow_but_progressing_read_passes_a_tiny_stall() {
        let mut reader = FakeReader::new(Behaviour::AcceptOneByte);
        let mut buf = [0u8; 8];
        read_exact_with_stall(&mut reader, MIN_SOCKET_STALL, &mut buf)
            .expect("every syscall makes progress, so the stall never fires");

        assert_eq!(buf, [0x5A; 8]);
        assert_eq!(reader.calls(), 8);
    }

    #[test]
    fn the_stall_bound_still_reports_eof() {
        let mut reader = FakeReader::new(Behaviour::Eof);
        let mut buf = [0u8; 4];
        let error = read_exact_with_stall(&mut reader, STALL, &mut buf).expect_err("eof");

        assert_eq!(
            error.kind(),
            io::ErrorKind::UnexpectedEof,
            "a closed peer is EOF, not a stall"
        );
    }
}
