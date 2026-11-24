//! The connection's ONE time policy, applied in both directions.
//!
//! These numbers decide when a connection gives up on its peer, and every
//! party — the daemon and the client, the reader and the writer — must agree on
//! them, so they live here rather than beside either bounded-I/O loop or in
//! either caller:
//!
//! * [`CONNECTION_STALL`] is the per-syscall **detection latency**: a peer that
//!   accepts (or supplies) nothing for this long has stopped, so the syscall
//!   fails, the caller closes the transport, and the connection tears down. It
//!   is deliberately **not** a writer-only bound — the read side applies the
//!   same latency once a frame is in flight, where a reader knows a message is
//!   due but not how long its remainder will take.
//! * [`WRITER_MIN_RATE_BYTES_PER_SEC`] is the floor throughput a writer must
//!   sustain, and [`message_budget`] turns it into the whole-message **total**
//!   budget for one outbound message, scaled by that message's size so a large
//!   frame on a slow-but-alive link still finishes. There is no ceiling: the
//!   floor rate is what bounds how slow a link may be.
//! * [`MIN_SOCKET_STALL`] is the floor every configured stall is validated
//!   against (through [`effective_stall`]), so a zero can never disable either
//!   bound: `std` rejects a zero socket timeout rather than reading it as
//!   "unbounded", which would break the first syscall instead of unbounding it.
//! * [`writer_join_grace`] derives how long a teardown waits for a connection's
//!   writer thread: the stall **plus** [`WRITER_JOIN_GRACE_MARGIN`], so the wait
//!   outlasts the one blocking syscall the writer may be sitting inside.
//!
//! This module is numbers and arithmetic only — it performs no I/O. The loops
//! that apply the policy are [`super::read`] and [`super::write`]; taking the
//! bounds from here is what keeps neither side of a connection re-deriving its
//! own.

use std::time::Duration;

use tracing::warn;

/// How long a socket syscall may wait for progress before the peer is treated as
/// wedged.
///
/// This is the **detection latency** bound of a connection, in both directions:
/// it is handed to [`crate::write_all_with_stall`] for a write and to the bounded
/// read helper (see [`crate::read_exact_by`]'s module) for a read. A peer that
/// accepts (or supplies) nothing for this long has stopped, so the call fails,
/// the caller closes the transport, and the connection tears down. It is
/// deliberately independent of the message size — a silent peer is noticed in
/// five seconds whether the message is one kilobyte or one mebibyte — and
/// independent of the link speed, because a slow-but-alive peer is always making
/// *some* progress.
///
/// The forward-looking name (`CONNECTION_`, not `WRITER_`) is deliberate: the
/// rate-derived budget below is a writer-only concept (a reader cannot know a
/// message's total size until it has all of it), but the stall is not.
pub const CONNECTION_STALL: Duration = Duration::from_secs(5);

/// Floor throughput a writer must sustain to a peer for a message to be worth
/// finishing, in bytes per second.
///
/// A slow-but-alive link is allowed to take longer for a bigger message, so the
/// per-message budget scales with the frame size at this rate instead of being
/// one fixed number that would false-kill a large legitimate frame.
pub const WRITER_MIN_RATE_BYTES_PER_SEC: u64 = 8 * 1024;

/// The absolute budget for writing one message of `wire_len` bytes: the message's
/// own size divided by the floor rate, floored at [`CONNECTION_STALL`] and
/// **never capped**.
///
/// The size-scaled part is what lets a large frame finish on a link that is merely
/// slow. The [`CONNECTION_STALL`] floor is a jitter allowance, not a throughput
/// bound: a small message on a link that is slow but alive still completes each
/// syscall promptly, and the floor is what absorbs a retransmit or a scheduling gap
/// without failing the write. A silent peer is caught by the stall instead,
/// regardless of size.
///
/// There is deliberately no upper bound. A time ceiling and "support the slowest
/// link we accept for the largest message we can send" are contradictory: 64 MiB at
/// [`WRITER_MIN_RATE_BYTES_PER_SEC`] legitimately takes 8192 s, so a ceiling below
/// that silently re-imposes a much higher minimum link speed (a 120 s cap demands
/// ~4.4 Mbps for the same frame) and a ceiling above it never fires. The bound a
/// peer is actually held to is the floor rate; one that cannot sustain it trips the
/// per-syscall stall instead. The cost is that a large transfer on an absurdly slow
/// link can hold its writer, and its socket, for hours — which the connection's
/// teardown tolerates by detaching the writer rather than joining it unboundedly.
///
/// Integer arithmetic throughout, with saturating operations: a 64 MiB frame
/// must not overflow the millisecond computation, and no input may panic.
#[must_use]
pub fn message_budget(wire_len: usize) -> Duration {
    // `usize` is 64-bit on every target we ship, but converting rather than
    // casting keeps a 128-bit-`usize` future (or a narrower one) from silently
    // truncating; saturation is unreachable in practice and cannot panic.
    let bytes = u64::try_from(wire_len).unwrap_or(u64::MAX);
    // bytes / rate seconds, in milli-seconds, so a sub-second frame keeps its
    // precision instead of rounding down to zero.
    let millis = bytes
        .saturating_mul(1000)
        .saturating_div(WRITER_MIN_RATE_BYTES_PER_SEC);
    Duration::from_millis(millis).max(CONNECTION_STALL)
}

