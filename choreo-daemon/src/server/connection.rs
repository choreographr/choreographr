use crate::daemon::DaemonCommand;
use crate::sessions::SessionCommand;
use choreo_proto::{
    ClientMessage, ClientMessageType, ContextConfig, DaemonMessage, DaemonMessageType, MessageKind,
    ProtoError, SessionEvent, read_message, write_message,
};
use std::io::{self, BufReader, BufWriter, Write};
use std::net::{Shutdown, TcpStream};
#[cfg(unix)]
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use tracing::{debug, error, info, warn};
#[cfg(windows)]
use uds_windows::UnixStream;

/// Bound for joining a connection's writer thread during cleanup. A healthy
/// writer exits immediately on channel disconnect (dropping `writer_tx` in
/// `cleanup_client` disconnects `writer_rx`); the grace covers a writer
/// wedged in a blocking socket write — a client that is open but not reading
/// — which cannot exit on the channel disconnect alone (see the comment at
/// the call site in `cleanup_client`).
const WRITER_JOIN_GRACE: Duration = Duration::from_secs(5);

/// Socket write timeout applied to every connection's writer. Bounds a single
/// blocking `write` syscall so a wedged client — one whose socket receive
/// window is permanently zero — cannot stall its writer thread forever.
///
/// This is the mechanism that makes LAG EVICTION work without the daemon
/// holding a force-close handle on the connection (no retained socket clone,
/// no extra FD per connection): when the daemon evicts a lagging client it
/// enqueues the best-effort `Evicted` advisory and drops every sink; a
/// healthy writer flushes the advisory and closes its own socket (notify-
/// before-EOF), while a wedged writer hits this timeout on its in-flight
/// write, the write fails, and the writer shuts the socket down itself —
/// which unblocks the reader's blocking read and runs the normal
/// `cleanup_client` teardown. Either way the connection is reaped promptly
/// and its queued bytes released.
///
/// A slow-but-alive client is never falsely killed: the timeout is per
/// syscall, so a socket that makes any progress (each write completes in
/// under this) survives; only a client that stops reading entirely trips it,
/// which is exactly the lag condition eviction targets.
pub(crate) const WRITER_WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// A per-connection message sink implementing the single-writer contract.
///
/// Both transports (Unix socket and TCP/Noise) implement this so the writer
/// thread loop in [`writer_thread`] lives in exactly one place. The
/// `ShuttingDown` special case — flush the notification, close the socket,
/// stop draining — is what makes notify-before-EOF deterministic: the thread
/// that writes the message is the same thread that closes the socket.
trait ConnectionWriter {
    /// Serialize and send one message. Errors are fatal for the connection
    /// (the socket is broken) — the caller stops draining.
    fn send_message(&mut self, msg: &DaemonMessage) -> Result<(), String>;
    /// Close the underlying socket (both directions).
    fn shutdown(&mut self);
}

impl ConnectionWriter for BufWriter<UnixStream> {
    fn send_message(&mut self, msg: &DaemonMessage) -> Result<(), String> {
        write_message(self, msg).map_err(|e| e.to_string())?;
        self.flush().map_err(|e| e.to_string())
    }
    fn shutdown(&mut self) {
        let _ = self.get_ref().shutdown(Shutdown::Both);
    }
}

