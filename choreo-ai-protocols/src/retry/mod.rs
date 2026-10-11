//! Shared HTTP retry machinery used by every provider client.
//!
//! Owns the retry/backoff loop ([`retry_loop`]), the `Retry-After` parsing and
//! budget gate (`retry_decision`), cancellation-aware waits, and the
//! normalized [`ProviderHttpError`] each adapter maps into its own error type.
//! The retry decision is status + `Retry-After` only — never the response body.
//!
//! The request-body send path — the serialized JSON payload plus the
//! upload-complete hook that marks the daemon's `Waiting` boundary — lives in
//! the `send_body` submodule.
//!
//! `retry.rs` is large; the split keeps the retry/backoff policy here and the
//! send-body machinery in `send_body.rs`, with the unit tests in `tests.rs`.

use std::collections::HashSet;
use std::io;
use std::sync::{Arc, LazyLock, Mutex};
use std::thread;
use std::time::Duration;

mod send_body;

pub(crate) use send_body::HookedJsonBody;

/// Raw `(initial_backoff_ms, max_backoff_ms)` pairs [`RetryConfig::new`] has
/// already warned about in this process.
///
/// [`RetryConfig::new`] runs on every provider request, so a misconfigured
/// programmatic caller (a hand-built `ServiceConfig` with over-ceiling or
/// inverted backoff knobs — the daemon rejects those at accounts load) would
/// otherwise emit one `warn!` per request, forever.  Inserting the raw pair
/// here makes the warning fire once per distinct misconfiguration instead.
/// This is logging-only bookkeeping, not cross-thread communication (it
/// carries no protocol data), and the mutex is cold: it is touched only on
/// the clamped (misconfigured) path, and each bad config inserts once — a
/// `HashSet` lookup after that.  The poisoned-mutex recovery below follows
/// AGENTS.md's rule (`.into_inner()`), and losing the warning entirely on a
/// poisoned mutex is an acceptable failure mode for a log-dedup registry.
static WARNED_CLAMPS: LazyLock<Mutex<HashSet<(u64, u64)>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

/// Called before each retry attempt with (`current_attempt`, `max_attempts`, delay).
pub type RetryCallback = Box<dyn FnMut(u32, u32, Duration) + Send>;

/// Called once when a request's payload has been fully written to the
/// transport — every byte of the body has been handed to the provider's
/// connection, before the response is read.  It marks the boundary between
/// "the request is being sent" and "the provider is now processing it", so a
/// status bar can distinguish the upload phase from the time-to-first-token
/// wait.
///
/// Anchoring the boundary at upload-completion (rather than at the response
/// headers) is deliberate: a buffering gateway can withhold the response head
/// until the model's first token is ready, which would collapse the
/// "waiting-for-the-model" phase into "sending".  The client-side upload
/// boundary is the last point every transport exposes before the opaque
/// server-side wait, so it is the only anchor that is correct for both
/// buffering and non-buffering providers.
///
/// The hook fires once per attempt — including an attempt that the provider
/// later rejects and the retry loop re-sends — because it is driven by the
/// upload, which completes before the response status is known.  A retried
/// turn therefore reports the boundary once per attempt.
///
/// `Arc` (not `Box`) so the callback can be cloned into the per-attempt request
/// body reader on each retry; it carries no state of its own.
pub type RequestUploadedCallback = Arc<dyn Fn() + Send + Sync>;

/// Hard ceiling on a single retry delay, in milliseconds.
///
/// `retry_max_backoff_ms` is user-facing configuration with no upper bound of
/// its own, and it doubles as the Retry-After budget gate (see
/// `retry_decision`): without a ceiling, a typo (or hostile config) would
/// make that gate tautological — every provider cooldown "fits" the budget —
/// and let a single retry sleep past every other timeout in the request path
/// (`AttemptDeadline` bounds the send, not the wait).  1 hour is the policy
/// bound ("no wait is ever worth more than an hour"); all defaults (30 s) sit
/// well below it, so ordinary configs are unaffected.
pub const MAX_BACKOFF_MS: u64 = 3_600_000;

