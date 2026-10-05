//! Bounded transport-reconnect policy and its off-loop reconnect worker.
//!
//! A dispatcher whose engine's transport dies must rebuild the connection. Both
//! the backoff sleep and the rebuild itself would otherwise run on the
//! dispatcher thread and wedge it — it could not service `CancelSession`,
//! `Shutdown`, or new calls while it waited (up to the backoff plus the connect
//! duration). [`Reconnector`] instead runs the backoff and the rebuild on a
//! detached worker thread and hands the fresh engine back over a channel, so
//! the dispatcher keeps draining commands throughout a reconnect.

use crate::error::McpError;
use crate::session::{EngineFactory, McpEngine};
use crossbeam_channel::{Receiver, Sender};
use std::sync::Arc;
use std::time::Duration;

/// Restart policy for a dispatcher whose engine's transport dies.
///
/// Bounded attempts with exponential backoff (capped at 60 s) so a flapping
/// server is retried a few times and then left alone until the next request.
pub(super) struct RestartPolicy {
    pub(super) max_attempts: u32,
    pub(super) base_backoff: Duration,
    pub(super) failures: u32,
}

impl Default for RestartPolicy {
    fn default() -> Self {
        Self::new(crate::config::DEFAULT_MAX_RESTARTS)
    }
}

impl RestartPolicy {
    /// Build a policy that rebuilds a dead transport at most `max_attempts`
    /// times in a row (`0` disables reconnect).
    pub(super) fn new(max_attempts: u32) -> Self {
        Self {
            max_attempts,
            base_backoff: crate::retry::BASE_BACKOFF,
            failures: 0,
        }
    }

    /// Backoff for the current failure count: the shared exponential schedule
    /// (`base · 2^(n-1)`) capped at [`crate::retry::MAX_BACKOFF`].
    pub(super) fn backoff(&self) -> Duration {
        crate::retry::backoff(self.failures, self.base_backoff, crate::retry::MAX_BACKOFF)
    }

    /// Charge the budget for one transport-failure incident: bump the
    /// consecutive-failure count and, if the budget is not yet spent, return the
    /// backoff to wait before rebuilding.
    ///
    /// The failure count is NOT reset by a successful rebuild: it counts
    /// consecutive transport-failure incidents since the last exchange that
    /// genuinely COMPLETED (see [`record_success`](Self::record_success)). A
    /// server that reconnects and then immediately dies again therefore still
    /// exhausts the budget and is left alone, rather than being rebuilt forever.
    /// `None` means the budget is spent (a `warn!` is logged) and no reconnect
    /// runs.
    pub(super) fn begin_attempt(&mut self) -> Option<Duration> {
        self.failures = self.failures.saturating_add(1);
        if self.failures > self.max_attempts {
            tracing::warn!(
                failures = self.failures,
                "MCP server exceeded restart budget; not reconnecting"
            );
            return None;
        }
        Some(self.backoff())
    }

    /// Record that an exchange genuinely completed, resetting the consecutive
    /// transport-failure budget.
    ///
    /// Called from the dispatcher when a call or listing from the CURRENT engine
    /// reached the server or returned a settled answer (see `DoneOutcome`): the
    /// connection is proven usable, so the restart budget starts fresh. A
    /// deadline or a client-side cancel does NOT count — it leaves the
    /// connection's health unproven — and neither does the reconnect itself.
    /// This is what bounds a flapping server that reconnects but never survives
    /// a request: the budget resets only on a genuine completion, never on the
    /// rebuild.
    pub(super) fn record_success(&mut self) {
        self.failures = 0;
    }
}

/// A shared, cloneable engine factory.
///
/// The dispatcher's boxed [`EngineFactory`] is widened to an `Arc` so the
/// reconnect worker thread can own a clone while the dispatcher keeps its own;
/// the worker calls it to build a fresh engine off the dispatcher thread.
pub(super) type SharedFactory = Arc<dyn Fn() -> Result<Arc<dyn McpEngine>, McpError> + Send + Sync>;

