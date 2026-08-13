use choreo_proto::{ClientMessage, DaemonMessage, MAX_FRAME_SIZE, decode_frame, encode_payload};
use snow::TransportState;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tracing::{debug, info};

use crate::error::TransportError;

/// AES-256-GCM authentication tag length, appended to every encrypted
/// message (snow's `MAXMSGLEN` counts the tag).
const GCM_TAG_LEN: usize = 16;
/// Length of the authenticated per-fragment continuation header: one byte at
/// the START of each fragment's plaintext, so the AES-GCM tag covers it (see
/// [`FRAGMENT_CONTINUATION`]).
const FRAGMENT_HEADER_LEN: usize = 1;
/// Maximum payload bytes in a single Noise fragment. snow caps one message's
/// ciphertext at 65535 bytes (MAXMSGLEN); the 16-byte GCM tag and the 1-byte
/// authenticated continuation header leave 65518 bytes of payload per
/// fragment.
const MAX_PLAINTEXT_CHUNK: usize = 65535 - GCM_TAG_LEN - FRAGMENT_HEADER_LEN;
/// Bit 0 of a fragment's first plaintext byte: 1 = more fragments of the same
/// logical message follow, 0 = this fragment terminates it. Because the byte
/// lives INSIDE the plaintext, the AES-GCM tag authenticates it — the wire
/// length prefix that precedes the ciphertext is NOT authenticated, so the
/// reassembly decision must not be trusted from there.
const FRAGMENT_CONTINUATION: u8 = 0x01;

/// Read timeout applied to the socket for the duration of the Noise IK
/// handshake only (cleared before the TransportState is handed over; the data
/// plane has no timeout by design — readers block until a message or EOF, and
/// the daemon's shutdown path closes sockets to unblock them).
///
/// The handshake runs BEFORE any authentication (the responder's ACL check
/// happens mid-handshake), so without a bound an unauthenticated peer could
/// hold a connection thread + socket FD open forever by connecting and
/// sending nothing — a resource-exhaustion vector on the daemon's TCP
/// listener. 10 s is far beyond the single round trip a healthy handshake
/// needs (sub-millisecond on loopback, a few ms on a real network).
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Resets the single-writer flag when a `send_message` finishes — on every
/// exit path, including early `?` returns. Without the guard a concurrent
/// sender that errored would leave the flag set and wedge the stream.
struct SendGuard<'a>(&'a AtomicBool);

impl Drop for SendGuard<'_> {
    fn drop(&mut self) {
        // Relaxed is sufficient: the flag publishes no data (the TransportState
        // mutex provides its own ordering for the cipher state, and socket bytes
        // are kernel-buffered), so exclusion needs only the atomic coherence of
        // the flag itself — a concurrent `swap(true)` sees either `false` (the
        // previous holder's drop already happened) or `true` (still in flight),
        // never both, so exactly one sender proceeds at a time.
        self.0.store(false, Ordering::Relaxed);
    }
}

/// An encrypted Noise IK transport stream.
///
/// Wraps a TcpStream with a snow TransportState. Every message is
/// length-prefixed (4-byte BE u32) and encrypted with AES-256-GCM.
///
/// The TransportState is behind an Arc<Mutex<>> so the stream can be
/// cloned for concurrent reader/writer threads via try_clone().
pub struct NoiseStream {
    tcp: TcpStream,
    transport: Arc<Mutex<TransportState>>,
    /// Single-writer guard: a concurrent second `send_message` on one stream
    /// would interleave fragments of different logical messages on the wire
    /// (silent corruption). The flag is shared across clones; `send_message`
    /// swaps it and fails loudly with a protocol error if it was already set,
    /// so a future violation is caught instead of corrupting the stream.
    sender_active: Arc<AtomicBool>,
}

