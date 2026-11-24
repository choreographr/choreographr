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
//! `slot` owns the per-direction single-actor right both directions take, and
//! [`wire`] publishes the data plane's fragment sizes and framing overhead as a
//! wire contract for anything that frames a message itself.
//!
//! The two directions share ONE stall — the connection's per-syscall detection
//! latency, applied to every write and to every read of a message already in
//! flight — and neither bounds an idle connection. A send additionally derives a
//! whole-message budget from the wire bytes it is about to write; a receive clears
//! the socket timeout and reads the first byte of the next message with no bound at
//! all (an idle connection is a normal state, not a fault) before the stall applies
//! to everything after it. Neither puts a wall clock on a legitimate slow transfer,
//! and a peer that vanished has its own bound in the socket's TCP keepalive.
//!
//! The stall, the snow `TransportState` and the post-failure `unusable` flag are
//! all connection-scoped, so they live in ONE `Shared` record behind one
//! `Arc<Mutex<…>>`: a connection has one reader thread and one writer thread over
//! a `try_clone`d socket (see AGENTS.md's sanctioned shared-state exception for
//! the transport — the `unusable` flag rides inside that already-sanctioned
//! record rather than adding an exception of its own), and per-direction
//! exclusivity is a single-bit `AtomicBool` in the spirit of the cancellation-flag
//! exception.
//!
//! A transfer that fails part-way leaves the encrypted byte stream at an unknown
//! offset, so the failure poisons the connection: every later send and receive is
//! refused with [`crate::error::TransportError::StreamUnusable`] rather than
//! silently desyncing the peer. The check sits after each direction's slot and
//! before its first socket call, so only a failed transfer poisons — never a
//! caller-side refusal that leaves the wire untouched.

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

/// The data plane's wire figures: fragment sizes, framing overhead, and the
/// authenticated continuation bit.
///
/// Public because they are a wire contract, not an implementation detail: a test
/// harness (or a peer implementation) that drives `snow` directly must chunk at
/// [`MAX_PLAINTEXT_CHUNK`](wire::MAX_PLAINTEXT_CHUNK) and budget for
/// [`GCM_TAG_LEN`](wire::GCM_TAG_LEN) plus
/// [`FRAGMENT_HEADER_LEN`](wire::FRAGMENT_HEADER_LEN) using THESE values, never a
/// local copy that could drift from what the crate actually puts on the wire —
/// the same reason [`crate::handshake::MAX_HANDSHAKE_FRAME`] is public.
pub mod wire {
    pub use super::frame::{
        FRAGMENT_CONTINUATION, FRAGMENT_HEADER_LEN, GCM_TAG_LEN, LEN_PREFIX_LEN,
        MAX_CIPHERTEXT_LEN, MAX_FRAGMENTS_PER_MESSAGE, MAX_PLAINTEXT_CHUNK,
    };
}

/// The state every clone of one connection shares: the snow transport, the
/// connection's one stall, and whether a failed transfer has made the stream
/// unusable.
///
/// One record rather than three fields on [`NoiseStream`], so the three cannot
/// drift apart across a `try_clone`: a stall set through any handle is the stall
/// every handle reads on its next message, and a failure in either direction
/// poisons the whole connection — the wire is ONE byte stream, so a half-written
/// or half-read frame leaves both directions at an unknown offset.
struct Shared {
    /// The snow transport. The lock is held only around a single encrypt or
    /// decrypt call, never across a blocking socket operation (see
    /// `send::write_frame` and `recv::recv_message_body`).
    transport: TransportState,
    /// The connection's per-syscall detection latency in **both directions**: a
    /// read applies it once a message is in flight, a write applies it to every
    /// syscall. Defaults to [`choreo_proto::CONNECTION_STALL`] and is settable at
    /// any time through [`NoiseStream::set_stall`].
    ///
    /// A **stall**, not a whole-message deadline, and deliberately so on both
    /// sides. On the receive side a message's total size is unknown until its last
    /// fragment arrives, so there is no figure to scale a whole-message budget
    /// from. On the send side there is (the wire length sizes one), but the stall
    /// still is not a deadline: a peer that keeps making progress passes however
    /// long the transfer legitimately takes, and one that stops is caught here
    /// instead of only when a large frame's minutes-long budget runs out. The idle
    /// wait *between* messages is unbounded and lives outside this bound (see
    /// `recv::recv_message_plain`).
    stall: Duration,
    /// Set once a send or receive fails, so the connection's byte stream is at an
    /// unknown offset. Every later transfer is refused with
    /// [`crate::error::TransportError::StreamUnusable`] instead of silently
    /// desyncing the peer.
    unusable: bool,
}

