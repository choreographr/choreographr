//! The encrypted Noise transport stream and its two directions.
//!
//! [`NoiseStream`] length-prefixes (4-byte big-endian) and fragments plaintext
//! into AES-256-GCM messages, with an authenticated per-fragment continuation
//! byte driving reassembly. The stream core here is protocol-agnostic — it moves
//! opaque byte frames — and only construction and cloning live in this module:
//! the typed `ClientMessage`/`DaemonMessage` conveniences are in `typed`, and
//! each direction lives in its own submodule: `send` owns framing, encryption and
//! the outbound write, `recv` owns reassembly, decryption and the peer-close
//! classification, `frame` owns the fragment sizes and the length-prefix codec,
//! and `reassembly` owns the message-level byte and fragment-count bounds.
//! `slot` owns the per-direction single-actor right both directions take.
//!
//! The two directions bound a stalled peer the same way and an idle connection not
//! at all. A send uses the caller's deadline and stall; a receive clears the socket
//! timeout, reads the first byte of the next message with no bound (an idle
//! connection is a normal state, not a fault) and then applies [`NoiseStream`]'s
//! `read_stall` to everything after it. Neither puts a wall clock on a legitimate
//! slow transfer, and a peer that vanished has its own bound in the socket's TCP
//! keepalive.
//!
//! The `TransportState` is shared through an `Arc<Mutex<…>>` because a
//! connection has one reader thread and one writer thread over a
//! `try_clone`d socket (see AGENTS.md's sanctioned shared-state exception for
//! the transport), and per-direction exclusivity is a single-bit `AtomicBool` in
//! the spirit of the cancellation-flag exception.

mod frame;
mod reassembly;
mod recv;
mod send;
mod slot;
mod typed;

use std::net::TcpStream;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use snow::TransportState;

use slot::SingleSlot;

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
    /// The outbound single-writer slot, shared across clones: a concurrent
    /// second `send_message` on one stream would interleave fragments of
    /// different logical messages on the wire (silent corruption). The slot
    /// turns that into a loud protocol error instead.
    send_slot: SingleSlot,
    /// The inbound single-reader slot, likewise shared: two concurrent
    /// `recv_message` calls would splice two logical messages together, which
    /// is just as silent. The daemon and the client give a connection one
    /// reader thread, so this only ever fires on a misuse.
    recv_slot: SingleSlot,
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
    /// The receive direction's no-progress bound: a read fails if it accepts
    /// nothing for this long. Defaults to [`choreo_proto::CONNECTION_STALL`], the
    /// same detection latency the writer applies to the same connection, and is
    /// settable so the daemon can inject a shorter one under test.
    ///
    /// This is a **stall**, not a deadline, and that is deliberate: a message's
    /// total size is unknown until its last fragment arrives, so there is no figure
    /// to scale a whole-message budget from, and any fixed wall clock would kill a
    /// legitimate large transfer on a slow link. A peer that keeps making progress
    /// passes however long the message takes; one that stops is caught here. The
    /// idle wait *between* messages is unbounded and lives outside this bound (see
    /// `recv::recv_message_plain`).
    read_stall: Duration,
}

impl NoiseStream {
    /// Create a new `NoiseStream` wrapping a `TcpStream` and `TransportState`.
    pub fn new(tcp: TcpStream, transport: TransportState) -> Self {
        tracing::trace!("NoiseStream created");
        NoiseStream {
            tcp,
            transport: Arc::new(Mutex::new(transport)),
            send_slot: SingleSlot::new("send_message"),
            recv_slot: SingleSlot::new("recv_message"),
            // All four reusable buffers start empty; send_message/recv_message
            // grow the ones they actually use to their worst-case size on
            // first use (see `frame::FRAME_BUF_LEN`/`FRAG_BUF_LEN`).
            send_buf: Vec::new(),
            send_frag: Vec::new(),
            recv_ct_buf: Vec::new(),
            recv_pt_buf: Vec::new(),
            read_stall: choreo_proto::CONNECTION_STALL,
        }
    }

    /// Clone the stream for concurrent reader/writer threads.
    /// The underlying `TcpStream` is cloned; the `TransportState` and both
    /// single-actor slots are shared, so encrypt/decrypt calls are serialized
    /// and two clones still cannot send (or receive) at once.
    ///
    /// # Errors
    ///
    /// Returns `std::io::Error` if the underlying stream cannot be cloned.
    pub fn try_clone(&self) -> std::io::Result<Self> {
        Ok(NoiseStream {
            tcp: self.tcp.try_clone()?,
            transport: Arc::clone(&self.transport),
            send_slot: self.send_slot.clone(),
            recv_slot: self.recv_slot.clone(),
            // Buffer reuse is per-direction: a reader clone never sends and a
            // writer clone never receives, so each clone starts the other
            // side's buffers empty and never grows them — no wasted memory on
            // unused sides.
            send_buf: Vec::new(),
            send_frag: Vec::new(),
            recv_ct_buf: Vec::new(),
            recv_pt_buf: Vec::new(),
            // The bound is a property of the connection, not of one direction's
            // buffer, so a clone inherits it.
            read_stall: self.read_stall,
        })
    }

    /// Override the receive direction's no-progress bound.
    ///
    /// The default is [`choreo_proto::CONNECTION_STALL`], which is what a
    /// production connection wants on both sides — the same detection latency its
    /// writer applies. It is settable so a caller can reuse its own configured knob
    /// (the daemon hands its `peer_stall` to both directions) and so a test can
    /// shrink it instead of waiting out five seconds.
    pub fn set_read_stall(&mut self, stall: Duration) {
        self.read_stall = stall;
    }

    /// Access the underlying `TcpStream` for shutdown etc.
    #[must_use]
    pub fn get_ref(&self) -> &TcpStream {
        &self.tcp
    }
}

#[cfg(test)]
mod tests;