impl ConnectionWriter for choreo_transport::noise::NoiseStream {
    fn send_message(&mut self, msg: &DaemonMessage) -> Result<(), String> {
        self.send_daemon_message(msg).map_err(|e| e.to_string())
    }
    fn shutdown(&mut self) {
        let _ = self.get_ref().shutdown(Shutdown::Both);
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
struct ChannelConnectionWriter {
    /// Forward target = the GUI's read half. Wrapped in an Option so
    /// [`shutdown`] can DROP it: a dropped sender closes the receiver
    /// immediately — the channel analogue of `Shutdown::Both` — which is
    /// what preserves notify-before-close (the writer thread forwards the
    /// special-cased `ShuttingDown`/`Evicted` FIRST, then calls
    /// `shutdown()`, then breaks; the GUI observes the value, then `Err`
    /// on the next recv).
    tx: Option<crossbeam_channel::Sender<DaemonMessage>>,
}

impl ChannelConnectionWriter {
    fn new(tx: crossbeam_channel::Sender<DaemonMessage>) -> Self {
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
/// counterpart of [`SubscriberSink::enqueue`]'s increment. Decrementing even
/// on a failed send keeps the daemon-wide backlog honest — the bytes left the
/// queue regardless of whether the socket accepted them, and the connection
/// is being torn down either way. Whatever is still QUEUED when the loop
/// stops (send error, or the `Evicted`/`ShuttingDown` stop) is drained below
/// the loop and decremented too, so an abandoned backlog can never stay
/// frozen in the daemon-wide counter and silently eat the global budget.
fn writer_thread<W: ConnectionWriter>(
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
    // send, which it self-corrects (see [`SubscriberSink::send_accounted`]).
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
    u64,
    crate::broadcast::SubscriberSink,
    crossbeam_channel::Receiver<DaemonMessage>,
) {
    let (writer_tx, writer_rx) = crossbeam_channel::unbounded::<DaemonMessage>();
    let sink = crate::broadcast::SubscriberSink::new(writer_tx);
    let client_id = rand::random::<u64>();
    let _ = daemon_tx.send(DaemonCommand::RegisterClientWriter {
        client_id,
        writer: sink.clone(),
    });
    (client_id, sink, writer_rx)
}

/// Owns the obligation to answer exactly one request.
///
/// [`send`](Self::send) consumes the handle; dropping an unused handle trips
/// the debug guard, so a connection-thread handler cannot silently forget its
/// reply — "exactly one reply" is enforced, not merely commented. One handle is
/// minted per in-scope request from the acting connection's context (see
/// [`ClientCtx::reply_handle`]) and passed by value to the handler that
/// computes the reply inline.
///
/// Handlers whose reply is produced OFF the connection thread (a session thread
/// or the daemon command loop) mint an owned [`crate::broadcast::ReplyTarget`]
/// instead (see [`ClientCtx::reply_target`]); this guard shares the same
/// [`crate::broadcast::ReplySink`] mechanism but adds the exactly-once guard and
/// can only answer on the connection thread.
struct ReplyHandle {
    /// The shared reply mechanism (request id + delivery sink + lag counter).
    sink: crate::broadcast::ReplySink,
    /// Whether the reply obligation has been met (sent) or deliberately
    /// abandoned. `false` only on the drop-unanswered path the guard targets.
    sent: bool,
}

impl ReplyHandle {
    /// Answer the request: stamp `id` onto `inner` and enqueue the reply.
    ///
    /// Routes through the shared [`crate::broadcast::ReplySink`] send (the
    /// no-threshold path) because a reply is a request/response contract that
    /// must never be dropped for lag.
    fn send(mut self, inner: DaemonMessageType) {
        // Mark satisfied BEFORE the send so the guard is met even if the send
        // path is unwound through: the enqueue on an unbounded channel cannot
        // fail, but keeping the flag set first makes the invariant local.
        self.sent = true;
        self.sink.send(inner);
    }

    /// Relinquish the reply obligation WITHOUT sending.
    ///
    /// Reserved for the daemon-disconnected path: the command loop that would
    /// have produced the reply is gone and the connection is being torn down,
    /// so there is nobody left to answer. Marking the handle satisfied keeps
    /// the drop guard meaningful — it must fire on a *forgotten* reply, never
    /// on an impossible one.
    fn abandon(mut self) {
        self.sent = true;
    }
}

impl Drop for ReplyHandle {
    fn drop(&mut self) {
        debug_assert!(self.sent, "request {} left unacknowledged", self.sink.id());
    }
}

/// Shared per-client context passed through the dispatch and handler functions.
/// Bundles the channels and mutable per-connection state into one struct so
/// the call sites don't pass 5–6 individual arguments to every function.
struct ClientCtx<'a> {
    /// This connection's delivery sink.
    writer: &'a crate::broadcast::SubscriberSink,
    /// This connection's own redb handle, shared with every other connection
    /// (redb's `Database` is built for concurrent readers). Used to serve
    /// on-demand image reads DIRECTLY on the connection thread instead of
    /// serializing them on the command loop.
    db: &'a redb::Database,
    /// Daemon-wide lag counter, shared by every connection; replies must
    /// increment it so the writer thread's per-dequeue decrement stays
    /// balanced. Held as the `Arc` (not a `&AtomicUsize`) so a
    /// [`crate::broadcast::ReplyTarget`] can clone it and cross to another
    /// thread.
    global_lag: &'a Arc<AtomicUsize>,
    daemon_tx: &'a crossbeam_channel::Sender<DaemonCommand>,
    attached_session_id: &'a mut Option<u64>,
    attached_session_tx: &'a mut Option<crossbeam_channel::Sender<SessionCommand>>,
    client_id: u64,
    /// The correlation id of the request currently being dispatched on this
    /// connection — the value stamped onto every reply. Set once at dispatch
    /// entry from the inbound [`ClientMessage::id`]; a connection thread
    /// handles one request at a time, so a single field is sufficient.
    request_id: u64,
    /// Whether this connection arrived over the local Unix socket (vs the
    /// TCP/Noise listener). Trust-boundary input for local-only commands:
    /// `AclAdd` is refused on TCP because the approver for a trust decision
    /// must be at the machine, not on the network.
    is_unix: bool,
}

impl ClientCtx<'_> {
    /// Mint the [`ReplyHandle`] for the request currently being dispatched.
    ///
    /// The handle owns its [`crate::broadcast::ReplySink`] (a cheap clone: a
    /// channel `Sender` plus an `Arc`), so it does not borrow `ctx` and a
    /// dispatch arm can mint the handle and still pass `&mut ctx` alongside it.
    fn reply_handle(&self) -> ReplyHandle {
        ReplyHandle {
            sink: crate::broadcast::ReplySink::new(
                self.request_id,
                self.writer.clone(),
                Arc::clone(self.global_lag),
            ),
            sent: false,
        }
    }

    /// Mint an OWNED [`crate::broadcast::ReplyTarget`] for the request
    /// currently being dispatched, tagged with its `kind`.
    ///
    /// Unlike [`reply_handle`](Self::reply_handle) the returned target OWNS
    /// everything it needs (a sink clone and a clone of the `global_lag` `Arc`),
    /// so it can travel over a `SessionCommand`/`DaemonCommand` channel to the
    /// session thread or daemon command loop that will produce the reply. The
    /// daemon owns no correlation state: the target echoes `request_id` and
    /// nothing more.
    fn reply_target(&self, kind: MessageKind) -> crate::broadcast::ReplyTarget {
        crate::broadcast::ReplyTarget::new(
            self.request_id,
            kind,
            self.writer.clone(),
            Arc::clone(self.global_lag),
        )
    }

    /// Answer the request with a generic success acknowledgement
    /// (`Accepted { kind }`) — the terminal reply for a fire-and-confirm request
    /// whose outcome carries no richer payload (subscriptions, cancel). Shorthand
    /// for minting a [`ReplyHandle`] and sending `Accepted`.
    fn ack(&self, kind: MessageKind) {
        self.reply_handle()
            .send(DaemonMessageType::Accepted { kind });
    }
}

/// Clean up a client connection: detach from session, unregister the summary
/// subscriber, wait for the writer thread to drain, and record the disconnect
/// metric.  Owns the `writer_tx` sender and writer handle so both are consumed.
fn cleanup_client(
    attached_session_tx: Option<&crossbeam_channel::Sender<SessionCommand>>,
    client_id: u64,
    daemon_tx: &crossbeam_channel::Sender<DaemonCommand>,
    writer: crate::broadcast::SubscriberSink,
    writer_handle: std::thread::JoinHandle<()>,
) {
    if let Some(tx) = attached_session_tx {
        let _ = tx.send(SessionCommand::Detach { client_id });
    }
    let _ = daemon_tx.send(DaemonCommand::ClientDisconnected { client_id });
    drop(writer);
    // Join the writer with a bound: a wedged writer (client open but not
    // reading) is stuck in a blocking socket write and cannot exit on the
    // channel disconnect alone. Cleanup must not hang the connection thread on
    // that forever; the daemon shutdown drain is the backstop, and the
    // concurrent-connection cap bounds how many wedged writers can accumulate.
    // A writer that times out is detached — it keeps its socket until the
    // client goes away, then exits on its own.
    crate::server::lifecycle::join_thread_bounded(
        writer_handle,
        Instant::now() + WRITER_JOIN_GRACE,
    );
    crate::metrics::record_client_disconnected();
}

/// Dispatch a decoded `ClientMessage` through the shared handler functions.
/// Returns an error only when the daemon has disconnected (caller should
/// terminate the client connection).
fn dispatch_client_message(msg: ClientMessage, ctx: &mut ClientCtx) -> io::Result<()> {
    // Capture the request id up front: it is stamped onto every reply this
    // dispatch produces, and the payload is matched by value below.
    let ClientMessage { id, inner } = msg;
    ctx.request_id = id;
    // The request's kind tag, computed before the payload is matched by value.
    // The defensive wildcard arm (a future wire variant) needs it to build a
    // typed `Failed` reply without re-deriving from the moved payload.
    let kind = inner.kind();
    match inner {
        ClientMessageType::CreateSession {
            title,
            parent_session_id,
            working_dir,
            context_config,
            account_name,
            selected_model,
            reasoning_effort,
        } => {
            // Mint the reply handle here and hand it to the handler by value:
            // `handle_client_create_session` answers on THIS thread (it blocks
            // on the daemon round-trip), so it owns the reply obligation.
            let handle = ctx.reply_handle();
            if !handle_client_create_session(
                title,
                parent_session_id,
                working_dir,
                context_config,
                account_name,
                selected_model,
                reasoning_effort,
                ctx,
                handle,
            ) {
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionAborted,
                    "daemon disconnected",
                ));
            }
        }
        ClientMessageType::AttachSession { session_id } => {
            let handle = ctx.reply_handle();
            if !handle_client_attach_session(session_id, ctx, handle) {
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionAborted,
                    "daemon disconnected",
                ));
            }
        }
        ClientMessageType::ListSessions => {
            debug!("client {}: ListSessions", ctx.client_id);
            let (reply, rx) = mpsc::channel();
            let _ = ctx.daemon_tx.send(DaemonCommand::ListSessions { reply });
            let handle = ctx.reply_handle();
            match rx.recv() {
                Ok(sessions) => handle.send(DaemonMessageType::Sessions { sessions }),
                // Daemon gone: the command loop that would have produced the
                // list no longer exists, so the reply is impossible (not
                // forgotten) and the connection is being torn down.
                Err(_) => handle.abandon(),
            }
        }
        ClientMessageType::SubscribeSessionsSummary => {
            let _ = ctx
                .daemon_tx
                .send(DaemonCommand::RegisterSummarySubscriber {
                    client_id: ctx.client_id,
                    writer: ctx.writer.clone(),
                });
            // No-arg ack: nothing is fire-and-forget. The register command is
            // applied by the daemon command loop; this just confirms receipt.
            ctx.ack(kind);
        }
        ClientMessageType::UnsubscribeSessionsSummary => {
            let _ = ctx
                .daemon_tx
                .send(DaemonCommand::UnregisterSummarySubscriber {
                    client_id: ctx.client_id,
                });
            ctx.ack(kind);
        }
        ClientMessageType::RunInput { input } => {
            debug!("client {}: RunInput", ctx.client_id);
            // Hand the reply obligation to the session thread: it sends the
            // TARGETED acceptance reply (`Started` on accept, `Failed` on
            // reject) plus the unchanged broadcast stream. The daemon assigns
            // the run's `stream_id` there and reports it on `Started`.
            let target = ctx.reply_target(kind);
            if let Some(tx) = ctx.attached_session_tx.as_ref() {
                let _ = tx.send(SessionCommand::RunInput {
                    input,
                    reply: Some(target),
                });
            } else {
                target.fail("no session attached");
            }
        }
        ClientMessageType::Cancel { stream_id } => {
            debug!("client {}: Cancel id={}", ctx.client_id, stream_id);
            // Route through the daemon so it can also cancel child
            // sub-sessions without requiring a round-trip message.
            if let Some(session_id) = *ctx.attached_session_id {
                let _ = ctx.daemon_tx.send(DaemonCommand::CancelRequest {
                    session_id,
                    stream_id,
                });
            }
            // A no-arg ack so `Cancel` is not fire-and-forget: the request was
            // received (the stream's own `Cancelled` broadcast is the outcome).
            ctx.ack(kind);
        }
        ClientMessageType::Undo => {
            debug!("client {}: Undo", ctx.client_id);
            let target = ctx.reply_target(kind);
            if let Some(tx) = ctx.attached_session_tx.as_ref() {
                let _ = tx.send(SessionCommand::Undo {
                    reply: Some(target),
                });
            } else {
                target.fail("no session attached");
            }
        }
        ClientMessageType::Redo => {
            debug!("client {}: Redo", ctx.client_id);
            let target = ctx.reply_target(kind);
            if let Some(tx) = ctx.attached_session_tx.as_ref() {
                let _ = tx.send(SessionCommand::Redo {
                    reply: Some(target),
                });
            } else {
                target.fail("no session attached");
            }
        }
        ClientMessageType::ContinueGeneration => {
            debug!("client {}: ContinueGeneration", ctx.client_id);
            let target = ctx.reply_target(kind);
            if let Some(tx) = ctx.attached_session_tx.as_ref() {
                let _ = tx.send(SessionCommand::RunInput {
                    input: b"Continue.".to_vec(),
                    reply: Some(target),
                });
            } else {
                target.fail("no session attached");
            }
        }
        ClientMessageType::Ping => {
            debug!("client {}: Ping", ctx.client_id);
            ctx.reply_handle().send(DaemonMessageType::Pong);
        }
        ClientMessageType::SetModel { model } => {
            info!(
                "client {}: SetModel model={} attached={}",
                ctx.client_id,
                model,
                ctx.attached_session_tx.is_some()
            );
            let target = ctx.reply_target(kind);
            if let Some(tx) = ctx.attached_session_tx.as_ref() {
                let _ = tx.send(SessionCommand::SetModel {
                    model,
                    reply: Some(target),
                });
            } else {
                target.fail("no session attached");
            }
        }
        ClientMessageType::SetReasoningEffort { effort } => {
            info!(
                "client {}: SetReasoningEffort effort={} attached={}",
                ctx.client_id,
                effort,
                ctx.attached_session_tx.is_some()
            );
            let target = ctx.reply_target(kind);
            if let Some(tx) = ctx.attached_session_tx.as_ref() {
                let _ = tx.send(SessionCommand::SetReasoningEffort {
                    effort,
                    reply: Some(target),
                });
            } else {
                target.fail("no session attached");
            }
        }
        ClientMessageType::GetReasoningEffort => {
            if let Some(tx) = ctx.attached_session_tx {
                let (reply, rx) = mpsc::channel();
                let _ = tx.send(SessionCommand::GetReasoningEffort { reply });
                let handle = ctx.reply_handle();
                if let Ok(effort) = rx.recv() {
                    // Session-scoped reply to the attached session: carry its
                    // real id (do NOT fall back to the None sentinel).
                    handle.send(DaemonMessageType::Session {
                        session_id: *ctx.attached_session_id,
                        event: SessionEvent::ReasoningEffortSet { effort },
                    });
                } else {
                    // The session thread is gone; the connection is being
                    // torn down, so the reply is impossible.
                    handle.abandon();
                }
            } else {
                ctx.reply_handle().send(DaemonMessageType::Session {
                    session_id: None,
                    event: SessionEvent::ReasoningEffortSet {
                        effort: "off".to_string(),
                    },
                });
            }
        }
        ClientMessageType::Unlock { private_key } => {
            info!("client {}: Unlock", ctx.client_id);
            handle_unlock_sync(ctx, kind, private_key);
        }
        ClientMessageType::BindKeystore { key } => {
            info!("client {}: BindKeystore", ctx.client_id);
            handle_bind_keystore_sync(ctx, kind, key);
        }
        ClientMessageType::Lock => {
            info!("client {}: Lock", ctx.client_id);
            let handle = ctx.reply_handle();
            handle_lock_sync(ctx, handle);
        }
        ClientMessageType::AddCredential {
            service,
            encrypted_payload,
            unlock_key,
        } => {
            info!(
                "client {}: AddCredential service={}",
                ctx.client_id, service
            );
            handle_add_credential_sync(&mut *ctx, kind, &service, encrypted_payload, unlock_key);
        }
        ClientMessageType::RemoveCredential { service } => {
            info!(
                "client {}: RemoveCredential service={}",
                ctx.client_id, service
            );
            let handle = ctx.reply_handle();
            handle_remove_credential_sync(ctx, handle, service);
        }
        ClientMessageType::AclAdd { pubkey } => {
            info!("client {}: AclAdd (local={})", ctx.client_id, ctx.is_unix);
            let handle = ctx.reply_handle();
            handle_acl_add_sync(ctx, handle, &pubkey);
        }
        ClientMessageType::ListModels => {
            debug!("client {}: ListModels", ctx.client_id);
            let session_id = *ctx.attached_session_id;
            let handle = ctx.reply_handle();
            handle_list_models_sync(ctx, handle, session_id);
        }
        ClientMessageType::GetImage {
            session_id,
            turn_id,
            key,
        } => {
            debug!(
                "client {}: GetImage session={} turn={} key={:?}",
                ctx.client_id, session_id, turn_id, key
            );
            let handle = ctx.reply_handle();
            handle_client_get_image(session_id, turn_id, key, ctx, handle);
        }
        ClientMessageType::RefreshModels { force } => {
            debug!("client {}: RefreshModels force={}", ctx.client_id, force);
            let handle = ctx.reply_handle();
            handle_refresh_models_sync(ctx, handle, force);
        }
        ClientMessageType::McpStatusRequest => {
            debug!("client {}: McpStatusRequest", ctx.client_id);
            let handle = ctx.reply_handle();
            handle_mcp_status_sync(ctx, handle);
        }
        ClientMessageType::McpReconnect { slug } => {
            debug!("client {}: McpReconnect slug={}", ctx.client_id, slug);
            let handle = ctx.reply_handle();
            handle_mcp_reconnect_sync(ctx, handle, slug);
        }
        ClientMessageType::McpReload => {
            debug!("client {}: McpReload", ctx.client_id);
            let handle = ctx.reply_handle();
            handle_mcp_reload_sync(ctx, handle);
        }
        ClientMessageType::McpTrust => {
            debug!("client {}: McpTrust", ctx.client_id);
            let handle = ctx.reply_handle();
            handle_mcp_trust_sync(ctx, handle, true);
        }
        ClientMessageType::McpUntrust => {
            debug!("client {}: McpUntrust", ctx.client_id);
            let handle = ctx.reply_handle();
            handle_mcp_trust_sync(ctx, handle, false);
        }
        ClientMessageType::McpTrustList => {
            debug!("client {}: McpTrustList", ctx.client_id);
            let handle = ctx.reply_handle();
            handle_mcp_trust_list_sync(ctx, handle);
        }
        ClientMessageType::DeleteSession { session_id } => {
            info!("client {}: DeleteSession id={}", ctx.client_id, session_id);
            let handle = ctx.reply_handle();
            handle_delete_session_sync(ctx, handle, session_id);
        }
        ClientMessageType::SetSessionPinned { session_id, pinned } => {
            info!(
                "client {}: SetSessionPinned id={} pinned={}",
                ctx.client_id, session_id, pinned
            );
            let handle = ctx.reply_handle();
            handle_set_session_flags_sync(ctx, handle, session_id, Some(pinned), None, kind);
        }
        ClientMessageType::SetSessionArchived {
            session_id,
            archived,
        } => {
            info!(
                "client {}: SetSessionArchived id={} archived={}",
                ctx.client_id, session_id, archived
            );
            let handle = ctx.reply_handle();
            handle_set_session_flags_sync(ctx, handle, session_id, None, Some(archived), kind);
        }
        ClientMessageType::GetCredential { service } => {
            let handle = ctx.reply_handle();
            handle_get_credential_sync(ctx, handle, service);
        }
        ClientMessageType::AddAccount {
            name,
            provider,
            base_url,
            streaming,
            retry_max_attempts,
            connect_timeout_secs,
            request_timeout_secs,
            total_timeout_secs,
        } => {
            let handle = ctx.reply_handle();
            let result = request_daemon(ctx.daemon_tx, |reply| DaemonCommand::AddAccountCmd {
                name: name.clone(),
                provider,
                base_url,
                streaming,
                retry_max_attempts,
                connect_timeout_secs,
                request_timeout_secs,
                total_timeout_secs,
                reply,
            });
            match result {
                Ok(Ok(())) => handle.send(DaemonMessageType::AccountAdded { name }),
                Ok(Err(e)) => handle.send(DaemonMessageType::AccountAddFailed { name, error: e }),
                // Daemon gone: the reply is impossible, not forgotten.
                Err(_) => handle.abandon(),
            }
        }
        ClientMessageType::RemoveAccount { name } => {
            let handle = ctx.reply_handle();
            let result = request_daemon(ctx.daemon_tx, |reply| DaemonCommand::RemoveAccountCmd {
                name: name.clone(),
                reply,
            });
            match result {
                Ok(Ok(())) => handle.send(DaemonMessageType::AccountRemoved { name }),
                Ok(Err(e)) => {
                    handle.send(DaemonMessageType::AccountRemoveFailed { name, error: e });
                }
                Err(_) => handle.abandon(),
            }
        }
        ClientMessageType::ListAccounts => {
            let handle = ctx.reply_handle();
            let result = request_daemon(ctx.daemon_tx, |reply| DaemonCommand::ListAccountsCmd {
                reply,
            });
            match result {
                Ok(Ok(accounts)) => handle.send(DaemonMessageType::Accounts { accounts }),
                Ok(Err(e)) => handle.send(DaemonMessageType::AccountListFailed { error: e }),
                Err(_) => handle.abandon(),
            }
        }
        ClientMessageType::SetSessionAccount { name } => {
            handle_client_set_session_account(name, ctx);
        }
        ClientMessageType::SubscribeAllActivity => {
            let _ = ctx
                .daemon_tx
                .send(DaemonCommand::RegisterActivitySubscriber {
                    client_id: ctx.client_id,
                    writer: ctx.writer.clone(),
                });
            ctx.ack(kind);
        }
        ClientMessageType::UnsubscribeAllActivity => {
            let _ = ctx
                .daemon_tx
                .send(DaemonCommand::UnregisterActivitySubscriber {
                    client_id: ctx.client_id,
                });
            ctx.ack(kind);
        }
        _ => {
            // Defensive net for a request the dispatch does not (yet) answer —
            // today only `GetSessionState`, which is defined on the wire but
            // has no connection-thread handler. The variant set IS the wire
            // contract, so this arm should not be reached in practice, but it
            // must still REPLY rather than drop: an unanswered request strands
            // the client's pending slot until its timeout.
            warn!("unhandled client message (kind {kind:?})");
            ctx.reply_handle().send(DaemonMessageType::Failed {
                kind,
                error: "unsupported request".into(),
            });
        }
    }
    Ok(())
}

