//! The handshake's absolute-deadline I/O plumbing.
//!
//! Every handshake read and write goes through the two helpers here, so all
//! four Noise flows are bounded by the same rule stated once: each syscall is
//! limited to the time *remaining* until one absolute deadline, which a peer
//! that dribbles bytes one per read window cannot stretch.
//!
//! The socket timeout alone is not enough. `SO_RCVTIMEO` bounds a single
//! `recv`, so a peer that sends one byte per window keeps resetting it and can
//! hold a connection thread and socket FD open indefinitely — before any
//! authentication has happened, on a listener that accepts from the network.
//! Re-arming the timeout to `deadline - now` before every call makes the total
//! duration bounded no matter how the peer paces its bytes.

use std::io::{self, Read};
use std::net::TcpStream;
use std::time::Instant;

use crate::error::TransportError;

/// Read exactly `buf.len()` bytes from `stream`, never taking longer than
/// `deadline` in total.
///
/// A bare `read_exact` under `SO_RCVTIMEO` only bounds each individual `recv`:
/// a peer that dribbles one byte per read window could stretch the handshake
/// out indefinitely. Here every read is limited to the time *remaining* until
/// `deadline` (the socket timeout is re-armed before each call), so the total
/// duration is bounded by `deadline` plus scheduler slop no matter how the
/// peer paces its bytes. EOF before the buffer fills is `UnexpectedEof`,
/// matching `read_exact`'s semantics.
///
/// Fills a caller-provided buffer rather than returning a `Vec`, so a whole
/// handshake performs no heap allocation.
pub(super) fn read_handshake_exact(
    stream: &mut TcpStream,
    deadline: Instant,
    buf: &mut [u8],
) -> Result<(), TransportError> {
    let mut filled = 0;
    while filled < buf.len() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(TransportError::HandshakeTimeout);
        }
        // Bound THIS read by the remaining budget so a slow peer cannot
        // stretch the total past `deadline` by dribbling bytes.
        stream.set_read_timeout(Some(remaining))?;
        let n = match stream.read(crate::buf::slice_mut(buf, filled.., "handshake read tail")?) {
            Ok(n) => n,
            // The read timeout was armed to exactly the time remaining until
            // `deadline`, and the socket is BLOCKING (never non-blocking), so
            // a WouldBlock/TimedOut can only mean the deadline fired — a slow
            // peer cannot otherwise produce them. Surface it as the honest
            // HandshakeTimeout variant rather than leaking a raw
            // io::Error::WouldBlock that callers cannot distinguish from a
            // genuine non-blocking-mode condition.
            Err(e)
                if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::TimedOut =>
            {
                return Err(TransportError::HandshakeTimeout);
            }
            Err(e) => return Err(e.into()),
        };
        if n == 0 {
            return Err(io::Error::from(io::ErrorKind::UnexpectedEof).into());
        }
        filled += n;
    }
    Ok(())
}

/// Write exactly `data` to `stream`, never taking longer than `deadline` in
/// total. Mirrors [`read_handshake_exact`]: the socket write timeout is
/// re-armed to the time remaining until `deadline` before every write, so a
/// peer that stops reading mid-handshake cannot hold the writer past the
/// deadline (in practice handshake messages are a few hundred bytes and fit
/// the socket buffer, so this bound is a backstop, not the normal path).
///
/// The re-arm-per-syscall loop itself lives in [`choreo_proto::write_all_by`],
/// shared with the data plane, so the handshake and the message path bound a
/// write one way rather than each carrying its own copy. Only the handshake's
/// error mapping differs.
pub(super) fn write_handshake_all(
    stream: &mut TcpStream,
    deadline: Instant,
    data: &[u8],
) -> Result<(), TransportError> {
    // The socket is blocking and armed to the remaining budget, so a
    // WouldBlock/TimedOut means the deadline fired — surface it as the honest
    // HandshakeTimeout rather than leaking a raw io::Error that callers cannot
    // distinguish from a genuine non-blocking-mode condition. WriteZero (a
    // writer accepting no bytes) is what `write_all_by` returns and flows through
    // the generic arm carrying its own kind, matching `write_all`'s semantics.
    match choreo_proto::write_all_by(stream, deadline, data) {
        Ok(()) => Ok(()),
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
            ) =>
        {
            Err(TransportError::HandshakeTimeout)
        }
        Err(e) => Err(e.into()),
    }
}
