use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};
use tracing::{debug, info};

use crate::error::TransportError;
use crate::noise::NoiseStream;

/// Default total-duration budget for the Noise IK handshake. The
/// `handshake_*_with_timeout` variants take their own budget; the plain
/// functions delegate to them with this constant.
///
/// The budget is an ABSOLUTE deadline, not a per-read socket timeout: every
/// handshake read is bounded by the time remaining until it (see
/// [`read_handshake_exact`]), so a peer that dribbles bytes to keep resetting
/// a per-recv timeout is still cut off at the deadline. It is cleared before
/// the TransportState is handed over; the data plane has no timeout by design
/// — readers block until a message or EOF, and the daemon's shutdown path
/// closes sockets to unblock them.
///
/// The handshake runs BEFORE any authentication (the responder's ACL check
/// happens mid-handshake), so without a bound an unauthenticated peer could
/// hold a connection thread + socket FD open forever by connecting and
/// sending nothing — a resource-exhaustion vector on the daemon's TCP
/// listener. 10 s is far beyond the single round trip a healthy handshake
/// needs (sub-millisecond on loopback, a few ms on a real network).
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

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
fn read_handshake_exact(
    stream: &mut TcpStream,
    deadline: Instant,
    len: usize,
) -> Result<Vec<u8>, TransportError> {
    let mut buf = Vec::with_capacity(len);
    // Stack scratch buffer reused across reads — no per-read allocation. A
    // handshake message is at most a few hundred bytes, so 1 KiB is ample.
    let mut scratch = [0u8; 1024];
    while buf.len() < len {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(TransportError::HandshakeTimeout);
        }
        // Bound THIS read by the remaining budget so a slow peer cannot
        // stretch the total past `deadline` by dribbling bytes.
        stream.set_read_timeout(Some(remaining))?;
        let want = (len - buf.len()).min(scratch.len());
        let n = match stream.read(&mut scratch[..want]) {
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
        buf.extend_from_slice(&scratch[..n]);
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
        match stream.write(&data[written..]) {
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

/// Perform the Noise IK handshake as the **initiator** (client side), using
/// the default [`HANDSHAKE_TIMEOUT`] budget.
///
/// * `stream` — the already-connected TCP stream.
/// * `static_sk` — the client's transport.sec (32-byte X25519 secret key).
/// * `server_pk` — the server's transport.pub (32-byte X25519 public key).
///
/// On success returns a `NoiseStream` ready for encrypted message I/O.
pub fn handshake_initiator(
    stream: TcpStream,
    static_sk: &[u8; 32],
    server_pk: &[u8; 32],
) -> Result<NoiseStream, TransportError> {
    handshake_initiator_with_timeout(stream, static_sk, server_pk, HANDSHAKE_TIMEOUT)
}

/// [`handshake_initiator`] with an explicit total-duration budget for the
/// WHOLE handshake (see [`HANDSHAKE_TIMEOUT`] for why the budget is absolute,
/// not per-read). Exposed so integration tests can exercise the timeout path
/// in milliseconds instead of waiting out the 10 s default.
pub fn handshake_initiator_with_timeout(
    mut stream: TcpStream,
    static_sk: &[u8; 32],
    server_pk: &[u8; 32],
    timeout: Duration,
) -> Result<NoiseStream, TransportError> {
    use snow::Builder;

    let deadline = Instant::now() + timeout;

    let mut handshake = Builder::new("Noise_IK_25519_AESGCM_SHA256".parse()?)
        .local_private_key(static_sk)?
        .remote_public_key(server_pk)?
        .build_initiator()?;

    // Write first handshake message (e, encrypted s). The 2-byte length
    // prefix and the body are written as ONE bounded payload (see
    // write_handshake_all), so a server that stops reading mid-handshake
    // cannot hold the client's writer past the deadline — the write side of
    // the handshake is bounded just like the read side.
    let mut buf = vec![0u8; 2 + 1024];
    let n = handshake.write_message(&[], &mut buf[2..])?;
    // Use 2-byte length prefix for handshake messages (max size < 65535)
    buf[..2].copy_from_slice(&(n as u16).to_be_bytes());
    write_handshake_all(&mut stream, deadline, &buf[..2 + n])?;

    // Read second handshake message (e, encrypted empty) — every read is
    // bounded by the time remaining until `deadline`, so a stalled server
    // cannot hold the client forever (see read_handshake_exact).
    let len_buf = read_handshake_exact(&mut stream, deadline, 2)?;
    let msg_len = u16::from_be_bytes([len_buf[0], len_buf[1]]) as usize;
    let rbuf = read_handshake_exact(&mut stream, deadline, msg_len)?;
    handshake.read_message(&rbuf, &mut buf[2..])?;

    // Handshake complete — restore the unbounded data-plane I/O. Clear BOTH
    // the read and write timeouts: read_handshake_exact armed SO_RCVTIMEO
    // and write_handshake_all armed SO_SNDTIMEO to the remaining budget, and
    // neither belongs in the data plane (readers block until a message or
    // EOF by design).
    stream.set_read_timeout(None)?;
    stream.set_write_timeout(None)?;

    let transport = handshake.into_transport_mode()?;
    debug!("Noise IK handshake complete (initiator)");
    Ok(NoiseStream::new(stream, transport))
}

/// Perform the Noise IK handshake as the **responder** (server side), using
/// the default [`HANDSHAKE_TIMEOUT`] budget.
///
/// * `stream` — the accepted TCP stream from the listener.
/// * `static_sk` — the server's transport.sec (32-byte X25519 secret key).
/// * `check_client` — a closure that receives the client's 32-byte static
///   public key (extracted from the handshake) and returns `true` if the
///   client is authorized.
///
/// On success returns a `NoiseStream` ready for encrypted message I/O.
/// On authentication failure the connection is closed and `Err(AuthFailed)` is returned.
pub fn handshake_responder<F>(
    stream: TcpStream,
    static_sk: &[u8; 32],
    check_client: F,
) -> Result<NoiseStream, TransportError>
where
    F: FnOnce(&[u8; 32]) -> bool,
{
    handshake_responder_with_timeout(stream, static_sk, check_client, HANDSHAKE_TIMEOUT)
}

/// [`handshake_responder`] with an explicit total-duration budget for the
/// WHOLE handshake (see [`HANDSHAKE_TIMEOUT`]). Exposed so integration tests
/// can exercise the timeout path in milliseconds instead of waiting out the
/// 10 s default.
pub fn handshake_responder_with_timeout<F>(
    mut stream: TcpStream,
    static_sk: &[u8; 32],
    check_client: F,
    timeout: Duration,
) -> Result<NoiseStream, TransportError>
where
    F: FnOnce(&[u8; 32]) -> bool,
{
    use snow::Builder;

    let deadline = Instant::now() + timeout;

    let mut handshake = Builder::new("Noise_IK_25519_AESGCM_SHA256".parse()?)
        .local_private_key(static_sk)?
        .build_responder()?;

    // Read first handshake message from client (e, encrypted s) — every read
    // is bounded by the time remaining until `deadline`: the ACL check has
    // not happened yet, so a peer that connects and stalls (or dribbles) must
    // not be able to hold this thread and FD forever. Cleared once the
    // handshake completes and the client is authenticated.
    let len_buf = read_handshake_exact(&mut stream, deadline, 2)?;
    let msg_len = u16::from_be_bytes([len_buf[0], len_buf[1]]) as usize;
    let rbuf = read_handshake_exact(&mut stream, deadline, msg_len)?;
    let mut out_buf = vec![0u8; 1024];
    handshake.read_message(&rbuf, &mut out_buf)?;

    // Extract client's static public key for ACL check.
    if let Some(client_pk) = handshake.get_remote_static() {
        let mut pk = [0u8; 32];
        pk.copy_from_slice(client_pk);
        if !check_client(&pk) {
            info!("Noise IK handshake rejected: client not in ACL");
            return Err(TransportError::AuthFailed);
        }
    } else {
        info!("Noise IK handshake rejected: client did not send static key");
        return Err(TransportError::AuthFailed);
    }

    // Write second handshake message (e, encrypted empty) — one bounded
    // write of the 2-byte length prefix + body, so a client that stops
    // reading after msg1 cannot hold the responder's writer past `deadline`
    // (see write_handshake_all).
    let mut buf = vec![0u8; 2 + 1024];
    let n = handshake.write_message(&[], &mut buf[2..])?;
    buf[..2].copy_from_slice(&(n as u16).to_be_bytes());
    write_handshake_all(&mut stream, deadline, &buf[..2 + n])?;

    // Handshake complete — restore the unbounded data-plane I/O: clear BOTH
    // the read and write timeouts (see handshake_initiator_with_timeout).
    stream.set_read_timeout(None)?;
    stream.set_write_timeout(None)?;

    let transport = handshake.into_transport_mode()?;
    debug!("Noise IK handshake complete (responder)");
    Ok(NoiseStream::new(stream, transport))
}
