//! The Streamable HTTP transport for the `rmcp`-backed engine.
//!
//! Builds the reqwest client and rmcp `StreamableHttpClientTransport`, retries
//! transient connect failures with a bounded backoff, recovers the HTTP status
//! from a failed connect or service call, and probes for the removed 2024-11-05
//! HTTP+SSE transport so its rejection carries a clear message rather than an
//! opaque failure.

use super::{MAX_SSE_EVENT_BYTES, SSE_MAX_RECONNECTS};
use crate::config::{McpServerConfig, McpTransport};
use crate::error::McpError;
use reqwest::header::{HeaderName, HeaderValue};
use rmcp::service::ClientInitializeError;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::common::client_side_sse::SseRetryPolicy;
use rmcp::transport::streamable_http_client::{
    StreamableHttpClientTransportConfig, StreamableHttpError,
};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

/// Deadline for the deprecated-transport probe that runs only after an HTTP
/// connect has already failed, so it never adds latency to a healthy connect.
const LEGACY_SSE_PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// Build the reqwest client used by the Streamable HTTP transport.
///
/// Idle pooling is disabled and redirects are off (matching rmcp's own default
/// client): reuse can stall on Linux's delayed ACK, and a redirect would replay
/// user-supplied headers to a new host, defeating the per-server header
/// contract. The connect timeout uses the server's request timeout so a black-
/// holed endpoint cannot hang the connect phase.
pub(super) fn http_client(config: &McpServerConfig) -> Result<reqwest::Client, McpError> {
    reqwest::Client::builder()
        .pool_max_idle_per_host(0)
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(config.request_timeout())
        .build()
        .map_err(|e| McpError::InitializeFailed(format!("failed to build HTTP client: {e}")))
}

/// Build the Streamable HTTP transport for `config` over `client`.
///
/// Config-supplied headers are validated up front so a bad name or a reserved
/// header fails the connect with a clear message rather than at the first
/// request. rmcp generates `Mcp-*` routing headers from the request body; user
/// headers ride alongside them (`MCP-Protocol-Version` is also generated, so it
/// is not user-settable).
pub(super) fn build_http_transport(
    config: &McpServerConfig,
    client: reqwest::Client,
) -> Result<StreamableHttpClientTransport<reqwest::Client>, McpError> {
    let McpTransport::Http { url, headers } = &config.transport else {
        return Err(McpError::ProtocolError(
            "build_http_transport called for a non-HTTP server".into(),
        ));
    };
    let mut custom: HashMap<HeaderName, HeaderValue> = HashMap::new();
    for (name, value) in headers {
        let header = HeaderName::from_bytes(name.as_bytes()).map_err(|e| {
            McpError::ProtocolError(format!("invalid HTTP header name {name:?}: {e}"))
        })?;
        if is_reserved_header(&header) {
            return Err(McpError::ProtocolError(format!(
                "HTTP header {name:?} is reserved by the MCP transport"
            )));
        }
        let value = HeaderValue::from_str(value).map_err(|e| {
            McpError::ProtocolError(format!("invalid HTTP header value for {name:?}: {e}"))
        })?;
        custom.insert(header, value);
    }
    let mut http_config = StreamableHttpClientTransportConfig::with_uri(url.as_str())
        .custom_headers(custom)
        .max_sse_event_size(MAX_SSE_EVENT_BYTES);
    // Bound the SSE reconnect policy. rmcp's default retries a dropped server
    // stream forever, which would hammer a permanently dead endpoint; this caps
    // the attempts with a bounded exponential backoff so the failure surfaces.
    // No idle-read timeout is set: rmcp exposes no read-idle hook, and the
    // per-request deadline (`PeerRequestOptions::with_timeout`) already bounds
    // every ordinary request, while a `subscriptions/listen` stream is
    // re-established by the dispatcher's own restart policy.
    http_config.retry_config = Arc::new(BoundedSseRetry {
        max_attempts: SSE_MAX_RECONNECTS,
        base: crate::retry::BASE_BACKOFF,
        ceiling: crate::retry::MAX_BACKOFF,
    });
    Ok(StreamableHttpClientTransport::with_client(
        client,
        http_config,
    ))
}

/// A bounded SSE stream-reconnect policy for the Streamable HTTP transport.
///
/// Implements rmcp's [`SseRetryPolicy`] with a finite attempt budget and a
/// capped exponential backoff (the same shape as [`crate::retry`]'s connect
/// policy), replacing the crate default of unbounded reconnection. rmcp's own
/// policy types are `#[non_exhaustive]` and so cannot be constructed here.
#[derive(Debug)]
struct BoundedSseRetry {
    /// Maximum reconnect attempts before the stream is abandoned.
    max_attempts: usize,
    /// First-retry delay.
    base: Duration,
    /// Ceiling on a single reconnect delay.
    ceiling: Duration,
}

