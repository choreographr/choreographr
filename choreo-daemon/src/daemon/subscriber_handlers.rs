//! Client-connection subscriber lifecycle: registration, lossless broadcast
//! fan-out, lag-eviction, shutdown notification, and disconnect cleanup.
//!
//! These are the `impl DaemonState` methods that manage the per-client
//! subscriber maps (`summary_subscribers`, `activity_subscribers`,
//! `client_writers`, `client_subscribed_sessions`) and apply the shared
//! lossless broadcast policy from `crate::broadcast`. They live in a child
//! module so `daemon.rs` stays focused on the daemon's core command handling
//! (session CRUD, accounts, catalog); the methods are `pub(super)` because
//! `handle_command` in the parent dispatches the corresponding
//! `DaemonCommand` variants here.
//!
//! As a CHILD of `crate::daemon`, this module reaches the parent's private
//! items (`DaemonState` fields, `catalog_provider_pairs`, ...) via
//! `use super::*`. The one shared broadcast helper it needs from outside the
//! daemon module is imported explicitly.

use super::*;
use crate::broadcast::fan_out_evicting;

impl DaemonState {
    /// Send a message to all summary subscribers, removing dead ones.
    ///
    /// Lossless + lag-eviction, shared with the activity broadcast and the
    /// per-session broadcast (see `crate::broadcast`): every message is
    /// enqueued into each subscriber's UNBOUNDED queue (never dropped, never
    /// blocking the command loop), and a subscriber whose queue crossed the
    /// lag limits is evicted (disconnected) so the backlog stays bounded.
    pub(super) fn broadcast(&mut self, msg: DaemonMessage) {
        let (evict_clients, evict_largest) = fan_out_evicting(
            &mut self.summary_subscribers,
            &msg,
            &self.lag_limits,
            &self.global_lag,
            |_| false, // summary subscribers are never duplicate-suppressed
        );
        self.finish_evictions(evict_clients, evict_largest);
    }

    /// Process the eviction work collected by [`fan_out_evicting`]:
    /// disconnect each over-lag client, and (when the daemon-wide budget was
    /// crossed) disconnect the currently most-lagging client. Runs AFTER the
    /// retain loop because eviction mutates `self` (removing sinks) while
    /// the loop still borrows the subscriber map.
    pub(super) fn finish_evictions(&mut self, evict_clients: Vec<u64>, evict_largest: bool) {
        for client_id in evict_clients {
            self.handle_evict_client(client_id);
        }
        if evict_largest {
            self.handle_evict_largest_lagging();
        }
    }

    /// Register a client to receive session summary broadcasts.
    pub(super) fn handle_register_summary_subscriber(
        &mut self,
        client_id: u64,
        writer: SubscriberSink,
    ) {
        self.summary_subscribers.insert(client_id, writer);
    }

    /// Unregister a client from session summary broadcasts.
    pub(super) fn handle_unregister_summary_subscriber(&mut self, client_id: u64) {
        self.summary_subscribers.remove(&client_id);
    }

