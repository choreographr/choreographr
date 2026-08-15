use std::io;
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

/// Per-attempt wall-clock deadline, re-armed by [`retry_loop`] at the start
/// of every attempt (including the first).
///
/// This exists to close a gap in the SSE timeout layering.  ureq's
/// `timeout_global` bounds a single attempt from DNS through the response
/// headers, but its per-read timeout is floored at ~1s, so keep-alive
/// trickles can outlive it; the SSE consumer's own deadline check (in
/// `crate::stream`) is the real hard backstop for the body read.  If that
/// consumer deadline were computed only when the body read begins, a slow
/// header phase and a trickling body could each consume a full
/// `total_timeout_secs` budget — up to 2× the configured bound for one
/// attempt.  Threading one deadline through [`retry_loop`] instead makes the
/// consumer-side check span the entire attempt (DNS + connect + headers +
/// body), and re-arming it per attempt preserves the documented "each retry
/// restarts the deadline" semantics.
pub struct AttemptDeadline {
    /// Configured per-attempt budget in seconds; `0` disables the deadline.
    total_timeout_secs: u64,
    /// Absolute deadline for the current attempt; `None` when disabled.
    current: Option<std::time::Instant>,
}

impl AttemptDeadline {
    /// Create a deadline for the given budget, left unarmed until
    /// [`AttemptDeadline::reset`] is called — [`retry_loop`] does that at the
    /// top of every attempt, including the first.
    pub fn new(total_timeout_secs: u64) -> Self {
        Self {
            total_timeout_secs,
            current: None,
        }
    }

    /// Re-arm the deadline for a fresh attempt.  Called by [`retry_loop`] at
    /// the top of every attempt, so the budget covers the whole attempt and
    /// a retried request gets a fresh budget rather than a stale one.
    pub(crate) fn reset(&mut self) {
        self.current = (self.total_timeout_secs > 0).then(|| {
            std::time::Instant::now() + std::time::Duration::from_secs(self.total_timeout_secs)
        });
    }

    /// The successful attempt's deadline, for the SSE consumer to enforce
    /// across the whole response-body read.
    pub(crate) fn current(&self) -> Option<std::time::Instant> {
        self.current
    }
}

/// Per-call retry context threaded through [`retry_loop`]: the retry
/// callback, the cancellation channel, and the optional per-attempt
/// wall-clock deadline.  Bundled so the retry entry points do not grow a new
/// parameter every time a knob is added.
pub struct AttemptContext<'a> {
    /// Invoked before each retry wait with (attempt, max_attempts, delay).
    pub on_retry: &'a mut Option<RetryCallback>,
    /// Cancellation channel; `None` when cancellation is not wired up.
    /// Crossbeam so waits can `select!` on it alongside a retry timer.
    pub cancel_rx: Option<&'a crossbeam_channel::Receiver<()>>,
    /// Per-attempt wall-clock deadline, re-armed at the top of every attempt;
    /// `None` when the deadline is disabled or not provided.
    pub attempt_deadline: Option<&'a mut AttemptDeadline>,
}

