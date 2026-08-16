//! Shared subscriber-broadcast policy.
//!
//! The daemon thread (summary + all-activity broadcasts in `daemon.rs`) and
//! the session threads (per-session `broadcast()` in `sessions.rs`) all fan a
//! message out to a map of per-client [`SubscriberSink`]s.  They must apply
//! the SAME policy to a subscriber that cannot keep up, or behavior drifts
//! between paths (one loop evicting a client another loop keeps, one blocking
//! while another drops, etc.).
//!
//! The shared policy is **lossless delivery with lag-based eviction**:
//!
//! * Every subscriber's writer channel is UNBOUNDED, so an enqueue can never
//!   be `Full` — the daemon NEVER drops a broadcast message.  A slow client
//!   can never stall a session thread or the daemon command loop: `send` on
//!   an unbounded crossbeam channel never blocks.
//! * Delivery is guaranteed, in-order (channels are FIFO), exactly-once for
//!   every connected non-evicted client.
//! * A client that falls too far behind is EVICTED (disconnected): the
//!   per-client in-flight byte counter crossing [`LagLimits::per_client_cap`]
//!   or the daemon-wide total crossing [`LagLimits::global_budget`] returns
//!   [`EnqueueOutcome::ClientOverLag`]/[`EnqueueOutcome::GlobalOverBudget`]
//!   and the caller triggers the eviction.  The client reconciles on
//!   reconnect via the attach/snapshot path (client-side reconnect is a
//!   later phase).
//! * receiver gone -> the client is dead; the caller drops the subscriber.
//!
//! The thresholds are SOFT bounds: the crossing message itself is still
//! enqueued (lossless), and concurrent producers can overshoot the cap by at
//! most one message's worth of bytes before the eviction command is
//! processed.  That is deliberate — an exact hard cutoff would require a
//! blocking or dropping send, which is exactly what this design eliminates.

use choreo_proto::DaemonMessage;
use crossbeam_channel::Sender;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Per-subscriber delivery sink: an UNBOUNDED crossbeam channel plus a
/// shared in-flight byte counter.
///
/// Clone cheaply (channel sender + `Arc<AtomicUsize>`) so the same sink can
/// be registered in several maps at once — e.g. a connection's writer sink
/// appears in `client_writers`, `summary_subscribers`, and
/// `activity_subscribers`, all sharing ONE byte counter.
#[derive(Clone)]
pub struct SubscriberSink {
    pub tx: Sender<DaemonMessage>,
    /// Bytes sitting in this subscriber's queue right now. Producers
    /// increment (on enqueue), the connection's writer thread decrements
    /// (on dequeue). This is the 6th sanctioned shared-state exception —
    /// lock-free, single-purpose, carries no protocol data. It exists
    /// because the byte lag of a queue is inherently shared state: the
    /// producers and the draining writer thread run on different threads and
    /// must both touch the same running total, which a channel cannot
    /// express without a dedicated accounting thread. The lock-free atomic
    /// is safe because every mutation is a single independent
    /// `fetch_add`/`fetch_sub` — no read-modify-write composite that needs a
    /// critical section (the one-place soft-bound check is per-producer on
    /// the post-add value, deliberately racy, see `enqueue`).
    /// (Sanctioned exception #6 — see AGENTS.md and ARCHITECTURE.md's
    /// `broadcast.rs` module row for the full rationale.)
    pub bytes_in_flight: Arc<AtomicUsize>,
}

