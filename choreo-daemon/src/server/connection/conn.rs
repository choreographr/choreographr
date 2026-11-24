//! The per-connection state machine: the dispatch context, the connection
//! itself, and its teardown.
//!
//! [`ClientCtx`] is the borrowed view a handler sees for one request;
//! [`ClientConn`] owns everything that outlives a single message (the command
//! channel, the delivery sink, the lag counter, the session attachment, and the
//! writer thread's join handle). The three transport entry threads differ only in
//! HOW they read one message off the wire and classify transport errors —
//! everything between message-read and teardown lives here.

use std::io;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::time::Instant;

use choreo_proto::{DaemonMessage, DaemonMessageType, MessageKind};

use super::WRITER_JOIN_GRACE;
use super::reply::ReplyHandle;
use super::writer::{ConnectionWriter, writer_thread};
use crate::broadcast::{ReplyTarget, SubscriberSink};
use crate::daemon::DaemonCommand;
use crate::sessions::SessionCommand;

/// Shared per-client context passed through the dispatch and handler functions.
/// Bundles the channels and mutable per-connection state into one struct so
/// the call sites don't pass 5–6 individual arguments to every function.
///
/// The fields are `pub(super)` rather than private because the handler module and
/// the unit tests build these literals directly, from inside the `connection`
/// subtree.
pub(crate) struct ClientCtx<'a> {
    /// This connection's delivery sink.
    pub(super) writer: &'a SubscriberSink,
    /// This connection's own redb handle, shared with every other connection
    /// (redb's `Database` is built for concurrent readers). Used to serve
    /// on-demand image reads DIRECTLY on the connection thread instead of
    /// serializing them on the command loop.
    pub(super) db: &'a redb::Database,
    /// Daemon-wide lag counter, shared by every connection; replies must
    /// increment it so the writer thread's per-dequeue decrement stays
    /// balanced. Held as the `Arc` (not a `&AtomicUsize`) so a
    /// [`ReplyTarget`] can clone it and cross to another thread.
    pub(super) global_lag: &'a Arc<AtomicUsize>,
    pub(super) daemon_tx: &'a crossbeam_channel::Sender<DaemonCommand>,
    pub(super) attached_session_id: &'a mut Option<u64>,
    pub(super) attached_session_tx: &'a mut Option<crossbeam_channel::Sender<SessionCommand>>,
    pub(super) client_id: crate::broadcast::ClientId,
    /// The correlation id of the request currently being dispatched on this
    /// connection — the value stamped onto every reply. Set once at dispatch
    /// entry from the inbound [`choreo_proto::ClientMessage::id`]; a connection
    /// thread handles one request at a time, so a single field is sufficient.
    pub(super) request_id: u64,
    /// Whether this connection arrived over the local Unix socket (vs the
    /// TCP/Noise listener). Trust-boundary input for local-only commands:
    /// `AclAdd` is refused on TCP because the approver for a trust decision
    /// must be at the machine, not on the network.
    pub(super) is_unix: bool,
}