impl<'a> AttemptContext<'a> {
    pub fn new(
        on_retry: &'a mut Option<RetryCallback>,
        cancel_rx: Option<&'a crossbeam_channel::Receiver<()>>,
        attempt_deadline: Option<&'a mut AttemptDeadline>,
    ) -> Self {
        Self {
            on_retry,
            cancel_rx,
            attempt_deadline,
        }
    }
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
        status: u16,
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
                status,
                retry_after_secs,
                detail,
            } => {
                if let Some(secs) = retry_after_secs {
                    write!(f, "rate limited ({status}, retry after {secs}s): {detail}")
                } else {
                    write!(f, "rate limited ({status}): {detail}")
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
pub(crate) fn cancellation_pending(cancel_rx: Option<&crossbeam_channel::Receiver<()>>) -> bool {
    cancel_rx.is_some_and(|rx| rx.try_recv().is_ok())
}

/// Check whether a cancellation signal has been received on `cancel_rx`.
/// Returns `Err(ProviderHttpError::Cancelled)` if the channel contains a
/// pending message, or `Ok(())` when no cancellation is pending (or when
/// no channel is provided).
pub fn check_cancelled(
    cancel_rx: Option<&crossbeam_channel::Receiver<()>>,
) -> Result<(), ProviderHttpError> {
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
    cancel_rx: Option<&crossbeam_channel::Receiver<()>>,
) -> Result<(), ProviderHttpError> {
    if let Some(rx) = cancel_rx {
        // Wait on the cancel channel and a delay timer simultaneously, so a
        // cancellation wakes this thread the instant it is sent instead of at
        // the next poll tick.  `select_biased!` (cancel arm first) is the
        // event-driven replacement for the old `recv_timeout(delay)` loop:
        // an already-queued cancel is selected deterministically (the biased
        // fast path scans arms in order), and a cancel that arrives just
        // before the backoff timer expires is *more likely* to win the race
        // than it would under an unbiased `select!` (which shuffles the
        // arms).  Either outcome is correct: a cancel stops the retry loop
        // here, and a timer expiry leaves the cancel queued for the next
        // `check_cancelled` call, so it is never silently swallowed.
        crossbeam_channel::select_biased! {
            recv(rx) -> msg => match msg {
                Ok(()) => return Err(ProviderHttpError::Cancelled),
                Err(_) => {
                    // The cancel sender is held by `ActiveRequest` and dropped
                    // only at `RequestFinished`, so a disconnect here is
                    // unreachable while the worker waits (see the invariant
                    // documented on `ActiveRequest.cancel_tx`). Proceeding —
                    // rather than aborting the retry loop — is the safe
                    // fallback either way.
                    tracing::trace!("cancel_rx sender dropped — proceeding without cancellation");
                }
            },
            recv(crossbeam_channel::after(delay)) -> _ => {}
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
    cancel_rx: Option<&crossbeam_channel::Receiver<()>>,
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
            status,
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
    ctx: &mut AttemptContext,
) -> Result<ureq::http::Response<ureq::Body>, ProviderHttpError>
where
    F: Fn() -> Result<ureq::http::Response<ureq::Body>, ureq::Error>,
{
    let mut attempt: u32 = 0;
    loop {
        attempt += 1;

        // Per-attempt deadline: re-arm at the top of every attempt so a
        // retried request gets a fresh budget, and the first attempt's
        // budget is measured from just before `send_request` (covering
        // DNS → connect → headers → body, not just the body read).
        if let Some(deadline) = ctx.attempt_deadline.as_deref_mut() {
            deadline.reset();
        }

        check_cancelled(ctx.cancel_rx)?;

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
                    wait_before_retry(
                        attempt,
                        retry.max_attempts,
                        delay,
                        ctx.on_retry,
                        ctx.cancel_rx,
                    )?;
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
            wait_before_retry(
                attempt,
                retry.max_attempts,
                delay,
                ctx.on_retry,
                ctx.cancel_rx,
            )?;
            continue;
        }

        let body_text = response.into_body().read_to_string().unwrap_or_default();
        let trimmed_body = body_text.trim();
        let detail = if trimmed_body.is_empty() {
            format!("request failed with status {status}")
        } else {
            match extract_error_message(trimmed_body) {
                // The body wrapped a recognizable JSON error envelope — use
                // its human-readable `message` instead of dumping the whole
                // body.  The status is already carried by the error variant's
                // own prefix (e.g. "client error (402): …"), so it is not
                // repeated here.
                Some(message) => message,
                // Not JSON / no recognizable message field: keep the status
                // context plus the verbatim body so nothing is lost.
                None => format!("request failed with status {status}: {trimmed_body}"),
            }
        };
        return Err(status_to_error(status, &detail, retry_after_secs));
    }
}

/// Extract a concise human-readable message from a provider error body,
/// unwrapping the standard JSON error envelopes used across providers:
///
/// - OpenAI-compatible APIs (OpenAI, DeepSeek, OpenRouter, Groq, …):
///   `{"error": {"message": "…", "type": "…", "param": …,
///   "code": …}}` — `error.message` is the standard human-readable field.
/// - Anthropic: `{"type": "error", "error": {"type": "…",
///   "message": "…"}}` — same `error.message` shape.
/// - Gemini: `{"error": {"code": …, "message": "…", "status":
///   "…"}}` — same `error.message` shape.
/// - Some proxies: `{"message": "…"}` or `{"error": "plain string"}`.
///
/// Returns `None` (the caller keeps the verbatim body) when the body is not
/// JSON or carries no recognizable string `message` — an unknown or hostile
/// body is never mis-summarized, and the raw text still reaches the user.
fn extract_error_message(body: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    let message = match value.get("error") {
        // `{"error": {"message": "…"}}` — the standard envelope.
        Some(serde_json::Value::Object(map)) => {
            map.get("message").and_then(serde_json::Value::as_str)
        }
        // `{"error": "plain string"}` — rare compat-layer shape.
        Some(serde_json::Value::String(s)) => Some(s.as_str()),
        _ => None,
    };
    // Top-level `{"message": "…"}` (some reverse-proxies unwrap for you).
    let message = message.or_else(|| value.get("message").and_then(serde_json::Value::as_str));
    let message = message.map(str::trim).filter(|m| !m.is_empty())?;
    Some(message.to_string())
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

    // ── extract_error_message ────────────────────────────────────────────

    #[test]
    fn extract_error_message_openai_envelope() {
        // The standard OpenAI-compatible shape (DeepSeek, OpenRouter, …):
        // `error.message` is the human-readable summary.
        let body = r#"{"error":{"message":"Insufficient Balance","type":"unknown_error","param":null,"code":"invalid_request_error"}}"#;
        assert_eq!(
            extract_error_message(body).as_deref(),
            Some("Insufficient Balance")
        );
    }

    #[test]
    fn extract_error_message_anthropic_and_gemini_shapes() {
        // Anthropic nests under `type` + `error`; Gemini puts `code` next to
        // `message`.  Both still expose `error.message`, so the same lookup
        // unwraps them.
        let anthropic =
            r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#;
        assert_eq!(
            extract_error_message(anthropic).as_deref(),
            Some("Overloaded")
        );
        let gemini =
            r#"{"error":{"code":429,"message":"Quota exceeded","status":"RESOURCE_EXHAUSTED"}}"#;
        assert_eq!(
            extract_error_message(gemini).as_deref(),
            Some("Quota exceeded")
        );
    }

    #[test]
    fn extract_error_message_compat_shapes() {
        // Top-level `message` (proxy-unwrapped) and a plain-string `error`
        // (rare compat layer) are both recognized.
        assert_eq!(
            extract_error_message(r#"{"message":"bad request"}"#).as_deref(),
            Some("bad request")
        );
        assert_eq!(
            extract_error_message(r#"{"error":"plain failure"}"#).as_deref(),
            Some("plain failure")
        );
    }

    #[test]
    fn extract_error_message_falls_back_for_non_envelope_bodies() {
        // Not JSON, JSON without a string message, or an empty message all
        // yield None — the caller keeps the verbatim body rather than
        // mis-summarizing an unknown shape.
        assert_eq!(extract_error_message("<html>502 Bad Gateway</html>"), None);
        assert_eq!(extract_error_message(r#"{"error":{"code":7}}"#), None);
        assert_eq!(
            extract_error_message(r#"{"error":{"message":"   "}}"#),
            None
        );
        assert_eq!(extract_error_message(r#"{"error":null}"#), None);
        assert_eq!(extract_error_message("not json at all"), None);
    }

    #[test]
    fn rate_limited_status_survives_the_whole_error_chain() {
        // Regression: the pre-fix detail de-dup dropped the status for 429s
        // because `RateLimited` carried no `status` field — a parseable body
        // rendered as `rate limited: Quota exceeded` with the status silently
        // lost.  The status must appear in the Display at every layer.
        let http = ProviderHttpError::RateLimited {
            status: 429,
            retry_after_secs: None,
            detail: "Quota exceeded".into(),
        };
        assert_eq!(http.to_string(), "rate limited (429): Quota exceeded");

        let with_retry_after = ProviderHttpError::RateLimited {
            status: 429,
            retry_after_secs: Some(30),
            detail: "Quota exceeded".into(),
        };
        assert_eq!(
            with_retry_after.to_string(),
            "rate limited (429, retry after 30s): Quota exceeded"
        );

        // The provider → inference conversions must thread the status through
        // unchanged (each Display still carries it).
        let provider: crate::shared::ProviderError = http.into();
        assert_eq!(provider.to_string(), "rate limited (429): Quota exceeded");
        let inference = crate::shared::provider_error_to_inference(provider);
        assert_eq!(inference.to_string(), "rate limited (429): Quota exceeded");
    }

    // ── AttemptDeadline ──────────────────────────────────────────────────

    #[test]
    fn attempt_deadline_disabled_when_zero() {
        // A zero budget disables the deadline entirely (matches the
        // `total_timeout_secs = 0 disables` contract).
        let deadline = AttemptDeadline::new(0);
        assert!(deadline.current().is_none());
    }

    #[test]
    fn attempt_deadline_arms_on_reset() {
        // `new` leaves the deadline unarmed; `reset` arms it for an attempt
        // (`retry_loop` calls reset at the top of every attempt, including
        // the first).  Deterministic: we only observe the clock — no waiting.
        let before = std::time::Instant::now();
        let mut deadline = AttemptDeadline::new(3600);
        assert!(
            deadline.current().is_none(),
            "new() must not arm the deadline; retry_loop does that on reset"
        );
        deadline.reset();
        let first = deadline
            .current()
            .expect("reset must arm a deadline with a nonzero budget");
        assert!(
            first > before,
            "deadline must be in the future (budget 3600s)"
        );
        // A second reset must move the deadline forward, not keep a stale
        // instant.
        deadline.reset();
        let second = deadline
            .current()
            .expect("reset must keep the deadline armed");
        assert!(
            second >= first,
            "reset must not move the deadline backwards"
        );
    }
}