    /// Broadcast a session status change to all summary subscribers and keep
    /// the metadata index in sync.
    ///
    /// This is the choke point that fixes stale statuses on the sessions page:
    /// the session thread broadcasts status changes (see `handle_status_changed`
    /// in sessions.rs) but never updates the daemon's `session_metadata` index,
    /// so a subsequent ListSessions would serve an outdated status.  Updating
    /// the index here covers every status-transition path.
    ///
    /// Status transitions are internal pipeline churn, not modifications: the
    /// index *status* is refreshed but `last_modified` is left untouched, so
    /// the sessions list does not re-sort on every tool call mid-request.
    /// Only completed requests / explicit edits bump the timestamp (via
    /// `UpdateMetadata`).  The message carries the index's current
    /// `last_modified` so clients' monotonic `max()` guards keep both sides
    /// in sync.
    ///
    /// Duplicate-suppression: every sender of `BroadcastSessionStatus` (the
    /// session thread's `handle_status_changed` and the exit-to-Inactive path)
    /// has ALREADY broadcast the same `SessionStatusChanged` through the
    /// per-session fan-out (`crate::broadcast::fan_out_evicting` on the
    /// session's own subscriber map) — which also forwards it to the
    /// all-activity subscribers via `BroadcastActivity`. So a client that is a
    /// direct subscriber of this session received the change there, and a
    /// client subscribed to all activity received it through the activity
    /// fan-out; delivering either of them again here would duplicate the
    /// message. The summary fan-out therefore skips both classes and only
    /// serves clients that subscribe to the session list without receiving
    /// the change elsewhere (the ordering is safe: the session thread sends
    /// the activity forward and this summary command over the SAME daemon
    /// channel in that order, so the daemon processes the activity delivery
    /// before this fan-out runs).
    pub(super) fn handle_broadcast_session_status(
        &mut self,
        session_id: u64,
        status: SessionStatus,
    ) {
        let last_modified = match self.session_metadata.get_mut(&session_id) {
            Some(meta) => {
                meta.status = status.clone();
                meta.last_modified
            }
            // Deleted sessions have no index entry; the message is dropped
            // below anyway, so a default timestamp is harmless.
            None => 0,
        };
        let msg = DaemonMessage::SessionStatusChanged {
            session_id,
            status,
            last_modified,
        };
        // A deleted session's still-shutting-down thread must not emit ghost
        // status events for a session the user removed; the index is empty
        // for deleted sessions, so use its presence as the "session exists"
        // signal.
        if self.session_metadata.contains_key(&session_id) {
            // Shared lossless + lag-eviction policy, with the duplicate
            // suppression described above: skip direct session subscribers of
            // this session (they got the change via the per-session fan-out)
            // and activity subscribers (they got it via the activity fan-out),
            // so every client receives `SessionStatusChanged` exactly once.
            let (evict_clients, evict_largest) = fan_out_evicting(
                &mut self.summary_subscribers,
                &msg,
                &self.lag_limits,
                &self.global_lag,
                |client_id| {
                    // Direct session subscriber of the changed session — the
                    // per-session broadcast already delivered this change.
                    if self
                        .client_subscribed_sessions
                        .get(&client_id)
                        .is_some_and(|sessions| sessions.contains(&session_id))
                    {
                        return true;
                    }
                    // All-activity subscriber — the session thread's broadcast
                    // forwarded this exact change via `BroadcastActivity`.
                    self.activity_subscribers.contains_key(&client_id)
                },
            );
            self.finish_evictions(evict_clients, evict_largest);
        }
    }

    /// Register a client to receive all session activity broadcasts.
    pub(super) fn handle_register_activity_subscriber(
        &mut self,
        client_id: u64,
        writer: SubscriberSink,
    ) {
        info!("registering activity subscriber: client_id={}", client_id);
        self.activity_subscribers.insert(client_id, writer.clone());
        // Send the CURRENT provider list to the freshly-subscribed client so
        // its provider picker reflects the live catalog immediately (not just
        // the static default) — a client that connects after the daemon's
        // startup refresh has already broadcast would otherwise wait for the
        // next catalog change. Enqueued through the lossless sink so the
        // writer thread's byte accounting stays balanced; the outcome is
        // ignored because a fresh subscription cannot be over the lag cap.
        let providers = catalog_provider_pairs();
        let _ = writer.enqueue(
            &DaemonMessage::CatalogUpdated { providers },
            &self.lag_limits,
            &self.global_lag,
        );
    }

    /// Unregister a client from all session activity broadcasts.
    ///
    /// Only removes from the activity subscriber map — does NOT clear
    /// `client_subscribed_sessions`.  Session subscription tracking is
    /// cleaned up by explicit `UntrackSessionSubscription` messages sent
    /// from session threads on client detach, and by `handle_client_disconnected`
    /// when the client fully disconnects.
    ///
    /// This preserves the invariant that a client that explicitly unsubscribes
    /// from all activity but remains attached to sessions can re-subscribe
    /// without causing duplicate delivery (the dedup filter in
    /// `handle_broadcast_activity` still knows about their session subscriptions).
    pub(super) fn handle_unregister_activity_subscriber(&mut self, client_id: u64) {
        debug!("unregistering activity subscriber: client_id={}", client_id);
        self.activity_subscribers.remove(&client_id);
    }

    /// Register a connection's writer channel so the shutdown path can route
    /// `ShuttingDown` through that connection's single writer thread.
    pub(super) fn handle_register_client_writer(&mut self, client_id: u64, writer: SubscriberSink) {
        debug!("registering client writer: client_id={}", client_id);
        // A fresh connection owns its client_id, so any prior entry is stale.
        self.client_writers.insert(client_id, writer);
    }

