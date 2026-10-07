//! The client's pending-request table: the single outbound path plus the
//! reply-correlation side table.
//!
//! Every client→server message is a [`ClientMessage`] carrying a
//! per-connection request `id`. The daemon answers each request with exactly
//! one `DaemonMessage { id: Some(the same id), .. }` (the terminal reply), and
//! never resolves a request with a broadcast (`id: None`). This module owns the
//! client half of that contract: [`PendingReplies::send`] allocates the id,
//! records the in-flight slot, frames the message, and sends it — so a
//! front-end never constructs a `ClientMessage` by hand and can always resolve
//! a reply by `id` instead of guessing from payload shape, arrival order, or UI
//! state.
//!
//! The table is plain single-threaded state owned by the UI thread (the TUI and
//! GUI own their front-end state on one thread), so it takes no locks. It is a
//! *side table*: resolving a slot reports which request a reply answered; it
//! never replaces the payload's own state handling.
//!
//! ## Testability
//!
//! The clock is injected rather than read internally: [`PendingReplies::send`]
//! and [`PendingReplies::expire`] take an [`Instant`], so timeout behaviour is
//! deterministic in tests (no time-based waits, per the workspace test
//! discipline).

use choreo_proto::{ClientMessage, ClientMessageType, ImageKey, MessageKind};
use crossbeam_channel::Sender;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tracing::warn;
use zeroize::Zeroize;

/// The per-kind reply budget: how long a request may stay in flight before its
/// slot is considered timed out.
///
/// Fast queries (pings, listings, flags) get a short budget; the two requests
/// that legitimately block on remote work — a catalog refresh and session
/// creation — get a longer one. The budget is a UI-liveness backstop, not a
/// wire timeout: the daemon never sees it.
#[must_use]
pub fn deadline_for(kind: MessageKind) -> Duration {
    match kind {
        // A remote catalog fetch (models.dev) or a session create that may wait
        // on first-turn setup can legitimately take longer than a local query.
        MessageKind::RefreshModels | MessageKind::CreateSession => Duration::from_secs(30),
        // RunInput/ContinueGeneration are acknowledged (accepted or rejected)
        // promptly; the subsequent stream is a separate axis and is not timed
        // here.
        _ => Duration::from_secs(10),
    }
}

/// What a pending request needs carried alongside its id so the reply can be
/// acted on when it arrives.
///
/// The context is opaque to [`PendingReplies`] — the table only stores and
/// returns it; the front-end interprets it. Its most common use is to hold
/// request-specific data that a reply does not echo (a pending unlock key to be
/// recorded on confirmation, an image fetch key).
pub enum PendingContext {
    /// No extra context — the reply's payload is self-sufficient.
    None,
    /// The unlock/bind key presented in this request, held until the daemon's
    /// terminal reply either confirms it (record it per-daemon) or rejects it.
    /// Secret material — never logged, and wiped on drop (see the `Drop` impl,
    /// so a freed allocation never retains it on any exit path: resolve,
    /// timeout, `clear()`, or connection reset).
    UnlockKey(Vec<u8>),
    /// A turn-attachment fetch ([`ClientMessageType::GetImage`]): the
    /// (session, turn, key) the reply's bytes belong to, so a fetch can be
    /// routed without relying on the reply's echoed fields.
    Image {
        /// The session the attachment belongs to.
        session_id: u64,
        /// The turn within the session.
        turn_id: u32,
        /// The attachment slot within the turn.
        key: ImageKey,
    },
}

impl PendingContext {
    /// The held unlock key, if this context carries one. Used by a front-end
    /// reconciling a keystore operation's outcome.
    #[must_use]
    pub fn unlock_key(&self) -> Option<&[u8]> {
        match self {
            PendingContext::UnlockKey(key) => Some(key),
            _ => None,
        }
    }
}

impl Drop for PendingContext {
    /// Zeroize the presented unlock/bind key when the slot is dropped, so the
    /// secret is never left in a freed heap allocation. This runs on EVERY exit
    /// path a slot can take — resolve, per-kind timeout, `clear()` on connection
    /// reset, or the front-end dropping the resolved slot itself — which is what
    /// makes the `UnlockKey` contract hold without every caller remembering to
    /// wipe it. The other variants carry no secret material.
    fn drop(&mut self) {
        if let PendingContext::UnlockKey(key) = self {
            key.zeroize();
        }
    }
}

