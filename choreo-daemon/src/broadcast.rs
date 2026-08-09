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

use choreo_proto::DaemonMessage;
use std::sync::mpsc;
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
}