    /// Disconnect a client whose delivery queue crossed the lag limits.
    ///
    /// Idempotent (no-op for an unknown client): multiple producers can
    /// observe `ClientOverLag` for the same client before the first eviction
    /// command lands, and each re-signal must not double-evict or panic.
    ///
    /// The connection is torn down WITHOUT the daemon holding a socket
    /// handle: the `Evicted` advisory is enqueued best-effort, and the
    /// connection is reaped by its own writer thread — a healthy writer
    /// flushes the advisory and closes its socket (notify-before-EOF); a
    /// wedged writer (client not reading) hits its socket write timeout
    /// (`server::connection::WRITER_WRITE_TIMEOUT`), the write fails, and
    /// the writer shuts the socket down, unblocking the reader's blocking
    /// read and running the normal `cleanup_client` teardown.
    pub(super) fn handle_evict_client(&mut self, client_id: u64) {
        if !self.client_writers.contains_key(&client_id) {
            return;
        }
        warn!(
            "evicting lagging client: client_id={}, backlog_bytes={}",
            client_id,
            self.client_writers
                .get(&client_id)
                .map_or(0, |s| s.bytes_in_flight.load(Ordering::Relaxed))
        );
        self.summary_subscribers.remove(&client_id);
        self.activity_subscribers.remove(&client_id);
        // Promptly remove this client from every session's subscriber map
        // instead of waiting for the lazy disconnect detection on the next
        // broadcast — the evicted client's queued bytes should be released
        // as soon as possible, and a session must not keep streaming to a
        // client that is being torn down.
        self.remove_client_from_sessions(client_id);
        // Best-effort advisory: a healthy writer flushes it and closes its
        // own socket; a wedged writer never sees it (the write timeout
        // reaps the connection instead). Enqueue BEFORE dropping the sink,
        // through the accounting path: the writer's per-dequeue decrement
        // (or the exit drain, if the advisory is abandoned behind the stop
        // point) needs a matching increment, and a dead receiver
        // self-corrects inside `send_unchecked`.
        if let Some(sink) = self.client_writers.get(&client_id) {
            let _ = sink.send_unchecked(&DaemonMessage::Evicted, &self.global_lag);
        }
        self.client_writers.remove(&client_id);
        crate::metrics::record_eviction();
    }

    /// Disconnect the currently most-lagging client (used when the daemon-wide
    /// backlog crosses [`LagLimits::global_budget`]). Only `client_writers`
    /// is scanned: every real connection's per-client counter lives on its
    /// writer sink (the activity/summary/session maps hold clones of that
    /// same sink, sharing one `Arc<AtomicUsize>`), and a client without a
    /// writer entry has no connection to tear down — `handle_evict_client`
    /// would no-op on it, silently failing to relieve the pressure.
    pub(super) fn handle_evict_largest_lagging(&mut self) {
        let mut best: Option<(u64, usize)> = None;
        let mut consider = |id: &u64, sink: &SubscriberSink| {
            let bytes = sink.bytes_in_flight.load(Ordering::Relaxed);
            if bytes > 0 && best.is_none_or(|(_, b)| bytes > b) {
                best = Some((*id, bytes));
            }
        };
        for (id, sink) in &self.client_writers {
            consider(id, sink);
        }
        if let Some((client_id, _)) = best {
            self.handle_evict_client(client_id);
        }
    }

    /// Deliver `DaemonMessage::ShuttingDown` to every connected client via its
    /// writer channel; each connection's writer thread then closes its own
    /// socket, so clients observe the notification before EOF.
    ///
    /// With the lossless unbounded channels an enqueue can never be `Full` —
    /// the old bounded round-robin poll existed only for the bounded 128-slot
    /// channels this design replaced. The wedged-writer case (client open but
    /// not reading, writer stuck in a blocking socket write) is still bounded
    /// by the writer-join grace in `cleanup_client` + `run_server`, unchanged.
    pub(super) fn handle_broadcast_shutting_down(&mut self) {
        let clients = self.client_writers.len();
        info!("broadcasting ShuttingDown to {clients} client(s)");
        self.client_writers.retain(|client_id, sink| {
            // Accounted send: the writer thread decrements on dequeue (and
            // the exit drain picks up anything queued behind the
            // notification), so the notification must be counted like every
            // other message; `send_unchecked` self-corrects when the
            // receiver is gone.
            if sink.send_unchecked(&DaemonMessage::ShuttingDown, &self.global_lag) {
                true
            } else {
                warn!("removing disconnected client {client_id} during shutdown");
                false
            }
        });
    }

