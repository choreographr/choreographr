//! The connection's ONE time policy, applied in both directions.
//!
//! Three numbers decide when a connection gives up on its peer, and every
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
        CONNECTION_STALL, MIN_SOCKET_STALL, WRITER_MIN_RATE_BYTES_PER_SEC, effective_stall,
        message_budget,
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
}