/// Margin a writer-teardown grace adds on top of the connection's stall.
///
/// The stall bounds a single socket syscall, so a grace has to exceed it to leave
/// the writer any room to finish exiting once that syscall returns; one second is
/// the headroom, and a larger margin would only lengthen the teardown of a
/// genuinely wedged writer.
pub const WRITER_JOIN_GRACE_MARGIN: Duration = Duration::from_secs(1);

/// How long a teardown waits for a connection's writer thread before detaching it:
/// `stall` plus [`WRITER_JOIN_GRACE_MARGIN`].
///
/// The grace must outlast the stall, never merely match it, and that ordering is
/// load-bearing rather than cautious: the wait covers a writer that may be inside a
/// blocking syscall for up to `stall`, so a grace equal to the stall would expire in
/// exactly the worst case the wait exists to cover, detaching a writer that was
/// about to exit cleanly. A healthy writer exits at once on its stop signal (or when
/// its channel closes), so the grace is only ever spent on a wedged one.
///
/// The wait has to cover the stall **in force**, which is why this is a function of
/// the stall rather than one constant: a caller running its connection under a
/// smaller stall derives a correspondingly smaller grace, where a hard-coded
/// default would wait out a stall that connection never had. [`WRITER_JOIN_GRACE`]
/// is the default-stall form of it.
///
/// Saturating, so no caller can panic by passing a huge stall: a stall within
/// [`WRITER_JOIN_GRACE_MARGIN`] of [`Duration::MAX`] yields [`Duration::MAX`], which
/// is already the longest wait a `Duration` can express.
#[must_use]
pub const fn writer_join_grace(stall: Duration) -> Duration {
    stall.saturating_add(WRITER_JOIN_GRACE_MARGIN)
}

/// The grace for a connection running the default stall: [`writer_join_grace`]
/// applied to [`CONNECTION_STALL`].
///
/// This is what a teardown joins its writer under when it has no stall of its own
/// to derive from — both ends of a connection bound the wait with it rather than
/// each computing the relation again.
pub const WRITER_JOIN_GRACE: Duration = writer_join_grace(CONNECTION_STALL);

/// Smallest stall a socket accepts.
///
/// `std` rejects a zero duration outright (`ErrorKind::InvalidInput`, "cannot set
/// a 0 duration timeout") rather than reading it as "no timeout", which is the
/// opposite of what a `setsockopt`-era reader expects from `SO_RCVTIMEO`/
/// `SO_SNDTIMEO` = 0. A zero reaching a socket would therefore fail the very first
/// syscall and tear the connection down; this floor turns that into a
/// one-millisecond bound instead. One millisecond is the smallest value the kernel
/// reliably honours (a smaller timeout rounds up to it anyway).
pub const MIN_SOCKET_STALL: Duration = Duration::from_millis(1);

/// Validate a configured connection stall, flooring it at [`MIN_SOCKET_STALL`].
///
/// The single seam every stall passes through on its way to a socket timeout, in
/// EITHER direction, so neither reader nor writer can arm a zero. A config that
/// resolves to zero is a mistake rather than a request to disable the bound — the
/// bound is what reaps a wedged peer — so it is floored and warned about rather
/// than propagated.
#[must_use]
pub fn effective_stall(configured: Duration) -> Duration {
    if configured < MIN_SOCKET_STALL {
        warn!(
            configured = ?configured,
            floor = ?MIN_SOCKET_STALL,
            "connection stall is below the socket's minimum; flooring it (a zero timeout is rejected by the kernel, not read as unbounded)"
        );
        return MIN_SOCKET_STALL;
    }
    configured
}

#[cfg(test)]
mod tests {
    use super::{
        CONNECTION_STALL, MIN_SOCKET_STALL, WRITER_JOIN_GRACE, WRITER_MIN_RATE_BYTES_PER_SEC,
        effective_stall, message_budget, writer_join_grace,
    };
    use std::time::Duration;

    /// A small frame cannot finish in less than the stall, so its budget is
    /// exactly [`CONNECTION_STALL`].
    #[test]
    fn a_small_frame_budgets_exactly_the_writer_stall() {
        assert_eq!(message_budget(64), CONNECTION_STALL);
        assert_eq!(message_budget(0), CONNECTION_STALL);
        // 20 KiB at the floor rate is 2.5 s — still below the stall.
        assert_eq!(message_budget(20 * 1024), CONNECTION_STALL);
    }