/// Retry/backoff policy for a provider client.
#[derive(Debug, Clone)]
pub struct RetryConfig {
    /// Maximum number of attempts, including the first (`1` means no retry).
    pub max_attempts: u32,
    /// Initial backoff before the first retry; grows exponentially after.
    pub initial_backoff_ms: u64,
    /// Ceiling on the backoff delay and the `Retry-After` budget gate.
    pub max_backoff_ms: u64,
}

impl RetryConfig {
    /// Build a retry config with the backoff knobs clamped to sane bounds.
    ///
    /// - `initial_backoff_ms` and `max_backoff_ms` are both capped at
    ///   [`MAX_BACKOFF_MS`].  The Retry-After budget gate and the delay both
    ///   read `max_backoff_ms`, so clamping it once here keeps them
    ///   consistent instead of re-applying the bound at every use site.
    /// - `max_backoff_ms` is never allowed below `initial_backoff_ms`: an
    ///   `initial > max` config would silently collapse the exponential
    ///   backoff to its cap on the very first attempt.
    ///
    /// A clamped value is surfaced with a `warn!` so silent adjustment never
    /// hides a config mistake.
    pub fn new(max_attempts: u32, initial_backoff_ms: u64, max_backoff_ms: u64) -> Self {
        let raw_initial = initial_backoff_ms;
        let raw_max = max_backoff_ms;
        let initial_backoff_ms = initial_backoff_ms.min(MAX_BACKOFF_MS);
        let max_backoff_ms = max_backoff_ms.min(MAX_BACKOFF_MS).max(initial_backoff_ms);
        if initial_backoff_ms != raw_initial || max_backoff_ms != raw_max {
            // Warn once per distinct raw pair per process (see WARNED_CLAMPS):
            // this constructor runs per provider request, so a permanently
            // misconfigured caller must not spam the log for its whole life.
            let fresh = WARNED_CLAMPS
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert((raw_initial, raw_max));
            if fresh {
                tracing::warn!(
                    raw_initial_backoff_ms = raw_initial,
                    raw_max_backoff_ms = raw_max,
                    clamped_initial_backoff_ms = initial_backoff_ms,
                    clamped_max_backoff_ms = max_backoff_ms,
                    "retry backoff outside sane bounds — clamped"
                );
            }
        }
        Self {
            max_attempts,
            initial_backoff_ms,
            max_backoff_ms,
        }
    }

    /// Collapse this config to a single attempt when `no_retry` is set,
    /// preserving the backoff/ceiling values; a no-op otherwise.
    ///
    /// The retry loop only resends while `attempt < retry.max_attempts`, so
    /// `max_attempts == 1` means exactly one attempt and no wait: the request
    /// is a single best-effort call. This is how `ChatTurnRequest::no_retry`
    /// is realised — the cache-warming ping must never linger retrying, so it
    /// pays neither the extra cost nor the delay a second attempt would add.
    #[must_use]
    pub fn with_no_retry(mut self, no_retry: bool) -> Self {
        if no_retry {
            self.max_attempts = 1;
        }
        self
    }
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
    /// `AttemptDeadline::reset` is called — [`retry_loop`] does that at the
    /// top of every attempt, including the first.
    #[must_use]
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
    /// Invoked before each retry wait with (attempt, `max_attempts`, delay).
    pub on_retry: &'a mut Option<RetryCallback>,
    /// Cancellation channel; `None` when cancellation is not wired up.
    /// Crossbeam so waits can `select!` on it alongside a retry timer.
    pub cancel_rx: Option<&'a crossbeam_channel::Receiver<()>>,
    /// Per-attempt wall-clock deadline, re-armed at the top of every attempt;
    /// `None` when the deadline is disabled or not provided.
    pub attempt_deadline: Option<&'a mut AttemptDeadline>,
}

