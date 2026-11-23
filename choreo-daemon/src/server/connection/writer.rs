//! The connection's outbound path: the single-writer contract, the per-transport
//! sinks, and the one thread that drains a connection's queue.
//!
//! Every transport's writer is bounded by the SAME policy, stated once in
//! `choreo-proto`: an absolute per-message budget scaled by the frame size
//! ([`choreo_proto::message_budget`]) plus a per-syscall stall
//! ([`WRITER_WRITE_TIMEOUT`]). A large frame to a slow-but-alive client is allowed
//! the time its size implies, while a client that has stopped reading is noticed
//! promptly — whichever transport carried it. Before this policy was shared, the
//! Noise writer had it and the Unix writer had only a socket-level timeout armed
//! once at accept, so the same constant meant two different things.
//!
//! The stall is the mechanism that makes LAG EVICTION work without the daemon
//! holding a force-close handle on the connection (no retained socket clone, no
//! extra FD per connection): when the daemon evicts a lagging client it enqueues
//! the best-effort `Evicted` advisory and drops every sink; a healthy writer
//! flushes the advisory and closes its own socket (notify-before-EOF), while a
//! wedged writer hits the stall on its in-flight write, the write fails, and the
//! writer shuts the socket down itself — which unblocks the reader's blocking
//! read and runs the normal `cleanup_client` teardown. Either way the connection
//! is reaped promptly and its queued bytes released.

use std::net::Shutdown;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use choreo_proto::{
    DaemonMessage, DaemonMessageType, UnixStream, WRITER_STALL, encode_frame, encode_payload,
    message_budget, write_all_with_stall,
};
use tracing::warn;

use crate::broadcast::ClientId;
use crate::daemon::DaemonCommand;

/// Per-syscall stall applied to every connection's writer: how long one blocking
/// `write` syscall may wait for progress before the peer counts as wedged.
///
/// It is the daemon's own statement of [`choreo_proto::WRITER_STALL`] — the same
/// policy the client applies to its side of the connection — so the two ends
/// cannot drift into different detection latencies. The name stays because this
/// is the daemon's knob: `DaemonState::writer_write_timeout` carries it into every
/// connection thread, and a test substitutes a tiny value to exercise the
/// wedged-writer reap without waiting out the default.
///
/// Raising it above `choreo_proto::WRITER_STALL` has no effect on a small message:
/// the effective wait is `min(stall, message_budget(len))`, and the budget's floor
/// IS that constant. Lowering it (the only direction a test needs) is fully
/// effective.
pub(crate) const WRITER_WRITE_TIMEOUT: Duration = WRITER_STALL;

/// Bound for joining a connection's writer thread during cleanup.
///
/// Strictly greater than [`WRITER_WRITE_TIMEOUT`], and that ordering is
/// load-bearing: cleanup waits for a writer that may be mid-syscall for up to the
/// stall, so equal values would run out the join window in exactly the worst case
/// the join exists to cover and detach a writer that was about to exit cleanly. A
/// healthy writer exits immediately on channel disconnect (dropping the sink
/// disconnects its receiver), so the grace only ever matters for the wedged case.
pub(crate) const WRITER_JOIN_GRACE: Duration =
    WRITER_WRITE_TIMEOUT.saturating_add(Duration::from_secs(1));

// Compile-time, because nothing about the relationship is conditional: a smaller
// grace is a defect, and it must not be possible to introduce one by editing
// either constant.
const _: () = assert!(
    WRITER_JOIN_GRACE.as_nanos() > WRITER_WRITE_TIMEOUT.as_nanos(),
    "WRITER_JOIN_GRACE must outlast the writer stall it waits out"
);

/// Smallest stall a socket will accept.
///
/// `std` rejects a zero duration outright (`ErrorKind::InvalidInput`, "cannot set
/// a 0 duration timeout") rather than reading it as "no timeout", which is the
/// opposite of what a `setsockopt`-era reader expects from `SO_SNDTIMEO = 0`. A
/// zero reaching a writer would therefore fail the very first write and tear the
/// connection down; the floor turns that into a one-millisecond bound instead. One
/// millisecond is the smallest value the kernel reliably honours (a smaller
/// timeout rounds up to it anyway).
const MIN_WRITER_STALL: Duration = Duration::from_millis(1);