/// Owns a dispatcher's reconnect state and runs the blocking part off the loop.
///
/// The dispatcher drives this from its own thread: [`note_failure`] charges the
/// budget and, when allowed, spawns a detached worker that sleeps the backoff
/// and calls the factory; the worker sends its `Result` back over a channel the
/// dispatcher `select!`s on. The dispatcher keeps servicing commands while the
/// worker runs, and promotes queued calls only once a fresh engine arrives
/// ([`take_result`]) — new calls are parked meanwhile rather than spawned
/// against the dead engine.
///
/// [`note_failure`]: Self::note_failure
/// [`take_result`]: Self::take_result
pub(super) struct Reconnector {
    policy: RestartPolicy,
    factory: SharedFactory,
    in_flight: bool,
    tx: Sender<Result<Arc<dyn McpEngine>, McpError>>,
    rx: Receiver<Result<Arc<dyn McpEngine>, McpError>>,
}

impl Reconnector {
    /// Build a reconnector around `factory`, sharing the boxed factory with the
    /// worker threads it spawns.
    pub(super) fn new(factory: EngineFactory, policy: RestartPolicy) -> Self {
        let factory: SharedFactory = Arc::from(factory);
        let (tx, rx) = crossbeam_channel::unbounded();
        Self {
            policy,
            factory,
            in_flight: false,
            tx,
            rx,
        }
    }

    /// Whether a reconnect worker is currently running.
    ///
    /// While true the dispatcher parks new calls instead of spawning them
    /// against the dead engine, and defers promoting queued calls until the
    /// worker reports back.
    pub(super) fn is_in_flight(&self) -> bool {
        self.in_flight
    }

    /// The channel the reconnect worker reports its result on.
    ///
    /// Cloned into the dispatcher's `select!` so the loop can observe a finished
    /// reconnect without borrowing `self` across an arm that mutates it.
    pub(super) fn receiver(&self) -> &Receiver<Result<Arc<dyn McpEngine>, McpError>> {
        &self.rx
    }

    /// Reset the consecutive-failure budget on a genuinely completed exchange.
    pub(super) fn record_success(&mut self) {
        self.policy.record_success();
    }

    /// Charge one transport-failure incident and, if the budget allows, start a
    /// reconnect on a detached worker thread.
    ///
    /// Idempotent while a reconnect is already running: a whole batch of calls
    /// lost on one dead transport reports a failure per notice, but only the
    /// first (with no worker yet in flight) spawns the worker, so N lost calls
    /// cost one rebuild, not N. When the budget is spent the policy returns
    /// `None` and no worker is spawned.
    pub(super) fn note_failure(&mut self) {
        if self.in_flight {
            return;
        }
        let Some(backoff) = self.policy.begin_attempt() else {
            return;
        };
        self.in_flight = true;
        let factory = Arc::clone(&self.factory);
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            // Skip the sleep entirely when the backoff is zero, which is how the
            // unit tests keep the reconnect path free of any time-based wait.
            if !backoff.is_zero() {
                std::thread::sleep(backoff);
            }
            let _ = tx.send(factory());
        });
    }

    /// Consume a reconnect worker's result, clearing the in-flight flag.
    ///
    /// Returns the fresh engine on success (the dispatcher adopts it); logs and
    /// returns `None` on a failed rebuild, leaving the old engine in place so a
    /// later failed request retries within the budget.
    pub(super) fn take_result(
        &mut self,
        result: Result<Arc<dyn McpEngine>, McpError>,
    ) -> Option<Arc<dyn McpEngine>> {
        self.in_flight = false;
        match result {
            Ok(fresh) => {
                tracing::info!("MCP server reconnected after transport failure");
                Some(fresh)
            }
            Err(e) => {
                tracing::warn!(error = %e, "MCP reconnect failed");
                None
            }
        }
    }

    /// The current consecutive-failure count (test observability).
    #[cfg(test)]
    pub(super) fn failures(&self) -> u32 {
        self.policy.failures
    }
}

/// Whether an error indicates the transport (not the request) failed, which is
/// the trigger for a reconnect.
///
/// The set is deliberately narrow: a request that failed because its payload was
/// rejected or malformed is a settled answer a rebuild cannot fix, so it must
/// *not* appear here. A send that failed at the transport layer
/// ([`McpError::Transport`]), a closed connection, a dispatcher that has exited,
/// and a raw I/O error all mean the connection is (or may be) unusable, so each
/// is worth one bounded rebuild attempt.
pub(super) fn is_transport_error(error: &McpError) -> bool {
    matches!(
        error,
        McpError::ServerShutdown
            | McpError::Transport(_)
            | McpError::Io(_)
            | McpError::NotConnected
    )
}
