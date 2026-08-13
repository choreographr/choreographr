//! Shared subscriber-broadcast policy.
//!
//! The daemon thread (summary + all-activity broadcasts in `daemon.rs`) and
//! the session threads (per-session `broadcast()` in `sessions.rs`) all fan a
//! message out to a map of per-client `SyncSender` subscribers.  They must
//! apply the SAME policy to a subscriber that cannot keep up, or behavior
//! drifts between paths (one loop evicting a client another loop keeps, one
//! blocking while another drops, etc.).
//!
//! The shared policy, applied by [`try_send_keep_on_full`]:
//!
//! * buffer full   -> drop the message, KEEP the subscriber.  A
//!   momentarily-full buffer is a transient backpressure condition; the
//!   subscriber (e.g. the TUI) is still connected and may catch up, and the
//!   final message of a turn (`ToolCallFinished` + `SessionMessageAppended`)
//!   delivers the complete content anyway.  Evicting here would permanently
//!   blind a subscriber that never re-subscribes (the TUI registers for all
//!   activity exactly once at startup).  Each drop is counted by
//!   `metrics::record_broadcast_dropped` under the caller's `path` label, so
//!   a wedged subscriber stays observable via `/metrics` without log noise.
//! * receiver gone -> evict the subscriber.  A disconnected client is dead;
//!   keeping its sender would leak the entry and burn a `try_send` + clone on
//!   every future broadcast.
//!
//! Crucially the policy NEVER blocks: `try_send` returns immediately, so a
//! slow subscriber can never stall the daemon's single-threaded command loop
//! or a session thread.
//!
//! The shutdown fan-out (`send_shutting_down_bounded`) is the one deliberate
//! exception to drop-on-full: `ShuttingDown` is the notify-before-EOF
//! guarantee, so a momentarily-full writer channel is polled (round-robin,
//! bounded) until the writer drains instead of being silently dropped.  It
//! still never *blocks* on the writer — only the poll interval is slept.

use choreo_proto::DaemonMessage;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};
use tracing::warn;

/// Try to deliver `message` to one subscriber, returning whether the
/// subscriber should remain registered.
///
/// `path` labels the fan-out in the drop metric ("summary", "activity",
/// "session").  This is meant to be used as the retain predicate of the
/// subscriber maps:
///
/// ```ignore
/// subscribers.retain(|client_id, tx| try_send_keep_on_full(tx, *client_id, "summary", &msg));
/// ```
///
/// See the module docs for the drop-on-full / evict-on-disconnect policy.
pub(crate) fn try_send_keep_on_full(
    tx: &mpsc::SyncSender<DaemonMessage>,
    client_id: u64,
    path: &str,
    message: &DaemonMessage,
) -> bool {
    match tx.try_send(message.clone()) {
        // Delivered — keep the subscriber.
        Ok(()) => true,
        // Buffer full: drop the message but KEEP the subscriber.  Logging
        // every dropped chunk here would be pure noise under a fast burst
        // (one line per message per subscriber), so we stay silent in the
        // logs and instead count the drop so a wedged subscriber remains
        // observable via /metrics.
        Err(mpsc::TrySendError::Full(_)) => {
            crate::metrics::record_broadcast_dropped(path);
            true
        }
        // Receiver gone — the client is dead, stop sending to it.
        Err(mpsc::TrySendError::Disconnected(_)) => {
            warn!("removing disconnected subscriber {client_id}");
            false
        }
    }
}

