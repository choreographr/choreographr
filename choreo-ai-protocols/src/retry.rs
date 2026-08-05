use std::io;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

/// Called before each retry attempt with (current_attempt, max_attempts, delay).
pub type RetryCallback = Box<dyn FnMut(u32, u32, Duration) + Send>;

#[derive(Debug, Clone)]
pub struct RetryConfig {
    pub max_attempts: u32,
    pub initial_backoff_ms: u64,
    pub max_backoff_ms: u64,
}

/// Generic HTTP error produced by the retry layer.  Each provider maps this to
/// its own error type via `From`.
#[derive(Debug)]
pub enum ProviderHttpError {
    Unauthorized {
        status: u16,
        detail: String,
    },
    RateLimited {
        retry_after_secs: Option<u64>,
        detail: String,
    },
    ServerError {
        status: u16,
        detail: String,
    },
    ClientError {
        status: u16,
        detail: String,
    },
    EmptyResponse,
    Cancelled,
    Io(io::Error),
}

impl std::fmt::Display for ProviderHttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unauthorized { status, detail } => {
                write!(f, "unauthorized ({status}): {detail}")
            }
            Self::RateLimited {
                retry_after_secs,
                detail,
            } => {
                if let Some(secs) = retry_after_secs {
                    write!(f, "rate limited (retry after {secs}s): {detail}")
                } else {
                    write!(f, "rate limited: {detail}")
                }
            }
            Self::ServerError { status, detail } => {
                write!(f, "server error ({status}): {detail}")
            }
            Self::ClientError { status, detail } => {
                write!(f, "client error ({status}): {detail}")
            }
            Self::EmptyResponse => f.write_str("empty response"),
            Self::Cancelled => f.write_str("cancelled"),
            Self::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ProviderHttpError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

pub fn backoff_duration(retry_number: u32, config: &RetryConfig) -> Duration {
    let multiplier = 2u64.saturating_pow(retry_number.saturating_sub(1));
    let base = config.initial_backoff_ms.saturating_mul(multiplier);
    let capped = base.min(config.max_backoff_ms);
    let jitter: f64 = rand::random_range(0.75..=1.25);
    Duration::from_millis((capped as f64 * jitter) as u64)
}

pub fn is_retryable_status(status: u16) -> bool {
    matches!(status, 429 | 500 | 502 | 503 | 504)
}

/// Extract the retry-after header value as seconds from an optional string
/// value (e.g. `response.header("retry-after")`).
pub fn parse_retry_after_secs(value: Option<&str>) -> Option<u64> {
    value.and_then(|v| v.parse::<u64>().ok())
}

/// Returns `true` when a cancellation signal is pending on `cancel_rx`.
/// With no channel provided, cancellation is never pending.
pub(crate) fn cancellation_pending(cancel_rx: Option<&mpsc::Receiver<()>>) -> bool {
    cancel_rx.is_some_and(|rx| rx.try_recv().is_ok())
}

/// Check whether a cancellation signal has been received on `cancel_rx`.
/// Returns `Err(ProviderHttpError::Cancelled)` if the channel contains a
/// pending message, or `Ok(())` when no cancellation is pending (or when
/// no channel is provided).
pub fn check_cancelled(cancel_rx: Option<&mpsc::Receiver<()>>) -> Result<(), ProviderHttpError> {
    if cancellation_pending(cancel_rx) {
        tracing::debug!("operation cancelled by user");
        return Err(ProviderHttpError::Cancelled);
    }
    Ok(())
}

/// Block for `delay`, returning `Cancelled` early if a signal arrives on
/// `cancel_rx`.  When no channel is provided, falls back to `thread::sleep`.
pub fn sleep_or_cancel(
    delay: Duration,
    cancel_rx: Option<&mpsc::Receiver<()>>,
) -> Result<(), ProviderHttpError> {
    if let Some(rx) = cancel_rx {
        match rx.recv_timeout(delay) {
            Ok(()) => return Err(ProviderHttpError::Cancelled),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                tracing::trace!("cancel_rx sender dropped — proceeding without cancellation");
            }
        }
    } else {
        thread::sleep(delay);
    }
    Ok(())
}

/// Invoke the retry callback (if any) then wait for the backoff duration.
/// Returns `Cancelled` if the user cancelled during the wait.
pub fn wait_before_retry(
    attempt: u32,
    max_attempts: u32,
    delay: Duration,
    on_retry: &mut Option<RetryCallback>,
    cancel_rx: Option<&mpsc::Receiver<()>>,
) -> Result<(), ProviderHttpError> {
    if let Some(cb) = on_retry.as_mut() {
        cb(attempt, max_attempts, delay);
    }
    sleep_or_cancel(delay, cancel_rx)
}

/// Compute the delay before the next retry attempt.
///
/// Honours a provider-supplied `Retry-After` for 429 responses, but caps it
/// at `retry.max_backoff_ms` so a malicious/huge header value cannot wedge a
/// request thread for an unbounded time.  All other statuses (and 429s
/// without a Retry-After header) fall back to exponential backoff.
fn retry_delay(
    status: u16,
    retry_after_secs: Option<u64>,
    attempt: u32,
    retry: &RetryConfig,
) -> Duration {
    if status == 429 {
        retry_after_secs
            .map(Duration::from_secs)
            .map(|d| d.min(Duration::from_millis(retry.max_backoff_ms)))
            .unwrap_or_else(|| backoff_duration(attempt, retry))
    } else {
        backoff_duration(attempt, retry)
    }
}