/// Transport-agnostic per-connection protocol state machine.
///
/// Owns everything the read loop used to thread through per-message borrowed
/// `ClientCtx` constructions: the daemon command channel, the delivery sink,
/// the daemon-wide lag counter, the attachment state, and the writer thread's
/// join handle (so `finish()` can run the bounded writer join). The three
/// connection threads (Unix socket, TCP/Noise, and the in-process embedded
/// channel path in `crate::embedded`) differ only in HOW they read one
/// message off the wire and classify transport errors; everything between
/// message read and teardown lives here.
pub(crate) struct ClientConn {
    daemon_tx: crossbeam_channel::Sender<DaemonCommand>,
    /// This connection's own handle to the shared redb database (see
    /// [`ClientCtx::db`]): on-demand image reads run here, on the connection
    /// thread, never as a command-loop round-trip.
    db: Arc<redb::Database>,
    /// This connection's delivery sink.
    writer: crate::broadcast::SubscriberSink,
    /// Daemon-wide lag counter, shared by every connection; kept so replies
    /// can increment it in balance with the writer thread's per-dequeue
    /// decrement.
    global_lag: Arc<AtomicUsize>,
    client_id: u64,
    /// Whether this connection arrived over the local Unix socket (vs the
    /// TCP/Noise listener). Trust-boundary input for local-only commands
    /// (see `ClientCtx::is_unix`).
    is_unix: bool,
    attached_session_id: Option<u64>,
    attached_session_tx: Option<crossbeam_channel::Sender<SessionCommand>>,
    /// Handle to the writer thread spawned in `new`; joined (with a bound) by
    /// `finish()` via `cleanup_client`, exactly as the pre-refactor loops did.
    writer_handle: std::thread::JoinHandle<()>,
}