impl NoiseStream {
    /// Create a new NoiseStream wrapping a TcpStream and TransportState.
    pub fn new(tcp: TcpStream, transport: TransportState) -> Self {
        NoiseStream {
            tcp,
            transport: Arc::new(Mutex::new(transport)),
            sender_active: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Clone the stream for concurrent reader/writer threads.
    /// The underlying TcpStream is cloned; the TransportState is shared
    /// via Arc<Mutex<>> so encrypt/decrypt calls are serialized.
    pub fn try_clone(&self) -> std::io::Result<Self> {
        Ok(NoiseStream {
            tcp: self.tcp.try_clone()?,
            transport: Arc::clone(&self.transport),
            sender_active: Arc::clone(&self.sender_active),
        })
    }

    /// Send an encrypted message.
    ///
    /// Encrypts the plaintext into one or more ciphertext fragments (leaving
    /// the input untouched), then writes each fragment's 4-byte big-endian
    /// ciphertext length prefix followed by the ciphertext. Payloads larger
    /// than [`MAX_PLAINTEXT_CHUNK`] are split into multiple fragments; each
    /// fragment's plaintext starts with an AUTHENTICATED continuation byte
    /// ([`FRAGMENT_CONTINUATION`]) that tells the receiver whether to keep
    /// reading.
    ///
    /// The shared `TransportState` lock is held only per-chunk, during the
    /// encrypt call, and is NEVER held across the blocking `write_all`s —
    /// the reader must always be able to acquire it (briefly) and drain the
    /// socket, or two peers sending large messages concurrently would
    /// deadlock once the socket buffers filled. A runtime single-writer guard
    /// (see `sender_active`) rejects a concurrent second `send_message` with a
    /// protocol error rather than interleaving fragments.
    pub fn send_message(&mut self, plaintext: &[u8]) -> Result<(), TransportError> {
        // The single-writer-per-connection invariant is load-bearing:
        // fragments of one logical message must never interleave with
        // another message's on the wire, which holds only while exactly one
        // thread calls send_message per stream. Enforce it at runtime: a
        // second concurrent sender gets a loud protocol error instead of
        // silently corrupting the stream.
        if self.sender_active.swap(true, Ordering::Relaxed) {
            return Err(TransportError::InvalidFragment(
                "concurrent send_message on one NoiseStream".to_string(),
            ));
        }
        let _guard = SendGuard(&self.sender_active);

        // Mirror the receiver's reassembly cap so an over-limit message is
        // rejected at the source instead of by the peer's recv_message.
        if plaintext.len() > MAX_FRAME_SIZE {
            return Err(TransportError::InvalidFragment(format!(
                "outgoing message exceeds the {MAX_FRAME_SIZE}-byte limit"
            )));
        }

        // Reusable ciphertext buffer: one Noise message's worst case is
        // MAX_PLAINTEXT_CHUNK payload bytes plus the 1-byte continuation header
        // and the 16-byte AES-GCM tag. Reusing it across chunks avoids
        // reallocating per fragment.
        let mut buf = vec![0u8; MAX_PLAINTEXT_CHUNK + FRAGMENT_HEADER_LEN + GCM_TAG_LEN];
        // Reusable per-fragment plaintext: 1 continuation byte + payload. The
        // header goes INSIDE the plaintext (not the length prefix) so the
        // AES-GCM tag authenticates it — the wire prefix is not authenticated,
        // so the continuation flag must not be trusted from there.
        let mut frag = vec![0u8; FRAGMENT_HEADER_LEN + MAX_PLAINTEXT_CHUNK];

        // Serializing sends is the writer thread's job (each NoiseStream has a
        // single writer), so the TransportState lock is only needed to make the
        // encrypt call atomic against the reader's decrypts. It must NEVER be
        // held across the blocking write_all below: a reader waiting on the
        // lock would stop draining the socket, and a peer doing the same in the
        // other direction would deadlock once the socket buffers filled. Each
        // chunk is therefore encrypted under a brief lock and written outside
        // it, so the reader can always acquire the lock and drain.
        let mut remaining = plaintext.len();
        for chunk in plaintext.chunks(MAX_PLAINTEXT_CHUNK) {
            // This chunk is non-final iff it did not consume all remaining
            // plaintext; a full-size chunk in the middle leaves a remainder,
            // while the final chunk (even one exactly MAX_PLAINTEXT_CHUNK
            // bytes) leaves none.
            let more = remaining > chunk.len();
            remaining -= chunk.len();
            frag[0] = if more { FRAGMENT_CONTINUATION } else { 0 };
            frag[FRAGMENT_HEADER_LEN..FRAGMENT_HEADER_LEN + chunk.len()].copy_from_slice(chunk);
            let n = {
                let mut transport = self.transport.lock().unwrap_or_else(|e| e.into_inner());
                transport.write_message(&frag[..FRAGMENT_HEADER_LEN + chunk.len()], &mut buf)?
            };
            self.tcp.write_all(&(n as u32).to_be_bytes())?;
            self.tcp.write_all(&buf[..n])?;
        }
        Ok(())
    }

    /// Receive a decrypted message.
    ///
    /// Reads one or more length-prefixed ciphertext fragments, decrypting
    /// each into a shared plaintext buffer, until a fragment whose
    /// AUTHENTICATED continuation header is clear arrives — that fragment
    /// terminates the logical message.
    pub fn recv_message(&mut self) -> Result<Vec<u8>, TransportError> {
        let mut plaintext = Vec::new();
        // Reusable per-fragment buffers, sized to the largest legitimate
        // fragment (65535 bytes of ciphertext). `resize` below only sets the
        // length after the first fragment — no per-fragment allocations.
        let mut ct_buf = vec![0u8; MAX_PLAINTEXT_CHUNK + FRAGMENT_HEADER_LEN + GCM_TAG_LEN];
        let mut pt_buf = vec![0u8; MAX_PLAINTEXT_CHUNK + FRAGMENT_HEADER_LEN + GCM_TAG_LEN];
        loop {
            // Read the raw frame bytes WITHOUT holding the TransportState
            // lock: snow tracks separate send and recv nonce counters, so an
            // interleaved send_message from another thread is safe, and a
            // sender can never be blocked by this reader. If recv_message
            // instead held the mutex while blocked on read_exact, a
            // concurrent sender (e.g. the daemon's shutdown-notification
            // thread sending ShuttingDown) would block forever on a reader
            // waiting for data that may never come — a shutdown deadlock.
            let mut len_buf = [0u8; 4];
            self.tcp.read_exact(&mut len_buf)?;
            let ct_len = u32::from_be_bytes(len_buf) as usize;
            // The length prefix is NOT authenticated — it precedes the GCM
            // ciphertext and is trusted before decryption — so it must be
            // validated before any allocation or read. snow caps one
            // message's ciphertext at 65535 bytes; a larger prefix is
            // protocol garbage, and rejecting it here stops a hostile or
            // corrupted peer from making us allocate (and block in
            // read_exact for) an arbitrarily large buffer. Any smaller
            // tamper with the prefix changes ct_len and is caught by the GCM
            // authentication below (a wrong byte count never decrypts), so a
            // wire flip can never silently truncate or extend a message.
            if ct_len > MAX_PLAINTEXT_CHUNK + FRAGMENT_HEADER_LEN + GCM_TAG_LEN {
                return Err(TransportError::InvalidFragment(format!(
                    "fragment ciphertext length {ct_len} exceeds the {}-byte Noise cap",
                    MAX_PLAINTEXT_CHUNK + FRAGMENT_HEADER_LEN + GCM_TAG_LEN
                )));
            }
            // Cumulative cap during reassembly: the codec's MAX_FRAME_SIZE is
            // the effective per-message limit, and enforcing it HERE rather
            // than after reassembly in decode_frame bounds memory for an
            // endless stream of continuation-marked fragments. Each fragment
            // contributes at most ct_len - (tag + continuation header)
            // plaintext bytes.
            if plaintext.len() + ct_len.saturating_sub(GCM_TAG_LEN + FRAGMENT_HEADER_LEN)
                > MAX_FRAME_SIZE
            {
                return Err(TransportError::InvalidFragment(format!(
                    "reassembled message exceeds the {MAX_FRAME_SIZE}-byte limit"
                )));
            }
            ct_buf.resize(ct_len, 0);
            self.tcp.read_exact(&mut ct_buf)?;

            // Take the lock briefly to decrypt this one fragment, then
            // release it before the next blocking read (see above).
            pt_buf.resize(ct_len, 0);
            let n = self
                .transport
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .read_message(&ct_buf, &mut pt_buf)?;
            // Every fragment from this implementation carries the 1-byte
            // continuation header, so a zero-length plaintext is impossible
            // from a legitimate peer; guard the index below rather than panic
            // on a hostile or corrupted 16-byte ciphertext (a bare GCM tag).
            if n == 0 {
                return Err(TransportError::InvalidFragment(
                    "fragment carries no authenticated continuation header".to_string(),
                ));
            }
            // The continuation flag is read from the AUTHENTICATED plaintext
            // (the GCM tag covers it), so a wire-level tamper with the length
            // prefix cannot change the reassembly decision — see the prefix
            // validation above.
            let more = pt_buf[0] & FRAGMENT_CONTINUATION != 0;
            plaintext.extend_from_slice(&pt_buf[FRAGMENT_HEADER_LEN..n]);

            if !more {
                return Ok(plaintext);
            }
        }
    }

    /// Send a typed ClientMessage (convenience for clients).
    pub fn send_client_message(&mut self, msg: &ClientMessage) -> Result<(), TransportError> {
        let payload = encode_payload(msg)?;
        self.send_message(&payload)
    }

    /// Receive a typed DaemonMessage (convenience for clients).
    pub fn recv_daemon_message(&mut self) -> Result<DaemonMessage, TransportError> {
        let payload = self.recv_message()?;
        Ok(decode_frame(&payload)?)
    }

    /// Send a typed DaemonMessage (convenience for servers).
    pub fn send_daemon_message(&mut self, msg: &DaemonMessage) -> Result<(), TransportError> {
        let payload = encode_payload(msg)?;
        self.send_message(&payload)
    }

    /// Receive a typed ClientMessage (convenience for servers).
    pub fn recv_client_message(&mut self) -> Result<ClientMessage, TransportError> {
        let payload = self.recv_message()?;
        Ok(decode_frame(&payload)?)
    }

    /// Access the underlying TcpStream for shutdown etc.
    pub fn get_ref(&self) -> &TcpStream {
        &self.tcp
    }
}

/// Perform the Noise IK handshake as the **initiator** (client side).
///
/// * `stream` — the already-connected TCP stream.
/// * `static_sk` — the client's transport.sec (32-byte X25519 secret key).
/// * `server_pk` — the server's transport.pub (32-byte X25519 public key).
///
/// On success returns a `NoiseStream` ready for encrypted message I/O.
pub fn handshake_initiator(
    mut stream: TcpStream,
    static_sk: &[u8; 32],
    server_pk: &[u8; 32],
) -> Result<NoiseStream, TransportError> {
    use snow::Builder;

    // Bound the handshake reads (see HANDSHAKE_TIMEOUT): a stalled or dead
    // server must not hang the client's connect forever. Cleared below once
    // the TransportState is derived.
    stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;

    let mut handshake = Builder::new("Noise_IK_25519_AESGCM_SHA256".parse()?)
        .local_private_key(static_sk)?
        .remote_public_key(server_pk)?
        .build_initiator()?;

    // Write first handshake message (e, encrypted s)
    let mut buf = vec![0u8; 1024];
    let n = handshake.write_message(&[], &mut buf)?;
    // Use 2-byte length prefix for handshake messages (max size < 65535)
    let len_bytes = (n as u16).to_be_bytes();
    stream.write_all(&len_bytes)?;
    stream.write_all(&buf[..n])?;

    // Read second handshake message (e, encrypted empty)
    let mut len_buf = [0u8; 2];
    stream.read_exact(&mut len_buf)?;
    let msg_len = u16::from_be_bytes(len_buf) as usize;
    let mut rbuf = vec![0u8; msg_len];
    stream.read_exact(&mut rbuf)?;
    handshake.read_message(&rbuf, &mut buf)?;

    // Handshake complete — restore the unbounded data-plane reads.
    stream.set_read_timeout(None)?;

    let transport = handshake.into_transport_mode()?;
    debug!("Noise IK handshake complete (initiator)");
    Ok(NoiseStream::new(stream, transport))
}

/// Perform the Noise IK handshake as the **responder** (server side).
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
    mut stream: TcpStream,
    static_sk: &[u8; 32],
    check_client: F,
) -> Result<NoiseStream, TransportError>
where
    F: FnOnce(&[u8; 32]) -> bool,
{
    use snow::Builder;

    // Bound the handshake reads (see HANDSHAKE_TIMEOUT): the ACL check has
    // not happened yet, so a peer that connects and stalls must not be able
    // to hold this thread and FD forever. Cleared once the handshake
    // completes and the client is authenticated.
    stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;

    let mut handshake = Builder::new("Noise_IK_25519_AESGCM_SHA256".parse()?)
        .local_private_key(static_sk)?
        .build_responder()?;

    // Read first handshake message from client (e, encrypted s)
    let mut len_buf = [0u8; 2];
    stream.read_exact(&mut len_buf)?;
    let msg_len = u16::from_be_bytes(len_buf) as usize;
    let mut rbuf = vec![0u8; msg_len];
    stream.read_exact(&mut rbuf)?;
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

    // Write second handshake message (e, encrypted empty)
    let mut buf = vec![0u8; 1024];
    let n = handshake.write_message(&[], &mut buf)?;
    let len_bytes = (n as u16).to_be_bytes();
    stream.write_all(&len_bytes)?;
    stream.write_all(&buf[..n])?;

    // Handshake complete — restore the unbounded data-plane reads.
    stream.set_read_timeout(None)?;

    let transport = handshake.into_transport_mode()?;
    debug!("Noise IK handshake complete (responder)");
    Ok(NoiseStream::new(stream, transport))
}

#[cfg(test)]
mod tests {
    use snow::Builder;
    use x25519_dalek::{PublicKey, StaticSecret};