fn status_to_error(status: u16, detail: &str, retry_after_secs: Option<u64>) -> ProviderHttpError {
    match status {
        401 | 403 => ProviderHttpError::Unauthorized {
            status,
            detail: detail.to_string(),
        },
        429 => ProviderHttpError::RateLimited {
            retry_after_secs,
            detail: detail.to_string(),
        },
        _ if (500..600).contains(&status) => ProviderHttpError::ServerError {
            status,
            detail: detail.to_string(),
        },
        _ if (400..500).contains(&status) => ProviderHttpError::ClientError {
            status,
            detail: detail.to_string(),
        },
        _ => ProviderHttpError::Io(io::Error::other(detail.to_string())),
    }
}

/// Core retry loop: calls `send_request`, inspects the HTTP status, and retries
/// on retryable errors up to `retry.max_attempts` times.
///
/// The agent must be created with `http_status_as_error(false)` so that 4xx/5xx
/// arrive as `Ok(response)` rather than `Err`.  Only transport errors
/// (connection refused, timeout, etc.) produce `Err`.
pub fn retry_loop<F>(
    send_request: F,
    retry: &RetryConfig,
    on_retry: &mut Option<RetryCallback>,
    cancel_rx: Option<&mpsc::Receiver<()>>,
) -> Result<ureq::http::Response<ureq::Body>, ProviderHttpError>
where
    F: Fn() -> Result<ureq::http::Response<ureq::Body>, ureq::Error>,
{
    let mut attempt: u32 = 0;
    loop {
        attempt += 1;

        check_cancelled(cancel_rx)?;

        // With http_status_as_error(false), all HTTP responses (even 4xx/5xx)
        // arrive as Ok; only transport errors are Err.
        let response = match send_request() {
            Ok(resp) => resp,
            Err(err) => {
                if attempt < retry.max_attempts {
                    let delay = backoff_duration(attempt, retry);
                    tracing::warn!(
                        attempt,
                        max_attempts = retry.max_attempts,
                        ?err,
                        delay_ms = delay.as_millis(),
                        "retrying request after transport error"
                    );
                    wait_before_retry(attempt, retry.max_attempts, delay, on_retry, cancel_rx)?;
                    continue;
                }
                return Err(ProviderHttpError::Io(io::Error::other(format!("{err}"))));
            }
        };

        let status: u16 = response.status().as_u16();
        if (200..300).contains(&status) {
            return Ok(response);
        }

        // Parse retry-after once, used in both the retry and terminal-error paths.
        let retry_after_secs = parse_retry_after_secs(
            response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok()),
        );

        if is_retryable_status(status) && attempt < retry.max_attempts {
            let delay = retry_delay(status, retry_after_secs, attempt, retry);
            let body_text = response.into_body().read_to_string().unwrap_or_default();
            tracing::warn!(
                attempt,
                max_attempts = retry.max_attempts,
                status,
                %body_text,
                delay_ms = delay.as_millis(),
                "retrying request"
            );
            wait_before_retry(attempt, retry.max_attempts, delay, on_retry, cancel_rx)?;
            continue;
        }

        let body_text = response.into_body().read_to_string().unwrap_or_default();
        let trimmed_body = body_text.trim();
        let detail = if trimmed_body.is_empty() {
            format!("request failed with status {status}")
        } else {
            format!("request failed with status {status}: {trimmed_body}")
        };
        return Err(status_to_error(status, &detail, retry_after_secs));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> RetryConfig {
        RetryConfig {
            max_attempts: 5,
            initial_backoff_ms: 1000,
            max_backoff_ms: 30000,
        }
    }

    // The Retry-After path is deterministic (no jitter), so exact equality is
    // fine there.  The backoff path applies 0.75..=1.25 jitter, so those tests
    // assert bounds instead.

    #[test]
    fn huge_retry_after_is_capped_at_max_backoff() {
        // A malicious/broken provider could send Retry-After: u64::MAX; the
        // delay must be clamped to max_backoff_ms rather than ~584bn years.
        let retry = test_config();
        let delay = retry_delay(429, Some(u64::MAX), 1, &retry);
        assert_eq!(delay, Duration::from_millis(retry.max_backoff_ms));
    }

    #[test]
    fn retry_after_above_max_is_capped() {
        // A large but plausible header value is also capped.
        let retry = test_config();
        let delay = retry_delay(429, Some(1_000_000), 1, &retry);
        assert_eq!(delay, Duration::from_millis(retry.max_backoff_ms));
    }

    #[test]
    fn small_retry_after_passes_through() {
        // A provider-honoured value below the cap is used verbatim.
        let retry = test_config();
        let delay = retry_delay(429, Some(2), 1, &retry);
        assert_eq!(delay, Duration::from_secs(2));
    }

    #[test]
    fn retry_after_absent_falls_back_to_backoff() {
        // 429 without a Retry-After header → exponential backoff (jittered).
        let retry = test_config();
        let delay = retry_delay(429, None, 1, &retry);
        let millis = delay.as_millis() as f64;
        let base = retry.initial_backoff_ms as f64;
        assert!(millis >= base * 0.75, "delay {millis}ms below jitter floor");
        assert!(
            millis <= base * 1.25,
            "delay {millis}ms above jitter ceiling"
        );
    }

    #[test]
    fn non_429_status_uses_backoff() {
        // Retry-After is only honoured for 429; a 503 with a huge header still
        // backs off exponentially (jittered), ignoring the header.
        let retry = test_config();
        let delay = retry_delay(503, Some(u64::MAX), 1, &retry);
        let millis = delay.as_millis() as f64;
        let base = retry.initial_backoff_ms as f64;
        assert!(millis >= base * 0.75, "delay {millis}ms below jitter floor");
        assert!(
            millis <= base * 1.25,
            "delay {millis}ms above jitter ceiling"
        );
    }
}
