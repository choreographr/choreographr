//! Cooperative cancellation token for in-flight calls.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// A cloneable, single-bit cooperative cancellation token.
///
/// This is the sanctioned cancellation-flag exception (AGENTS.md): a channel
/// message cannot interrupt a request already in flight inside `rmcp`, so a
/// data-free flag relays "stop" to the waiting call task. `tokio::sync::Notify`
/// makes the wait event-driven — the task parks on the token and wakes the
/// instant a session cancel fires, never polling or sleeping.
#[derive(Clone)]
pub(crate) struct CancelToken(Arc<CancelInner>);

struct CancelInner {
    flag: AtomicBool,
    notify: tokio::sync::Notify,
}

impl CancelToken {
    /// Create an un-cancelled token.
    pub(crate) fn new() -> Self {
        Self(Arc::new(CancelInner {
            flag: AtomicBool::new(false),
            notify: tokio::sync::Notify::new(),
        }))
    }

    /// Set the flag and wake any waiter. Idempotent.
    pub(crate) fn cancel(&self) {
        self.0.flag.store(true, Ordering::SeqCst);
        self.0.notify.notify_waiters();
    }

    /// Resolve once [`cancel`](Self::cancel) has been called.
    ///
    /// Registers the waiter before re-checking the flag, so a cancel that races
    /// this call is never lost.
    pub(crate) async fn cancelled(&self) {
        if self.0.flag.load(Ordering::SeqCst) {
            return;
        }
        let notified = self.0.notify.notified();
        if self.0.flag.load(Ordering::SeqCst) {
            return;
        }
        notified.await;
    }
}
