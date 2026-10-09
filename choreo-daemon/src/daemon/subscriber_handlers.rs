//! Client-connection subscriber lifecycle: registration, lossless broadcast
//! fan-out, lag-eviction, shutdown notification, and disconnect cleanup.
//!
//! These are the `impl DaemonState` methods that manage the per-client
//! `clients` map (each [`ClientState`] carrying its writer sink, its
//! summary/activity flags, and its attached sessions) and apply the shared
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

use super::{
    ClientId, ClientState, DaemonMessage, DaemonState, Ordering, SessionCommand, SessionEvent,
    SessionStatus, SubscriberSink, catalog_provider_pairs, debug, info, warn,
};
use crate::broadcast::fan_out_evicting;
use choreo_proto::{DaemonMessageType, KeystoreState};

/// True when a [`DaemonCommand::BroadcastActivity`] command's provenance and
/// its message's origin disagree — a dedup-contract violation.
///
/// The dedup filter reads ONLY the command field: `Some(origin)` skips
/// clients that are also direct subscribers of that origin session (they are
/// assumed to receive the message via the per-session bus instead, where the
/// envelope's own `session_id` is the origin). The command and the message
/// must therefore AGREE on the origin. Three disagreement modes:
/// - `Some(origin)` command + non-`Session` message: the origin's direct
///   subscribers are skipped on the activity path (dedup) yet never receive
///   the message on the per-session path (only `Session` envelopes ride it)
///   — lost entirely for them.
/// - `Some(origin)` command + `Session` envelope whose own `session_id` is
///   absent or different: the dedup suppresses against the wrong session —
///   the envelope's REAL-origin subscribers miss the message (they are only
///   skipped when subscribed to the command's origin), and the command
///   origin's subscribers receive a foreign session's event that neither the
///   per-session path nor their own subscription produced.
/// - `None` command + a session-scoped `Session` envelope: no dedup runs, so
///   the envelope origin's direct subscribers receive the event TWICE (here
///   and on the per-session bus).
///
/// Every current producer satisfies the contract (the session thread
/// forwards `Some(ctx.session_id)` paired with a `Session { session_id:
/// Some(ctx.session_id), .. }` envelope; the daemon's catalog broadcast uses
/// `None` with a flat message); the predicate is the tripwire that keeps a
/// future misuse from silently mis-routing messages. Split out as a pure
/// function so the contract is unit-testable without capturing `tracing`.
pub(super) fn violates_broadcast_origin_contract(
    session_id: Option<u64>,
    msg: &DaemonMessage,
) -> bool {
    match &msg.inner {
        // The envelope carries its own origin — it must match the command's.
        DaemonMessageType::Session {
            session_id: envelope_id,
            ..
        } => match (session_id, envelope_id) {
            (Some(origin), Some(inner)) => origin != *inner,
            (Some(_), None) | (None, Some(_)) => true,
            (None, None) => false,
        },
        // Flat messages have no origin of their own: a `Some` command origin
        // is a contract violation (mode 1 above); `None` is the global/
        // control provenance.
        _ => session_id.is_some(),
    }
}

impl DaemonState {
    /// Send a message to all session-summary subscribers, removing dead ones.
    ///
    /// This is the daemon-generated LIFECYCLE broadcast — `SessionCreated`,
    /// `SessionDeleted`, and the exit `SessionStatusChanged(Sleeping)` — the
    /// only session-list messages NOT produced by a session thread, so unlike
    /// `handle_broadcast_session_status` there is no per-session fan-out or
    /// `BroadcastActivity` forward to dedup against. Delivery must therefore
    /// reach BOTH subscriber classes directly, or a client subscribed to all
    /// activity (but not the summary bus) would never learn of sessions being
    /// created/updated/deleted:
    /// - all-activity subscribers first (they receive every lifecycle event),
    /// - then summary subscribers, SKIPPING all-activity clients (they just
    ///   got it via the activity fan-out, exactly as the status-change summary
    ///   fan-out skips them), so a client on both buses gets exactly one copy.
    ///
    /// Lossless + lag-eviction, shared with the activity broadcast and the
    /// per-session broadcast (see `crate::broadcast`): every message is
    /// enqueued into each subscriber's UNBOUNDED queue (never dropped, never
    /// blocking the command loop), and a subscriber whose queue crossed the
    /// lag limits is evicted (disconnected) so the backlog stays bounded. The
    /// two fan-outs run on the daemon command thread in the order written
    /// here, so the summary skip always observes the same membership the
    /// activity fan-out just served.
    pub(super) fn broadcast(&mut self, inner: &DaemonMessageType) {
        // Wrap the payload as a broadcast (`id: None`): summary + activity
        // fan-outs deliver process-wide notifications, never a targeted reply.
        let msg = DaemonMessage::broadcast(inner.clone());
        // Lifecycle events ride the activity bus too — an all-activity
        // subscriber must see sessions appear and disappear even though it
        // never joined the session-list bus.
        let (evict_activity, evict_activity_largest) = fan_out_evicting(
            &mut self.clients,
            &msg,
            &self.lag_limits,
            &self.global_lag,
            |_id, client| !client.wants_activity, // only activity subscribers
        );
        self.finish_evictions(evict_activity, evict_activity_largest);

        let (evict_clients, evict_largest) = fan_out_evicting(
            &mut self.clients,
            &msg,
            &self.lag_limits,
            &self.global_lag,
            // Summary subscribers, EXCEPT all-activity clients the fan-out
            // above already served — skipping keeps per-client delivery
            // exactly-once across the two buses.
            |_id, client| !client.wants_summary || client.wants_activity,
        );
        self.finish_evictions(evict_clients, evict_largest);
    }

