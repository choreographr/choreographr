//! TCP socket policy for the client↔daemon control plane.
//!
//! [`tune_control_socket`] disables Nagle's algorithm (`TCP_NODELAY`) on a
//! control-plane `TcpStream`. The Noise control plane carries small,
//! message-oriented traffic — streaming deltas, rapid control messages, the
//! handshake's request/response round trips — so Nagle's "hold a small write
//! until the previous one is acknowledged" rule, interacting with the peer's
//! delayed ACK, would stall each small write for tens of milliseconds. That is
//! invisible in bulk transfers (full segments are never delayed) but shows up as
//! visible stutter in streamed output and added latency in interactive round
//! trips, so the option is turned off for this stream.
//!
//! The tuning is deliberately best-effort: `set_nodelay` on an already-connected
//! TCP socket effectively cannot fail, and a latency nicety must never abort a
//! connection that otherwise works — so a failure is logged and swallowed rather
//! than returned. This mirrors the keepalive tuning's "a tuning nicety is not
//! worth failing the connection" policy.
//!
//! Unix-domain sockets (the daemon's local transport) have no Nagle behaviour to
//! disable, and the HTTP provider/tool sockets already run `NODELAY` through
//! `ureq`'s default `no_delay`, so this helper is applied only to the TCP/Noise
//! control-plane sockets.

use std::net::TcpStream;

/// Disable Nagle's algorithm on a control-plane TCP socket (`TCP_NODELAY`).
///
/// Called at two points on every client↔daemon TCP connection: before the Noise
/// handshake (so the handshake's own small round trips are not delayed), and
/// again when the handshake hands the socket to the data plane (the intrinsic
/// guarantee — see [`crate::handshake`]'s `finish`). Re-applying an already-set
/// option is idempotent and costs one syscall.
///
/// Failure is non-fatal: it is logged at `warn` and the connection continues.
pub fn tune_control_socket(stream: &TcpStream) {
    if let Err(e) = stream.set_nodelay(true) {
        tracing::warn!(error = %e, "could not set TCP_NODELAY on control-plane socket");
    }
}
