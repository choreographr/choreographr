//! The request/reply machinery the connection thread answers through.
//!
//! Every client request gets exactly one terminal reply, and each reply carries
//! the correlation id that lets the front-end match it. [`ReplyHandle`] is how the
//! exactly-once obligation is made structural rather than commented: the guard is
//! minted per in-scope request, `send`/`abandon` consume it, and dropping an
//! unanswered one trips a debug assertion.
//!
//! Handlers whose reply is produced OFF the connection thread (a session thread or
//! the daemon command loop) mint an owned [`crate::broadcast::ReplyTarget`]
//! instead — see [`super::ClientCtx::reply_target`].

use std::sync::Arc;
use std::sync::atomic::AtomicUsize;

use choreo_proto::DaemonMessageType;

use crate::broadcast::{ReplySink, SubscriberSink};

/// Owns the obligation to answer exactly one request.
///
/// [`send`](Self::send) consumes the handle; dropping an unused handle trips the
/// debug guard, so a connection-thread handler cannot silently forget its
/// reply — "exactly one reply" is enforced, not merely commented. One handle is
/// minted per in-scope request from the acting connection's context (see
/// [`super::ClientCtx::reply_handle`]) and passed by value to the handler that
/// computes the reply inline.
pub(crate) struct ReplyHandle {
    /// The shared reply mechanism (request id + delivery sink + lag counter).
    sink: ReplySink,
    /// Whether the reply obligation has been met (sent) or deliberately
    /// abandoned. `false` only on the drop-unanswered path the guard targets.
    sent: bool,
}

impl ReplyHandle {
    /// Mint a handle answering request `id` through `sink`, accounting bytes into
    /// `global`.
    pub(crate) fn new(id: u64, sink: SubscriberSink, global: Arc<AtomicUsize>) -> Self {
        Self {
            sink: ReplySink::new(id, sink, global),
            sent: false,
        }
    }

    /// Answer the request: stamp the id onto `inner` and enqueue the reply.
    ///
    /// Routes through the shared [`ReplySink`] send (the no-threshold path)
    /// because a reply is a request/response contract that must never be dropped
    /// for lag.
    pub(crate) fn send(mut self, inner: DaemonMessageType) {
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
    pub(crate) fn abandon(mut self) {
        self.sent = true;
    }
}

impl Drop for ReplyHandle {
    fn drop(&mut self) {
        debug_assert!(self.sent, "request {} left unacknowledged", self.sink.id());
    }
}