/// The transport-independent inputs shared by every connection thread: the
/// daemon command channel, this connection's writer sink and its receiver, the
/// daemon-wide lag counter, this connection's DB handle, and the local-domain
/// flag.
///
/// Bundled so each of the three transport entry points ([`client_thread`],
/// [`tcp_handshake_and_client_thread`]/[`tcp_client_thread`], and
/// [`embedded_client_thread`]) takes ONE such value instead of seven positional
/// arguments, and so [`ClientConn::new`] takes a single value plus the one
/// transport-specific field (the byte-writer buffer). The `too_many_arguments`
/// lint then never applies.
pub(crate) struct ConnThreadArgs {
    pub daemon_tx: crossbeam_channel::Sender<DaemonCommand>,
    /// This connection's own handle to the shared redb database (see
    /// [`ClientCtx::db`]).
    pub db: Arc<redb::Database>,
    /// This connection's delivery sink.
    pub writer: crate::broadcast::SubscriberSink,
    /// The receiver end of this connection's writer channel.
    pub writer_rx: crossbeam_channel::Receiver<DaemonMessage>,
    /// Daemon-wide lag counter, shared by every connection.
    pub global_lag: Arc<AtomicUsize>,
    pub client_id: u64,
    /// Whether this connection is the LOCAL trust domain — the Unix socket or
    /// the in-process embedded link — as opposed to the TCP/Noise listener.
    /// Trust-boundary input for local-only commands (see [`ClientCtx::is_unix`]).
    pub is_unix: bool,
}

impl ClientConn {
    /// Build a connection and spawn its writer thread over the given
    /// transport-specific writer buffer. Spawning here keeps the socket
    /// threads to just: set write timeout, clone the stream into a writer
    /// buffer, call this, then read/dispatch/finish.
    fn new<W: ConnectionWriter + Send + 'static>(args: ConnThreadArgs, writer_buf: W) -> Self {
        let ConnThreadArgs {
            daemon_tx,
            db,
            writer,
            writer_rx,
            global_lag,
            client_id,
            is_unix,
        } = args;
        // The writer thread decrements the SAME per-client byte counter the
        // daemon's sinks increment on enqueue, plus the daemon-wide counter.
        let bytes = Arc::clone(&writer.bytes_in_flight);
        let global = Arc::clone(&global_lag);
        let writer_handle =
            std::thread::spawn(move || writer_thread(writer_buf, &writer_rx, &bytes, &global));
        Self {
            daemon_tx,
            db,
            writer,
            global_lag,
            client_id,
            is_unix,
            attached_session_id: None,
            attached_session_tx: None,
            writer_handle,
        }
    }

    /// Dispatch one decoded client message through the shared handlers.
    /// Constructs the borrowed `ClientCtx` view the handler functions expect.
    /// Returns an error only when the daemon has disconnected (caller should
    /// terminate the connection) — identical to the pre-refactor per-message
    /// `ClientCtx` construction + `dispatch_client_message` call.
    pub(crate) fn dispatch(&mut self, msg: ClientMessage) -> io::Result<()> {
        let mut ctx = ClientCtx {
            writer: &self.writer,
            db: &self.db,
            global_lag: &self.global_lag,
            daemon_tx: &self.daemon_tx,
            attached_session_id: &mut self.attached_session_id,
            attached_session_tx: &mut self.attached_session_tx,
            client_id: self.client_id,
            // Overwritten from the inbound frame at dispatch entry; the
            // per-connection request id is per-request, not per-connection
            // state held across dispatches.
            request_id: 0,
            is_unix: self.is_unix,
        };
        dispatch_client_message(msg, &mut ctx)
    }

    /// Tear the connection down: detach from any attached session, notify the
    /// daemon, drop the sink, and join the writer thread with a bound.
    pub(crate) fn finish(self) {
        cleanup_client(
            self.attached_session_tx.as_ref(),
            self.client_id,
            &self.daemon_tx,
            self.writer,
            self.writer_handle,
        );
    }
}

pub(crate) fn client_thread(
    stream: UnixStream,
    args: ConnThreadArgs,
    writer_write_timeout: Duration,
) -> io::Result<()> {
    // Bound the writer's blocking socket writes so a wedged client (receive
    // window permanently zero) cannot stall it forever — this is what makes
    // lag eviction reap the connection without a daemon-held close handle.
    // The timeout applies to every clone of this socket. The value comes from
    // `DaemonState::writer_write_timeout` (default [`WRITER_WRITE_TIMEOUT`]);
    // it is injectable so a wedged-writer test can use a tiny timeout instead
    // of waiting out the 5 s default.
    stream.set_write_timeout(Some(writer_write_timeout))?;
    let reader = BufReader::new(stream.try_clone()?);
    let writer_buf = BufWriter::new(stream);

    // Capture the id before `args` is consumed by `ClientConn::new`; it is
    // only used for this connect log here.
    let client_id = args.client_id;
    let mut conn = ClientConn::new(args, writer_buf);

    // The writer channel was registered with the daemon by the acceptor
    // (register_client_writer) before this thread was spawned, so the shutdown
    // path can route `ShuttingDown` through this single writer thread instead
    // of writing to the socket from another thread.
    info!("client connected: id={}", client_id);
    crate::metrics::record_client_connected();

    let mut reader = reader;
    loop {
        match read_message::<_, ClientMessage>(&mut reader) {
            Ok(msg) => {
                if let Err(e) = conn.dispatch(msg) {
                    debug!("daemon disconnected: {e}");
                    break;
                }
            }
            Err(ProtoError::Io(e))
                if matches!(
                    e.kind(),
                    io::ErrorKind::UnexpectedEof | io::ErrorKind::ConnectionReset
                ) =>
            {
                debug!("client disconnected");
                break;
            }
            Err(e) => {
                error!(error = %e, "failed to read client message");
                break;
            }
        }
    }

    conn.finish();
    Ok(())
}

