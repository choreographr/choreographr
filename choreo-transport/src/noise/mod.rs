//! The encrypted Noise transport stream and its two directions.
//!
//! [`NoiseStream`] length-prefixes (4-byte big-endian) and fragments plaintext
//! into AES-256-GCM messages, with an authenticated per-fragment continuation
//! byte driving reassembly. The stream itself is here — construction, cloning,
//! the typed send/receive conveniences — while each direction lives in its own
//! submodule: `send` owns framing, encryption and the single-writer slot,
//! `recv` owns reassembly, decryption and the peer-close classification, and
//! `frame` owns the fragment sizes and the length-prefix codec both use.
//!
//! The `TransportState` is shared through an `Arc<Mutex<…>>` because a
//! connection has one reader thread and one writer thread over a
//! `try_clone`d socket (see AGENTS.md's sanctioned shared-state exception for
//! the transport), and per-send exclusivity is a single-bit `AtomicBool` in the
//! spirit of the cancellation-flag exception.

mod frame;
mod recv;
mod send;

use std::net::TcpStream;
use std::sync::{Arc, Mutex};

use choreo_proto::{ClientMessage, DaemonMessage, DaemonMessageType, decode_frame, encode_payload};
use snow::TransportState;
use zeroize::{Zeroize, Zeroizing};

use crate::error::TransportError;
use send::SendSlot;

/// An encrypted Noise transport stream.
///
/// Wraps a `TcpStream` with a snow `TransportState`. Every message is
/// length-prefixed (4-byte BE u32) and encrypted with AES-256-GCM. The stream
/// is what a completed handshake produces and does not itself know which
/// pattern ran — IK and XX both end here (see [`crate::handshake`]).
///
/// The `TransportState` is behind an `Arc<Mutex<>>` so the stream can be
/// cloned for concurrent reader/writer threads via [`Self::try_clone`].
pub struct NoiseStream {
    tcp: TcpStream,
    transport: Arc<Mutex<TransportState>>,
    /// The single-writer slot, shared across clones: a concurrent second
    /// `send_message` on one stream would interleave fragments of different
    /// logical messages on the wire (silent corruption). The slot turns that
    /// into a loud protocol error instead (see `send::SendSlot`).
    send_slot: SendSlot,
    /// Reused outbound ciphertext frame buffer: 4 bytes of length-prefix
    /// headroom + the largest possible single Noise message's ciphertext
    /// (65535 bytes). Lazy — `new`/`try_clone` start it empty and
    /// `send_message` grows it once — so a clone used only for reading never
    /// pays for it. One `write_all` sends the prefix + ciphertext together.
    send_buf: Vec<u8>,
    /// Reused per-fragment plaintext: 1 authenticated continuation byte + the
    /// largest possible payload chunk. Lazy like `send_buf`; the reader clone's
    /// copy stays empty. Wiped after every send.
    send_frag: Vec<u8>,
    /// Reused inbound per-fragment ciphertext, resized to the actual `ct_len`
    /// of each frame. Capacity carries across messages, so there is no
    /// per-fragment allocation; the buffer only grows to the largest fragment
    /// actually seen.
    recv_ct_buf: Vec<u8>,
    /// Reused inbound per-fragment plaintext, sized like `recv_ct_buf` (the
    /// writer clone's copy stays empty). Wiped after every fragment is copied
    /// out.
    recv_pt_buf: Vec<u8>,
}

impl NoiseStream {
    /// Create a new `NoiseStream` wrapping a `TcpStream` and `TransportState`.
    pub fn new(tcp: TcpStream, transport: TransportState) -> Self {
        tracing::trace!("NoiseStream created");
        NoiseStream {
            tcp,
            transport: Arc::new(Mutex::new(transport)),
            send_slot: SendSlot::new(),
            // All four reusable buffers start empty; send_message/recv_message
            // grow the ones they actually use to their worst-case size on
            // first use (see `frame::FRAME_BUF_LEN`/`FRAG_BUF_LEN`).
            send_buf: Vec::new(),
            send_frag: Vec::new(),
            recv_ct_buf: Vec::new(),
            recv_pt_buf: Vec::new(),
        }
    }

    /// Clone the stream for concurrent reader/writer threads.
    /// The underlying `TcpStream` is cloned; the `TransportState` and the
    /// single-writer slot are shared, so encrypt/decrypt calls are serialized
    /// and two clones still cannot send at once.
    ///
    /// # Errors
    ///
    /// Returns `std::io::Error` if the underlying stream cannot be cloned.
    pub fn try_clone(&self) -> std::io::Result<Self> {
        Ok(NoiseStream {
            tcp: self.tcp.try_clone()?,
            transport: Arc::clone(&self.transport),
            send_slot: self.send_slot.clone(),
            // Buffer reuse is per-direction: a reader clone never sends and a
            // writer clone never receives, so each clone starts the other
            // side's buffers empty and never grows them — no wasted memory on
            // unused sides.
            send_buf: Vec::new(),
            send_frag: Vec::new(),
            recv_ct_buf: Vec::new(),
            recv_pt_buf: Vec::new(),
        })
    }

    /// Send a typed `ClientMessage` (convenience for clients).
    ///
    /// # Errors
    ///
    /// Returns any error propagated by [`Self::send_message`] or the payload
    /// encoding.
    pub fn send_client_message(&mut self, msg: &ClientMessage) -> Result<(), TransportError> {
        let payload: Zeroizing<Vec<u8>> = encode_payload(msg)?;
        self.send_message(&payload)
    }