/// Poll interval between delivery attempts to a full writer channel during
/// the shutdown fan-out.  A healthy writer drains a slot in well under this;
/// the poll only paces retries to a slow-but-alive writer.
pub(crate) const SHUTDOWN_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Deliver `ShuttingDown` to every client whose writer channel was full on
/// the fast path of `handle_broadcast_shutting_down`, polling each channel
/// round-robin so all clients share one `grace` window — a wedged client can
/// no longer delay the others, and total shutdown latency is bounded by
/// `grace`, not `grace × N`.
///
/// Delivery never blocks on a writer thread: `try_send` returns immediately
/// and only `poll` is slept, so a slow-but-alive writer gets a fresh chance
/// each pass while a wedged one (writer stuck in a blocking socket write,
/// client not reading) is abandoned after `grace`.  Abandonment is safe: the
/// daemon exits right after shutdown and the OS closes the socket, so the
/// client sees a bare EOF; each abandoned client is counted via
/// `metrics::record_broadcast_dropped` so it stays observable on `/metrics`.
///
/// Returns the ids of clients that were NOT delivered (disconnected or
/// timed-out) so the caller can drop them from its registry.  `poll` is
/// injectable so unit tests can use `Duration::ZERO` (a deterministic busy
/// retry with no time-based waits); production passes
/// [`SHUTDOWN_POLL_INTERVAL`].
pub(crate) fn send_shutting_down_bounded(
    clients: Vec<(u64, mpsc::SyncSender<DaemonMessage>)>,
    grace: Duration,
    poll: Duration,
) -> Vec<u64> {
    let mut pending = clients;
    if pending.is_empty() {
        return Vec::new();
    }
    let deadline = Instant::now() + grace;
    let mut failed = Vec::new();
    while !pending.is_empty() {
        pending.retain(|(client_id, tx)| {
            match tx.try_send(DaemonMessage::ShuttingDown) {
                // Delivered — the writer thread will flush it and close its
                // own socket; drop this client from the pending set.
                Ok(()) => false,
                // Receiver gone — the client is dead mid-shutdown.
                Err(mpsc::TrySendError::Disconnected(_)) => {
                    warn!("removing disconnected client {client_id} during shutdown");
                    failed.push(*client_id);
                    false
                }
                // Still full — keep polling this client on the next pass.
                Err(mpsc::TrySendError::Full(_)) => true,
            }
        });
        if pending.is_empty() {
            break;
        }
        if Instant::now() >= deadline {
            // The remaining writers did not drain in time. Abandon them (the
            // process exits momentarily and the OS closes their sockets, so
            // each client sees a bare EOF) and count every drop so a wedged
            // client stays observable via /metrics.
            for (client_id, _) in &pending {
                crate::metrics::record_broadcast_dropped("client");
                warn!("shutdown notification timed out for slow client {client_id}");
            }
            failed.extend(pending.into_iter().map(|(client_id, _)| client_id));
            break;
        }
        thread::sleep(poll);
    }
    failed
}

#[cfg(test)]
mod tests {
    use super::*;
    use choreo_proto::SessionStatus;
    use std::sync::mpsc;

    fn status_msg(session_id: u64) -> DaemonMessage {
        DaemonMessage::SessionStatusChanged {
            session_id,
            status: SessionStatus::Inactive,
            last_modified: 0,
        }
    }

    #[test]
    fn try_send_keep_on_full_delivers_and_keeps_subscriber() {
        let (tx, rx) = mpsc::sync_channel::<DaemonMessage>(1);
        let msg = status_msg(1);
        assert!(try_send_keep_on_full(&tx, 7, "summary", &msg));
        assert_eq!(rx.recv().unwrap(), msg);
    }

    #[test]
    fn try_send_keep_on_full_drops_on_full_buffer_and_keeps_subscriber() {
        let (tx, rx) = mpsc::sync_channel::<DaemonMessage>(1);
        // A capacity-1 channel lets one filler message fill the buffer so the
        // next try_send fails with Full.
        let filler = status_msg(1);
        tx.send(filler.clone()).unwrap();

        let broadcast = status_msg(2);
        // Full buffer: the message is dropped but the subscriber is retained.
        assert!(try_send_keep_on_full(&tx, 7, "session", &broadcast));
        assert_eq!(rx.recv().unwrap(), filler);
        assert!(rx.try_recv().is_err(), "full-buffer message was dropped");

        // Once the buffer drains, delivery resumes.
        assert!(try_send_keep_on_full(&tx, 7, "session", &broadcast));
        assert_eq!(rx.recv().unwrap(), broadcast);
    }

    #[test]
    fn try_send_keep_on_full_evicts_on_disconnected() {
        let (tx, rx) = mpsc::sync_channel::<DaemonMessage>(1);
        drop(rx); // Disconnect the receiver
        assert!(!try_send_keep_on_full(&tx, 7, "summary", &status_msg(1)));
    }

