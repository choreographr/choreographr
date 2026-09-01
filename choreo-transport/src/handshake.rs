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

/// Handshake-mode preamble byte: Noise IK (the existing authenticated
/// mode — the client knows the server's static key in advance).
///
/// TCP wire protocol v5: every TCP connection now starts with exactly one
/// unauthenticated mode byte before any handshake message. 0x00 is
/// deliberately reserved (never assigned) so an all-zero garbage fill or a
/// trailing-NUL bug can never be mistaken for a valid mode.
pub const PREAMBLE_IK: u8 = 0x01;

/// Handshake-mode preamble byte: Noise XX (first-contact mode — the client
/// does NOT know the server's static key in advance and learns it from the
/// handshake; the caller then verifies the key out-of-band before any
/// protocol traffic flows).
pub const PREAMBLE_XX: u8 = 0x02;

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

/// Read the 1-byte handshake-mode preamble from `stream`, using the default
/// [`HANDSHAKE_TIMEOUT`] budget (see [`read_handshake_preamble_with_timeout`]).
///
/// The preamble runs BEFORE any authentication, so it must be bounded by the
/// same absolute-deadline machinery as the handshake itself: an
/// unauthenticated peer that connects and sends nothing must not be able to
/// hold a connection thread + FD open. The preamble read gets its own full
/// budget, and the handshake that follows gets its own — the two are
/// independent deadlines, which only widens the worst case by one budget and
/// keeps the deadline plumbing in exactly one place.
pub fn read_handshake_preamble(stream: &mut TcpStream) -> Result<u8, TransportError> {
    read_handshake_preamble_with_timeout(stream, HANDSHAKE_TIMEOUT)
}