/// TCP accept path: read the 1-byte handshake-mode preamble, run the
/// matching Noise responder handshake (IK or XX), and hand the encrypted
/// stream to [`tcp_client_thread`].
///
/// The writer channel is registered with the daemon by the acceptor BEFORE
/// this function runs (see `register_client_writer`), so every failure path
/// here — unknown preamble, silent/garbage peer, rejected handshake — must
/// unregister via `ClientDisconnected`, exactly as the old inline handshake
/// failure path in `server/lifecycle.rs` did. This keeps the daemon's
/// `client_writers` registry honest: a connection that never produced a
/// working transport must not leave a stale writer entry behind.
///
/// **The preamble is UNAUTHENTICATED by design.** It is a cleartext mode
/// selector read before any keying material exists, so it cannot carry
/// authentication itself. That is safe because it authorizes NOTHING: it
/// only selects which handshake runs. Everything that matters is
/// authenticated by the subsequent Noise handshake — IK and XX both
/// authenticate both parties' static keys via the DH operations, so a
/// man-in-the-middle cannot downgrade the mode or impersonate either side
/// (a MITM would have to complete the chosen handshake, which requires the
/// server's private key), and the daemon's ACL check runs inside whichever
/// handshake the client picked. The worst an attacker controls is which
/// of two equally-authenticated handshakes runs.
pub(crate) fn tcp_handshake_and_client_thread(
    mut tcp: TcpStream,
    transport_sk: [u8; 32],
    acl: &Arc<crate::server::acl::SharedAcl>,
    args: ConnThreadArgs,
    writer_write_timeout: Duration,
) -> io::Result<()> {
    // The preamble read runs BEFORE any authentication, so it is bounded by
    // the transport's absolute-deadline machinery (same as the handshake
    // itself): a peer that connects and sends nothing is cut off instead of
    // holding this thread + FD open forever.
    let preamble = match choreo_transport::handshake::read_handshake_preamble(&mut tcp) {
        Ok(p) => p,
        Err(e) => {
            warn!(
                error = %e,
                "TCP client never sent a valid handshake-mode preamble; closing"
            );
            // Drop `tcp` (closes the socket) and unregister the writer
            // channel this connection registered at accept time.
            let _ = args.daemon_tx.send(DaemonCommand::ClientDisconnected {
                client_id: args.client_id,
            });
            return Ok(());
        }
    };

    // Dispatch on the mode byte. Each arm runs the full responder handshake
    // with the SAME ACL closure, so XX connections are authorized exactly
    // like IK ones (the check lives inside the handshake in both cases).
    let handshake_result = match preamble {
        choreo_transport::handshake::PREAMBLE_IK => {
            debug!("TCP client selected Noise IK handshake");
            choreo_transport::handshake::handshake_responder(tcp, &transport_sk, |pk| {
                acl.contains(pk)
            })
        }
        choreo_transport::handshake::PREAMBLE_XX => {
            debug!("TCP client selected Noise XX (first-contact) handshake");
            choreo_transport::handshake::handshake_responder_xx(tcp, &transport_sk, |pk| {
                acl.contains(pk)
            })
        }
        other => {
            warn!(
                preamble = other,
                "unknown handshake-mode preamble byte; closing connection"
            );
            let _ = args.daemon_tx.send(DaemonCommand::ClientDisconnected {
                client_id: args.client_id,
            });
            return Ok(()); // dropping `tcp` closes the connection
        }
    };

    let noise = match handshake_result {
        Ok(noise) => noise,
        Err(e) => {
            error!(error = %e, "Noise handshake rejected");
            let _ = args.daemon_tx.send(DaemonCommand::ClientDisconnected {
                client_id: args.client_id,
            });
            return Ok(());
        }
    };

    tcp_client_thread(noise, args, writer_write_timeout)
}

pub(crate) fn tcp_client_thread(
    noise: choreo_transport::noise::NoiseStream,
    args: ConnThreadArgs,
    writer_write_timeout: Duration,
) -> io::Result<()> {
    // Writer thread: blocks on writer_rx, sends via NoiseStream encryption.
    // Bound the underlying socket's blocking writes (see
    // `DaemonState::writer_write_timeout`, default [`WRITER_WRITE_TIMEOUT`]) so
    // a wedged client cannot stall the writer forever; the timeout applies to
    // every clone of the TcpStream.
    noise
        .get_ref()
        .set_write_timeout(Some(writer_write_timeout))?;
    let writer_buf = noise.try_clone()?;

    let client_id = args.client_id;
    let mut conn = ClientConn::new(args, writer_buf);

    // The writer channel was registered with the daemon by the acceptor
    // (register_client_writer) before this thread was spawned, so the shutdown
    // path can route `ShuttingDown` through this single writer thread (see
    // client_thread). The NoiseStream's TransportState lock is only safe to
    // take per-message because this is the sole sender.
    info!("TCP client connected: id={}", client_id);
    crate::metrics::record_client_connected();

    // Summary subscription is an explicit client decision on this transport,
    // exactly as on the Unix path: a Noise client opts in via
    // ClientMessageType::SubscribeSessionsSummary (dispatched in
    // dispatch_client_message). Previously every TCP connection was
    // auto-registered here, which pushed broadcasts about other clients'
    // sessions to clients that never asked.
    let mut reader = noise;
    loop {
        match reader.recv_client_message() {
            Ok(msg) => {
                if let Err(e) = conn.dispatch(msg) {
                    debug!("daemon disconnected: {e}");
                    break;
                }
            }
            Err(choreo_transport::error::TransportError::ConnectionClosed) => {
                info!("TCP client closed connection");
                break;
            }
            Err(e) => {
                error!(error = %e, "failed to read client message");
                break;
            }
        }
    }

    conn.finish();
    Ok(())
}

/// Per-connection inputs for the embedded (in-process) transport, bundled
/// into one struct so the spawn call site stays a single argument. The
/// transport-independent half is the shared [`ConnThreadArgs`]; the embedded
/// path adds only its channel endpoints (it has no socket).
pub(crate) struct EmbeddedConnArgs {
    /// GUI→daemon message values. Channel close IS the EOF.
    pub client_rx: crossbeam_channel::Receiver<ClientMessage>,
    /// The writer's forward target = the GUI's read half.
    pub out_tx: crossbeam_channel::Sender<DaemonMessage>,
    /// The shared per-connection inputs; the embedded link is the LOCAL trust
    /// domain, so the caller sets `is_unix: true`.
    pub conn: ConnThreadArgs,
}

/// The embedded (in-process) connection thread — the third transport, next
/// to [`client_thread`] (Unix) and [`tcp_client_thread`] (TCP/Noise).
///
/// The connection never becomes bytes: client messages arrive as Rust values
/// on `client_rx` and daemon messages leave as values on `out_tx` (via the
/// [`ChannelConnectionWriter`] the shared writer thread drains). There is no
/// error classification and no timeout machinery: the `for` loop over
/// `client_rx` ends exactly when the GUI drops its `EmbeddedLink` (channel
/// close IS the EOF), and `conn.finish()` runs the same teardown the socket
/// paths use.
///
/// `is_unix: true` — an embedded connection is the LOCAL trust domain, like
/// the Unix socket: both peers are the same process, so `/acl add` (and the
/// local-only command semantics generally) apply.
// The embedded connection has no error surface beyond dispatch logging: the
// `for` loop ends on channel close (the EOF) and teardown is infallible, so
// the thread entry returns () instead of a transparent Ok wrapper.
pub(crate) fn embedded_client_thread(args: EmbeddedConnArgs) {
    let EmbeddedConnArgs {
        client_rx,
        out_tx,
        conn: conn_args,
    } = args;

    let client_id = conn_args.client_id;
    let mut conn = ClientConn::new(conn_args, ChannelConnectionWriter::new(out_tx));

    // The writer channel was registered with the daemon by `connect()`
    // (register_client_writer) BEFORE this thread was spawned, so the
    // shutdown path can route `ShuttingDown` through this single writer
    // thread — same ordering invariant as the socket paths.
    info!("embedded client connected: id={}", client_id);
    crate::metrics::record_client_connected();

    for msg in client_rx {
        if let Err(e) = conn.dispatch(msg) {
            debug!("daemon disconnected: {e}");
            break;
        }
    }
    info!("embedded client disconnected: id={}", client_id);

    conn.finish();
}