    /// A bounded send to an empty channel delivers immediately — `grace = 0`
    /// proves no waiting was needed and no client is reported failed.
    #[test]
    fn bounded_shutdown_delivers_when_channel_has_room() {
        let (tx, rx) = mpsc::sync_channel::<DaemonMessage>(1);
        let failed = send_shutting_down_bounded(vec![(7, tx)], Duration::ZERO, Duration::ZERO);
        assert!(
            failed.is_empty(),
            "a channel with room must accept ShuttingDown: {failed:?}"
        );
        assert_eq!(rx.try_recv().unwrap(), DaemonMessage::ShuttingDown);
    }

    /// A channel that stays full past the grace period is abandoned and its
    /// client reported failed. `grace = 0` makes the first full attempt the
    /// deadline, so the test is fully deterministic (no sleeps).
    #[test]
    fn bounded_shutdown_gives_up_on_still_full_channel() {
        let (tx, rx) = mpsc::sync_channel::<DaemonMessage>(1);
        // Fill the single slot so every try_send sees Full.
        tx.send(DaemonMessage::Pong).unwrap();
        let failed = send_shutting_down_bounded(vec![(7, tx)], Duration::ZERO, Duration::ZERO);
        assert_eq!(failed, vec![7], "stuck client must be reported failed");
        // The filler is still there; ShuttingDown was never delivered.
        assert_eq!(rx.try_recv().unwrap(), DaemonMessage::Pong);
        assert!(rx.try_recv().is_err());
    }

    /// A disconnected writer is reported failed and removed from the pending
    /// set instead of being polled forever.
    #[test]
    fn bounded_shutdown_evicts_disconnected_client() {
        let (tx, rx) = mpsc::sync_channel::<DaemonMessage>(1);
        drop(rx);
        let failed = send_shutting_down_bounded(vec![(7, tx)], Duration::ZERO, Duration::ZERO);
        assert_eq!(failed, vec![7]);
    }

    /// A momentarily-full channel is NOT dropped: the fan-out polls
    /// round-robin until the writer drains a slot. A capacity-0 rendezvous
    /// channel makes the wait load-bearing — `try_send` only succeeds while
    /// a receiver is blocked in `recv`, so delivery provably waits for the
    /// drainer rather than giving up. `poll = ZERO` (a busy retry) plus the
    /// ready-handshake keep the test deterministic: the drainer is
    /// guaranteed to block, so the retry loop always completes.
    #[test]
    fn bounded_shutdown_waits_for_writer_that_drains() {
        let (tx, rx) = mpsc::sync_channel::<DaemonMessage>(0);
        let (ready_tx, ready_rx) = mpsc::channel::<()>();
        let drainer = thread::spawn(move || {
            let _ = ready_tx.send(());
            rx.recv().expect("rendezvous must deliver ShuttingDown")
        });
        ready_rx.recv().expect("drainer must become ready");
        let failed =
            send_shutting_down_bounded(vec![(7, tx)], Duration::from_secs(5), Duration::ZERO);
        assert!(
            failed.is_empty(),
            "delivery must wait for the writer to drain, got failures: {failed:?}"
        );
        assert_eq!(
            drainer.join().expect("drainer panicked"),
            DaemonMessage::ShuttingDown
        );
    }

    /// Mixed clients: a connected one is delivered, a stuck one is reported
    /// failed — the two outcomes coexist in a single fan-out.
    #[test]
    fn bounded_shutdown_delivers_healthy_and_reports_stuck() {
        let (healthy_tx, healthy_rx) = mpsc::sync_channel::<DaemonMessage>(1);
        let (stuck_tx, stuck_rx) = mpsc::sync_channel::<DaemonMessage>(1);
        stuck_tx.send(DaemonMessage::Pong).unwrap(); // fill the only slot

        let failed = send_shutting_down_bounded(
            vec![(1, healthy_tx), (2, stuck_tx)],
            Duration::ZERO,
            Duration::ZERO,
        );
        assert_eq!(failed, vec![2]);
        assert_eq!(healthy_rx.try_recv().unwrap(), DaemonMessage::ShuttingDown);
        assert_eq!(stuck_rx.try_recv().unwrap(), DaemonMessage::Pong);
    }
}