/// [`read_handshake_preamble`] with an explicit total-duration budget.
/// Reuses [`read_handshake_exact`] (1 byte), so the deadline semantics are
/// byte-for-byte the handshake's: a silent peer is cut off with
/// `HandshakeTimeout`, and a dribbling peer cannot stretch the read past the
/// deadline by keeping per-read timers alive.
pub fn read_handshake_preamble_with_timeout(
    stream: &mut TcpStream,
    timeout: Duration,
) -> Result<u8, TransportError> {
    let deadline = Instant::now() + timeout;
    let byte = read_handshake_exact(stream, deadline, 1)?;
    Ok(byte[0])
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

/// Perform the Noise XX handshake as the **initiator** (client side, first
/// contact), using the default [`HANDSHAKE_TIMEOUT`] budget.
///
/// Unlike [`handshake_initiator`] (Noise IK), the XX initiator does NOT know
/// the server's static key in advance — that is the point of first-contact
/// mode. The server's static public key is transmitted (encrypted) in
/// handshake message 2 and is returned alongside the transport so the caller
/// can verify it out-of-band (fingerprint confirmation) BEFORE any encrypted
/// protocol traffic other than the handshake itself flows.
///
/// * `stream` — the already-connected TCP stream (the handshake-mode
///   preamble byte has already been sent by the caller).
/// * `static_sk` — the client's transport.sec (32-byte X25519 secret key).
///
/// On success returns `(NoiseStream, [u8; 32])` — the encrypted transport
/// and the server's static public key learned from the handshake. Fails if
/// the server never transmitted its static key (an XX protocol-contract
/// violation, surfaced as `InvalidFragment`).
pub fn handshake_initiator_xx(
    stream: TcpStream,
    static_sk: &[u8; 32],
) -> Result<(NoiseStream, [u8; 32]), TransportError> {
    handshake_initiator_xx_with_timeout(stream, static_sk, HANDSHAKE_TIMEOUT)
}

/// [`handshake_initiator_xx`] with an explicit total-duration budget for the
/// WHOLE handshake (see [`HANDSHAKE_TIMEOUT`]). Reuses the exact same
/// absolute-deadline plumbing as the IK variants — [`read_handshake_exact`]
/// and [`write_handshake_all`] — so the XX timeout semantics are identical:
/// a silent or dribbling peer is cut off at the deadline with
/// `HandshakeTimeout`, and both socket timeouts are cleared before the data
/// plane.
pub fn handshake_initiator_xx_with_timeout(
    mut stream: TcpStream,
    static_sk: &[u8; 32],
    timeout: Duration,
) -> Result<(NoiseStream, [u8; 32]), TransportError> {
    use snow::Builder;

    let deadline = Instant::now() + timeout;

    // XX initiator: NO `.remote_public_key()` — the server's static key is
    // unknown until the handshake reveals it (IK hard-codes it here, which
    // is what makes IK a 2-message pattern instead of XX's 3).
    let mut handshake = Builder::new("Noise_XX_25519_AESGCM_SHA256".parse()?)
        .local_private_key(static_sk)?
        .build_initiator()?;

    // The write side is bounded exactly like IK: the 2-byte length prefix
    // and the body go out as ONE bounded payload (see write_handshake_all).
    let mut buf = vec![0u8; 2 + 1024];

    // Message 1 (-> e): the client's ephemeral key only.
    let n = handshake.write_message(&[], &mut buf[2..])?;
    buf[..2].copy_from_slice(&(n as u16).to_be_bytes());
    write_handshake_all(&mut stream, deadline, &buf[..2 + n])?;

    // Message 2 (<- e, ee, s, es): the server's ephemeral AND static keys,
    // with the static authenticated by the DH operations. After reading this
    // the initiator can extract the server's static via get_remote_static().
    let len_buf = read_handshake_exact(&mut stream, deadline, 2)?;
    let msg_len = u16::from_be_bytes([len_buf[0], len_buf[1]]) as usize;
    let rbuf = read_handshake_exact(&mut stream, deadline, msg_len)?;
    handshake.read_message(&rbuf, &mut buf[2..])?;

    // Message 3 (-> s, es): the client's static key, authenticating us to
    // the server (the server's responder-side ACL check consumes it).
    let n = handshake.write_message(&[], &mut buf[2..])?;
    buf[..2].copy_from_slice(&(n as u16).to_be_bytes());
    write_handshake_all(&mut stream, deadline, &buf[..2 + n])?;

    // Handshake complete — restore the unbounded data-plane I/O (see
    // handshake_initiator_with_timeout: BOTH timeouts must be cleared,
    // read_handshake_exact armed SO_RCVTIMEO and write_handshake_all armed
    // SO_SNDTIMEO).
    stream.set_read_timeout(None)?;
    stream.set_write_timeout(None)?;

    // Learn the server's static key — the entire reason this mode exists.
    // XX always transmits `s` in message 2, so an absent value is a protocol
    // contract violation, not an authentication failure; InvalidFragment is
    // the closest honest variant (the responder side DOES use AuthFailed for
    // the analogous check, because there it gates the ACL decision).
    let server_pk_bytes = handshake
        .get_remote_static()
        .ok_or_else(|| {
            TransportError::InvalidFragment(
                "XX handshake: server did not transmit its static key".to_string(),
            )
        })?
        .to_owned();
    let mut server_pk = [0u8; 32];
    server_pk.copy_from_slice(&server_pk_bytes);

    let transport = handshake.into_transport_mode()?;
    debug!("Noise XX handshake complete (initiator)");
    Ok((NoiseStream::new(stream, transport), server_pk))
}

/// Perform the Noise XX handshake as the **responder** (server side,
/// first-contact), using the default [`HANDSHAKE_TIMEOUT`] budget.
///
/// Mirrors [`handshake_responder`] (Noise IK) except for the message flow:
/// XX is 3 messages instead of IK's 2, and the client's static key arrives
/// with message 3 (not message 1), so the `check_client` ACL closure runs
/// AFTER the full key exchange — see the comment at the check below for why
/// that is still safe.
pub fn handshake_responder_xx<F>(
    stream: TcpStream,
    static_sk: &[u8; 32],
    check_client: F,
) -> Result<NoiseStream, TransportError>
where
    F: FnOnce(&[u8; 32]) -> bool,
{
    handshake_responder_xx_with_timeout(stream, static_sk, check_client, HANDSHAKE_TIMEOUT)
}

/// [`handshake_responder_xx`] with an explicit total-duration budget for the
/// WHOLE handshake (see [`HANDSHAKE_TIMEOUT`]). Same absolute-deadline
/// plumbing as the IK responder — every read AND write is bounded by the
/// time remaining until the deadline.
pub fn handshake_responder_xx_with_timeout<F>(
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

    let mut handshake = Builder::new("Noise_XX_25519_AESGCM_SHA256".parse()?)
        .local_private_key(static_sk)?
        .build_responder()?;

    // Message 1 (-> e): read the client's ephemeral key. Every read is
    // bounded by the remaining budget — pre-authentication, exactly like IK.
    let len_buf = read_handshake_exact(&mut stream, deadline, 2)?;
    let msg_len = u16::from_be_bytes([len_buf[0], len_buf[1]]) as usize;
    let rbuf = read_handshake_exact(&mut stream, deadline, msg_len)?;
    let mut out_buf = vec![0u8; 1024];
    handshake.read_message(&rbuf, &mut out_buf)?;

    // Message 2 (<- e, ee, s, es): our ephemeral + static keys. Unlike IK,
    // this MUST be written before the ACL check — the client's static only
    // arrives with message 3, and message 2 is what carries the server's
    // static to the client. This is inherent to XX (the server cannot know
    // who it is talking to until the client finishes authenticating), and it
    // is not a disclosure risk: msg2's static is encrypted to the client's
    // ephemeral key, so a passive observer learns nothing, and an active
    // attacker would have to complete the key exchange to decrypt it — at
    // which point its own static is exposed to the ACL check.
    let mut buf = vec![0u8; 2 + 1024];
    let n = handshake.write_message(&[], &mut buf[2..])?;
    buf[..2].copy_from_slice(&(n as u16).to_be_bytes());
    write_handshake_all(&mut stream, deadline, &buf[..2 + n])?;

    // Message 3 (-> s, es): the client's static key arrives here — this is
    // the point where the responder finally learns who it is talking to.
    let len_buf = read_handshake_exact(&mut stream, deadline, 2)?;
    let msg_len = u16::from_be_bytes([len_buf[0], len_buf[1]]) as usize;
    let rbuf = read_handshake_exact(&mut stream, deadline, msg_len)?;
    handshake.read_message(&rbuf, &mut out_buf)?;

    // ACL check — the same closure pattern as the IK responder, so XX
    // connections are authorized identically. The check runs after the key
    // exchange (see the msg2 comment) but BEFORE the transport is handed
    // over: a rejected client gets a closed socket and can never send or
    // receive a single data-plane byte.
    if let Some(client_pk) = handshake.get_remote_static() {
        let mut pk = [0u8; 32];
        pk.copy_from_slice(client_pk);
        if !check_client(&pk) {
            info!("Noise XX handshake rejected: client not in ACL");
            return Err(TransportError::AuthFailed);
        }
    } else {
        info!("Noise XX handshake rejected: client did not send static key");
        return Err(TransportError::AuthFailed);
    }

    // Handshake complete — restore the unbounded data-plane I/O (see
    // handshake_responder_with_timeout).
    stream.set_read_timeout(None)?;
    stream.set_write_timeout(None)?;

    let transport = handshake.into_transport_mode()?;
    debug!("Noise XX handshake complete (responder)");
    Ok(NoiseStream::new(stream, transport))
}
