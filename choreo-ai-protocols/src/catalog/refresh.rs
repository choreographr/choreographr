//! models.dev runtime refresh — conditional GET with etag support.
//!
//! This module owns the *fetch* half of the S4 runtime refresh: it is the
//! only place that talks to models.dev, and it lives in `choreo-ai-protocols`
//! because that crate already owns `ureq` + the normalization pipeline
//! ([`crate::catalog::normalize_modelsdev`]). The daemon's maintenance thread
//! calls [`fetch_modelsdev`] with the cached etag and hands the result to the
//! daemon command loop, which merges overlays, swaps the [`PROVIDER_CATALOG`],
//! and persists the cache — so the fetch itself stays out of the command loop
//! (it can block for the whole timeout) and the pure normalization/merge stays
//! unit-testable here.
//!
//! models.dev serves `ETag` + `must-revalidate`, so a conditional GET with
//! `If-None-Match` returns `304 Not Modified` when the cache is current. A
//! `--force` refresh sends `Cache-Control: no-cache` and skips the etag,
//! forcing a fresh 200.

use std::io::Read;
use std::time::Duration;

use thiserror::Error;
use tracing::warn;

/// The models.dev API snapshot endpoint. Serves the full provider/model
/// catalog as a JSON object keyed by provider slug.
pub const MODELSDEV_URL: &str = "https://models.dev/api.json";

/// Total wall-clock budget for one refresh fetch (DNS → connect → headers →
/// body). models.dev serves a multi-MB JSON body; 30s is generous for that.
const FETCH_TIMEOUT: Duration = Duration::from_secs(30);

/// Hard cap on the response body. models.dev's snapshot is ~2–3 MB; the cap
/// bounds memory if the endpoint ever starts serving something pathological.
const MAX_BODY_BYTES: u64 = 32 * 1024 * 1024;

/// Result of a conditional GET against models.dev.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefreshOutcome {
    /// `304 Not Modified` — the cached catalog is current.
    NotModified,
    /// `200 OK` — the remote changed. `etag` is the new entity tag (absent
    /// if the server did not send one) and must replace the cached value.
    Fetched { json: String, etag: Option<String> },
}

/// Structured fetch errors — never a panic. The maintenance thread logs them
/// and retries later; a `/refresh-models` request surfaces them to the client
/// as `ModelsRefreshFailed`.
#[derive(Debug, Error)]
pub enum RefreshError {
    /// ureq transport failure (DNS, connect, TLS, timeout, …).
    #[error("network error: {0}")]
    Network(String),
    /// Non-200/304 HTTP status (the agent is configured with
    /// `http_status_as_error(false)`, so these arrive as responses, not
    /// errors).
    #[error("models.dev returned HTTP {status}")]
    HttpStatus { status: u16 },
    /// Failed to read the response body.
    #[error("failed to read response body: {0}")]
    Body(#[from] std::io::Error),
    /// The body is not valid UTF-8 (it should be JSON).
    #[error("response body is not valid UTF-8")]
    InvalidUtf8,
    /// The body exceeded the cap.
    #[error("response body exceeded the {MAX_BODY_BYTES}-byte cap")]
    BodyTooLarge,
}

/// Fetch the models.dev snapshot with a conditional GET.
///
/// * `current_etag` — the cached etag, sent as `If-None-Match` (skipped when
///   `None`, and always skipped when `force` is set).
/// * `force` — send `Cache-Control: no-cache` and bypass the etag so the
///   server returns a fresh 200 even if nothing changed.
///
/// Returns [`RefreshOutcome::NotModified`] on 304, [`RefreshOutcome::Fetched`]
/// on 200, and a structured [`RefreshError`] on transport/status/body
/// problems. Never panics.
pub fn fetch_modelsdev(
    current_etag: Option<&str>,
    force: bool,
) -> Result<RefreshOutcome, RefreshError> {
    let agent = ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .timeout_global(Some(FETCH_TIMEOUT))
            // Status errors are handled explicitly below: 304 is a *normal*
            // outcome here, not an error.
            .http_status_as_error(false)
            .build(),
    );

    let mut request = agent.get(MODELSDEV_URL);
    if force {
        // `--force`: tell any intermediate cache to revalidate too, and do
        // not send If-None-Match so the origin must produce a fresh body.
        request = request.header("Cache-Control", "no-cache");
    } else if should_send_if_none_match(current_etag) {
        request = request.header("If-None-Match", current_etag.unwrap_or_default());
    }

    let response = request
        .call()
        .map_err(|e| RefreshError::Network(e.to_string()))?;
    let status = response.status();

    use ureq::http::StatusCode;
    match status {
        StatusCode::NOT_MODIFIED => Ok(RefreshOutcome::NotModified),
        StatusCode::OK => {
            // Capture the etag BEFORE consuming the body (headers are borrowed
            // from the response).
            let etag = response
                .headers()
                .get("etag")
                .and_then(|v| v.to_str().ok())
                .map(str::to_string);
            let json = read_body(response)?;
            if json.is_empty() {
                // A 200 with an empty body would normalize to an empty
                // catalog; treat it as a fetch failure so the caller keeps
                // the current catalog instead of swapping in nothing.
                warn!("models.dev returned an empty body");
                return Err(RefreshError::Body(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "models.dev returned an empty body",
                )));
            }
            Ok(RefreshOutcome::Fetched { json, etag })
        }
        other => Err(RefreshError::HttpStatus {
            status: other.as_u16(),
        }),
    }
}

/// Whether to attach an `If-None-Match` header for the given cached etag.
/// An empty etag (e.g. a truncated sidecar) must NOT be sent — an empty
/// entity tag is never valid and would break the conditional GET. Extracted
/// as a pure function so the header logic is unit-testable without a network
/// round trip.
fn should_send_if_none_match(etag: Option<&str>) -> bool {
    matches!(etag, Some(e) if !e.is_empty())
}

/// Read the full response body into a `String`, bounded by [`MAX_BODY_BYTES`].
fn read_body(response: ureq::http::Response<ureq::Body>) -> Result<String, RefreshError> {
    let mut bytes = Vec::with_capacity(256 * 1024);
    let mut reader = response.into_body().into_reader().take(MAX_BODY_BYTES);
    reader.read_to_end(&mut bytes)?;
    if bytes.len() as u64 >= MAX_BODY_BYTES {
        // The take() cap was hit (or exactly reached): the body is
        // pathological — refuse it rather than feed a truncated catalog into
        // normalization.
        return Err(RefreshError::BodyTooLarge);
    }
    String::from_utf8(bytes).map_err(|_| RefreshError::InvalidUtf8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_is_the_models_dev_snapshot_endpoint() {
        // The endpoint is load-bearing: a typo here would silently refresh
        // against the wrong host forever. Pin it.
        assert_eq!(MODELSDEV_URL, "https://models.dev/api.json");
    }

    #[test]
    fn etag_guard_skips_empty_etags() {
        // The maintenance thread may hold an empty etag (e.g. a sidecar that
        // was truncated). Sending `If-None-Match: ""` would be invalid — the
        // guard must fall back to a plain GET for empty/absent etags.
        assert!(!should_send_if_none_match(None));
        assert!(!should_send_if_none_match(Some("")));
        assert!(should_send_if_none_match(Some("\"abc123\"")));
    }
}
