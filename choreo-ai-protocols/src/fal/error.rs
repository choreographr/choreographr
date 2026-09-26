//! The shared fal.ai error mapper.
//!
//! fal returns two distinct error bodies, keyed by the JSON field shape and
//! switched on the machine-readable `type`/`error_type` value — NEVER the
//! free-text `msg`:
//!
//! 1. **Model / validation (HTTP 422):** `{"detail":[{loc,msg,type,url,…}]}`
//!    — a `content_policy_violation` entry maps to
//!    [`ProviderError::ContentFiltered`], a `no_media_generated` entry to
//!    [`ProviderError::EmptyResponse`], anything else to a 422 client error.
//! 2. **Request / infra:** a flat `{"detail":"<str>","error_type":"<snake>"}`
//!    (the same snake value also travels in the `X-Fal-Error-Type` header) —
//!    `client_cancelled`/`client_disconnected` (499) → [`ProviderError::Cancelled`];
//!    `request_timeout`/`startup_timeout` (504) and `runner_*`/`internal_error`
//!    → [`ProviderError::ServerError`]; `bad_request` (400) →
//!    [`ProviderError::ClientError`].
//!
//! Remaining statuses fall back to the HTTP contract: 401 → `Unauthorized`,
//! 429 → `RateLimited` (honored via the shared retry budget), other 5xx →
//! `ServerError`, other 4xx → `ClientError`.
//!
//! A THIRD error site exists on the video queue path: a `COMPLETED` status
//! body can itself carry `error` + `error_type` for a job that failed
//! server-side (there is no HTTP error in that case — the status GET was a
//! 2xx). [`classify_error_type`] is exposed so that site routes through the
//! exact same snake-case classifier as the flat HTTP body above.

use crate::shared::ProviderError;
use std::io;

/// One entry of fal's model/validation error array (the HTTP 422 shape).
///
/// `type` is the machine-readable classifier the mapper switches on (never
/// `msg`); `msg` is the human-readable sentence surfaced in the error detail.
#[derive(Debug, serde::Deserialize)]
struct FalValidationEntry {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    msg: String,
}

/// Parse fal's model/validation error body (`{"detail":[{type,msg,…}]}`).
///
/// Returns `None` when `body` is not that array shape (so the caller can try
/// the flat request-error shape and then the status fallback). The decisions
/// are keyed on the entry `type` — `msg` is prose and is never matched.
fn parse_model_validation_error(status: u16, body: &str) -> Option<ProviderError> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    // `detail` must be an ARRAY for this shape; a string `detail` belongs to
    // the flat request-error shape handled below.
    let entries: Vec<FalValidationEntry> =
        serde_json::from_value(value.get("detail")?.clone()).ok()?;
    // Content-filter and no-media are carried by `type`, not the HTTP status
    // (every validation failure is a 422), so they must be checked BEFORE the
    // generic mapping.
    if let Some(entry) = entries
        .iter()
        .find(|e| e.kind == "content_policy_violation")
    {
        let detail = if entry.msg.is_empty() {
            entry.kind.clone()
        } else {
            entry.msg.clone()
        };
        return Some(ProviderError::ContentFiltered { detail });
    }
    if entries.iter().any(|e| e.kind == "no_media_generated") {
        return Some(ProviderError::EmptyResponse);
    }
    // Any other validation failure is an ordinary client error; join every
    // entry so a multi-field validation error is fully described.
    let detail = entries
        .iter()
        .map(|e| format!("{}: {}", e.kind, e.msg))
        .collect::<Vec<_>>()
        .join("; ");
    Some(ProviderError::ClientError {
        status,
        detail: if detail.is_empty() {
            body.to_string()
        } else {
            detail
        },
    })
}

/// Classify a fal snake-case `error_type` into a [`ProviderError`], or `None`
/// when the value is one the caller should map by HTTP status instead.
///
/// This is the single classifier behind BOTH the flat request/infra HTTP body
/// and the video queue's third error site (a `COMPLETED` status carrying
/// `error`/`error_type`). `status` is threaded into the produced variant for
/// the status-carrying cases; the queue path passes its own response status.
pub(crate) fn classify_error_type(
    error_type: &str,
    status: u16,
    detail: &str,
) -> Option<ProviderError> {
    match error_type {
        // The client went away mid-request (499): terminal, not retryable.
        "client_cancelled" | "client_disconnected" => Some(ProviderError::Cancelled),
        // A timeout is infrastructural — the shared retry budget already
        // exhausted its attempts by the time this is mapped.
        "request_timeout" | "startup_timeout" => Some(ProviderError::ServerError {
            status,
            detail: detail.to_string(),
        }),
        // Worker/runner failures and a generic internal error are the
        // server's fault, not the request's.
        _ if error_type == "internal_error" || error_type.starts_with("runner_") => {
            Some(ProviderError::ServerError {
                status,
                detail: detail.to_string(),
            })
        }
        "bad_request" => Some(ProviderError::ClientError {
            status,
            detail: detail.to_string(),
        }),
        _ => None,
    }
}