impl SseRetryPolicy for BoundedSseRetry {
    fn retry(&self, current_times: usize) -> Option<Duration> {
        if current_times >= self.max_attempts {
            return None;
        }
        // rmcp passes a 0-based attempt count; the shared formula is 1-based.
        // A hostile value cannot overflow: `saturating_add` pins it and the
        // shared shift clamp bounds the left shift.
        let attempt = u32::try_from(current_times)
            .unwrap_or(u32::MAX)
            .saturating_add(1);
        Some(crate::retry::backoff(attempt, self.base, self.ceiling))
    }
}

/// Whether a config-supplied header collides with one the transport owns.
///
/// This mirrors the reserved set rmcp rejects at request time, so a config
/// mistake surfaces at connect rather than mid-session. `MCP-Protocol-Version`
/// is intentionally *not* reserved here: rmcp allows and injects it.
fn is_reserved_header(name: &HeaderName) -> bool {
    let name = name.as_str();
    ["accept", "mcp-session-id", "last-event-id"]
        .iter()
        .any(|reserved| name.eq_ignore_ascii_case(reserved))
}

/// Whether a failed connect is worth retrying under [`crate::retry`].
///
/// The HTTP status is recovered by [`connect_error_status`].
pub(super) fn retryable_connect(error: &ClientInitializeError) -> bool {
    connect_error_status(error).is_some_and(crate::retry::is_retryable_status)
}

/// Recover the HTTP status from a failed connect, if it carries one.
///
/// Starts the shared source-chain walk at the transport error's root: a bare
/// `reqwest::Error` carries a status directly, whereas a rejected POST surfaces
/// the status inside rmcp's `"HTTP <status>: <body>"` message.
pub(super) fn connect_error_status(error: &ClientInitializeError) -> Option<u16> {
    let root: &dyn std::error::Error = match error {
        ClientInitializeError::TransportError { error, .. } => error.error.as_ref(),
        _ => return None,
    };
    status_from_chain(root)
}

/// Walk `root`'s `source()` chain looking for rmcp's `StreamableHttpError`,
/// returning the HTTP status it carries, if any.
///
/// The one chain walk shared by the connect-time ([`connect_error_status`]) and
/// service-time ([`service_error_status`](super::convert::service_error_status))
/// status recovery.
pub(super) fn status_from_chain(root: &(dyn std::error::Error + 'static)) -> Option<u16> {
    let mut current = Some(root);
    while let Some(err) = current {
        if let Some(http) = err.downcast_ref::<StreamableHttpError<reqwest::Error>>()
            && let Some(status) = http_error_status(http)
        {
            return Some(status);
        }
        current = err.source();
    }
    None
}

/// Extract the HTTP status from an rmcp Streamable HTTP error, when it carries
/// one.
fn http_error_status(error: &StreamableHttpError<reqwest::Error>) -> Option<u16> {
    match error {
        StreamableHttpError::Client(e) => e.status().map(|s| s.as_u16()),
        StreamableHttpError::UnexpectedServerResponse(message) => parse_http_status(message),
        // rmcp models a 401 challenge and an insufficient-scope 403 as their
        // own variants (no status number); map them to the codes they represent
        // so the caller can turn either into an actionable `AuthRequired`.
        StreamableHttpError::AuthRequired(_) => Some(401),
        StreamableHttpError::InsufficientScope(_) => Some(403),
        _ => None,
    }
}

/// Parse the status out of rmcp's `"HTTP <Status>: <body>"` rejection message.
///
/// A rejected POST that is not a JSON-RPC error is surfaced as
/// `"HTTP <status>: <body>"`, where `<status>` is `reqwest::StatusCode`'s
/// Display (`"503 Service Unavailable"`), so the code is the first token.
///
/// This depends on the exact message format of rmcp's
/// `UnexpectedServerResponse` variant, so it is a BEST-EFFORT fallback only:
/// whenever rmcp exposes the status structurally (a bare `reqwest::Error`, or
/// its dedicated `AuthRequired`/`InsufficientScope` variants) the structural
/// path in [`http_error_status`] is preferred, and this parser runs only for
/// the one rejection shape rmcp does not model structurally.
fn parse_http_status(message: &str) -> Option<u16> {
    message
        .strip_prefix("HTTP ")?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

/// Best-effort detection of the deprecated 2024-11-05 HTTP+SSE transport.
///
/// That transport (removed from the specification) is a single GET whose first
/// SSE event is `endpoint`; Streamable HTTP removed the GET stream. A positive
/// result turns an otherwise opaque connect failure into a clear error. The
/// probe is request-timeout bounded, so a server that accepts the GET and stalls
/// cannot hang the connect.
pub(super) async fn looks_like_legacy_sse(client: &reqwest::Client, url: &str) -> bool {
    let Ok(response) = client
        .get(url)
        .header(reqwest::header::ACCEPT, "text/event-stream")
        .timeout(LEGACY_SSE_PROBE_TIMEOUT)
        .send()
        .await
    else {
        return false;
    };
    let is_event_stream = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|ct| ct.starts_with("text/event-stream"));
    if !is_event_stream {
        return false;
    }
    // The `endpoint` event is the first event a legacy server sends; read a
    // bounded prefix rather than the whole (never-ending) stream.
    let mut response = response;
    let mut seen = String::new();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                seen.push_str(&String::from_utf8_lossy(&chunk));
                if is_legacy_sse_event(&seen) {
                    return true;
                }
                if seen.len() > 8192 {
                    return false;
                }
            }
            // End of stream or the probe deadline elapsed before the event.
            Ok(None) | Err(_) => return is_legacy_sse_event(&seen),
        }
    }
}