/// One in-flight request: its kind, when it was sent, its reply deadline, and
/// its opaque context.
pub struct Pending {
    /// The request kind (the log/timeout key).
    pub kind: MessageKind,
    /// When the request was sent (for elapsed-time reporting on timeout).
    pub sent_at: Instant,
    /// The instant after which the slot is considered timed out.
    pub deadline: Instant,
    /// Request-specific data threaded through to the reply handler.
    pub context: PendingContext,
}

/// A request whose reply never arrived within its per-kind budget.
pub struct Timeout {
    /// The request id that expired (its slot has been removed).
    pub id: u64,
    /// The request kind.
    pub kind: MessageKind,
    /// How long the request was in flight when it expired.
    pub elapsed: Duration,
}

/// The client's single pending-request table: allocates per-connection request
/// ids, records in-flight slots, and resolves/expires them by `id`.
///
/// Owned by exactly one thread (the front-end's UI thread) — no interior
/// mutability, no locks. A fresh table starts its id counter at 0, matching the
/// per-connection contract (each new connection is a fresh table).
pub struct PendingReplies {
    /// The next per-connection request id (starts at 0, never reused).
    next_id: u64,
    /// In-flight requests by their id.
    inflight: HashMap<u64, Pending>,
}

impl Default for PendingReplies {
    fn default() -> Self {
        Self::new()
    }
}

impl PendingReplies {
    /// A fresh table for a new connection (id counter at 0, nothing in flight).
    #[must_use]
    pub fn new() -> Self {
        Self {
            next_id: 0,
            inflight: HashMap::new(),
        }
    }

    /// The number of requests currently in flight.
    #[must_use]
    pub fn len(&self) -> usize {
        self.inflight.len()
    }

