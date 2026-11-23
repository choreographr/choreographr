//! The deadline-bounded blocking read machinery shared by the handshake and any
//! future bounded reader.
//!
//! The mirror image of [`super::write`]: a blocking `read` under a single armed
//! `SO_RCVTIMEO` bounds only that one syscall, so a reader that must finish a
//! FIXED-LENGTH buffer within an absolute budget re-arms the timeout to the time
//! remaining on each iteration. That loop and the [`TimedRead`] capability that
//! arms it live here once, so no caller hand-rolls a second copy whose budget
//! arithmetic could drift.
//!
//! There is deliberately no `read_exact_with_stall` counterpart to
//! [`super::write::write_all_with_stall`]: the two bounds exist on the write side
//! because a long-lived connection must distinguish "large frame on a slow but
//! alive link" from "peer stopped accepting bytes", and only a writer has a
//! message-sized queue to push. A bounded reader has no equivalent — the one
//! caller is the handshake, which bounds the whole unauthenticated phase with one
//! absolute deadline (see `choreo-transport`'s handshake `deadline` module), and
//! the data plane is untimed by design (readers block until a message or EOF, and
//! shutdown closes the socket to unblock them).

use std::io::Read;
use std::net::TcpStream;
use std::time::{Duration, Instant};

use super::UnixStream;

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

/// The error a total-budget expiry returns: a `TimedOut` whose message names the
/// bytes still outstanding, so a caller logging it never has to guess whether the
/// budget or the peer ended the read.
fn read_deadline_elapsed(pending: usize) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        format!("read deadline elapsed with {pending} byte(s) still to read"),
    )
}

/// Fill `buf` completely, never taking longer than `deadline` in total.
///
/// Before every `read` syscall this re-arms the socket's read timeout to the time
/// **remaining** until `deadline`, not to the caller's original budget. That is
/// the difference from arming a per-syscall timeout once: such a timeout bounds
/// each individual `read`, so a peer that sends one byte per window keeps
/// resetting it and can hold the reader — a connection thread and a socket FD,
/// before any authentication — open indefinitely. Re-arming to the remaining time
/// makes the total bounded by our own arithmetic: once the deadline passes,
/// `remaining` is zero and the next iteration fails immediately instead of
/// starting another full-budget syscall.
///
/// Mirrors [`std::io::Read::read_exact`]'s contract otherwise: an empty `buf`
/// completes without arming anything, EOF before the buffer fills is
/// [`std::io::ErrorKind::UnexpectedEof`], and an interrupted syscall is retried.
///
/// # Socket state
///
/// The function never clears the stall: on return the socket's read timeout stays
/// armed at the last value it computed, so a caller that hands the socket to a
/// later untimed read must clear or re-arm it itself.
///
/// # Errors
///
/// - [`std::io::ErrorKind::TimedOut`] when `deadline` has already passed (nothing
///   is read) or when an armed read returns `WouldBlock`/`TimedOut` — the socket
///   is blocking and armed to the remaining budget, so those kinds mean this
///   deadline fired.
/// - [`std::io::ErrorKind::UnexpectedEof`] when the peer closes before `buf` is
///   full.
/// - Any other error from the reader, propagated unchanged, as is the error from
///   arming the stall.
pub fn read_exact_by<R: TimedRead>(
    reader: &mut R,
    deadline: Instant,
    buf: &mut [u8],
) -> std::io::Result<()> {
    // `filled` is the length of the satisfied prefix of `buf`; growing it is what
    // advances the read without ever indexing (which could panic on a bogus
    // cursor) — the tail is taken through a checked `get_mut`.
    let mut filled = 0;
    while filled < buf.len() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            tracing::debug!(
                pending = buf.len() - filled,
                "read deadline already elapsed; refusing to start another syscall"
            );
            return Err(read_deadline_elapsed(buf.len() - filled));
        }

        // Bound THIS read by the remaining budget so a peer that dribbles bytes
        // cannot stretch the total past `deadline`.
        reader.set_read_stall(Some(remaining))?;

        let Some(tail) = buf.get_mut(filled..) else {
            // Unreachable: the loop condition keeps `filled < buf.len()`, so the
            // range is always in bounds. Reported rather than indexed so a future
            // refactor of the loop cannot turn this into a panic in a connection
            // thread.
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "read cursor escaped the buffer",
            ));
        };

        match reader.read(tail) {
            Ok(0) => {
                return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof));
            }
            Ok(read) => filled += read,
            // A signal interrupting the syscall says nothing about the deadline,
            // so retry with a freshly computed budget.
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            // The socket is blocking and its stall was just armed to the remaining
            // budget, so either kind means that budget ran out.
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                tracing::debug!(
                    pending = buf.len() - filled,
                    "armed read stall fired before the buffer was full"
                );
                return Err(read_deadline_elapsed(buf.len() - filled));
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{TimedRead, read_exact_by};
    use std::cell::RefCell;
    use std::io::{self, Read};
    use std::time::{Duration, Instant};

    /// What a [`FakeReader`] does on each `read` call: a pure function of the
    /// call index, so a test needs no sockets, timers, or sleeps.
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

    /// One byte per syscall makes the re-arming observable: a "arm once"
    /// implementation would record a single stall, and one that re-armed the
    /// caller's whole budget each time would record values equal to the budget
    /// rather than shrinking ones.
    #[test]
    fn each_syscall_is_rearmed_to_the_time_remaining() {
        let mut reader = FakeReader::new(Behaviour::AcceptOneByte);
        let start = Instant::now();
        let budget = Duration::from_secs(30);
        let mut buf = [0u8; 5];
        read_exact_by(&mut reader, start + budget, &mut buf).expect("read");

        assert_eq!(buf, [0x5A; 5]);
        let stalls = reader.stalls.borrow();
        assert_eq!(stalls.len(), 5, "one stall armed per read syscall");

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
        assert_eq!(reader.stalls.borrow().len(), 2, "the retry is re-armed");
    }

    #[test]
    fn unrelated_errors_propagate_unchanged() {
        let mut reader = FakeReader::new(Behaviour::AlwaysError(io::ErrorKind::PermissionDenied));
        let mut buf = [0u8; 4];
        let error = read_exact_by(&mut reader, future_deadline(), &mut buf).expect_err("denied");

        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }
}
