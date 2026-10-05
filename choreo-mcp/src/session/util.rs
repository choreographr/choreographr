//! A bounded thread-join helper for shutdown.

use crossbeam_channel::RecvTimeoutError;
use std::time::Duration;

/// Join a thread, giving up after `timeout` and leaving it detached.
///
/// The handle is moved into a waiter thread that joins and signals over a
/// crossbeam channel; this thread waits on that channel with `recv_timeout`, so
/// a wedged dispatcher cannot hang shutdown.
pub(super) fn join_bounded(join: std::thread::JoinHandle<()>, timeout: Duration) {
    let (done_tx, done_rx) = crossbeam_channel::bounded::<()>(1);
    std::thread::spawn(move || {
        let _ = join.join();
        let _ = done_tx.send(());
    });
    match done_rx.recv_timeout(timeout) {
        Ok(()) | Err(RecvTimeoutError::Disconnected) => {}
        Err(RecvTimeoutError::Timeout) => {
            tracing::warn!(
                ?timeout,
                "MCP dispatcher did not exit within timeout; detaching"
            );
        }
    }
}