    /// Whether no requests are in flight.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.inflight.is_empty()
    }

    /// Allocate a request id, record the pending slot, frame `inner` as a
    /// [`ClientMessage`], and send it — **the one outbound path** every
    /// front-end message takes.
    ///
    /// The slot is recorded before the send so a reply that races back (on the
    /// embedded in-process transport) still finds it. A send failure (the
    /// connection is gone) is logged; the slot then simply times out with the
    /// rest of the table. Returns the allocated id so the caller can attach a
    /// [`PendingContext`] via [`Self::set_context`].
    pub fn send(&mut self, tx: &Sender<ClientMessage>, inner: ClientMessageType) -> u64 {
        self.send_at(tx, inner, Instant::now())
    }

    /// [`Self::send`] against an explicit clock — the deterministic seam unit
    /// tests drive instead of sleeping.
    pub fn send_at(
        &mut self,
        tx: &Sender<ClientMessage>,
        inner: ClientMessageType,
        now: Instant,
    ) -> u64 {
        let (id, framed) = self.frame_at(inner, now);
        if let Err(error) = tx.send(framed) {
            warn!(
                id,
                kind = ?self.kind_of(id),
                %error,
                "failed to send correlated request over the daemon channel; it will time out"
            );
        }
        id
    }

    /// Allocate a request id, record the pending slot, and return the framed
    /// [`ClientMessage`] WITHOUT sending it.
    ///
    /// For a front-end that owns its transport and must observe the send error
    /// itself (e.g. an adapter that aborts on a broken pipe), this is the same
    /// id-allocation path as [`Self::send`]; read the id back from the returned
    /// frame's `id` field.
    pub fn frame(&mut self, inner: ClientMessageType) -> ClientMessage {
        self.frame_at(inner, Instant::now()).1
    }

    /// Allocate the id, record the slot at `now`, and frame `inner`. The one
    /// place the id counter and the in-flight map are touched on the outbound
    /// path, so `send`/`send_at`/`frame` cannot drift.
    fn frame_at(&mut self, inner: ClientMessageType, now: Instant) -> (u64, ClientMessage) {
        let id = self.next_id;
        // Wrapping is unreachable in practice (2^64 sends) but keeps the
        // counter total rather than panicking on a pathological overflow.
        self.next_id = self.next_id.wrapping_add(1);
        let kind = inner.kind();
        self.inflight.insert(
            id,
            Pending {
                kind,
                sent_at: now,
                deadline: now + deadline_for(kind),
                context: PendingContext::None,
            },
        );
        (id, ClientMessage::request(id, inner))
    }

    /// The kind recorded for `id`, if its slot is still in flight (used only
    /// for the send-failure log line).
    fn kind_of(&self, id: u64) -> Option<MessageKind> {
        self.inflight.get(&id).map(|pending| pending.kind)
    }

    /// Attach a [`PendingContext`] to an in-flight slot created by
    /// [`Self::send`]. A no-op warning for an unknown id (e.g. the request
    /// already resolved — an in-process transport reply can arrive before the
    /// caller sets its context).
    pub fn set_context(&mut self, id: u64, context: PendingContext) {
        if let Some(pending) = self.inflight.get_mut(&id) {
            pending.context = context;
        } else {
            warn!(
                id,
                "set_context for an unknown (already resolved?) request id"
            );
        }
    }

    /// Remove and return the slot for `id` — the reply resolved it.
    ///
    /// A miss means the reply's id matched no in-flight request: a duplicate
    /// (already resolved), a late reply (the slot already timed out), or a
    /// mislabelled send. It is logged with the id so the misattribution is
    /// named rather than silently dropped.
    pub fn resolve(&mut self, id: u64) -> Option<Pending> {
        if let Some(pending) = self.inflight.remove(&id) {
            Some(pending)
        } else {
            warn!(
                id,
                "daemon reply matched no pending request (duplicate, late, or mislabelled id)"
            );
            None
        }
    }

    /// Remove and return every slot whose deadline has passed at `now`.
    ///
    /// Called from the UI loop tick — event-driven, with no timer channel: the
    /// tick is the clock source, and `now` is injected so the sweep is
    /// deterministic under test.
    pub fn expire(&mut self, now: Instant) -> Vec<Timeout> {
        let mut expired = Vec::new();
        self.inflight.retain(|&id, pending| {
            if pending.deadline <= now {
                expired.push(Timeout {
                    id,
                    kind: pending.kind,
                    elapsed: now.saturating_duration_since(pending.sent_at),
                });
                false
            } else {
                true
            }
        });
        expired
    }

    /// Void every in-flight slot — used on connection reset, where any reply
    /// still in the transport is for a request this front-end can no longer
    /// attribute. The id counter is deliberately NOT reset: ids are never
    /// reused for the life of the table (a genuinely new connection gets a
    /// fresh [`PendingReplies`], whose counter starts at 0).
    pub fn clear(&mut self) {
        self.inflight.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use choreo_proto::DaemonMessageType;

    #[test]
    fn send_allocates_sequential_ids_and_frames_message() {
        let (tx, rx) = crossbeam_channel::unbounded::<ClientMessage>();
        let mut pending = PendingReplies::new();
        let now = Instant::now();
        let id0 = pending.send_at(&tx, ClientMessageType::Ping, now);
        let id1 = pending.send_at(&tx, ClientMessageType::ListSessions, now);
        assert_eq!(id0, 0, "ids start at 0");
        assert_eq!(id1, 1, "ids increment per send");
        // Both frames carry their allocated id and the payload.
        let frames: Vec<ClientMessage> = rx.try_iter().collect();
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].id, 0);
        assert!(matches!(frames[0].inner, ClientMessageType::Ping));
        assert_eq!(frames[1].id, 1);
        assert!(matches!(frames[1].inner, ClientMessageType::ListSessions));
        assert_eq!(pending.len(), 2, "both slots recorded");
    }

    #[test]
    fn resolve_returns_the_slot_and_removes_it() {
        let (tx, _rx) = crossbeam_channel::unbounded::<ClientMessage>();
        let mut pending = PendingReplies::new();
        let id = pending.send_at(&tx, ClientMessageType::GetReasoningEffort, Instant::now());
        assert_eq!(pending.len(), 1);
        let slot = pending.resolve(id).expect("slot for the sent id");
        assert_eq!(slot.kind, MessageKind::GetReasoningEffort);
        assert!(pending.is_empty(), "the resolved slot is removed");
        // A second resolve of the same id is a miss (duplicate).
        assert!(pending.resolve(id).is_none());
    }

    #[test]
    fn resolve_of_unknown_id_is_none() {
        let mut pending = PendingReplies::new();
        assert!(pending.resolve(42).is_none());
    }

    #[test]
    fn expire_only_returns_slots_past_their_deadline() {
        let (tx, _rx) = crossbeam_channel::unbounded::<ClientMessage>();
        let mut pending = PendingReplies::new();
        let t0 = Instant::now();
        // Ping has a 10s budget.
        let ping = pending.send_at(&tx, ClientMessageType::Ping, t0);
        // RefreshModels has a 30s budget.
        let create = pending.send_at(&tx, ClientMessageType::RefreshModels { force: false }, t0);

        // At 11s the ping is past its deadline; the create is not.
        let expired = pending.expire(t0 + Duration::from_secs(11));
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].id, ping);
        assert_eq!(expired[0].kind, MessageKind::Ping);
        assert_eq!(expired[0].elapsed, Duration::from_secs(11));
        assert_eq!(pending.len(), 1, "only the expired slot is removed");

        // At 31s the create expires too.
        let expired = pending.expire(t0 + Duration::from_secs(31));
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].id, create);
        assert!(pending.is_empty());
    }

    #[test]
    fn expire_is_a_noop_before_any_deadline() {
        let (tx, _rx) = crossbeam_channel::unbounded::<ClientMessage>();
        let mut pending = PendingReplies::new();
        let t0 = Instant::now();
        pending.send_at(&tx, ClientMessageType::Ping, t0);
        assert!(pending.expire(t0 + Duration::from_secs(1)).is_empty());
        assert_eq!(pending.len(), 1);
    }

    #[test]
    fn out_of_order_resolution_works() {
        let (tx, _rx) = crossbeam_channel::unbounded::<ClientMessage>();
        let mut pending = PendingReplies::new();
        let t0 = Instant::now();
        let a = pending.send_at(&tx, ClientMessageType::Ping, t0);
        let b = pending.send_at(&tx, ClientMessageType::ListModels, t0);
        // Resolve the second request first — the table is keyed by id, not
        // order.
        assert_eq!(pending.resolve(b).expect("b").kind, MessageKind::ListModels);
        assert_eq!(pending.resolve(a).expect("a").kind, MessageKind::Ping);
    }

    #[test]
    fn clear_voids_inflight_but_never_reuses_ids() {
        let (tx, _rx) = crossbeam_channel::unbounded::<ClientMessage>();
        let mut pending = PendingReplies::new();
        let t0 = Instant::now();
        let first = pending.send_at(&tx, ClientMessageType::Ping, t0);
        assert_eq!(first, 0);
        pending.clear();
        assert!(pending.is_empty(), "clear voids every in-flight slot");
        // A later send continues the id sequence (no reuse), so a stale reply
        // for id 0 cannot be attributed to a new request.
        let next = pending.send_at(&tx, ClientMessageType::Ping, t0);
        assert_eq!(next, 1);
    }

    #[test]
    fn context_round_trips_through_resolve() {
        let (tx, _rx) = crossbeam_channel::unbounded::<ClientMessage>();
        let mut pending = PendingReplies::new();
        let id = pending.send_at(
            &tx,
            ClientMessageType::Unlock {
                private_key: vec![1],
            },
            Instant::now(),
        );
        pending.set_context(id, PendingContext::UnlockKey(vec![7u8; 32]));
        let slot = pending.resolve(id).expect("slot");
        assert_eq!(slot.context.unlock_key(), Some([7u8; 32].as_slice()));
    }

    #[test]
    fn resolve_then_state_still_applies_independently() {
        // The dispatch layer runs regardless of the table: resolving a slot is
        // a side effect, and the reply's payload (here a status text) is
        // handled by the caller. This pins that the table only reports the
        // resolved kind and does not consume the payload.
        let (tx, _rx) = crossbeam_channel::unbounded::<ClientMessage>();
        let mut pending = PendingReplies::new();
        let id = pending.send_at(&tx, ClientMessageType::Ping, Instant::now());
        let reply = choreo_proto::DaemonMessage::reply(id, DaemonMessageType::Pong);
        assert_eq!(reply.id, Some(id));
        assert!(pending.resolve(reply.id.expect("reply id")).is_some());
        // The payload is still available to the dispatch layer.
        assert!(matches!(reply.inner, DaemonMessageType::Pong));
    }
}