    /// Receive a typed `DaemonMessage` (convenience for clients).
    ///
    /// The raw frame buffer is wiped after decoding UNLESS the decoded variant
    /// is one the daemon can only ever send in cleartext-equivalent bulk data
    /// (see `daemon_message_may_carry_secret`): [`DaemonMessageType::Image`]
    /// carries raw attachment bytes up to the 64 MiB frame cap, so exempting it
    /// avoids a pointless 64 MiB memset on the largest payload the daemon sends,
    /// while every credential-bearing variant (`Credential`, `Keystore`, …) and
    /// every variant not on the allowlist is still wiped. A decode failure is
    /// unknown content and is wiped too.
    ///
    /// # Errors
    ///
    /// Returns any error propagated by `recv_message_plain` or the
    /// payload decoding.
    pub fn recv_daemon_message(&mut self) -> Result<DaemonMessage, TransportError> {
        let mut payload = self.recv_message_plain()?;
        let decoded = decode_frame(&payload);
        // Wipe the frame unless it decoded to a known non-secret bulk variant;
        // the guard is an allowlist, so unknown content (a decode error, a
        // future variant) is wiped rather than skipped.
        if !matches!(&decoded, Ok(msg) if !daemon_message_may_carry_secret(msg)) {
            payload.zeroize();
        }
        Ok(decoded?)
    }

    /// Send a typed `DaemonMessage` (convenience for servers).
    ///
    /// # Errors
    ///
    /// Returns any error propagated by [`Self::send_message`] or the payload
    /// encoding.
    pub fn send_daemon_message(&mut self, msg: &DaemonMessage) -> Result<(), TransportError> {
        let payload: Zeroizing<Vec<u8>> = encode_payload(msg)?;
        self.send_message(&payload)
    }

    /// Receive a typed `ClientMessage` (convenience for servers).
    ///
    /// # Errors
    ///
    /// Returns any error propagated by [`Self::recv_message`] or the payload
    /// decoding.
    pub fn recv_client_message(&mut self) -> Result<ClientMessage, TransportError> {
        let payload = self.recv_message()?;
        Ok(decode_frame(&payload)?)
    }

    /// Access the underlying `TcpStream` for shutdown etc.
    #[must_use]
    pub fn get_ref(&self) -> &TcpStream {
        &self.tcp
    }
}

/// Whether an inbound [`DaemonMessage`] may carry credential material, and so
/// must have its raw frame wiped after decoding.
///
/// This is the ONE place the receive path decides to skip the wipe, and it is a
/// **fail-safe allowlist**: only a variant whose payload is structurally
/// incapable of holding a secret is listed false, and anything else — including
/// every future variant and a frame that failed to decode — is treated as
/// secret and wiped. A missed entry therefore only costs the memset it was
/// trying to save, never a leaked credential.
///
/// [`DaemonMessageType::Image`] is the only exemption: it carries raw attachment
/// bytes (`Option<Vec<u8>>`) and is the largest payload the daemon sends (up to
/// the 64 MiB frame cap), so skipping its wipe turns the per-message zeroize
/// from a 64 MiB memset into nothing for the one bulk, provably non-secret case.
/// Every credential-bearing variant (`Credential`, `Keystore`, `Unlock`-adjacent
/// state, …) falls through to `true` and is wiped.
fn daemon_message_may_carry_secret(msg: &DaemonMessage) -> bool {
    !matches!(&msg.inner, DaemonMessageType::Image { .. })
}

#[cfg(test)]
mod tests {
    use snow::Builder;
    use x25519_dalek::{PublicKey, StaticSecret};

    /// The wipe exemption is a fail-safe allowlist: `Image` (raw attachment
    /// bytes) is the only variant that skips the wipe, while a credential-bearing
    /// variant and any unlisted variant are treated as secret. A memory-safety
    /// regression here would be a silent credential leak, so it is pinned.
    #[test]
    fn only_image_is_exempt_from_the_post_decode_wipe() {
        use choreo_proto::{DaemonMessage, DaemonMessageType, ImageKey};

        let image = DaemonMessage::broadcast(DaemonMessageType::Image {
            session_id: 1,
            turn_id: 1,
            key: ImageKey::Displayed { index: 0 },
            data: Some(vec![0u8; 8]),
        });
        assert!(
            !super::daemon_message_may_carry_secret(&image),
            "Image carries raw attachment bytes and is the one exempt bulk variant"
        );

        // A credential-bearing reply is always wiped...
        let credential = DaemonMessage::broadcast(DaemonMessageType::Credential {
            service: "anthropic".to_string(),
            key: Some("sk-secret".to_string()),
        });
        assert!(super::daemon_message_may_carry_secret(&credential));

        // ...and so is an arbitrary unlisted variant (the fail-safe default).
        let pong = DaemonMessage::broadcast(DaemonMessageType::Pong);
        assert!(
            super::daemon_message_may_carry_secret(&pong),
            "an unlisted variant must default to being wiped"
        );
    }

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
        // (client = initiator with server's public key, server = responder),
        // and the same prologue both sides of an IK handshake derive from
        // HandshakeMode::Ik — the prologue feeds the handshake hash the
        // transport keys come from, so omitting it would derive states a
        // production peer could not interop with.
        let mode = crate::handshake::HandshakeMode::Ik;
        let prologue = mode.prologue();
        // The pattern comes from the mode, never a copy: a hand-typed string
        // here would keep validating a pattern production no longer uses.
        let mut initiator = Builder::new(mode.pattern().parse().unwrap())
            .local_private_key(&client_sk)
            .unwrap()
            .remote_public_key(&server_pk)
            .unwrap()
            .prologue(&prologue)
            .unwrap()
            .build_initiator()
            .unwrap();
        let mut responder = Builder::new(mode.pattern().parse().unwrap())
            .local_private_key(&server_sk)
            .unwrap()
            .prologue(&prologue)
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