    /// Process the eviction work collected by [`fan_out_evicting`]:
    /// disconnect each over-lag client, and (when the daemon-wide budget was
    /// crossed) disconnect the currently most-lagging client. Runs AFTER the
    /// retain loop because eviction mutates `self` (removing sinks) while
    /// the loop still borrows the subscriber map.
    pub(super) fn finish_evictions(&mut self, evict_clients: Vec<ClientId>, evict_largest: bool) {
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
        client_id: ClientId,
        writer: &SubscriberSink,
    ) {
        // Create the entry if the writer was not registered yet, then flip the
        // flag — keeps a subscribe self-contained regardless of its ordering
        // with `RegisterClientWriter`.
        self.clients
            .entry(client_id)
            .or_insert_with(|| ClientState::new(writer.clone()))
            .wants_summary = true;
    }

    /// Unregister a client from session summary broadcasts.
    pub(super) fn handle_unregister_summary_subscriber(&mut self, client_id: ClientId) {
        if let Some(client) = self.clients.get_mut(&client_id) {
            client.wants_summary = false;
        }
    }

    /// Broadcast a session status change to all summary subscribers and keep
    /// the metadata index in sync.
    ///
    /// This is the choke point that fixes stale statuses on the sessions page:
    /// the session thread broadcasts status changes (see `handle_status_changed`
    /// in sessions.rs) but never updates the daemon's `session_metadata` index,
    /// so a subsequent `ListSessions` would serve an outdated status.  Updating
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
        let msg = DaemonMessage::broadcast(DaemonMessageType::Session {
            session_id: Some(session_id),
            event: SessionEvent::SessionStatusChanged {
                status,
                last_modified,
            },
        });
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
                &mut self.clients,
                &msg,
                &self.lag_limits,
                &self.global_lag,
                |_id, client| {
                    // Only summary subscribers; skip a direct session
                    // subscriber of the changed session (the per-session
                    // broadcast already delivered this change) and any
                    // all-activity subscriber (the session thread's broadcast
                    // forwarded this exact change via `BroadcastActivity`).
                    !client.wants_summary
                        || client.sessions.contains(&session_id)
                        || client.wants_activity
                },
            );
            self.finish_evictions(evict_clients, evict_largest);
        }
    }

    /// Register a client to receive all session activity broadcasts, pushing it
    /// the current catalog + keystore state so its view is live at once.
    pub(super) fn handle_register_activity_subscriber(
        &mut self,
        client_id: ClientId,
        writer: &SubscriberSink,
    ) {
        info!("registering activity subscriber: client_id={}", client_id);
        // Create the entry if the writer was not registered yet, then flip the
        // flag (keeps a subscribe self-contained regardless of its ordering
        // with `RegisterClientWriter`).
        self.clients
            .entry(client_id)
            .or_insert_with(|| ClientState::new(writer.clone()))
            .wants_activity = true;
        // Send the CURRENT provider list to the freshly-subscribed client so
        // its provider picker reflects the live catalog immediately (not just
        // the static default) — a client that connects after the daemon's
        // startup refresh has already broadcast would otherwise wait for the
        // next catalog change. Enqueued through the lossless sink so the
        // writer thread's byte accounting stays balanced; the outcome is
        // ignored because a fresh subscription cannot be over the lag cap.
        let providers = catalog_provider_pairs();
        let _ = writer.enqueue(
            &DaemonMessage::broadcast(DaemonMessageType::CatalogUpdated { providers }),
            &self.lag_limits,
            &self.global_lag,
        );
        // Send the CURRENT keystore status so a freshly-connecting client
        // learns immediately whether the daemon is `Unbound` (no binding yet
        // — it must auto-bind), `Locked` (bound, present the key to unlock),
        // or `Unlocked`. This is what makes the startup banner possible
        // without waiting for the next lock-state *transition*, AND what lets
        // a first-run client with no key discover it should bind the daemon.
        // Mirrors the send-on-subscribe catalog: flat control message,
        // lossless enqueue, outcome ignored (a fresh subscription cannot be
        // over lag).
        let keystore_msg = self.current_keystore_message();
        let _ = writer.enqueue(&keystore_msg, &self.lag_limits, &self.global_lag);
    }

    /// Unregister a client from all session activity broadcasts.
    ///
    /// Only clears the entry's activity flag — it does NOT drop the client's
    /// session memberships.  Those are cleaned up by explicit
    /// `UntrackSessionSubscription` messages sent from session threads on client
    /// detach, and by `handle_client_disconnected`/
    /// [`remove_client`](Self::remove_client) when the client is torn down.
    ///
    /// This preserves the invariant that a client that explicitly unsubscribes
    /// from all activity but remains attached to sessions can re-subscribe
    /// without causing duplicate delivery (the dedup filter in
    /// `handle_broadcast_activity` still knows about their session subscriptions).
    pub(super) fn handle_unregister_activity_subscriber(&mut self, client_id: ClientId) {
        debug!("unregistering activity subscriber: client_id={}", client_id);
        if let Some(client) = self.clients.get_mut(&client_id) {
            client.wants_activity = false;
        }
    }

    /// The flat control message representing the daemon's CURRENT keystore
    /// status: `Unbound` when no binding exists yet, else `Locked`/`Unlocked`
    /// per the in-memory lock flag. One construction site so the
    /// subscribe-time push and the transition broadcast cannot drift.
    pub(super) fn current_keystore_message(&self) -> DaemonMessage {
        let state = if !self.keystore_bound {
            KeystoreState::Unbound
        } else if self.locked {
            KeystoreState::Locked
        } else {
            KeystoreState::Unlocked
        };
        // A broadcast (`id: None`): the keystore status push is unsolicited,
        // not a reply to any request.
        DaemonMessage::broadcast(DaemonMessageType::Keystore { state })
    }

    /// Broadcast the daemon's CURRENT keystore status to every activity
    /// subscriber.
    ///
    /// Called on a REAL status TRANSITION (unbound→bound+unlocked after a
    /// successful `BindKeystore`; locked→unlocked after a successful `Unlock` /
    /// `AddCredential` implicit unlock; unlocked→locked on `/lock`) so every
    /// connected client re-latches its banner — client B unlocking updates
    /// client A's status bar. `None` provenance (a flat, empty-variant control
    /// message) rides the standard lossless activity fan-out; the acting
    /// client, if it is an activity subscriber, receives this in addition to
    /// its own `send_to_writer` reply — that duplicate is idempotent and
    /// cheap, so keeping one shared broadcast path beats special-casing it
    /// away.
    pub(super) fn broadcast_keystore_state(&mut self) {
        let msg = self.current_keystore_message();
        self.handle_broadcast_activity(None, &msg);
    }

    /// Register a connection's writer channel so the shutdown path can route
    /// `ShuttingDown` through that connection's single writer thread, and so
    /// the client's entry exists before any subscription or membership command
    /// for it is processed.
    ///
    /// This is the FIRST command the daemon ever sees for a `ClientId`: the
    /// acceptor registers the writer before it spawns the connection thread (see
    /// [`register_client_writer`](crate::server::connection::register_client_writer)),
    /// and every later command for that id — subscribe, track — is sent on the
    /// same FIFO command channel by that thread, so it is always processed after
    /// this one. Should that order ever invert, the entry-based update below
    /// keeps any subscription state already recorded rather than wiping it, and
    /// simply adopts the fresh writer.
    pub(super) fn handle_register_client_writer(
        &mut self,
        client_id: ClientId,
        writer: SubscriberSink,
    ) {
        debug!("registering client writer: client_id={}", client_id);
        // Adopt the connection's writer while preserving any state a
        // (mis-ordered) subscribe already recorded, instead of resetting the
        // entry wholesale.
        let entry = self
            .clients
            .entry(client_id)
            .or_insert_with(|| ClientState::new(writer.clone()));
        entry.writer = writer;
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
    pub(super) fn handle_evict_client(&mut self, client_id: ClientId) {
        // Single lookup serving both the idempotency guard and the two uses
        // below (backlog read + advisory send). Idempotent (no-op for an
        // unknown client): multiple producers can observe `ClientOverLag`
        // for the same client before the first eviction command lands, and
        // each re-signal must not double-evict or panic.
        let Some(client) = self.clients.get(&client_id) else {
            return;
        };
        warn!(
            "evicting lagging client: client_id={}, backlog_bytes={}",
            client_id,
            client.writer.bytes_in_flight.load(Ordering::Relaxed)
        );
        // Best-effort advisory: a healthy writer flushes it and closes its
        // own socket; a wedged writer never sees it (the write timeout
        // reaps the connection instead). Enqueue BEFORE dropping the entry,
        // through the accounting path: the writer's per-dequeue decrement
        // (or the exit drain, if the advisory is abandoned behind the stop
        // point) needs a matching increment, and a dead receiver
        // self-corrects inside `send_unchecked`. Sent while the entry is
        // still borrowed; the borrow ends here, before `remove_client` below
        // (the daemon command loop is single-threaded, so ordering the
        // advisory ahead of the removal is unobservable).
        let _ = client.writer.send_unchecked(
            &DaemonMessage::broadcast(DaemonMessageType::Evicted),
            &self.global_lag,
        );
        // Drop the whole entry (writer + subscription state) in one step and
        // tell every session it was attached to stop streaming to it — the
        // advisory is already queued, and the connection thread's own sink
        // clone (dropped by `cleanup_client`) is what keeps the writer
        // draining until it closes the socket.
        self.remove_client(client_id);
        crate::metrics::record_eviction();
    }

    /// Disconnect the currently most-lagging client (used when the daemon-wide
    /// backlog crosses [`LagLimits::global_budget`]). Every connected client has
    /// exactly one `clients` entry, and its per-client byte counter lives on
    /// that entry's writer sink (the session subscriber maps hold clones of the
    /// same sink, sharing one `Arc<AtomicUsize>`), so the scan covers them all.
    pub(super) fn handle_evict_largest_lagging(&mut self) {
        // Hand-rolled max over the per-client byte counters, expressed as a
        // `max_by_key` scan: zero-lag writers are excluded (they have nothing
        // to relieve) and the winner is the largest in-flight backlog. The id
        // is copied out (ClientId is Copy) so the borrow ends before the evict.
        let best = self
            .clients
            .iter()
            .filter(|(_, client)| client.writer.bytes_in_flight.load(Ordering::Relaxed) > 0)
            .max_by_key(|(_, client)| client.writer.bytes_in_flight.load(Ordering::Relaxed))
            .map(|(client_id, _)| *client_id);
        if let Some(client_id) = best {
            self.handle_evict_client(client_id);
        }
    }

    /// Deliver `DaemonMessageType::ShuttingDown` to every connected client via its
    /// writer channel; each connection's writer thread then closes its own
    /// socket, so clients observe the notification before EOF.
    ///
    /// With the lossless unbounded channels an enqueue can never be `Full` —
    /// the old bounded round-robin poll existed only for the bounded 128-slot
    /// channels this design replaced. The wedged-writer case (client open but
    /// not reading, writer stuck in a blocking socket write) is still bounded
    /// by the writer-join grace in `cleanup_client` + `run_server`, unchanged.
    pub(super) fn handle_broadcast_shutting_down(&mut self) {
        // Hoist the shared counters out of the loop: the retain closure borrows
        // the map mutably, so it must not also touch `self` fields.
        let global = &self.global_lag;
        let clients = self.clients.len();
        info!("broadcasting ShuttingDown to {clients} client(s)");
        self.clients.retain(|client_id, client| {
            // Accounted send: the writer thread decrements on dequeue (and
            // the exit drain picks up anything queued behind the
            // notification), so the notification must be counted like every
            // other message; `send_unchecked` self-corrects when the
            // receiver is gone.
            if client.writer.send_unchecked(
                &DaemonMessage::broadcast(DaemonMessageType::ShuttingDown),
                global,
            ) {
                true
            } else {
                warn!("removing disconnected client {client_id} during shutdown");
                false
            }
        });
    }

    /// Clean up all per-client tracking when a client disconnects: drop its
    /// entry from [`DaemonState::clients`] (which also releases its writer
    /// channel so the connection's writer thread can exit once its
    /// connection-local sender is dropped) and tell every attached session to
    /// drop it — a single atomic operation so stale entries don't accumulate.
    pub(super) fn handle_client_disconnected(&mut self, client_id: ClientId) {
        info!("client disconnected cleanup: client_id={}", client_id);
        self.remove_client(client_id);
    }

    /// Remove a client's entry from [`DaemonState::clients`] and tell every
    /// session it was attached to drop it via `RemoveSubscriber`. Shared by full
    /// disconnect and lag-eviction, so a torn-down client stops being streamed
    /// to promptly (releasing its queued bytes) instead of waiting for the next
    /// broadcast to notice the dead sink.
    fn remove_client(&mut self, client_id: ClientId) {
        let Some(client) = self.clients.remove(&client_id) else {
            return;
        };
        for session_id in &client.sessions {
            if let Some(entry) = self.active_sessions.get(session_id) {
                let _ = entry
                    .cmd_tx
                    .send(SessionCommand::RemoveSubscriber { client_id });
            }
        }
    }

    /// Track that `client_id` is a direct subscriber of `session_id`.
    /// Idempotent — re-attach to the same session is a no-op.
    pub(super) fn handle_track_session_subscription(
        &mut self,
        client_id: ClientId,
        session_id: u64,
    ) {
        debug!(
            "track session subscription: client_id={}, session_id={}",
            client_id, session_id
        );
        // A `Track` for a client that has no entry (the disconnect raced ahead
        // of the session's command) is dropped rather than resurrecting a stale
        // entry — a disconnected client must not be recreated by a late track.
        if let Some(client) = self.clients.get_mut(&client_id) {
            client.sessions.insert(session_id);
        }
    }

    /// Untrack that `client_id` is no longer a direct subscriber of `session_id`.
    pub(super) fn handle_untrack_session_subscription(
        &mut self,
        client_id: ClientId,
        session_id: u64,
    ) {
        debug!(
            "untrack session subscription: client_id={}, session_id={}",
            client_id, session_id
        );
        if let Some(client) = self.clients.get_mut(&client_id) {
            client.sessions.remove(&session_id);
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
    /// Duplicate-suppression is keyed on the EXPLICIT origin session carried
    /// by the broadcast command (`session_id`), not on the message shape —
    /// mirroring the sibling `BroadcastSessionStatus { session_id, status }`
    /// command, which likewise carries its provenance explicitly. `Some` for
    /// session-originated broadcasts (the sending session thread knows its
    /// own id), `None` for global/control broadcasts. Clients that are also
    /// direct subscribers of the origin session are skipped: they receive
    /// the message through the per-session subscriber path, avoiding
    /// duplicate delivery.
    pub(super) fn handle_broadcast_activity(
        &mut self,
        session_id: Option<u64>,
        msg: &DaemonMessage,
    ) {
        // Tripwire for the dedup contract: the command provenance and the
        // message origin must AGREE. A `Some` origin on a non-session message
        // drops it for the origin session's direct subscribers on BOTH paths
        // (the activity path skips them via dedup, the per-session path never
        // carries non-session messages); a `Session` envelope whose own
        // origin differs from (or contradicts) the command's ships the event
        // to the wrong subscriber class. No current producer does this; warn
        // loudly if one ever does.
        if violates_broadcast_origin_contract(session_id, msg) {
            warn!(
                session_id,
                "BroadcastActivity violates the origin contract: command provenance and \
                 message origin disagree, so some subscriber class will miss this message \
                 or receive it twice — no current producer does this, inspect the caller"
            );
        }
        let (evict_clients, evict_largest) = fan_out_evicting(
            &mut self.clients,
            msg,
            &self.lag_limits,
            &self.global_lag,
            |_id, client| {
                // Only activity subscribers; skip a client that is also a direct
                // subscriber of the origin session — it receives the message
                // through the per-session broadcast path, avoiding duplicate
                // delivery. `Option<u64>` is Copy, so `session_id` is captured
                // by copy.
                !client.wants_activity
                    || session_id.is_some_and(|sid| client.sessions.contains(&sid))
            },
        );
        self.finish_evictions(evict_clients, evict_largest);
    }
}