/// Validate a configured writer stall, flooring it at [`MIN_WRITER_STALL`].
///
/// The single seam every stall passes through on its way to a socket timeout, so
/// neither sink can arm a zero. A config that resolves to zero is a mistake rather
/// than a request to disable the bound — the bound is what reaps a wedged writer —
/// so it is floored and warned about rather than propagated.
pub(crate) fn writer_stall(configured: Duration) -> Duration {
    if configured < MIN_WRITER_STALL {
        warn!(
            configured = ?configured,
            floor = ?MIN_WRITER_STALL,
            "writer stall is below the socket's minimum; flooring it (a zero timeout is rejected by the kernel, not read as unbounded)"
        );
        return MIN_WRITER_STALL;
    }
    configured
}

/// A per-connection message sink implementing the single-writer contract.
///
/// Both transports (Unix socket and TCP/Noise) implement this so the writer
/// thread loop in [`writer_thread`] lives in exactly one place. The
/// `ShuttingDown` special case — flush the notification, close the socket,
/// stop draining — is what makes notify-before-EOF deterministic: the thread
/// that writes the message is the same thread that closes the socket.
pub(crate) trait ConnectionWriter {
    /// Serialize and send one message. Errors are fatal for the connection
    /// (the socket is broken) — the caller stops draining.
    fn send_message(&mut self, msg: &DaemonMessage) -> Result<(), String>;
    /// Close the underlying socket (both directions).
    fn shutdown(&mut self);
}

/// The local-domain socket's writer sink.
///
/// One bounded write per frame, with nothing buffered in between: a `BufWriter`
/// here would only add a memcpy (the frame is handed over whole and flushed
/// immediately) and, worse, would put the syscall that matters behind a hop the
/// per-syscall re-arm cannot see — `write_all_with_stall` arms the socket, so it
/// must be the socket that is written to.
pub(crate) struct UnixConnectionWriter {
    stream: UnixStream,
    /// Per-syscall stall, already through [`writer_stall`].
    stall: Duration,
}

impl UnixConnectionWriter {
    /// Wrap `stream`, validating the stall through [`writer_stall`].
    pub(crate) fn new(stream: UnixStream, stall: Duration) -> Self {
        Self {
            stream,
            stall: writer_stall(stall),
        }
    }
}

impl ConnectionWriter for UnixConnectionWriter {
    fn send_message(&mut self, msg: &DaemonMessage) -> Result<(), String> {
        // `encode_frame` yields the complete length-prefixed frame, so this is one
        // bounded write with nothing left to flush afterwards.
        let frame = encode_frame(msg).map_err(|e| e.to_string())?;
        let deadline = Instant::now() + message_budget(frame.len());
        write_all_with_stall(&mut self.stream, deadline, self.stall, &frame)
            .map_err(|e| e.to_string())
    }
    fn shutdown(&mut self) {
        let _ = self.stream.shutdown(Shutdown::Both);
    }
}

/// The Noise transport's writer sink.
///
/// Every outbound message is bounded exactly like the Unix sink's, and like the
/// client's: one absolute per-message budget scaled by the frame size (a large
/// `Image` reply is allowed the time its size implies on a slow-but-alive link)
/// plus a per-syscall stall (a peer that stops reading entirely is noticed
/// promptly). Without the whole-message budget, a large frame to a wedged peer
/// could occupy the writer — and the teardown that joins it — for far longer than
/// the stall alone suggests.
pub(crate) struct NoiseConnectionWriter {
    stream: choreo_transport::noise::NoiseStream,
    /// Per-syscall stall, already through [`writer_stall`].
    stall: Duration,
}

impl NoiseConnectionWriter {
    /// Wrap `stream`, validating the stall through [`writer_stall`].
    pub(crate) fn new(stream: choreo_transport::noise::NoiseStream, stall: Duration) -> Self {
        Self {
            stream,
            stall: writer_stall(stall),
        }
    }
}

impl ConnectionWriter for NoiseConnectionWriter {
    fn send_message(&mut self, msg: &DaemonMessage) -> Result<(), String> {
        // Encode the payload once, then hand it to the deadline-bounded send: the
        // whole-message budget lets a large frame finish on a slow-but-alive link
        // while the stall notices a peer that stops reading entirely.
        let payload = encode_payload(msg).map_err(|e| e.to_string())?;
        let deadline = Instant::now() + message_budget(payload.len());
        self.stream
            .send_message_by(&payload, deadline, self.stall)
            .map_err(|e| e.to_string())
    }
    fn shutdown(&mut self) {
        let _ = self.stream.get_ref().shutdown(Shutdown::Both);
    }
}