impl<'a> AttemptContext<'a> {
    /// Bundle the per-call retry context for [`retry_loop`].
    #[must_use]
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
    /// Authentication/authorization failure (HTTP 401/403).
    Unauthorized {
        /// HTTP status code.
        status: u16,
        /// Provider's human-readable error detail.
        detail: String,
    },
    /// Rate limited (HTTP 429); the provider may name a cooldown.
    RateLimited {
        /// HTTP status code.
        status: u16,
        /// Parsed `Retry-After` cooldown, when the provider sent one.
        retry_after_secs: Option<u64>,
        /// Provider's human-readable error detail.
        detail: String,
    },
    /// Server-side failure (HTTP 5xx).
    ServerError {
        /// HTTP status code.
        status: u16,
        /// Provider's human-readable error detail.
        detail: String,
    },
    /// Non-retryable client error (4xx other than 401/403/429).
    ClientError {
        /// HTTP status code.
        status: u16,
        /// Provider's human-readable error detail.
        detail: String,
    },
    /// The provider returned a success status but an empty body.
    EmptyResponse,
    /// The request was cancelled during a retry wait or pre-attempt check.
    Cancelled,
    /// A transport error, or a terminal status with no structured wrapper.
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

/// The exponential backoff delay before attempt `retry_number`: the configured
/// initial backoff doubled per attempt, capped at `max_backoff_ms`, then
/// jittered by 0.75–1.25× so a fleet of callers does not retry in lockstep.
#[must_use]
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
pub fn backoff_duration(retry_number: u32, config: &RetryConfig) -> Duration {
    let multiplier = 2u64.saturating_pow(retry_number.saturating_sub(1));
    let base = config.initial_backoff_ms.saturating_mul(multiplier);
    let capped = base.min(config.max_backoff_ms);
    let jitter: f64 = rand::random_range(0.75..=1.25);
    Duration::from_millis((capped as f64 * jitter) as u64)
}

/// Statuses the HTTP contract defines as *potentially* transient, i.e. a
/// retry has a chance of succeeding.  Everything in the 4xx range other than
/// 429 (400/401/402/403/404/422/…) is absent by design: the server rejected
/// this exact request — bad input, missing/invalid auth, no balance, missing
/// entitlement — so resending it verbatim cannot succeed.
///
/// This is a status-level pre-filter only.  For 429 the full decision also
/// needs the `Retry-After` header (see `retry_decision`): the status alone
/// cannot distinguish a throttle that clears in seconds from a cooldown that
/// outlives the retry budget.
#[must_use]
pub fn is_retryable_status(status: u16) -> bool {
    matches!(status, 429 | 500 | 502 | 503 | 504)
}

/// Decide the wait before the next retry attempt for an already-retryable
/// status (callers gate on [`is_retryable_status`] first).
///
/// Returns `Some(delay)` when the response is transient and the caller should
/// wait `delay` then retry; `None` when the status is terminal and the caller
/// should fail now.
///
/// `Retry-After` is the server's rigid, machine-readable "come back then"
/// signal.  RFC 7231 defines it for 429 (rate limited) and 503 (service
/// unavailable) — the two retried statuses the header applies to — and it is
/// honored verbatim (no jitter: the server stated an exact wait) when it fits
/// within the backoff ceiling (`max_backoff_ms`).  When it exceeds the
/// ceiling, the server has already told us the cooldown outlives any single
/// delay the policy would ever make — retrying would just spam the endpoint
/// and delay the real error — so the status is terminal (`None`).  A missing
/// header falls back to exponential backoff.
///
/// Other retried statuses (500/502/504) have no defined `Retry-After` per
/// the RFC, so a stray header on them can never suppress a legitimate retry;
/// they always back off exponentially.
///
/// The ceiling is [`MAX_BACKOFF_MS`], enforced at two points so the budget
/// test and the waited delay can never disagree: [`RetryConfig::new`] clamps
/// `max_backoff_ms` for constructor-built configs, and the gate below clamps
/// it again at compare time for hand-built ones (the fields are `pub`, so a
/// struct literal bypasses the constructor).  `wait_before_retry`'s cap
/// remains the backstop for the exponential-backoff path (whose delay is
/// bounded by `max_backoff_ms`, not by this gate).
fn retry_decision(
    status: u16,
    retry_after_secs: Option<u64>,
    attempt: u32,
    retry: &RetryConfig,
) -> Option<Duration> {
    if !matches!(status, 429 | 503) {
        return Some(backoff_duration(attempt, retry));
    }
    let Some(secs) = retry_after_secs else {
        return Some(backoff_duration(attempt, retry));
    };
    let wait = Duration::from_secs(secs);
    // Clamp the ceiling to the hard bound at compare time: `max_backoff_ms`
    // is pub, so a struct literal can bypass the `RetryConfig::new` clamp.
    // Without this, a `u64::MAX` ceiling would make the gate tautological
    // again — every cooldown "fits" — voiding the fail-fast property (the
    // whole point of the gate) for hand-built configs.  The `Duration`
    // comparison is precise: a sub-second ceiling is not truncated to whole
    // seconds (max_backoff_ms may be < 1000).
    let ceiling = Duration::from_millis(retry.max_backoff_ms.min(MAX_BACKOFF_MS));
    (wait <= ceiling).then_some(wait)
}

/// Parse a `Retry-After` header value into seconds, per RFC 7231 §7.1.3.
/// Handles both allowed forms:
///
/// - delta-seconds (`Retry-After: 120`) — returned verbatim;
/// - HTTP-date (`Retry-After: Wed, 21 Oct 2015 07:28:00 GMT`) — the
///   remaining whole seconds from now, floored at 0 (a date already in the
///   past means "retry now").
///
/// Returns `None` when the value is absent or in neither form — a malformed
/// header is never guessed at, and the caller falls back to exponential
/// backoff.
#[must_use]
pub fn parse_retry_after_secs(value: Option<&str>) -> Option<u64> {
    let value = value?.trim();
    if value.is_empty() {
        return None;
    }
    // delta-seconds: the common, exact form (also rejects date strings, so
    // trying this first is safe).
    if let Ok(secs) = value.parse::<u64>() {
        return Some(secs);
    }
    // HTTP-date: the strict IMF-fixdate form servers are required to send
    // (chrono's RFC 2822 parser covers it).  The delta is measured against
    // our own clock; a value in the future beyond the retry budget is
    // handled by `retry_decision`, and one in the past retries immediately.
    let when = chrono::DateTime::parse_from_rfc2822(value).ok()?;
    let delta = when
        .with_timezone(&chrono::Utc)
        .signed_duration_since(chrono::Utc::now())
        .num_seconds();
    Some(delta.max(0).cast_unsigned())
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
/// Returns `Ok(())` when not cancelled.
///
/// # Errors
///
/// Returns [`ProviderHttpError::Cancelled`] when a pending signal is found.
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
/// Returns `Ok(())` once the sleep completes uncancelled.
///
/// # Errors
///
/// Returns [`ProviderHttpError::Cancelled`] when a signal arrives during
/// the sleep.
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
/// Returns `Ok(())` when the wait completed uncancelled.
///
/// # Errors
///
/// Returns [`ProviderHttpError::Cancelled`] if the user cancels during the
/// backoff wait.
pub fn wait_before_retry(
    attempt: u32,
    max_attempts: u32,
    delay: Duration,
    on_retry: &mut Option<RetryCallback>,
    cancel_rx: Option<&crossbeam_channel::Receiver<()>>,
) -> Result<(), ProviderHttpError> {
    // Hard cap: no retry wait may ever exceed MAX_BACKOFF_MS.  RetryConfig's
    // fields are pub, so a caller can construct a config that bypasses the
    // RetryConfig::new clamp (e.g. a literal `max_backoff_ms = u64::MAX`);
    // this is the unbreakable backstop that keeps a single wait bounded even
    // then.  Everything downstream — the callback's `delay` argument and the
    // actual sleep — sees the capped value, so an unbounded wait can never
    // wedge a worker past every other timeout in the request path
    // (AttemptDeadline bounds the send, not the wait).
    let delay = delay.min(Duration::from_millis(MAX_BACKOFF_MS));
    if let Some(cb) = on_retry.as_mut() {
        cb(attempt, max_attempts, delay);
    }
    sleep_or_cancel(delay, cancel_rx)
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

/// Core retry loop: calls `send_request`, inspects the HTTP status and
/// `Retry-After` header, and retries transient errors up to
/// `retry.max_attempts` times.  Transient means transport errors, 5xx
/// (overloaded/unreachable server), and 429/503 responses whose `Retry-After`
/// fits within the backoff budget.  Everything else — 4xx client errors, and
/// 429/503 whose cooldown outlives the budget — fails immediately.
///
/// The decision deliberately never consults the response body: status codes
/// and `Retry-After` are the HTTP contract, prose is not.
///
/// The agent must be created with `http_status_as_error(false)` so that 4xx/5xx
/// arrive as `Ok(response)` rather than `Err`.  Only transport errors
/// (connection refused, timeout, etc.) produce `Err`.
/// Returns the final (non-transient) HTTP response.
///
/// # Errors
///
/// Returns [`ProviderHttpError`] on transport failure, non-retryable
/// status, exhausted attempts, or cancellation.
pub fn retry_loop<F>(
    send_request: F,
    retry: &RetryConfig,
    ctx: &mut AttemptContext,
) -> Result<ureq::http::Response<ureq::Body>, ProviderHttpError>
where
    F: Fn() -> Result<ureq::http::Response<ureq::Body>, ureq::Error>,
{
    retry_loop_impl(send_request, retry, ctx, Terminal::ConsumeIntoError)
}

/// Like [`retry_loop`], but a terminal (non-2xx, non-retryable or
/// attempts-exhausted) response is returned **unconsumed** as `Ok` instead of
/// being summarized into a [`ProviderHttpError`], so a caller whose error
/// contract must read the response body can interpret it itself.
///
/// The retry/backoff machinery, the retryable-status decision, cancellation,
/// and transport-error handling are IDENTICAL to [`retry_loop`] — only the
/// final terminal-response disposition differs. Used by the fal.ai image
/// adapter, whose contract switches on the error body's `type`/`error_type`
/// fields (a distinction `retry_loop`'s status-only summary would erase).
///
/// # Errors
///
/// Returns [`ProviderHttpError`] on transport failure (after the retry
/// budget), cancellation, or a caller-supplied send error — never for a
/// terminal HTTP status, which is handed back as `Ok`.
pub fn retry_loop_raw<F>(
    send_request: F,
    retry: &RetryConfig,
    ctx: &mut AttemptContext,
) -> Result<ureq::http::Response<ureq::Body>, ProviderHttpError>
where
    F: Fn() -> Result<ureq::http::Response<ureq::Body>, ureq::Error>,
{
    retry_loop_impl(send_request, retry, ctx, Terminal::ReturnResponse)
}

/// How [`retry_loop_impl`] disposes of a terminal (non-2xx, non-retryable or
/// attempts-exhausted) response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Terminal {
    /// Summarize the body into a [`ProviderHttpError`] (the standard path).
    ConsumeIntoError,
    /// Hand the un-read response back to the caller.
    ReturnResponse,
}

fn retry_loop_impl<F>(
    send_request: F,
    retry: &RetryConfig,
    ctx: &mut AttemptContext,
    terminal: Terminal,
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

        // The retry decision is status + Retry-After only.  `retry_decision`
        // returns None for exactly one retryable-but-terminal case: a 429/503
        // whose Retry-After outlives the backoff ceiling.  That decline is
        // logged here (at the call site, with the request context) rather
        // than inside the decision fn, so the fail-fast path is explainable.
        if is_retryable_status(status) && attempt < retry.max_attempts {
            if let Some(delay) = retry_decision(status, retry_after_secs, attempt, retry) {
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
            tracing::warn!(
                status,
                ?retry_after_secs,
                max_backoff_ms = retry.max_backoff_ms,
                "retry declined: server Retry-After outlives the backoff budget"
            );
        }

        // In raw mode the terminal response is handed back UNCONSUMED so the
        // caller can read its body and apply its own error contract; the
        // standard mode summarizes the body into the typed error below.
        if terminal == Terminal::ReturnResponse {
            return Ok(response);
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
/// - OpenAI-compatible APIs (`OpenAI`, `DeepSeek`, `OpenRouter`, Groq, …):
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
// Test-module casts: f64 comparisons for backoff math are intentionally
// approximate, and the u128->u64 millis downcast is bounded by the backoff cap.
#[expect(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
mod tests;
