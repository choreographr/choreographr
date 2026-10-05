//! Connect-time retry policy for the Streamable HTTP transport.
//!
//! Remote servers are frequently transiently unavailable — a backend restart, a
//! rate limiter, or a load balancer answering `503`. Retrying the initial
//! `server/discover` probe a few times with exponential backoff turns most of
//! those into a successful connect instead of a permanently dropped server.
//!
//! Only genuinely transient statuses are retried: `408` (request timeout),
//! `429` (too many requests), and every `5xx`. A `4xx` protocol error (a `400`
//! from a malformed request, a `401`/`403` auth challenge, a `404`) is a
//! settled answer and is never retried. The policy is deliberately
//! rmcp-independent so it can be unit-tested without a live transport or a
//! clock — the backoff is a pure function of the attempt number, and the sleep
//! is owned by the caller.

use std::time::Duration;

/// Total connect attempts (the first try plus retries) before giving up.
pub(crate) const MAX_ATTEMPTS: u32 = 3;

/// Backoff for the first retry.
pub(crate) const BASE_BACKOFF: Duration = Duration::from_millis(500);

/// Ceiling for the exponential backoff.
pub(crate) const MAX_BACKOFF: Duration = Duration::from_mins(1);

/// Exponential backoff before retry number `attempt` (1-based):
/// `base · 2^(attempt-1)`, capped at `ceiling`.
///
/// The one shared formula behind every bounded-retry path in the crate (the
/// connect probe, the dispatcher's restart policy, and the SSE stream
/// reconnect). The shift is clamped so a hostile attempt count cannot overflow
/// the left shift; the final `.min` makes the clamp exact.
pub(crate) fn backoff(attempt: u32, base: Duration, ceiling: Duration) -> Duration {
    let shift = attempt.saturating_sub(1).min(7);
    base.saturating_mul(1u32 << shift).min(ceiling)
}

/// Backoff before retry number `attempt` (1-based) under the fixed-default
/// policy (base [`BASE_BACKOFF`], ceiling [`MAX_BACKOFF`]) the connect probe
/// uses.
pub(crate) fn connect_backoff(attempt: u32) -> Duration {
    backoff(attempt, BASE_BACKOFF, MAX_BACKOFF)
}

/// Whether an HTTP status is worth retrying.
///
/// `408`, `429`, and the whole `5xx` range are transient; every other status
/// (notably `4xx` protocol errors) is a settled answer.
pub(crate) fn is_retryable_status(status: u16) -> bool {
    matches!(status, 408 | 429) || (500..=599).contains(&status)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_exponentially_then_caps() {
        // The fixed-default connect policy is the general formula with the
        // standard base and ceiling.
        assert_eq!(connect_backoff(1), Duration::from_millis(500));
        assert_eq!(connect_backoff(2), Duration::from_secs(1));
        assert_eq!(connect_backoff(3), Duration::from_secs(2));
        assert_eq!(connect_backoff(4), Duration::from_secs(4));
        // The clamp keeps an absurd attempt count from overflowing and pins to
        // the ceiling.
        assert_eq!(connect_backoff(64), MAX_BACKOFF);
    }

    #[test]
    fn backoff_honours_a_custom_base_and_ceiling() {
        // A caller-supplied policy scales from its own base and never exceeds
        // its own ceiling.
        let base = Duration::from_millis(100);
        let ceiling = Duration::from_secs(1);
        assert_eq!(backoff(1, base, ceiling), Duration::from_millis(100));
        assert_eq!(backoff(2, base, ceiling), Duration::from_millis(200));
        assert_eq!(backoff(64, base, ceiling), ceiling);
    }

    #[test]
    fn retryable_statuses() {
        assert!(is_retryable_status(408));
        assert!(is_retryable_status(429));
        assert!(is_retryable_status(500));
        assert!(is_retryable_status(503));
        assert!(is_retryable_status(599));
        assert!(!is_retryable_status(400));
        assert!(!is_retryable_status(401));
        assert!(!is_retryable_status(403));
        assert!(!is_retryable_status(404));
        assert!(!is_retryable_status(200));
    }
}