impl SubscriberSink {
    pub fn new(tx: Sender<DaemonMessage>) -> Self {
        SubscriberSink {
            tx,
            bytes_in_flight: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Enqueue `msg` into this subscriber's queue and account its bytes.
    ///
    /// Never blocks and never drops: the channel is unbounded, so delivery
    /// is guaranteed for as long as the receiver is alive.  The return value
    /// only tells the caller whether an eviction is warranted AFTER the
    /// message has been enqueued.
    ///
    /// The two counters are bumped before the send so that every message
    /// that ever reaches the writer thread's dequeue decrement was
    /// previously counted — the writer's per-dequeue `fetch_sub` is the only
    /// counterpart, so the accounting stays balanced.
    pub fn enqueue(
        &self,
        msg: &DaemonMessage,
        limits: &LagLimits,
        global: &AtomicUsize,
    ) -> EnqueueOutcome {
        let size = msg.approx_wire_size();
        let new_total = global.fetch_add(size, Ordering::Relaxed) + size;
        let new_client = self.bytes_in_flight.fetch_add(size, Ordering::Relaxed) + size;

        // Classify against the soft thresholds.  The message is STILL
        // enqueued below regardless — the threshold only decides whether the
        // caller must evict this client, never whether delivery happens
        // (lossless).  `ClientOverLag` takes precedence over the global
        // budget: the client that crossed its own cap is the one to shed.
        let outcome = if new_client > limits.per_client_cap {
            EnqueueOutcome::ClientOverLag
        } else if new_total > limits.global_budget {
            EnqueueOutcome::GlobalOverBudget
        } else {
            EnqueueOutcome::Delivered
        };

        match self.tx.send(msg.clone()) {
            Ok(()) => outcome,
            Err(_) => {
                // Receiver gone — the writer thread has already exited, so
                // nothing will ever decrement these bytes. Self-correct BOTH
                // counters: the per-client one dies with the sink anyway, but
                // `global` is shared and read by every other enqueue, so
                // leaving it incremented would slowly leak the daemon-wide
                // budget and could eventually trigger spurious evictions.
                self.bytes_in_flight.fetch_sub(size, Ordering::Relaxed);
                global.fetch_sub(size, Ordering::Relaxed);
                EnqueueOutcome::Disconnected
            }
        }
    }

    /// Enqueue `msg` with byte accounting but WITHOUT the lag-threshold
    /// check, returning whether the receiver was alive. Used for one-shot
    /// guaranteed deliveries that are not evidence of a lagging client — the
    /// attach `SessionState` snapshot (a single large message to a
    /// freshly-attached client whose writer is healthy), connection replies,
    /// and the best-effort `Evicted`/`ShuttingDown` advisories. The
    /// accounting still matters: the writer thread decrements the same
    /// counters on every dequeue, so every enqueue must be counted or the
    /// counters would underflow — and a failed send (receiver gone) is
    /// self-corrected here for the same reason as in [`Self::enqueue`].
    pub fn send_unchecked(&self, msg: &DaemonMessage, global: &AtomicUsize) -> bool {
        let size = msg.approx_wire_size();
        self.bytes_in_flight.fetch_add(size, Ordering::Relaxed);
        global.fetch_add(size, Ordering::Relaxed);
        if self.tx.send(msg.clone()).is_ok() {
            true
        } else {
            // Receiver gone — no writer thread will ever decrement this;
            // restore both counters (see `enqueue`).
            self.bytes_in_flight.fetch_sub(size, Ordering::Relaxed);
            global.fetch_sub(size, Ordering::Relaxed);
            false
        }
    }
}

/// Outcome of one [`SubscriberSink::enqueue`].  Every variant other than
/// [`EnqueueOutcome::Disconnected`] means the message WAS delivered into the
/// subscriber's queue — the outcome only says what the caller must do next.
pub enum EnqueueOutcome {
    /// Delivered into the subscriber's queue.
    Delivered,
    /// Receiver gone — the client is dead; evict the subscriber.
    Disconnected,
    /// This client's in-flight bytes crossed `limits.per_client_cap`.
    /// The message WAS still enqueued (lossless); the caller must trigger
    /// eviction of this client.
    ClientOverLag,
    /// The daemon-wide total crossed `limits.global_budget`. The message WAS
    /// still enqueued; the caller must trigger eviction of the largest
    /// lagging client.
    GlobalOverBudget,
}

/// Lag thresholds. Default = 64 MiB per client, 512 MiB daemon-wide.
/// MUST be injectable so unit/integration tests can use tiny caps.
#[derive(Debug, Clone, Copy)]
pub struct LagLimits {
    pub per_client_cap: usize,
    pub global_budget: usize,
}

impl Default for LagLimits {
    fn default() -> Self {
        LagLimits {
            per_client_cap: 64 * 1024 * 1024,
            global_budget: 512 * 1024 * 1024,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use choreo_proto::SessionStatus;

    fn status_msg(session_id: u64) -> DaemonMessage {
        DaemonMessage::SessionStatusChanged {
            session_id,
            status: SessionStatus::Inactive,
            last_modified: 0,
        }
    }

    /// Tiny, injectable limits so a test can cross a cap with a handful of
    /// messages instead of megabytes.
    fn tiny_limits() -> LagLimits {
        LagLimits {
            per_client_cap: 128,
            global_budget: 256,
        }
    }

    /// Drain a crossbeam receiver to count messages; bounded crossbeam
    /// channels act as the receiver's drainer so we can observe delivery.
    #[test]
    fn enqueue_delivers_and_counts_bytes() {
        let (tx, rx) = crossbeam_channel::unbounded::<DaemonMessage>();
        let sink = SubscriberSink::new(tx);
        let global = AtomicUsize::new(0);
        let limits = tiny_limits();
        let msg = status_msg(1);
        let size = msg.approx_wire_size();

        let outcome = sink.enqueue(&msg, &limits, &global);
        assert!(matches!(outcome, EnqueueOutcome::Delivered));
        assert_eq!(rx.recv().unwrap(), msg, "message must be delivered");
        // Both counters reflect the enqueued bytes.
        assert_eq!(sink.bytes_in_flight.load(Ordering::Relaxed), size);
        assert_eq!(global.load(Ordering::Relaxed), size);

        // Dequeue-side accounting (the writer thread's job) balances them.
        sink.bytes_in_flight.fetch_sub(size, Ordering::Relaxed);
        global.fetch_sub(size, Ordering::Relaxed);
        assert_eq!(sink.bytes_in_flight.load(Ordering::Relaxed), 0);
        assert_eq!(global.load(Ordering::Relaxed), 0);
    }

    /// Crossing the per-client cap returns `ClientOverLag` AND the message is
    /// still enqueued (lossless).
    #[test]
    fn enqueue_over_per_client_cap_returns_client_over_lag_but_still_delivers() {
        let (tx, rx) = crossbeam_channel::unbounded::<DaemonMessage>();
        let sink = SubscriberSink::new(tx);
        let global = AtomicUsize::new(0);
        // per_client_cap = 128: a message with a ~100-byte payload + a
        // status message both fit, a second payload message crosses it.
        let limits = LagLimits {
            per_client_cap: 128,
            global_budget: usize::MAX, // isolate the per-client threshold
        };
        let payload_msg = || DaemonMessage::Failed {
            session_id: 1,
            request_id: 1,
            error: "x".repeat(100),
        };

        // First: well under the cap → Delivered.
        let m1 = status_msg(1);
        assert!(matches!(
            sink.enqueue(&m1, &limits, &global),
            EnqueueOutcome::Delivered
        ));

        // A big message crosses the cap but MUST still be delivered.
        let m2 = payload_msg();
        let outcome = sink.enqueue(&m2, &limits, &global);
        assert!(
            matches!(outcome, EnqueueOutcome::ClientOverLag),
            "crossing the per-client cap must report ClientOverLag"
        );
        assert_eq!(rx.recv().unwrap(), m1);
        assert_eq!(
            rx.recv().unwrap(),
            m2,
            "the over-lag message is still enqueued"
        );
    }

    /// Crossing the global budget (while no per-client cap is crossed)
    /// returns `GlobalOverBudget` and the message is still enqueued.
    #[test]
    fn enqueue_over_global_budget_returns_global_over_budget_but_still_delivers() {
        let (tx, rx) = crossbeam_channel::unbounded::<DaemonMessage>();
        let sink = SubscriberSink::new(tx);
        let global = AtomicUsize::new(0);
        // Global budget tiny; per-client cap huge so only the global fires.
        let limits = LagLimits {
            per_client_cap: usize::MAX,
            global_budget: 128,
        };

        // One status message (~small) stays under the global budget.
        let m1 = status_msg(1);
        assert!(matches!(
            sink.enqueue(&m1, &limits, &global),
            EnqueueOutcome::Delivered
        ));

        // A big message pushes the daemon-wide total over the budget.
        let m2 = DaemonMessage::Failed {
            session_id: 1,
            request_id: 2,
            error: "y".repeat(200),
        };
        let outcome = sink.enqueue(&m2, &limits, &global);
        assert!(
            matches!(outcome, EnqueueOutcome::GlobalOverBudget),
            "crossing the global budget must report GlobalOverBudget"
        );
        assert_eq!(rx.recv().unwrap(), m1);
        assert_eq!(
            rx.recv().unwrap(),
            m2,
            "the over-budget message is still enqueued"
        );
    }

    /// A dropped receiver yields `Disconnected` and both byte counters are
    /// restored: the writer thread is gone, so nothing will ever decrement
    /// the enqueue's bytes — the per-client counter dies with the sink, but
    /// the daemon-wide counter is shared and must not leak.
    #[test]
    fn enqueue_returns_disconnected_when_receiver_gone() {
        let (tx, rx) = crossbeam_channel::unbounded::<DaemonMessage>();
        let sink = SubscriberSink::new(tx);
        let global = AtomicUsize::new(0);
        let limits = tiny_limits();
        drop(rx); // receiver gone

        let msg = status_msg(1);
        let outcome = sink.enqueue(&msg, &limits, &global);
        assert!(matches!(outcome, EnqueueOutcome::Disconnected));
        // Self-correction: the failed enqueue must leave both counters at
        // zero, not leak the message's bytes into the daemon-wide budget.
        assert_eq!(sink.bytes_in_flight.load(Ordering::Relaxed), 0);
        assert_eq!(global.load(Ordering::Relaxed), 0);
    }

    /// `send_unchecked` (the no-threshold-check path used for attach
    /// snapshots and the `Evicted`/`ShuttingDown` advisories) self-corrects
    /// both counters the same way when the receiver is gone.
    #[test]
    fn send_unchecked_self_corrects_on_dead_receiver() {
        let (tx, rx) = crossbeam_channel::unbounded::<DaemonMessage>();
        let sink = SubscriberSink::new(tx);
        let global = AtomicUsize::new(0);
        drop(rx); // receiver gone

        let msg = status_msg(1);
        let size = msg.approx_wire_size();
        assert!(
            !sink.send_unchecked(&msg, &global),
            "dead receiver must report false"
        );
        assert_eq!(sink.bytes_in_flight.load(Ordering::Relaxed), 0);
        assert_eq!(global.load(Ordering::Relaxed), 0);

        // Sanity: with a live receiver the same call reports true and the
        // counters reflect the enqueued bytes.
        let (tx2, rx2) = crossbeam_channel::unbounded::<DaemonMessage>();
        let sink2 = SubscriberSink::new(tx2);
        let global2 = AtomicUsize::new(0);
        assert!(
            sink2.send_unchecked(&msg, &global2),
            "live receiver must report true"
        );
        assert_eq!(rx2.recv().unwrap(), msg, "message must be delivered");
        assert_eq!(sink2.bytes_in_flight.load(Ordering::Relaxed), size);
        assert_eq!(global2.load(Ordering::Relaxed), size);
    }

    /// The per-client counter increments on every enqueue and only a matching
    /// dequeue decrement brings it back down — this is the exact bookkeeping
    /// the writer thread performs per message.
    #[test]
    fn counters_increment_and_decrement_with_approx_wire_size() {
        let (tx, rx) = crossbeam_channel::unbounded::<DaemonMessage>();
        let sink = SubscriberSink::new(tx);
        let global = AtomicUsize::new(0);
        let limits = LagLimits {
            per_client_cap: usize::MAX,
            global_budget: usize::MAX,
        };

        // Two messages of known size: Failed with 100-byte and 50-byte errors.
        let m1 = DaemonMessage::Failed {
            session_id: 1,
            request_id: 1,
            error: "a".repeat(100),
        };
        let m2 = DaemonMessage::Failed {
            session_id: 2,
            request_id: 2,
            error: "b".repeat(50),
        };
        let s1 = m1.approx_wire_size();
        let s2 = m2.approx_wire_size();

        assert!(matches!(
            sink.enqueue(&m1, &limits, &global),
            EnqueueOutcome::Delivered
        ));
        assert!(matches!(
            sink.enqueue(&m2, &limits, &global),
            EnqueueOutcome::Delivered
        ));
        assert_eq!(sink.bytes_in_flight.load(Ordering::Relaxed), s1 + s2);
        assert_eq!(global.load(Ordering::Relaxed), s1 + s2);

        // Drain both from the channel, decrementing like the writer thread.
        assert_eq!(rx.recv().unwrap(), m1);
        assert_eq!(rx.recv().unwrap(), m2);
        sink.bytes_in_flight.fetch_sub(s1, Ordering::Relaxed);
        sink.bytes_in_flight.fetch_sub(s2, Ordering::Relaxed);
        global.fetch_sub(s1, Ordering::Relaxed);
        global.fetch_sub(s2, Ordering::Relaxed);
        assert_eq!(sink.bytes_in_flight.load(Ordering::Relaxed), 0);
        assert_eq!(global.load(Ordering::Relaxed), 0);
    }

    /// `LagLimits::default()` is the documented 64 MiB / 512 MiB pair.
    #[test]
    fn default_limits_are_64_mib_per_client_and_512_mib_global() {
        let limits = LagLimits::default();
        assert_eq!(limits.per_client_cap, 64 * 1024 * 1024);
        assert_eq!(limits.global_budget, 512 * 1024 * 1024);
    }

    /// A `ClientOverLag` on one sink must not be masked by the global budget
    /// also being crossed — per-client precedence is part of the policy.
    #[test]
    fn per_client_overlag_takes_precedence_over_global_over_budget() {
        let (tx, _rx) = crossbeam_channel::unbounded::<DaemonMessage>();
        let sink = SubscriberSink::new(tx);
        let global = AtomicUsize::new(0);
        // Both thresholds tiny: any message crosses both, per-client must win.
        let limits = LagLimits {
            per_client_cap: 8,
            global_budget: 8,
        };
        let outcome = sink.enqueue(&status_msg(1), &limits, &global);
        assert!(matches!(outcome, EnqueueOutcome::ClientOverLag));
    }
}