/// Switch the client's attachment from the old session to a new one.
/// Skips detaching when re-attaching to the same session to avoid
/// killing the session's only subscriber.
fn switch_attached_session(
    new_session_id: u64,
    session_tx: crossbeam_channel::Sender<SessionCommand>,
    ctx: &mut ClientCtx,
) {
    // Don't detach when re-attaching to the same session.
    if Some(new_session_id) != *ctx.attached_session_id
        && let Some(old_tx) = ctx.attached_session_tx.as_ref()
    {
        let _ = old_tx.send(SessionCommand::Detach {
            client_id: ctx.client_id,
        });
    }
    let _ = session_tx.send(SessionCommand::Attach {
        client_id: ctx.client_id,
        tx: ctx.writer.clone(),
    });
    *ctx.attached_session_tx = Some(session_tx);
    *ctx.attached_session_id = Some(new_session_id);
}

#[expect(clippy::too_many_arguments)]
/// Handle a `CreateSession` client message. Returns false if the daemon
/// disconnected, signaling `client_thread` to return.
fn handle_client_create_session(
    title: Option<String>,
    parent_session_id: Option<u64>,
    working_dir: Option<String>,
    context_config: Option<ContextConfig>,
    account_name: Option<String>,
    selected_model: Option<String>,
    reasoning_effort: Option<String>,
    ctx: &mut ClientCtx<'_>,
    handle: ReplyHandle,
) -> bool {
    info!("client {}: CreateSession", ctx.client_id);
    let cwd_str = working_dir.clone();
    let (reply, rx) = mpsc::channel();
    let _ = ctx.daemon_tx.send(DaemonCommand::CreateSession {
        title: title.clone(),
        parent_session_id,
        working_dir: working_dir.map(std::path::PathBuf::from),
        reasoning_effort: reasoning_effort.clone(),
        selected_model: selected_model.clone(),
        context_config,
        account_name: account_name.clone(),
        active_tool_groups: Vec::new(),
        reply,
    });
    match rx.recv() {
        Ok(Ok((sid, _session_tx))) => {
            // _session_tx is discarded here because the
            // daemon keeps its own clone in active_sessions
            // (keyed by sid).  When the client later calls
            // AttachSession the daemon returns another clone
            // — no need to hold one in the connection thread.
            //
            // Don't auto-attach or detach here — the TUI
            // attaches explicitly via AttachSession when
            // the user presses Enter on a session.
            // This keeps the old session alive when
            // creating from the session manager page.
            // The reply to THIS connection's CreateSession is
            // `SessionCreatedForRequester` — the frontend may attach to it.
            // The daemon separately broadcasts `SessionCreated` to every
            // subscriber (see `DaemonState::handle_create_session`), where it
            // is notification-only and must not move a client's view.
            handle.send(DaemonMessageType::Session {
                session_id: Some(sid),
                event: SessionEvent::SessionCreatedForRequester {
                    title,
                    parent_session_id,
                    working_dir: cwd_str,
                    account_name,
                    selected_model,
                    reasoning_effort,
                },
            });
        }
        Ok(Err(e)) => {
            handle.send(DaemonMessageType::Session {
                session_id: None,
                event: SessionEvent::SessionFailed {
                    operation: "create_session".into(),
                    error: e.to_string(),
                },
            });
        }
        Err(_) => {
            // Daemon disconnected: release the obligation (the caller returns
            // false and the connection is torn down) so the guard does not
            // fire on an impossible reply.
            handle.abandon();
            return false;
        }
    }
    true
}

/// Handle an `AttachSession` client message. Returns false if the daemon
/// disconnected, signaling `client_thread` to return.
fn handle_client_attach_session(
    session_id: u64,
    ctx: &mut ClientCtx<'_>,
    handle: ReplyHandle,
) -> bool {
    info!("client {}: AttachSession id={}", ctx.client_id, session_id);
    let (reply, rx) = mpsc::channel();
    let _ = ctx
        .daemon_tx
        .send(DaemonCommand::AttachSession { session_id, reply });
    match rx.recv() {
        Ok(Ok(session_tx)) => {
            // Send SessionAttached before SessionCommand::Attach so that
            // the TUI's attached_session_id is set before SessionState
            // arrives — otherwise SessionState is silently dropped.
            handle.send(DaemonMessageType::Session {
                session_id: Some(session_id),
                event: SessionEvent::SessionAttached,
            });
            switch_attached_session(session_id, session_tx, ctx);
        }
        Ok(Err(e)) => {
            handle.send(DaemonMessageType::Session {
                session_id: None,
                event: SessionEvent::SessionFailed {
                    operation: "attach_session".into(),
                    error: e.to_string(),
                },
            });
        }
        Err(_) => {
            handle.abandon();
            return false;
        }
    }
    true
}

/// Handle a `SetSessionAccount` client message: verify the account exists
/// via the daemon, then set it on the attached session. The account check runs
/// HERE, on the connection thread, so the not-found and no-session cases reply
/// a targeted session-scoped `SessionFailed { operation: "set_account" }`
/// directly (the shape every front-end already renders); on success the
/// session thread acks through the minted reply target.
fn handle_client_set_session_account(name: String, ctx: &mut ClientCtx) {
    // Clone the session sender so the reply-target mint below can borrow `ctx`
    // freely (an owned `Sender` avoids holding a borrow of `ctx` across it).
    let Some(tx) = ctx.attached_session_tx.clone() else {
        ctx.reply_handle().send(DaemonMessageType::Session {
            session_id: None,
            event: SessionEvent::SessionFailed {
                operation: "set_account".into(),
                error: "no session attached".into(),
            },
        });
        return;
    };
    // Verify the account exists before setting it.
    let (reply, rx) = mpsc::channel();
    let _ = ctx.daemon_tx.send(DaemonCommand::AccountExists {
        name: name.clone(),
        reply,
    });
    match rx.recv() {
        Ok(true) => {
            let target = ctx.reply_target(MessageKind::SetSessionAccount);
            let _ = tx.send(SessionCommand::SetAccount {
                name,
                reply: Some(target),
            });
        }
        _ => {
            ctx.reply_handle().send(DaemonMessageType::Session {
                session_id: *ctx.attached_session_id,
                event: SessionEvent::SessionFailed {
                    operation: "set_account".into(),
                    error: format!("account '{name}' not found"),
                },
            });
        }
    }
}

/// Send a `DaemonCommand` that expects a reply and wait for the response.
/// Returns the reply value, or None if the daemon dropped the sender.
fn request_daemon<R>(
    daemon_tx: &crossbeam_channel::Sender<DaemonCommand>,
    make_cmd: impl FnOnce(mpsc::Sender<R>) -> DaemonCommand,
) -> Result<R, mpsc::RecvError> {
    let (reply, rx) = mpsc::channel();
    if daemon_tx.send(make_cmd(reply)).is_err() {
        return Err(mpsc::RecvError);
    }
    rx.recv()
}

fn handle_unlock_sync(ctx: &mut ClientCtx, kind: MessageKind, private_key: Vec<u8>) {
    // The daemon command loop enqueues the targeted reply (Unlocked /
    // KeystoreUnbound / LockedError) through the minted reply target DIRECTLY
    // onto this client's writer queue BEFORE its lock-state broadcast — see
    // ORDERING INVARIANT in `DaemonState::handle_unlock`. This thread only waits
    // for the ack so a dropped daemon channel is reported.
    let result = request_daemon(ctx.daemon_tx, |ack| DaemonCommand::Unlock {
        private_key,
        reply: Some(ctx.reply_target(kind)),
        ack,
    });
    if result.is_err() {
        warn!("daemon disconnected while handling unlock");
    }
}

