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
mod tests;
