//! The handshake's framing and deadline plumbing.
//!
//! One shared read path and one shared write path serve all four Noise flows
//! in the sibling `ik` and `xx` modules, so the 2-byte big-endian length
//! prefix, the bounded read and the coalesced bounded write exist exactly
//! once.
//!
//! Every handshake read and write is bounded by an ABSOLUTE deadline: a
//! silent or dribbling unauthenticated peer must not be able to hold a
//! connection thread and socket FD open, which a per-read socket timeout
//! alone does not prevent (a peer can dribble one byte per read window
//! forever). The socket timeout is therefore re-armed before every individual
//! read and write to the time remaining until the deadline.

use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::slice;
use std::time::Instant;

use snow::HandshakeState;

use crate::error::TransportError;

/// Largest handshake frame body accepted from a peer, in bytes.
///
/// The 2-byte length prefix is **unauthenticated** and arrives before any key
/// material exists, so without a cap a peer could declare 65535 bytes and make
/// the responder allocate — and then block reading — that much before it has
/// authenticated anything. 1 KiB is generous: with the empty payloads these
/// patterns use, the largest handshake message is on the order of 96 bytes
/// (32-byte key material plus the 16-byte GCM tag), so no legitimate peer
/// comes close. A larger declaration is rejected by
/// [`declared_frame_len`] before any allocation happens.
pub(super) const MAX_HANDSHAKE_FRAME: usize = 1024;

/// Length of the big-endian handshake length prefix that precedes every
/// handshake frame body.
pub(super) const LEN_PREFIX_LEN: usize = 2;

/// Size of a handshake frame scratch buffer: [`LEN_PREFIX_LEN`] bytes of
/// length-prefix headroom followed by room for the largest body a peer may
/// declare ([`MAX_HANDSHAKE_FRAME`]). Snow encodes the body into the headroom
/// tail and the prefix is laid into the headroom, so one buffer carries the
/// whole frame.
pub(super) const HANDSHAKE_FRAME_BUF_LEN: usize = LEN_PREFIX_LEN + MAX_HANDSHAKE_FRAME;

/// The crate's protocol error for a broken internal buffer-size invariant.
fn invalid_slice(what: &str) -> TransportError {
    TransportError::InvalidFragment(format!("internal buffer size invariant violated ({what})"))
}

/// Decode a 2-byte big-endian length prefix.
fn be_len16(len_buf: &[u8]) -> Result<u16, TransportError> {
    len_buf
        .get(..LEN_PREFIX_LEN)
        .and_then(|prefix| <&[u8] as TryInto<[u8; 2]>>::try_into(prefix).ok())
        .map(u16::from_be_bytes)
        .ok_or_else(|| invalid_slice("length prefix"))
}

/// Encode a body length as its 2-byte big-endian prefix.
///
/// Handshake frame bodies are snow messages (snow caps every message at 65535
/// bytes) and [`MAX_HANDSHAKE_FRAME`] bounds every body we send well below
/// that, but `n` is measured as `usize`, so the wire's 16-bit prefix needs a
/// cast. Truncation is impossible at these sizes, and keeping the plain cast
/// (not `try_from`) preserves the exact byte behavior of the handshakes, which
/// round-trip tests pin down.
#[expect(clippy::cast_possible_truncation)]
fn len16(n: usize) -> [u8; 2] {
    (n as u16).to_be_bytes()
}

/// Decode a length prefix and validate it against [`MAX_HANDSHAKE_FRAME`].
///
/// This is the single place a peer-declared handshake body length is trusted,
/// and it runs before anything is allocated for the body: an unauthenticated
/// peer that declares a frame above the cap is rejected with
/// [`TransportError::InvalidFragment`] instead of making the responder
/// allocate an arbitrary buffer.
fn declared_frame_len(len_buf: &[u8]) -> Result<usize, TransportError> {
    let declared = usize::from(be_len16(len_buf)?);
    if declared > MAX_HANDSHAKE_FRAME {
        return Err(TransportError::InvalidFragment(format!(
            "handshake frame length {declared} exceeds the {MAX_HANDSHAKE_FRAME}-byte cap"
        )));
    }
    Ok(declared)
}

pub(crate) fn at<'a>(
    buf: &'a [u8],
    i: usize,
    what: &'static str,
) -> Result<&'a u8, TransportError> {
    buf.get(i).ok_or_else(|| invalid_slice(what))
}

pub(crate) fn at_mut<'a>(
    buf: &'a mut [u8],
    i: usize,
    what: &'static str,
) -> Result<&'a mut u8, TransportError> {
    buf.get_mut(i).ok_or_else(|| invalid_slice(what))
}

/// `&buf[range]` composed with the bounds check: `get` returns `None` only if
/// an internal buffer-size invariant broke (e.g. the handshake frame buffers,
/// or the worst-case reusable Noise frame buffers), which is turned into the
/// crate's protocol error instead of a panic, in line with the workspace's
/// no-panic lint policy.
pub(crate) fn slice<'a, S>(
    buf: &'a [u8],
    range: S,
    what: &'static str,
) -> Result<&'a [u8], TransportError>
where
    S: slice::SliceIndex<[u8], Output = [u8]>,
{
    buf.get(range).ok_or_else(|| invalid_slice(what))
}