/// Handle `ClientMessageType::BindKeystore`: the ONLY path that can create the
/// keystore binding. On an unbound keystore the daemon adopts the key (loud
/// TOFU log), runs the shared unlock tail, and the client gets the targeted
/// `DaemonMessageType::Bound` reply (sent by the daemon loop into this client's
/// sink BEFORE the lock-state broadcast — see ORDERING INVARIANT in
/// `handle_unlock`); on an already-bound keystore a wrong key is rejected
/// with the existing wrong-key semantics (`LockedError`) — no unlock, no
/// overwrite.
fn handle_bind_keystore_sync(ctx: &mut ClientCtx, kind: MessageKind, key: Vec<u8>) {
    let result = request_daemon(ctx.daemon_tx, |ack| DaemonCommand::BindKeystore {
        key,
        reply: Some(ctx.reply_target(kind)),
        ack,
    });
    if result.is_err() {
        warn!("daemon disconnected while handling bind keystore");
    }
}

/// Reply to a `ClientMessageType::Lock` (`/lock`): the daemon clears its
/// in-memory credentials, flips to the locked state, and broadcasts `Locked`
/// to every activity subscriber. This per-action reply confirms the wipe to
/// the acting client directly; the transition broadcast reaches it too (it
/// is an activity subscriber), harmlessly idempotent — the TUI latches
/// `keystore_locked` either way.
fn handle_lock_sync(ctx: &mut ClientCtx<'_>, handle: ReplyHandle) {
    let result = request_daemon(ctx.daemon_tx, |reply| DaemonCommand::Lock { reply });
    match result {
        Ok(Ok(())) => handle.send(DaemonMessageType::Locked),
        Ok(Err(e)) => handle.send(DaemonMessageType::LockedError { error: e }),
        // Daemon gone: the reply is impossible, not forgotten.
        Err(_) => handle.abandon(),
    }
}

fn handle_list_models_sync(
    ctx: &mut ClientCtx<'_>,
    handle: ReplyHandle,
    attached_session_id: Option<u64>,
) {
    let result = request_daemon(ctx.daemon_tx, |reply| DaemonCommand::ListModels {
        session_id: attached_session_id,
        reply,
    });
    match result {
        Ok(Ok((models, selected_model))) => handle.send(DaemonMessageType::Models {
            models,
            selected_model,
        }),
        Ok(Err(e)) => handle.send(DaemonMessageType::ModelsFailed { error: e }),
        Err(_) => handle.abandon(),
    }
}

/// Handle a `GetImage` client message: read the requested turn attachment's
/// bytes (a displayed image or a tool-result vision image) and reply with a
/// targeted [`DaemonMessageType::Image`].
///
/// Only attachments of the session THIS connection is attached to are served —
/// the same trust boundary every other session-scoped command enforces. A
/// request for any other (or no) session is answered `None` rather than reading
/// an arbitrary session's attachments.
///
/// The connection now owns a redb handle (see [`ClientCtx::db`]), so the read
/// runs RIGHT HERE on the connection thread via [`crate::db::read_attachment`]
/// — a single O(log n) `get` against the attachment table, with no
/// command-loop round-trip and no reply channel to drain. Each connection
/// opens its own read transaction, so concurrent connections never serialize.
fn handle_client_get_image(
    session_id: u64,
    turn_id: u32,
    key: choreo_proto::ImageKey,
    ctx: &ClientCtx,
    handle: ReplyHandle,
) {
    let data = if *ctx.attached_session_id == Some(session_id) {
        // `None` covers both "not found" and a redb read error; the client
        // treats them identically (mark the image failed, don't retry), so the
        // two collapse here intentionally — a transient redb hiccup must never
        // leak a raw error into the image-fetch protocol.
        match crate::db::read_attachment(ctx.db, session_id, turn_id, &key) {
            Ok(data) => data,
            Err(e) => {
                warn!(
                    session_id,
                    turn_id,
                    ?key,
                    error = %e,
                    "failed to read image attachment"
                );
                None
            }
        }
    } else {
        None
    };
    handle.send(DaemonMessageType::Image {
        session_id,
        turn_id,
        key,
        data,
    });
}

/// Handle a `RefreshModels` client message: forward the request to the daemon
/// (which hands it to the maintenance thread — the fetch never blocks this
/// connection), then route the reply back to the client. The request blocks
/// here until the maintenance thread has a result, which is the request/
/// response contract `/refresh-models` implies.
fn handle_refresh_models_sync(ctx: &mut ClientCtx<'_>, handle: ReplyHandle, force: bool) {
    let result = request_daemon(ctx.daemon_tx, |reply| DaemonCommand::RefreshModels {
        force,
        reply,
    });
    match result {
        Ok(Ok(report)) => handle.send(DaemonMessageType::ModelsRefreshed {
            providers: report.providers,
            models: report.models,
            status: report.status,
        }),
        Ok(Err(e)) => handle.send(DaemonMessageType::ModelsRefreshFailed { error: e }),
        Err(_) => handle.abandon(),
    }
}

/// Convert the daemon's MCP status record into the wire type sent to clients.
///
/// The two structs carry the same fields, so this is a field-for-field move;
/// it exists as a named function so the conversion has one home and can be
/// unit-tested against a status record.
fn wire_mcp_status(status: crate::mcp::McpServerStatus) -> choreo_proto::McpServerStatus {
    choreo_proto::McpServerStatus {
        slug: status.slug,
        tier: status.tier,
        transport: status.transport,
        target: status.target,
        connected: status.connected,
        tool_count: status.tool_count,
        server_name: status.server_name,
        server_version: status.server_version,
        last_error: status.last_error,
    }
}

/// Handle a `ClientMessageType::McpStatusRequest`: ask the daemon (the sole owner
/// of the `McpManager`) for every server visible to the ATTACHED session
/// (daemon tier plus that session's project servers), convert each record to
/// the wire type, and reply with [`DaemonMessageType::McpStatus`] — including the
/// session's project-root trust context.
fn handle_mcp_status_sync(ctx: &mut ClientCtx<'_>, handle: ReplyHandle) {
    let session_id = *ctx.attached_session_id;
    let result = request_daemon(ctx.daemon_tx, |reply| DaemonCommand::McpStatus {
        session_id,
        reply,
    });
    match result {
        Ok(report) => send_mcp_status(handle, report),
        Err(_) => handle.abandon(),
    }
}

/// Send an [`McpStatusReport`](crate::mcp::McpStatusReport) as a wire
/// [`DaemonMessageType::McpStatus`], converting each server record and carrying the
/// project-root trust context.
fn send_mcp_status(handle: ReplyHandle, report: crate::mcp::McpStatusReport) {
    let servers = report.servers.into_iter().map(wire_mcp_status).collect();
    handle.send(DaemonMessageType::McpStatus {
        servers,
        project_root: report
            .project_root
            .map(|p| p.to_string_lossy().into_owned()),
        project_trusted: report.project_trusted,
        ignored_project_servers: report.ignored_project_servers,
    });
}

/// Handle a `ClientMessageType::McpReconnect`: rebuild one MCP server's connection
/// through the daemon (which also swaps the refreshed tool catalogue), then
/// reply. Success is reported as a refreshed [`DaemonMessageType::McpStatus`] —
/// the same snapshot a status request would return, so the requester sees the
/// server's new connected state and tool count — while a failure is a
/// targeted [`DaemonMessageType::McpReconnectFailed`]. Both requests block here
/// until the daemon has a result, matching the `/mcp` request/reply contract.
fn handle_mcp_reconnect_sync(ctx: &mut ClientCtx<'_>, handle: ReplyHandle, slug: String) {
    let result = request_daemon(ctx.daemon_tx, |reply| DaemonCommand::McpReconnect {
        slug: slug.clone(),
        reply,
    });
    match result {
        Ok(Ok(())) => {
            let session_id = *ctx.attached_session_id;
            let status = request_daemon(ctx.daemon_tx, |reply| DaemonCommand::McpStatus {
                session_id,
                reply,
            });
            match status {
                Ok(report) => send_mcp_status(handle, report),
                Err(_) => handle.abandon(),
            }
        }
        Ok(Err(e)) => handle.send(DaemonMessageType::McpReconnectFailed { slug, error: e }),
        Err(_) => handle.abandon(),
    }
}