    /// In-memory crypto test (no sockets, no sleeps): prove that flipping a
    /// single ciphertext byte makes GCM decryption fail. The flipped byte is
    /// in the *payload* region of the ciphertext — the trailing 16 bytes are
    /// the AES-GCM auth tag — so this proves the tag authenticates the
    /// ciphertext itself, not just the tag.
    #[test]
    fn transport_state_rejects_tampered_ciphertext() {
        let client_secret = StaticSecret::random_from_rng(&mut rand::rng());
        let server_secret = StaticSecret::random_from_rng(&mut rand::rng());
        let client_sk = client_secret.to_bytes();
        let server_sk = server_secret.to_bytes();
        let server_pk = PublicKey::from(&server_secret).to_bytes();

        // Mirror the production handshake (handshake_initiator /
        // handshake_responder) exactly: same pattern string, same key roles
        // (client = initiator with server's public key, server = responder).
        let mut initiator = Builder::new("Noise_IK_25519_AESGCM_SHA256".parse().unwrap())
            .local_private_key(&client_sk)
            .unwrap()
            .remote_public_key(&server_pk)
            .unwrap()
            .build_initiator()
            .unwrap();
        let mut responder = Builder::new("Noise_IK_25519_AESGCM_SHA256".parse().unwrap())
            .local_private_key(&server_sk)
            .unwrap()
            .build_responder()
            .unwrap();

        // Message 1: initiator -> responder (ephemeral key + encrypted
        // static key). The plaintext is empty; the point is just to run the
        // same message order the wire protocol uses.
        let mut msg1 = vec![0u8; 1024];
        let n1 = initiator.write_message(&[], &mut msg1).unwrap();
        let mut scratch = vec![0u8; 1024];
        responder.read_message(&msg1[..n1], &mut scratch).unwrap();

        // Message 2: responder -> initiator (ephemeral key, empty payload).
        let mut msg2 = vec![0u8; 1024];
        let n2 = responder.write_message(&[], &mut msg2).unwrap();
        let mut scratch2 = vec![0u8; 1024];
        initiator.read_message(&msg2[..n2], &mut scratch2).unwrap();

        // Both sides now derive identical TransportStates (same key schedule,
        // same nonce counters starting at 0).
        let mut client_ts = initiator.into_transport_mode().unwrap();
        let mut server_ts = responder.into_transport_mode().unwrap();

        let plaintext = b"attack at dawn";

        // Sanity check first: an untampered message round-trips, proving the
        // two transport states were actually derived from the same handshake.
        let mut ct = vec![0u8; plaintext.len() + 16];
        let ct_len = client_ts.write_message(plaintext, &mut ct).unwrap();
        let mut pt = vec![0u8; plaintext.len() + 16];
        let pt_len = server_ts.read_message(&ct[..ct_len], &mut pt).unwrap();
        assert_eq!(&pt[..pt_len], plaintext);

        // Now encrypt a fresh message and flip one byte at index 4. The
        // plaintext is 14 bytes, so index 4 is deep inside the payload
        // region (not the 16-byte tag). GCM authenticates every ciphertext
        // byte, so read_message must return an error.
        let mut tampered = vec![0u8; plaintext.len() + 16];
        let tampered_len = client_ts.write_message(plaintext, &mut tampered).unwrap();
        tampered[4] ^= 0xff;
        let mut bad_pt = vec![0u8; plaintext.len() + 16];
        let result = server_ts.read_message(&tampered[..tampered_len], &mut bad_pt);
        assert!(
            result.is_err(),
            "tampered ciphertext must be rejected by GCM authentication"
        );
    }
}
