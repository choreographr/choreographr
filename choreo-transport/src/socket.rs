//! TCP socket policy for the client↔daemon control plane.
//!
//! [`tune_control_socket`] applies the two socket options a control-plane
//! connection needs — Nagle disabled and TCP keepalive — in one place, so every
//! connection gets both whichever side dialed it and whatever else the caller
//! tuned beforehand.
//!
//! # Nagle (`TCP_NODELAY`)
//!
//! The Noise control plane carries small, message-oriented traffic — streaming
//! deltas, rapid control messages, the handshake's request/response round trips —
//! so Nagle's "hold a small write until the previous one is acknowledged" rule,
//! interacting with the peer's delayed ACK, would stall each small write for tens
//! of milliseconds. That is invisible in bulk transfers (full segments are never
//! delayed) but shows up as visible stutter in streamed output and added latency
//! in interactive round trips, so the option is turned off for this stream.
//!
//! # Keepalive
//!
//! Keepalive is a **liveness** check, not an activity timeout, and that
//! distinction is the whole reason it is the right bound here:
//!
//! * A probe is only sent when the connection has been idle in both directions,
//!   so an active transfer — however slow — never triggers one, and a
//!   slow-but-alive peer is never judged dead.
//! * An idle-but-alive peer answers the probe from its kernel, so a session left
//!   open overnight survives (and, because the probes traverse the path, the
//!   NAT mapping stays warm instead of expiring under it).
//! * A peer that has vanished (a phone that slept, a network that changed) stops
//!   answering and is reaped after [`SocketTuning`]'s idle-plus-probe budget, which
//!   is what releases the daemon's connection slot and what keeps the channel's
//!   `TCP_USER_TIMEOUT` from letting a write dribble into a black hole.
//!
//! Both ends carry it. The daemon needs it to reclaim the slot a vanished client
//! is holding; the client needs it because a vanished daemon otherwise leaves its
//! reader blocked with no `ReaderClosed` to exit on.
//!
//! The numbers are [`SocketTuning`]'s defaults, unchanged. They are deliberately
//! biased toward *keeping an idle connection alive* over minimising probe
//! traffic: a dropped idle connection is visible (a front-end exits with an error
//! — there is no automatic reconnect), whereas probes are just some mobile-radio
//! wakeups. If a front-end ever gains automatic reconnection, that trade flips
//! and a lazy profile (a minutes-long idle) becomes defensible.
//!
//! The tuning is deliberately best-effort: `set_nodelay` on an already-connected
//! TCP socket effectively cannot fail, and a latency or liveness nicety must never
//! abort a connection that otherwise works — so a failure is logged and swallowed
//! rather than returned. `SocketTuning::apply_stream` reports only the fatal
//! `SO_KEEPALIVE` case, which on a freshly connected stream is effectively
//! impossible in the first place.
//!
//! Unix-domain sockets (the daemon's local transport) have no Nagle behaviour to
//! disable and no keepalive (the peer's death closes the socket, which the reader
//! sees as EOF), and the HTTP provider/tool sockets already run `NODELAY` through
//! `ureq`'s default `no_delay`, so this helper is applied only to the TCP/Noise
//! control-plane sockets.

use std::net::TcpStream;

use choreo_sockreg::SocketTuning;

/// Apply the control-plane policy to a TCP socket: `TCP_NODELAY` plus TCP
/// keepalive.
///
/// Called at every point a control-plane TCP socket is established or handed on:
/// before the Noise handshake on both sides (so the handshake's own small round
/// trips are not delayed and keepalive is armed for its whole life), and again
/// when the handshake hands the socket to the data plane (the intrinsic
/// guarantee — see [`crate::handshake`]'s `finish`). Re-applying an already-set
/// option is idempotent and costs one syscall.
///
/// Failure is non-fatal on either count: it is logged at `warn` and the
/// connection continues.
pub fn tune_control_socket(stream: &TcpStream) {
    if let Err(e) = stream.set_nodelay(true) {
        tracing::warn!(error = %e, "could not set TCP_NODELAY on control-plane socket");
    }
    // Best-effort for the reason the module docs give: `apply_stream` reports only
    // the fatal `SO_KEEPALIVE` case, and losing keepalive costs a slower reap of a
    // vanished peer — not the connection.
    if let Err(e) = SocketTuning.apply_stream(stream) {
        tracing::warn!(error = %e, "could not apply TCP keepalive to control-plane socket");
    }
}