/// Handle a `ClientMessageType::McpReload`: ask the daemon (the sole owner of the
/// `McpManager`) to re-read the MCP config and reconcile the running servers,
/// which also swaps the refreshed tool catalogue, then reply. Success is
/// reported as [`DaemonMessageType::McpReloaded`] — the reload summary plus the
/// refreshed status list — while a config read/parse failure is a
/// [`DaemonMessageType::McpReloadFailed`].
fn handle_mcp_reload_sync(ctx: &mut ClientCtx<'_>, handle: ReplyHandle) {
    let session_id = *ctx.attached_session_id;
    let result = request_daemon(ctx.daemon_tx, |reply| DaemonCommand::McpReload {
        session_id,
        reply,
    });
    match result {
        Ok(Ok(outcome)) => {
            let servers = outcome.servers.into_iter().map(wire_mcp_status).collect();
            handle.send(DaemonMessageType::McpReloaded {
                summary: outcome.summary,
                servers,
            });
        }
        Ok(Err(e)) => handle.send(DaemonMessageType::McpReloadFailed { error: e }),
        Err(_) => handle.abandon(),
    }
}

/// Handle a `ClientMessageType::McpTrust` / `McpUntrust`: set (or revoke) trust for
/// the ATTACHED session's project root, then reply with the resulting state.
fn handle_mcp_trust_sync(ctx: &mut ClientCtx<'_>, handle: ReplyHandle, trusted: bool) {
    let Some(session_id) = *ctx.attached_session_id else {
        handle.send(DaemonMessageType::McpTrustUpdated {
            root: None,
            trusted: false,
            message: "no session attached".to_string(),
        });
        return;
    };
    let result = request_daemon(ctx.daemon_tx, |reply| DaemonCommand::McpTrustSet {
        session_id,
        trusted,
        reply,
    });
    match result {
        Ok(outcome) => handle.send(DaemonMessageType::McpTrustUpdated {
            root: outcome.root.map(|p| p.to_string_lossy().into_owned()),
            trusted: outcome.trusted,
            message: outcome.message,
        }),
        Err(_) => handle.abandon(),
    }
}

/// Handle a `ClientMessageType::McpTrustList`: reply with the trusted project roots.
fn handle_mcp_trust_list_sync(ctx: &mut ClientCtx<'_>, handle: ReplyHandle) {
    let result = request_daemon(ctx.daemon_tx, |reply| DaemonCommand::McpTrustList { reply });
    match result {
        Ok(roots) => handle.send(DaemonMessageType::McpTrustList {
            roots: roots
                .into_iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect(),
        }),
        Err(_) => handle.abandon(),
    }
}

fn handle_get_credential_sync(ctx: &mut ClientCtx<'_>, handle: ReplyHandle, service: String) {
    let result = request_daemon(ctx.daemon_tx, |reply| DaemonCommand::GetCredential {
        service: service.clone(),
        reply,
    });
    match result {
        Ok(Some(key)) => handle.send(DaemonMessageType::Credential {
            service,
            key: Some(key),
        }),
        Ok(None) => handle.send(DaemonMessageType::Credential { service, key: None }),
        Err(_) => handle.abandon(),
    }
}

fn handle_delete_session_sync(ctx: &mut ClientCtx<'_>, handle: ReplyHandle, session_id: u64) {
    let result = request_daemon(ctx.daemon_tx, |reply| DaemonCommand::DeleteSession {
        session_id,
        reply,
    });
    match result {
        Ok(Ok(())) => {
            // The daemon broadcasts `SessionDeleted` to all summary subscribers
            // (this client included when it is viewing the session list); the
            // broadcast is unchanged, and this targeted `Accepted` is the
            // request's terminal reply.
            handle.send(DaemonMessageType::Accepted {
                kind: MessageKind::DeleteSession,
            });
        }
        // A delete failure is session-scoped and richer than a bare
        // `Failed`: carrying the `SessionDeleteFailed` event keeps the origin
        // session (which the ACP keys its pending delete on) and lets every
        // front-end reuse its existing session-scoped failure handling. The
        // reply still carries the correlation id (reply-ness is a property of
        // the send, not of the payload type).
        Ok(Err(e)) => handle.send(DaemonMessageType::Session {
            session_id: Some(session_id),
            event: SessionEvent::SessionDeleteFailed {
                error: e.to_string(),
            },
        }),
        // Daemon gone: the reply is impossible, not forgotten.
        Err(_) => handle.abandon(),
    }
}

/// Handle a `SetSessionPinned`/`SetSessionArchived` client message. On success
/// the requester gets a targeted `Accepted` IN ADDITION to the daemon's
/// `SessionFlagsChanged` broadcast (which stays `id: None` and reaches every
/// subscriber). On failure the requester gets a targeted session-scoped
/// `SessionFailed { operation, error }` (the operation names which of the two
/// messages it was) — the same event shape every front-end already renders, so
/// a pin/archive failure is never silently dropped. Follows the same shape as
/// [`handle_delete_session_sync`].
fn handle_set_session_flags_sync(
    ctx: &mut ClientCtx<'_>,
    handle: ReplyHandle,
    session_id: u64,
    pinned: Option<bool>,
    archived: Option<bool>,
    kind: MessageKind,
) {
    let result = request_daemon(ctx.daemon_tx, |reply| DaemonCommand::SetSessionFlags {
        session_id,
        pinned,
        archived,
        reply,
    });
    match result {
        Ok(Ok(())) => handle.send(DaemonMessageType::Accepted { kind }),
        Ok(Err(e)) => handle.send(DaemonMessageType::Session {
            session_id: Some(session_id),
            event: SessionEvent::SessionFailed {
                operation: operation_for_kind(kind).to_string(),
                error: e.to_string(),
            },
        }),
        Err(_) => handle.abandon(),
    }
}

/// The operation label a session-scoped [`SessionEvent::SessionFailed`] carries
/// for the flag mutations, so a front-end can name the failed command (and the
/// session-manager page can surface it inline).
fn operation_for_kind(kind: MessageKind) -> &'static str {
    match kind {
        MessageKind::SetSessionPinned => "set_session_pinned",
        MessageKind::SetSessionArchived => "set_session_archived",
        _ => "session mutation",
    }
}

fn handle_add_credential_sync(
    ctx: &mut ClientCtx,
    kind: MessageKind,
    service: &str,
    encrypted_payload: Vec<u8>,
    // REQUIRED since the per-daemon keystore TOFU design (Task 1 made the
    // proto field non-optional): the credential must be usable immediately.
    unlock_key: Vec<u8>,
) {
    // The daemon command loop enqueues the targeted replies (Unlocked +
    // CredentialAdded, or the failure variant) through the minted reply target
    // DIRECTLY onto this client's writer queue BEFORE its lock-state broadcast —
    // see ORDERING INVARIANT in `DaemonState::handle_unlock`. This thread only
    // waits for the ack.
    let result = request_daemon(ctx.daemon_tx, |ack| DaemonCommand::SaveCredential {
        service: service.to_string(),
        encrypted_blob: encrypted_payload,
        unlock_key,
        reply: Some(ctx.reply_target(kind)),
        ack,
    });
    if result.is_err() {
        warn!("daemon disconnected while handling add credential");
    }
}

/// Enroll a client key in the daemon's ACL. LOCAL (Unix socket) connections
/// only: the check happens HERE, on the connection thread, so a remote
/// client gets its refusal without the command loop ever seeing the command.
/// The trust approver must be at the machine (console or ssh) — an
/// already-remote client must not be able to mint new trust.
fn handle_acl_add_sync(ctx: &mut ClientCtx<'_>, handle: ReplyHandle, pubkey: &str) {
    if !ctx.is_unix {
        warn!(
            "client {}: AclAdd refused: remote connections cannot change the ACL",
            ctx.client_id
        );
        handle.send(DaemonMessageType::AclAddResult {
            ok: false,
            message: "ACL changes are only permitted from local connections".to_string(),
        });
        return;
    }
    let result = request_daemon(ctx.daemon_tx, |reply| DaemonCommand::AclAddCmd {
        pubkey: pubkey.to_string(),
        reply,
    });
    match result {
        Ok(Ok(count)) => handle.send(DaemonMessageType::AclAddResult {
            ok: true,
            message: format!("client key authorized ({count} client(s) now trusted)"),
        }),
        Ok(Err(e)) => handle.send(DaemonMessageType::AclAddResult {
            ok: false,
            message: e,
        }),
        Err(_) => handle.abandon(),
    }
}

fn handle_remove_credential_sync(ctx: &mut ClientCtx<'_>, handle: ReplyHandle, service: String) {
    let result = request_daemon(ctx.daemon_tx, |reply| DaemonCommand::RemoveCredentialCmd {
        service: service.clone(),
        reply,
    });
    match result {
        Ok(Ok(())) => handle.send(DaemonMessageType::CredentialRemoved { service }),
        Ok(Err(e)) => handle.send(DaemonMessageType::CredentialRemoveFailed { service, error: e }),
        Err(_) => handle.abandon(),
    }
}

#[cfg(test)]
mod tests;