/// Parse fal's flat request/infra error body
/// (`{"detail":"<str>","error_type":"<snake>"}`), switching on `error_type`.
///
/// `header_type` (the `X-Fal-Error-Type` header) backstops a body that omits
/// the field. Returns `None` when the body is not this shape OR the
/// `error_type` is one the caller should map by HTTP status instead.
fn parse_request_error(
    status: u16,
    body: &str,
    header_type: Option<&str>,
) -> Option<ProviderError> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    let body_type = value.get("error_type").and_then(serde_json::Value::as_str);
    let error_type = body_type.or(header_type)?;
    // Human detail: the flat body's own `detail` string when present, else the
    // error_type itself.
    let detail = value
        .get("detail")
        .and_then(serde_json::Value::as_str)
        .map_or_else(|| error_type.to_string(), str::to_owned);
    classify_error_type(error_type, status, &detail)
}

/// A human-readable detail for a status-fallback error: the flat body's
/// `detail` string when present, else the trimmed raw body, else a generic
/// phrase.
fn status_fallback_detail(body: &str) -> String {
    let trimmed = body.trim();
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed)
        && let Some(detail) = value.get("detail").and_then(serde_json::Value::as_str)
        && !detail.is_empty()
    {
        return detail.to_string();
    }
    if trimmed.is_empty() {
        "request failed".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Read the classifier header, the `Retry-After` budget input, and the body
/// from a non-2xx fal response and apply the two-shape mapping.
///
/// This is the single entry point both fal adapters use for a terminal HTTP
/// error (the synchronous image POST and the queue's submit/status/result
/// calls) so their error handling cannot drift — callers only decide WHICH
/// response is an error and convert the returned [`ProviderError`] to an
/// [`crate::shared::InferenceError`]. `context` labels the log line (e.g.
/// `"image generation"`, `"video request"`).
pub(crate) fn fal_error_from_response(
    response: ureq::http::Response<ureq::Body>,
    context: &'static str,
) -> ProviderError {
    let status = response.status().as_u16();
    let header_type = response
        .headers()
        .get("x-fal-error-type")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let retry_after_secs = crate::retry::parse_retry_after_secs(
        response
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok()),
    );
    // A body read failure is treated as empty — the status/shape fallbacks
    // still produce a usable error rather than masking the HTTP failure.
    let body_text = response.into_body().read_to_string().unwrap_or_default();
    tracing::warn!(
        status,
        error_type = header_type.as_deref().unwrap_or(""),
        context,
        "fal request failed"
    );
    fal_error(status, header_type.as_deref(), retry_after_secs, &body_text)
}