/// Mutable counterpart of [`slice`].
pub(crate) fn slice_mut<'a, S>(
    buf: &'a mut [u8],
    range: S,
    what: &'static str,
) -> Result<&'a mut [u8], TransportError>
where
    S: slice::SliceIndex<[u8], Output = [u8]>,
{
    buf.get_mut(range).ok_or_else(|| invalid_slice(what))
}

/// `&buf[start..]` variant of [`slice`] for `RangeFrom` slices.
pub(crate) fn slice_from<'a>(
    buf: &'a [u8],
    start: usize,
    what: &'static str,
) -> Result<&'a [u8], TransportError> {
    buf.get(start..).ok_or_else(|| invalid_slice(what))
}

/// Read exactly `len` bytes from `stream`, never taking longer than
/// `deadline` in total.
///
/// A bare `read_exact` under `SO_RCVTIMEO` only bounds each individual `recv`:
/// a peer that dribbles one byte per read window could stretch the handshake
/// out indefinitely. Here every read is limited to the time *remaining* until
/// `deadline` (the socket timeout is re-armed before each call), so the total
/// duration is bounded by `deadline` plus scheduler slop no matter how the
/// peer paces its bytes. EOF before `len` bytes is `UnexpectedEof`, matching
/// `read_exact`'s semantics.
pub(super) fn read_handshake_exact(
    stream: &mut TcpStream,
    deadline: Instant,
    len: usize,
) -> Result<Vec<u8>, TransportError> {
    let mut buf = Vec::with_capacity(len);
    // Stack scratch buffer reused across reads — no per-read allocation. A
    // handshake message is at most a few hundred bytes, so 1 KiB is ample.
    let mut scratch = [0u8; MAX_HANDSHAKE_FRAME];
    while buf.len() < len {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(TransportError::HandshakeTimeout);
        }
        // Bound THIS read by the remaining budget so a slow peer cannot
        // stretch the total past `deadline` by dribbling bytes.
        stream.set_read_timeout(Some(remaining))?;
        let want = (len - buf.len()).min(scratch.len());
        // `want <= scratch.len()` by the `min` above, and the read returns
        // `n <= want`, so both accesses are in bounds; bounds-checked anyway
        // (see the slice helpers).
        let n = match stream.read(slice_mut(&mut scratch, ..want, "scratch")?) {
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
        buf.extend_from_slice(slice(&scratch, ..n, "scratch read prefix")?);
    }
    Ok(buf)
}