/// The embedded (in-process) transport's writer: forward the message as a
/// Rust VALUE over a channel instead of serializing + encrypting it.
///
/// The embedded connection never becomes bytes: `ClientMessage`s travel
/// GUI→daemon as values over one channel, `DaemonMessage`s daemon→GUI as
/// values over the writer's target channel. `send_message` therefore just
/// forwards `msg.clone()` — the clone is the price of the `&msg` signature,
/// and it is strictly cheaper than the socket path's msgpack encode +
/// AES-GCM encrypt + syscall per message.
pub(crate) struct ChannelConnectionWriter {
    /// Forward target = the GUI's read half. Wrapped in an Option so
    /// [`ConnectionWriter::shutdown`] can DROP it: a dropped sender closes the
    /// receiver immediately — the channel analogue of `Shutdown::Both` — which
    /// is what preserves notify-before-close (the writer thread forwards the
    /// special-cased `ShuttingDown`/`Evicted` FIRST, then calls `shutdown()`,
    /// then breaks; the GUI observes the value, then `Err` on the next recv).
    tx: Option<crossbeam_channel::Sender<DaemonMessage>>,
}

impl ChannelConnectionWriter {
    pub(crate) fn new(tx: crossbeam_channel::Sender<DaemonMessage>) -> Self {
        Self { tx: Some(tx) }
    }
}

impl ConnectionWriter for ChannelConnectionWriter {
    fn send_message(&mut self, msg: &DaemonMessage) -> Result<(), String> {
        match &self.tx {
            Some(tx) => tx.send(msg.clone()).map_err(|_| {
                // The receiver (GUI read half) is gone — the embedded client
                // dropped its link. Same "connection is broken" class as a
                // broken pipe on the socket paths.
                "embedded client receiver dropped".to_string()
            }),
            None => Err("embedded writer already shut down".to_string()),
        }
    }
    fn shutdown(&mut self) {
        // Dropping the sender closes the GUI's receiver immediately (see the
        // field docs): the embedded analogue of closing the socket, ordered
        // AFTER the ShuttingDown/Evicted flush by the shared writer_thread.
        self.tx = None;
    }
}