/// Map a non-2xx fal response to a provider error, applying the documented
/// precedence: the 422 validation array shape first, then the flat
/// request-error shape (body `error_type`, else the header), then the HTTP
/// status fallback. `retry_after_secs` is read from the response header by the
/// caller so a 429 surfaces the server's stated cooldown.
pub(crate) fn fal_error(
    status: u16,
    header_type: Option<&str>,
    retry_after_secs: Option<u64>,
    body: &str,
) -> ProviderError {
    if let Some(err) = parse_model_validation_error(status, body) {
        return err;
    }
    if let Some(err) = parse_request_error(status, body, header_type) {
        return err;
    }
    let detail = status_fallback_detail(body);
    match status {
        401 => ProviderError::Unauthorized { status, detail },
        429 => ProviderError::RateLimited {
            status,
            retry_after_secs,
            detail,
        },
        s if (500..600).contains(&s) => ProviderError::ServerError { status: s, detail },
        s if (400..500).contains(&s) => ProviderError::ClientError { status: s, detail },
        _ => ProviderError::Io(io::Error::other(detail)),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        classify_error_type, fal_error, parse_model_validation_error, parse_request_error,
    };
    use crate::shared::ProviderError;

    // ── model/validation (422 array) parser ──────────────────────────────

    #[test]
    fn validation_content_policy_maps_to_content_filtered() {
        let body =
            r#"{"detail":[{"loc":["body"],"msg":"flagged","type":"content_policy_violation"}]}"#;
        match parse_model_validation_error(422, body) {
            Some(ProviderError::ContentFiltered { detail }) => assert_eq!(detail, "flagged"),
            other => panic!("expected ContentFiltered, got {other:?}"),
        }
    }

    #[test]
    fn validation_no_media_maps_to_empty_response() {
        let body = r#"{"detail":[{"loc":[],"msg":"no image","type":"no_media_generated"}]}"#;
        assert!(matches!(
            parse_model_validation_error(422, body),
            Some(ProviderError::EmptyResponse)
        ));
    }

    #[test]
    fn validation_other_type_maps_to_client_error_with_type_and_msg() {
        let body =
            r#"{"detail":[{"loc":["body","prompt"],"msg":"too long","type":"string_too_long"}]}"#;
        match parse_model_validation_error(422, body) {
            Some(ProviderError::ClientError { status, detail }) => {
                assert_eq!(status, 422);
                assert!(detail.contains("string_too_long"), "{detail}");
                assert!(detail.contains("too long"), "{detail}");
            }
            other => panic!("expected ClientError, got {other:?}"),
        }
    }

    #[test]
    fn validation_parser_rejects_the_flat_shape() {
        // A flat `detail` string is not the array shape → None so the caller
        // falls through to the request-error parser.
        assert!(parse_model_validation_error(400, r#"{"detail":"boom"}"#).is_none());
        assert!(parse_model_validation_error(400, "not json").is_none());
    }

    // ── flat request/infra error parser ──────────────────────────────────

    #[test]
    fn request_error_cancelled_types_map_to_cancelled() {
        for kind in ["client_cancelled", "client_disconnected"] {
            let body = format!(r#"{{"detail":"gone","error_type":"{kind}"}}"#);
            assert!(
                matches!(
                    parse_request_error(499, &body, None),
                    Some(ProviderError::Cancelled)
                ),
                "{kind} must map to Cancelled"
            );
        }
    }

    #[test]
    fn request_error_timeout_and_runner_types_map_to_server_error() {
        for kind in [
            "request_timeout",
            "startup_timeout",
            "runner_error",
            "runner_crash",
            "internal_error",
        ] {
            let body = format!(r#"{{"detail":"upstream","error_type":"{kind}"}}"#);
            match parse_request_error(504, &body, None) {
                Some(ProviderError::ServerError { detail, .. }) => assert_eq!(detail, "upstream"),
                other => panic!("{kind} expected ServerError, got {other:?}"),
            }
        }
    }

    #[test]
    fn request_error_bad_request_maps_to_client_error() {
        let body = r#"{"detail":"malformed","error_type":"bad_request"}"#;
        match parse_request_error(400, body, None) {
            Some(ProviderError::ClientError { status, detail }) => {
                assert_eq!(status, 400);
                assert_eq!(detail, "malformed");
            }
            other => panic!("expected ClientError, got {other:?}"),
        }
    }

    #[test]
    fn request_error_header_type_backstops_a_body_without_it() {
        // Body has no error_type; the X-Fal-Error-Type header supplies it.
        let body = r#"{"detail":"disconnected"}"#;
        assert!(matches!(
            parse_request_error(499, body, Some("client_disconnected")),
            Some(ProviderError::Cancelled)
        ));
        // Neither body nor header → None (fall through to status).
        assert!(parse_request_error(499, body, None).is_none());
    }

    #[test]
    fn request_error_unknown_type_is_not_this_shape() {
        // An unknown error_type is left for the status fallback.
        assert!(parse_request_error(500, r#"{"error_type":"mystery"}"#, None).is_none());
    }

    // ── full error mapping ───────────────────────────────────────────────

    #[test]
    fn fal_error_status_fallbacks() {
        // 401 → Unauthorized (no error_type matches).
        assert!(matches!(
            fal_error(401, None, None, r#"{"detail":"invalid key"}"#),
            ProviderError::Unauthorized { status: 401, .. }
        ));
        // 429 → RateLimited carrying Retry-After.
        match fal_error(429, None, Some(30), r#"{"detail":"slow down"}"#) {
            ProviderError::RateLimited {
                status,
                retry_after_secs,
                detail,
            } => {
                assert_eq!(status, 429);
                assert_eq!(retry_after_secs, Some(30));
                assert_eq!(detail, "slow down");
            }
            other => panic!("expected RateLimited, got {other:?}"),
        }
        // Other 5xx → ServerError, other 4xx → ClientError.
        assert!(matches!(
            fal_error(503, None, None, ""),
            ProviderError::ServerError { status: 503, .. }
        ));
        assert!(matches!(
            fal_error(418, None, None, ""),
            ProviderError::ClientError { status: 418, .. }
        ));
    }

    #[test]
    fn fal_error_prefers_the_validation_shape_over_status() {
        // A 422 content_policy_violation must NOT be flattened to a plain
        // ClientError by the status fallback.
        let body = r#"{"detail":[{"type":"content_policy_violation","msg":"blocked"}]}"#;
        assert!(matches!(
            fal_error(422, None, None, body),
            ProviderError::ContentFiltered { .. }
        ));
    }

    #[test]
    fn classify_error_type_matches_the_http_body_classifier() {
        // The bytes the flat body parser produces must be identical to a direct
        // classify call — the one-classifier invariant the doc pins.
        let via_body =
            parse_request_error(400, r#"{"detail":"x","error_type":"bad_request"}"#, None);
        let direct = classify_error_type("bad_request", 400, "x");
        assert!(matches!(
            (via_body, direct),
            (
                Some(ProviderError::ClientError { .. }),
                Some(ProviderError::ClientError { .. })
            )
        ));
    }
}