/// Write exactly `data` to `stream`, never taking longer than `deadline` in
/// total. Mirrors [`read_handshake_exact`]: the socket write timeout is
/// re-armed to the time remaining until `deadline` before every write, so a
/// peer that stops reading mid-handshake cannot hold the writer past the
/// deadline (in practice handshake messages are a few hundred bytes and fit
/// the socket buffer, so this bound is a backstop, not the normal path).
fn write_handshake_all(
    stream: &mut TcpStream,
    deadline: Instant,
    data: &[u8],
) -> Result<(), TransportError> {
    let mut written = 0;
    while written < data.len() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(TransportError::HandshakeTimeout);
        }
        // Bound THIS write by the remaining budget, exactly like the read
        // side: a peer that stops draining the socket (or vanishes) must not
        // be able to hold the handshake past `deadline`.
        stream.set_write_timeout(Some(remaining))?;
        // `written < data.len()` holds from the loop guard above, so the tail
        // access is in bounds; bounds-checked anyway (see the slice helpers).
        match stream.write(slice_from(data, written, "data tail")?) {
            Ok(0) => return Err(io::Error::from(io::ErrorKind::WriteZero).into()),
            Ok(n) => written += n,
            // Same argument as read_handshake_exact: the socket is blocking
            // and the write timeout is armed to the remaining budget, so a
            // WouldBlock/TimedOut means the deadline fired.
            Err(e)
                if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::TimedOut =>
            {
                return Err(TransportError::HandshakeTimeout);
            }
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

/// Read one complete handshake frame body: the 2-byte big-endian length
/// prefix, its validation against [`MAX_HANDSHAKE_FRAME`], then exactly that
/// many body bytes — every read bounded by `deadline`.
///
/// The prefix is decoded and validated before the body is allocated, so a
/// hostile declaration costs nothing. This is the single read path shared by
/// all four flows.
fn read_handshake_frame(
    stream: &mut TcpStream,
    deadline: Instant,
) -> Result<Vec<u8>, TransportError> {
    let len_buf = read_handshake_exact(stream, deadline, LEN_PREFIX_LEN)?;
    let body_len = declared_frame_len(&len_buf)?;
    read_handshake_exact(stream, deadline, body_len)
}

/// Read one handshake frame and feed its body to the Noise state machine,
/// decrypting into `out`.
///
/// Wraps [`read_handshake_frame`] so each flow is a single call for a whole
/// message exchange step, and so the frame read, the length validation and
/// the `read_message` invocation cannot drift apart between flows.
pub(super) fn read_handshake_message(
    handshake: &mut HandshakeState,
    stream: &mut TcpStream,
    deadline: Instant,
    out: &mut [u8],
) -> Result<(), TransportError> {
    let body = read_handshake_frame(stream, deadline)?;
    handshake.read_message(&body, out)?;
    Ok(())
}

/// Write one already-encoded handshake message as a complete wire frame: lay
/// its 2-byte big-endian length prefix into `buf`'s headroom, then send the
/// prefix and the `n`-byte body as ONE bounded write.
///
/// `buf` is the frame scratch buffer ([`HANDSHAKE_FRAME_BUF_LEN`]): two bytes
/// of length-prefix headroom followed by the body snow encoded at
/// `buf[LEN_PREFIX_LEN..]`. Keeping the prefix lay-down and the coalesced
/// write here means that sequence exists once for both flows, and the single
/// write means a peer that stops reading cannot hold the writer past
/// `deadline` between a prefix write and a body write.
pub(super) fn write_handshake_frame(
    stream: &mut TcpStream,
    deadline: Instant,
    buf: &mut [u8],
    n: usize,
) -> Result<(), TransportError> {
    slice_mut(buf, ..LEN_PREFIX_LEN, "length prefix")?.copy_from_slice(&len16(n));
    write_handshake_all(
        stream,
        deadline,
        slice(buf, ..LEN_PREFIX_LEN + n, "handshake frame")?,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The 2-byte length prefix round-trips: a length encodes to big-endian
    /// bytes and decodes back unchanged. Deterministic, no sockets.
    #[test]
    fn length_prefix_round_trips() {
        for len in [0usize, 1, 48, 96, MAX_HANDSHAKE_FRAME] {
            let encoded = len16(len);
            assert_eq!(
                usize::from(be_len16(&encoded).expect("prefix decodes")),
                len,
                "length prefix must round-trip"
            );
        }
        // Big-endian by construction: the high byte comes first.
        assert_eq!(len16(0x01_02), [0x01, 0x02]);
        assert_eq!(u16::from_be_bytes(len16(300)), 300);
    }

    /// A declared body length inside the cap is accepted and one above it is
    /// rejected with the crate's protocol error — the pre-authentication
    /// allocation bound. Calls the validation path directly, so no socket and
    /// no allocation is involved.
    #[test]
    fn declared_length_above_cap_is_rejected() {
        assert_eq!(
            declared_frame_len(&len16(MAX_HANDSHAKE_FRAME)).expect("at the cap is allowed"),
            MAX_HANDSHAKE_FRAME
        );
        assert_eq!(
            declared_frame_len(&len16(0)).expect("an empty body is a legal length"),
            0
        );

        let over = declared_frame_len(&len16(MAX_HANDSHAKE_FRAME + 1));
        assert!(
            matches!(over, Err(TransportError::InvalidFragment(_))),
            "a length one past the cap must be rejected, got {over:?}"
        );
        // The whole 16-bit range a hostile peer can declare is rejected above
        // the cap, including the maximum.
        let max = declared_frame_len(&len16(usize::from(u16::MAX)));
        assert!(
            matches!(max, Err(TransportError::InvalidFragment(_))),
            "the maximum 16-bit length must be rejected, got {max:?}"
        );
    }

    /// The bounds-checked slice helpers return the crate's protocol error for
    /// an out-of-range index or range instead of panicking, which is what let
    /// the no-panic lint policy stay in force on the frame path.
    #[test]
    fn slice_helpers_error_out_of_range() {
        let buf = [1u8, 2, 3];

        assert!(
            matches!(at(&buf, 3, "at"), Err(TransportError::InvalidFragment(_))),
            "an out-of-range index must be an error, not a panic"
        );
        assert!(
            matches!(
                slice(&buf, 2..5, "slice"),
                Err(TransportError::InvalidFragment(_))
            ),
            "an out-of-range range must be an error, not a panic"
        );
        assert!(
            matches!(
                slice_from(&buf, 4, "slice_from"),
                Err(TransportError::InvalidFragment(_))
            ),
            "an out-of-range start must be an error, not a panic"
        );

        let mut owned = [1u8, 2, 3];
        assert!(
            matches!(
                at_mut(&mut owned, 3, "at_mut"),
                Err(TransportError::InvalidFragment(_))
            ),
            "an out-of-range mutable index must be an error, not a panic"
        );
        assert!(
            matches!(
                slice_mut(&mut owned, 2..5, "slice_mut"),
                Err(TransportError::InvalidFragment(_))
            ),
            "an out-of-range mutable range must be an error, not a panic"
        );

        // The in-range cases still work, so the helpers are not erroring
        // unconditionally.
        assert_eq!(at(&buf, 1, "at").expect("in range"), &2);
        assert_eq!(slice(&buf, ..2, "slice").expect("in range"), &[1u8, 2]);
        assert_eq!(slice_from(&buf, 2, "slice_from").expect("in range"), &[3u8]);
    }
}