/// Whether the accumulated SSE text contains the legacy `endpoint` event.
fn is_legacy_sse_event(text: &str) -> bool {
    text.lines().any(|line| {
        line.trim_start()
            .strip_prefix("event:")
            .is_some_and(|rest| rest.trim() == "endpoint")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::transport::DynamicTransportError;

    #[test]
    fn retryable_connect_backoff_is_capped() {
        // Sanity check on the policy the engine defers to: retryable for the
        // transient range, not for settled 4xx answers.
        assert!(crate::retry::is_retryable_status(503));
        assert!(!crate::retry::is_retryable_status(400));
    }

    #[test]
    fn parse_http_status_reads_rmcp_rejection_message() {
        assert_eq!(
            parse_http_status("HTTP 503 Service Unavailable: down"),
            Some(503)
        );
        assert_eq!(
            parse_http_status("HTTP 429 Too Many Requests: slow"),
            Some(429)
        );
        assert_eq!(parse_http_status("unexpected server response"), None);
    }

    #[test]
    fn legacy_sse_event_is_detected() {
        assert!(is_legacy_sse_event(
            "event: endpoint\ndata: /messages?sessionId=x\n\n"
        ));
        assert!(is_legacy_sse_event("event:endpoint\ndata: x"));
        assert!(!is_legacy_sse_event("data: {\"jsonrpc\"}\n\n"));
        assert!(!is_legacy_sse_event("event: message\ndata: {}"));
    }

    #[test]
    fn reserved_headers_are_rejected() {
        assert!(is_reserved_header(&HeaderName::from_static("accept")));
        assert!(is_reserved_header(&HeaderName::from_static(
            "mcp-session-id"
        )));
        assert!(is_reserved_header(&HeaderName::from_static(
            "last-event-id"
        )));
        assert!(!is_reserved_header(&HeaderName::from_static(
            "authorization"
        )));
    }

    #[test]
    fn retryable_connect_classifies_http_status_from_chained_error() {
        fn init_error<E: std::error::Error + Send + Sync + 'static>(e: E) -> ClientInitializeError {
            ClientInitializeError::TransportError {
                error: DynamicTransportError::from_parts(
                    "test",
                    std::any::TypeId::of::<()>(),
                    Box::new(e),
                ),
                context: "test".into(),
            }
        }

        let retryable = init_error(
            StreamableHttpError::<reqwest::Error>::UnexpectedServerResponse(
                "HTTP 503 Service Unavailable: down".into(),
            ),
        );
        assert!(retryable_connect(&retryable));

        let fatal = init_error(
            StreamableHttpError::<reqwest::Error>::UnexpectedServerResponse(
                "HTTP 400 Bad Request: bad".into(),
            ),
        );
        assert!(!retryable_connect(&fatal));

        // A non-transport initialize error is never retryable.
        assert!(!retryable_connect(&ClientInitializeError::Cancelled));
    }

    #[test]
    fn bounded_sse_retry_stops_after_the_budget() {
        let policy = BoundedSseRetry {
            max_attempts: 3,
            base: Duration::from_millis(500),
            ceiling: Duration::from_mins(1),
        };
        // The first retries back off exponentially from the base...
        assert_eq!(policy.retry(0), Some(Duration::from_millis(500)));
        assert_eq!(policy.retry(1), Some(Duration::from_secs(1)));
        assert_eq!(policy.retry(2), Some(Duration::from_secs(2)));
        // ...then the budget is spent and the stream is abandoned.
        assert_eq!(policy.retry(3), None);
    }
}