    /// Above the boundary where the rate-derived duration overtakes the stall,
    /// the budget is the rate-derived one: a big frame on a slow link is allowed
    /// the time its size implies.
    #[test]
    fn a_frame_past_the_rate_boundary_gets_the_rate_derived_budget() {
        // 48 KiB at 8 KiB/s is 6 s, past the 5 s stall.
        let wire_len = 48 * 1024;
        let expected = Duration::from_millis(
            u64::try_from(wire_len).expect("fits u64") * 1000 / WRITER_MIN_RATE_BYTES_PER_SEC,
        );
        assert!(
            expected > CONNECTION_STALL,
            "the boundary case must exceed it"
        );
        assert_eq!(message_budget(wire_len), expected);
    }

    /// A 64 MiB frame — the codec's maximum — must not overflow the millisecond
    /// computation, and gets the full rate-derived budget rather than a clamped
    /// one: supporting the largest message on the slowest link we accept is exactly
    /// what the floor rate means, so there is no ceiling to hit.
    #[test]
    fn a_64_mib_frame_gets_the_full_rate_derived_budget() {
        let wire_len = 64 * 1024 * 1024;
        let expected = Duration::from_millis(
            u64::try_from(wire_len).expect("fits u64") * 1000 / WRITER_MIN_RATE_BYTES_PER_SEC,
        );
        assert_eq!(
            expected,
            Duration::from_secs(8192),
            "64 MiB at the 8 KiB/s floor is 2.3 hours"
        );
        assert_eq!(message_budget(wire_len), expected);
        assert!(
            message_budget(wire_len) > CONNECTION_STALL,
            "a large frame's budget must dominate the jitter floor"
        );
    }

    /// A zero (or sub-millisecond) stall never reaches a socket timeout: `std`
    /// rejects `Some(Duration::ZERO)` with `InvalidInput` rather than reading it as
    /// "unbounded", so without the floor the very first syscall would fail and tear
    /// the connection down. The floor keeps a mistake a slow bound instead of a
    /// dead connection.
    #[test]
    fn a_sub_millisecond_stall_is_floored_to_the_socket_minimum() {
        for configured in [
            Duration::ZERO,
            Duration::from_nanos(1),
            Duration::from_micros(500),
            Duration::from_millis(1) - Duration::from_nanos(1),
        ] {
            assert_eq!(
                effective_stall(configured),
                MIN_SOCKET_STALL,
                "{configured:?} must floor to the one-millisecond minimum"
            );
        }
    }

    /// At or above the floor the configured value is used verbatim, which is what
    /// lets a test substitute its own tiny-but-legal stall instead of waiting out
    /// the production default.
    #[test]
    fn a_stall_at_or_above_the_floor_is_returned_verbatim() {
        assert_eq!(effective_stall(MIN_SOCKET_STALL), MIN_SOCKET_STALL);
        assert_eq!(
            effective_stall(Duration::from_millis(200)),
            Duration::from_millis(200)
        );
    }

    /// The production stall is itself above the socket floor, so the connection's
    /// default detection latency survives validation untouched.
    #[test]
    fn the_connection_stall_survives_the_floor() {
        assert_eq!(effective_stall(CONNECTION_STALL), CONNECTION_STALL);
    }

    /// The grace a teardown joins its writer under is derived from the stall it has
    /// to outlast, so for every stall a connection can actually run under the grace
    /// is STRICTLY greater — the property an equal grace would break in exactly the
    /// worst case the wait exists to cover.
    #[test]
    fn the_writer_join_grace_outlasts_the_stall_it_is_derived_from() {
        for stall in [
            MIN_SOCKET_STALL,
            CONNECTION_STALL,
            Duration::from_secs(3600),
        ] {
            assert!(
                writer_join_grace(stall) > stall,
                "the grace for {stall:?} must outlast it, not merely match it"
            );
        }
    }

    /// A stall with no room left for the margin saturates to the longest wait a
    /// `Duration` can express rather than panicking or wrapping: there is nothing
    /// longer to return, and no input may crash a teardown.
    #[test]
    fn a_stall_at_the_top_of_the_range_saturates_instead_of_panicking() {
        assert_eq!(writer_join_grace(Duration::MAX), Duration::MAX);
        assert_eq!(
            writer_join_grace(Duration::MAX.saturating_sub(Duration::from_nanos(1))),
            Duration::MAX,
            "a stall one nanosecond under the ceiling still saturates"
        );
    }

    /// The exported default is exactly the helper applied to the default stall, so
    /// a caller that hard-codes nothing and a caller that derives from
    /// [`CONNECTION_STALL`] cannot disagree.
    #[test]
    fn the_default_grace_is_the_helper_applied_to_the_default_stall() {
        assert_eq!(WRITER_JOIN_GRACE, writer_join_grace(CONNECTION_STALL));
        assert!(
            WRITER_JOIN_GRACE > CONNECTION_STALL,
            "the default grace must outlast the default stall"
        );
    }
}