impl ClientCtx<'_> {
    /// Mint the [`ReplyHandle`] for the request currently being dispatched.
    ///
    /// The handle owns its [`crate::broadcast::ReplySink`] (a cheap clone: a
    /// channel `Sender` plus an `Arc`), so it does not borrow `ctx` and a
    /// dispatch arm can mint the handle and still pass `&mut ctx` alongside it.
    pub(super) fn reply_handle(&self) -> ReplyHandle {
        ReplyHandle::new(
            self.request_id,
            self.writer.clone(),
            Arc::clone(self.global_lag),
        )
    }

    /// Mint an OWNED [`ReplyTarget`] for the request currently being dispatched,
    /// tagged with its `kind`.
    ///
    /// Unlike [`reply_handle`](Self::reply_handle) the returned target OWNS
    /// everything it needs (a sink clone and a clone of the `global_lag` `Arc`),
    /// so it can travel over a `SessionCommand`/`DaemonCommand` channel to the
    /// session thread or daemon command loop that will produce the reply. The
    /// daemon owns no correlation state: the target echoes `request_id` and
    /// nothing more.
    pub(super) fn reply_target(&self, kind: MessageKind) -> ReplyTarget {
        ReplyTarget::new(
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
    pub(super) fn ack(&self, kind: MessageKind) {
        self.reply_handle()
            .send(DaemonMessageType::Accepted { kind });
    }
}

/// Clean up a client connection: detach from session, unregister the summary
/// subscriber, wait for the writer thread to drain, and record the disconnect
/// metric.  Owns the `writer_tx` sender and writer handle so both are consumed.
pub(crate) fn cleanup_client(
    attached_session_tx: Option<&crossbeam_channel::Sender<SessionCommand>>,
    client_id: crate::broadcast::ClientId,
    daemon_tx: &crossbeam_channel::Sender<DaemonCommand>,
    writer: SubscriberSink,
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
    // client goes away, then exits on its own. The grace is `choreo-proto`'s,
    // derived beside the stall it has to outlast, so no connection end keeps its
    // own copy of the relation.
    crate::server::lifecycle::join_thread_bounded(
        writer_handle,
        Instant::now() + WRITER_JOIN_GRACE,
    );
    crate::metrics::record_client_disconnected();
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
    writer: SubscriberSink,
    /// Daemon-wide lag counter, shared by every connection; kept so replies
    /// can increment it in balance with the writer thread's per-dequeue
    /// decrement.
    global_lag: Arc<AtomicUsize>,
    client_id: crate::broadcast::ClientId,
    /// Whether this connection arrived over the local Unix socket (vs the
    /// TCP/Noise listener). Trust-boundary input for local-only commands
    /// (see [`ClientCtx::is_unix`]).
    is_unix: bool,
    attached_session_id: Option<u64>,
    attached_session_tx: Option<crossbeam_channel::Sender<SessionCommand>>,
    /// Handle to the writer thread spawned in `new`; joined (with a bound) by
    /// `finish()` via `cleanup_client`.
    writer_handle: std::thread::JoinHandle<()>,
}

/// The transport-independent inputs shared by every connection thread: the
/// daemon command channel, this connection's writer sink and its receiver, the
/// daemon-wide lag counter, this connection's DB handle, and the local-domain
/// flag.
///
/// Bundled so each of the three transport entry points takes ONE such value
/// instead of seven positional arguments, and so [`ClientConn::new`] takes a
/// single value plus the one transport-specific field (the byte-writer). The
/// `too_many_arguments` lint then never applies.
pub(crate) struct ConnThreadArgs {
    pub daemon_tx: crossbeam_channel::Sender<DaemonCommand>,
    /// This connection's own handle to the shared redb database (see
    /// [`ClientCtx::db`]).
    pub db: Arc<redb::Database>,
    /// This connection's delivery sink.
    pub writer: SubscriberSink,
    /// The receiver end of this connection's writer channel.
    pub writer_rx: crossbeam_channel::Receiver<DaemonMessage>,
    /// Daemon-wide lag counter, shared by every connection.
    pub global_lag: Arc<AtomicUsize>,
    pub client_id: crate::broadcast::ClientId,
    /// Whether this connection is the LOCAL trust domain — the Unix socket or
    /// the in-process embedded link — as opposed to the TCP/Noise listener.
    /// Trust-boundary input for local-only commands (see [`ClientCtx::is_unix`]).
    pub is_unix: bool,
}

impl ClientConn {
    /// Build a connection and spawn its writer thread over the given
    /// transport-specific writer. Spawning here keeps the socket threads to just:
    /// clone the stream into a writer, call this, then read/dispatch/finish.
    pub(crate) fn new<W: ConnectionWriter + Send + 'static>(
        args: ConnThreadArgs,
        writer_buf: W,
    ) -> Self {
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
    /// terminate the connection).
    pub(crate) fn dispatch(&mut self, msg: choreo_proto::ClientMessage) -> io::Result<()> {
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
        super::handlers::dispatch_client_message(msg, &mut ctx)
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

/// Switch the client's attachment from the old session to a new one.
/// Skips detaching when re-attaching to the same session to avoid
/// killing the session's only subscriber.
pub(crate) fn switch_attached_session(
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
