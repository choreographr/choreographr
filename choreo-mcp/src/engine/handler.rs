//! The `rmcp` [`ClientHandler`] for one connection.
//!
//! rmcp routes every server-to-client notification to the handler rather than
//! to the caller that issued the request. This handler carries the `clientInfo`
//! and capability object advertised to the server and forwards
//! `notifications/message` to `tracing` under a per-connection rate limiter.
//! Progress notifications are NOT handled here: their ordering relative to a
//! `tools/call` response would otherwise be lossy, so they are forwarded inline
//! at the transport boundary instead (see [`super::transport`]).

use rmcp::ClientHandler;
use rmcp::model::{ClientConfig, ProgressToken};
use rmcp::service::{NotificationContext, RoleClient};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Maximum server logging notifications (`notifications/message`) forwarded to
/// `tracing` per second, per connection.
///
/// A server can emit log notifications far faster than is useful; without a
/// bound a chatty or hostile server would flood the daemon's log (and the I/O
/// behind it). Notifications beyond the budget are dropped and the count is
/// reported once the window rolls over. (`notifications/progress` has its own
/// per-call throttle, [`PROGRESS_MIN_INTERVAL`](super::call::PROGRESS_MIN_INTERVAL).)
const MAX_LOG_NOTIFICATIONS_PER_SECOND: u32 = 100;

/// A server-originated event the engine forwards to the rest of the client.
///
/// Progress events are produced by the transport wrapper
/// ([`super::transport::ProgressForwarding`]), which forwards each
/// `notifications/progress` inline as it is read; the broadcast carries them to
/// the in-flight call tasks that care.
#[derive(Debug, Clone)]
pub(super) enum ServerEvent {
    /// A `notifications/progress` for the call owning `token`.
    Progress {
        /// Correlates the notification with the originating request.
        token: ProgressToken,
        /// The current progress value.
        progress: f64,
        /// The total, when the server knows it.
        total: Option<f64>,
        /// Optional human-readable progress message.
        message: Option<String>,
    },
}

/// A fixed-window rate limiter for server-originated notifications.
///
/// Kept as a tiny counter with an injectable clock, so the allow/deny decision
/// is unit-testable without waiting on wall-clock time. One instance is shared
/// across a connection's notification callbacks through an `Arc`; the `Mutex`
/// guards only a few integers (no protocol data), and the same lock is never
/// held across an `await`.
#[derive(Debug)]
struct NotificationLimiter {
    /// Allowed notifications per window.
    max: u32,
    /// Window length.
    window: Duration,
    /// The shared counter, guarded by its own lock (the sanctioned shared-state
    /// exception #9; see AGENTS.md): rmcp invokes the notification callbacks on
    /// more than one task, so the budget must be shared. The lock guards only
    /// these integers and is never held across an `await`.
    state: std::sync::Mutex<LimiterState>,
}

/// Mutable state behind a [`NotificationLimiter`].
#[derive(Debug)]
struct LimiterState {
    /// Start of the current window, or `None` before the first notification.
    window_start: Option<Instant>,
    /// Notifications allowed so far in the current window.
    count: u32,
    /// Notifications dropped in the current window.
    suppressed: u64,
}

impl NotificationLimiter {
    /// Build a limiter allowing `max` notifications per one-second window.
    fn new(max: u32) -> Self {
        Self {
            max,
            window: Duration::from_secs(1),
            state: std::sync::Mutex::new(LimiterState {
                window_start: None,
                count: 0,
                suppressed: 0,
            }),
        }
    }

    /// Record one notification observed at `now`; returns whether it is within
    /// the budget (and so should be forwarded).
    ///
    /// Rolling into a new window resets the allowance and, when the previous
    /// window dropped anything, emits a single trace line naming the count — so
    /// a throttled server is visible without logging every dropped notification.
    fn allow_at(&self, now: Instant) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let in_window = state
            .window_start
            .is_some_and(|start| now.duration_since(start) < self.window);
        if !in_window {
            let suppressed = std::mem::take(&mut state.suppressed);
            if suppressed > 0 {
                tracing::debug!(
                    suppressed,
                    "MCP server logging notifications were rate-limited"
                );
            }
            state.count = 0;
            state.window_start = Some(now);
        }
        if state.count < self.max {
            state.count += 1;
            true
        } else {
            state.suppressed += 1;
            false
        }
    }
}

/// The `ClientHandler` for one connection.
///
/// rmcp routes every server-to-client notification here rather than to the
/// caller that issued the request. Logging (`notifications/message`) is turned
/// into a `tracing` event under a per-connection rate limiter; the handler also
/// carries the `clientInfo` and capability object advertised to the server.
/// Progress is forwarded by the transport wrapper, not here (see the module
/// docs).
#[derive(Clone)]
pub(super) struct ServerHandler {
    config: ClientConfig,
    /// Per-connection rate limiter for logging notifications.
    limiter: Arc<NotificationLimiter>,
}

impl ServerHandler {
    /// Build the handler for one connection, advertising `config`.
    ///
    /// The logging rate limiter is created here so the per-connection budget is
    /// owned entirely by the handler.
    pub(super) fn new(config: ClientConfig) -> Self {
        Self {
            config,
            limiter: Arc::new(NotificationLimiter::new(MAX_LOG_NOTIFICATIONS_PER_SECOND)),
        }
    }
}

impl ClientHandler for ServerHandler {
    fn get_info(&self) -> ClientConfig {
        self.config.clone()
    }

    // Progress notifications are NOT handled here: rmcp spawns a notification
    // callback on a separate task while resolving a response inline, so a
    // progress callback can lag the `tools/call` response it belongs to. They
    // are forwarded inline at the transport boundary instead (see
    // `super::transport`), which makes the ordering deterministic.

    // List-changed notifications are NOT handled here: the stateless era
    // delivers them only on a `subscriptions/listen` stream, which rmcp routes
    // to that stream's own receiver (see `spawn_list_change_listener`), not to
    // the handler. An unsolicited list-changed from a legacy peer carries no
    // actionable list to refresh, so it is intentionally ignored.

    // Logging is deprecated by the specification (SEP-2577), but a server may
    // still emit `notifications/message`, so it is forwarded to `tracing`
    // rather than dropped. The deprecation note is the framework's; there is no
    // replacement notification to consume instead.
    #[expect(
        deprecated,
        reason = "rmcp flags the logging notification types as deprecated (SEP-2577); consuming the notification is still the only way to observe a server that sends one"
    )]
    async fn on_logging_message(
        &self,
        params: rmcp::model::LoggingMessageNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) {
        // Bound the notification rate: a server that floods the log is
        // throttled rather than allowed to dominate the daemon's log output.
        if !self.limiter.allow_at(Instant::now()) {
            return;
        }
        let logger = params.logger.as_deref().unwrap_or("server");
        let data = params.data;
        tracing::info!(logger = %logger, level = ?params.level, "MCP server log: {data}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notification_limiter_enforces_a_per_window_budget() {
        let limiter = NotificationLimiter::new(2);
        let base = Instant::now();
        assert!(limiter.allow_at(base), "first is within budget");
        assert!(limiter.allow_at(base), "second is within budget");
        assert!(!limiter.allow_at(base), "third exceeds the budget");
        assert!(!limiter.allow_at(base), "fourth too");
        // A fresh window restores the allowance.
        assert!(limiter.allow_at(base + Duration::from_secs(1)));
    }
}