/// An encrypted Noise transport stream.
///
/// Wraps a `TcpStream` with a snow `TransportState`. Every message is
/// length-prefixed (4-byte BE u32) and encrypted with AES-256-GCM. The stream
/// is what a completed handshake produces and does not itself know which
/// pattern ran — IK and XX both end here (see [`crate::handshake`]).
///
/// Everything connection-scoped — the `TransportState`, the one stall and the
/// `unusable` flag — is behind one `Arc<Mutex<>>` (see the `Shared` record) so the
/// stream can be cloned for concurrent reader/writer threads via
/// [`Self::try_clone`] without any of the three being copied per handle.
pub struct NoiseStream {
    tcp: TcpStream,
    shared: Arc<Mutex<Shared>>,
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
    /// pays for it. One bounded write sends the prefix + ciphertext together.
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
    ///
    /// The connection starts at [`choreo_proto::CONNECTION_STALL`] and usable.
    pub fn new(tcp: TcpStream, transport: TransportState) -> Self {
        tracing::trace!("NoiseStream created");
        NoiseStream {
            tcp,
            shared: Arc::new(Mutex::new(Shared {
                transport,
                stall: choreo_proto::CONNECTION_STALL,
                unusable: false,
            })),
            send_slot: SingleSlot::new("send_message"),
            recv_slot: SingleSlot::new("recv_message"),
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
    ///
    /// Only the underlying `TcpStream` is cloned. Everything connection-scoped is
    /// SHARED with the original, never copied: the `TransportState` (so
    /// encrypt/decrypt calls are serialized), both single-actor slots (so two
    /// clones still cannot send, or receive, at once), the connection's one stall
    /// and the `unusable` flag. Both halves of one connection therefore give up on
    /// a wedged peer at the same latency, and a failure in either direction
    /// refuses later transfers on every handle.
    ///
    /// # Errors
    ///
    /// Returns `std::io::Error` if the underlying stream cannot be cloned.
    pub fn try_clone(&self) -> std::io::Result<Self> {
        Ok(NoiseStream {
            tcp: self.tcp.try_clone()?,
            shared: Arc::clone(&self.shared),
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
        })
    }

    /// Override the connection's per-syscall stall, flooring it at
    /// [`choreo_proto::MIN_SOCKET_STALL`].
    ///
    /// The default is [`choreo_proto::CONNECTION_STALL`], which is what a
    /// production connection wants in both directions. It is settable so a caller
    /// can reuse its own configured knob (the daemon hands its `peer_stall` to the
    /// stream) and so a test can shrink it instead of waiting out five seconds. A
    /// value below the socket minimum is raised to it rather than armed: a zero
    /// socket timeout is rejected by the kernel, not read as "unbounded" (see
    /// [`choreo_proto::effective_stall`]).
    ///
    /// There is ONE stall per connection, held in the connection's shared record,
    /// so it may be set before or after [`Self::try_clone`] and every handle reads
    /// the same value on its next message. That is also why this takes `&self`: the
    /// write goes through the shared record, not through this handle.
    pub fn set_stall(&self, stall: Duration) {
        // A brief write to the shared record — the same record the encrypt and
        // decrypt sites lock per chunk — so it is never held across I/O: a reader
        // waiting on this lock would stop draining the socket.
        self.shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .stall = choreo_proto::effective_stall(stall);
    }

    /// The connection's per-syscall stall.
    ///
    /// Read ONCE at the entry point of a message and threaded down to the framing
    /// loop, so the shared record is locked once per message rather than once per
    /// syscall, and never inside the encrypt/decrypt scope (where the lock must be
    /// released before any blocking socket call).
    fn stall(&self) -> Duration {
        self.shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .stall
    }

    /// Whether a failed transfer has already made this connection's byte stream
    /// unusable.
    fn is_unusable(&self) -> bool {
        self.shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .unusable
    }

    /// Record that a transfer failed, refusing every later send and receive on
    /// this connection.
    ///
    /// Deliberately conservative: the rule is "a failed transfer makes the stream
    /// unusable", including a failure that happened before any byte moved (an
    /// already-elapsed deadline). No error in either direction proves where the
    /// byte stream stands, and a caller that ignored a desynced stream would have
    /// its next frame silently misparsed by the peer. Callers already close the
    /// connection on any transport error, so the only cost of poisoning a
    /// harmless failure is a reconnect that was happening anyway.
    fn mark_unusable(&self) {
        // Same brief-lock rule as `stall`: one flag write, never across I/O.
        self.shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .unusable = true;
    }

    /// Access the underlying `TcpStream` for shutdown etc.
    #[must_use]
    pub fn get_ref(&self) -> &TcpStream {
        &self.tcp
    }
}

#[cfg(test)]
mod tests;