    /// Clean up all per-client tracking when a client disconnects.
    /// Removes from summary subscribers, activity subscribers, session
    /// subscription tracking, the writer registry, and the evict handle in a
    /// single atomic operation so stale entries don't accumulate.
    pub(super) fn handle_client_disconnected(&mut self, client_id: u64) {
        info!("client disconnected cleanup: client_id={}", client_id);
        self.summary_subscribers.remove(&client_id);
        self.activity_subscribers.remove(&client_id);
        // Promptly remove the client from every session it was attached to
        // (same as eviction), so a session does not keep streaming to a dead
        // client's sink until the next broadcast detects the disconnect.
        self.remove_client_from_sessions(client_id);
        // Drop the registered writer channel so this connection's writer
        // thread can exit: with the connection-local sender (dropped by
        // cleanup_client) gone too, writer_rx disconnects and the thread's
        // for-loop terminates.
        self.client_writers.remove(&client_id);
    }

    /// Remove `client_id` from every session's subscriber map via
    /// `RemoveSubscriber` commands, and drop its session-membership tracking.
    /// Used when a client is being torn down (lag-evicted or fully
    /// disconnected) so sessions stop streaming to it promptly instead of
    /// waiting for the next broadcast to notice the dead sink; releasing the
    /// queued bytes sooner also relieves lag-budget pressure earlier.
    pub(super) fn remove_client_from_sessions(&mut self, client_id: u64) {
        if let Some(sessions) = self.client_subscribed_sessions.remove(&client_id) {
            for session_id in &sessions {
                if let Some(entry) = self.active_sessions.get(session_id) {
                    let _ = entry
                        .cmd_tx
                        .send(SessionCommand::RemoveSubscriber { client_id });
                }
            }
        }
    }

    /// Track that `client_id` is a direct subscriber of `session_id`.
    /// Idempotent — re-attach to the same session is a no-op.
    pub(super) fn handle_track_session_subscription(&mut self, client_id: u64, session_id: u64) {
        debug!(
            "track session subscription: client_id={}, session_id={}",
            client_id, session_id
        );
        self.client_subscribed_sessions
            .entry(client_id)
            .or_default()
            .insert(session_id);
    }

    /// Untrack that `client_id` is no longer a direct subscriber of `session_id`.
    pub(super) fn handle_untrack_session_subscription(&mut self, client_id: u64, session_id: u64) {
        debug!(
            "untrack session subscription: client_id={}, session_id={}",
            client_id, session_id
        );
        if let std::collections::hash_map::Entry::Occupied(mut entry) =
            self.client_subscribed_sessions.entry(client_id)
        {
            entry.get_mut().remove(&session_id);
            if entry.get().is_empty() {
                entry.remove();
            }
        }
    }

    /// Broadcast a message to all activity subscribers, removing dead ones.
    ///
    /// Lossless + lag-eviction, shared with the summary broadcast and the
    /// per-session broadcast (see `crate::broadcast`): every message is
    /// enqueued into each subscriber's UNBOUNDED queue (never dropped, never
    /// blocking the command loop), and a subscriber whose queue crossed the
    /// lag limits is evicted so the backlog stays bounded.
    ///
    /// Skips delivery to clients that are also direct session subscribers of
    /// the originating session — those clients receive the message through
    /// the per-session subscriber path, avoiding duplicate delivery.
    pub(super) fn handle_broadcast_activity(&mut self, msg: DaemonMessage) {
        let origin_session_id = msg.session_id();
        let (evict_clients, evict_largest) = fan_out_evicting(
            &mut self.activity_subscribers,
            &msg,
            &self.lag_limits,
            &self.global_lag,
            |client_id| {
                // Skip if this client is also a direct subscriber of the
                // session that originated this message — they'll receive it
                // through the per-session broadcast path, avoiding duplicate
                // delivery.
                if let Some(ref sid) = origin_session_id
                    && let Some(sessions) = self.client_subscribed_sessions.get(&client_id)
                    && sessions.contains(sid)
                {
                    return true;
                }
                false
            },
        );
        self.finish_evictions(evict_clients, evict_largest);
    }
}