/// Drain a connection's writer channel — the connection's SOLE writer.
///
/// Each connection has exactly one writer thread, so messages on `rx` are
/// serialized and fragments of one logical message can never interleave.
/// `ShuttingDown` and `Evicted` are special-cased identically: each is
/// flushed, then the socket is closed HERE (by the writer thread itself), so
/// the client observes the notification before the EOF with no other thread
/// ever writing to or closing the socket. (`ShuttingDown` is only ever enqueued
/// by the daemon's shutdown broadcast; Evicted by a lag eviction.) An error at
/// any point stops the loop and SHUTS THE SOCKET DOWN — a send error can be a
/// broken pipe (socket gone) or a [`WRITER_WRITE_TIMEOUT`] on a wedged client
/// whose receive window is zero (socket still open); either way, shutting
/// down unblocks the reader's blocking read so `cleanup_client` reaps the
/// connection (shutdown on an already-broken socket is a harmless no-op).
///
/// Byte accounting: on EACH dequeue the per-client and daemon-wide lag
/// counters are decremented by the message's approximate wire size, the exact
/// counterpart of [`crate::broadcast::SubscriberSink::enqueue`]'s increment.
/// Decrementing even on a failed send keeps the daemon-wide backlog honest — the
/// bytes left the queue regardless of whether the socket accepted them, and the
/// connection is being torn down either way. Whatever is still QUEUED when the
/// loop stops (send error, or the `Evicted`/`ShuttingDown` stop) is drained below
/// the loop and decremented too, so an abandoned backlog can never stay
/// frozen in the daemon-wide counter and silently eat the global budget.
pub(crate) fn writer_thread<W: ConnectionWriter>(
    mut writer: W,
    rx: &crossbeam_channel::Receiver<DaemonMessage>,
    bytes: &Arc<AtomicUsize>,
    global: &Arc<AtomicUsize>,
) {
    for msg in rx {
        let size = msg.approx_wire_size();
        // Mirror the producer's per-client accounting split: a solicited
        // `Image` reply never incremented `bytes` (see
        // `broadcast::counts_toward_client_lag`), so it must not decrement it
        // here either — only the daemon-wide counter tracks those bytes. The
        // predicate is keyed on the message variant exactly like the producer's
        // increment, so the per-client counter stays balanced.
        let counts_client = crate::broadcast::counts_toward_client_lag(&msg);
        if let Err(e) = writer.send_message(&msg) {
            warn!("writer thread error: {e}");
            // The failing message still left the queue — account it so the
            // backlog reflects what is actually still queued, then stop.
            if counts_client {
                bytes.fetch_sub(size, Ordering::Relaxed);
            }
            global.fetch_sub(size, Ordering::Relaxed);
            writer.shutdown();
            break;
        }
        if counts_client {
            bytes.fetch_sub(size, Ordering::Relaxed);
        }
        global.fetch_sub(size, Ordering::Relaxed);
        if matches!(
            msg.inner,
            DaemonMessageType::ShuttingDown | DaemonMessageType::Evicted
        ) {
            writer.shutdown();
            break;
        }
    }
    // Drain-and-decrement whatever is still queued: after a send error or a
    // ShuttingDown/Evicted stop, the socket is closed and these messages will
    // never be written — but they were all counted at enqueue. Subtracting
    // them here keeps the daemon-wide total honest (the per-client counter
    // dies with the sink, but `global` is shared across every client: an
    // evicted client's abandoned backlog would otherwise stay frozen in it
    // forever and, accumulated across evictions, permanently exhaust the
    // global budget — cascading evictions of healthy clients). The drain is
    // non-blocking on purpose: the writer must exit promptly so the receiver
    // drops and any producer that enqueues after this point gets a failed
    // send, which it self-corrects (see `SubscriberSink::send_accounted`).
    //
    // The one residual race, bounded and accepted: a producer whose `send`
    // lands in the microsecond window between this drain's last pass and the
    // receiver being dropped (at function return) SUCCEEDS — the receiver is
    // still alive — and that message is never dequeued, so its bytes stay in
    // the daemon-wide counter forever. The leak is bounded to whatever a
    // producer manages to enqueue in that window — in practice zero or one
    // message (the daemon removes the sink from its maps in the same command
    // that starts this teardown, so no producer keeps broadcasting to it
    // beyond a straggler or two) — and a producer that sends after the
    // receiver is gone self-corrects, so the accounting stays honest to
    // within that tiny, event-bounded slack. It is not a strict one-message
    // guarantee, but it is never an unbounded stream.
    for msg in rx.try_iter() {
        let size = msg.approx_wire_size();
        // Same split as the dequeue path: only the daemon-wide counter tracks an
        // `Image` reply (the per-client counter never saw it).
        if crate::broadcast::counts_toward_client_lag(&msg) {
            bytes.fetch_sub(size, Ordering::Relaxed);
        }
        global.fetch_sub(size, Ordering::Relaxed);
    }
}

/// Create a connection's writer channel and register it with the daemon,
/// returning the client id and both channel ends for the connection thread.
///
/// Registration happens HERE — in the acceptor, BEFORE the connection thread
/// is spawned — so a connection accepted concurrently with shutdown is
/// guaranteed to receive `ShuttingDown`:
///
/// * Unix: the accept loop registers (then spawns) before it can observe the
///   shutdown flag and break out to broadcast, so the register command is
///   enqueued before the broadcast on the same FIFO command channel.
/// * TCP: the accept thread registers before spawning the handshake thread,
///   and `run_server` joins the accept thread BEFORE broadcasting, so the
///   register (sent strictly before the accept thread exited) is ordered
///   before the broadcast in the command channel.
///
/// If registration were deferred to inside the connection thread, a handshake
/// still in flight when shutdown began could land its register after the
/// broadcast was processed — and that client would miss the notification.
pub(crate) fn register_client_writer(
    daemon_tx: &crossbeam_channel::Sender<DaemonCommand>,
) -> (
    ClientId,
    crate::broadcast::SubscriberSink,
    crossbeam_channel::Receiver<DaemonMessage>,
) {
    let (writer_tx, writer_rx) = crossbeam_channel::unbounded::<DaemonMessage>();
    let sink = crate::broadcast::SubscriberSink::new(writer_tx);
    // Mint the connection's id from the process-wide monotonic counter (see
    // `ClientId`): small, readable, collision-free for a daemon's lifetime.
    let client_id = ClientId::next();
    let _ = daemon_tx.send(DaemonCommand::RegisterClientWriter {
        client_id,
        writer: sink.clone(),
    });
    (client_id, sink, writer_rx)
}
