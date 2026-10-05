//! Bounded transport-reconnect policy for a dispatcher whose engine dies.

use crate::error::McpError;
use crate::protocol::CallToolResult;
use crate::session::{EngineFactory, McpEngine};
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

    /// Rebuild the engine after a transport failure, subject to the budget.
    ///
    /// Resets the failure count on success. The sleep is skipped entirely when
    /// the backoff is zero, which is how the unit tests keep the restart path
    /// free of any time-based wait.
    pub(super) fn on_transport_failure(
        &mut self,
        factory: &EngineFactory,
        engine: &mut Arc<dyn McpEngine>,
    ) {
        self.failures = self.failures.saturating_add(1);
        if self.failures > self.max_attempts {
            tracing::warn!(
                failures = self.failures,
                "MCP server exceeded restart budget; not reconnecting"
            );
            return;
        }
        let backoff = self.backoff();
        if !backoff.is_zero() {
            std::thread::sleep(backoff);
        }
        match factory() {
            Ok(fresh) => {
                *engine = fresh;
                self.failures = 0;
                tracing::info!("MCP server reconnected after transport failure");
            }
            Err(e) => tracing::warn!(error = %e, "MCP reconnect failed"),
        }
    }
}

/// Whether an error indicates the transport (not the request) failed, which is
/// the trigger for a reconnect.
pub(super) fn is_transport_error(error: &McpError) -> bool {
    matches!(
        error,
        McpError::ServerShutdown | McpError::Io(_) | McpError::NotConnected
    )
}

/// `is_transport_error` over a `Result` reference (used by spawned call tasks).
pub(super) fn is_transport_error_ref(result: &Result<CallToolResult, McpError>) -> bool {
    result.as_ref().err().is_some_and(is_transport_error)
}
