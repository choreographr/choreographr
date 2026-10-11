pub(crate) use crate::retry::AttemptContext;
pub(crate) use crate::retry::AttemptDeadline;
pub use crate::retry::{RequestAcceptedCallback, RetryCallback, RetryConfig};

use super::{OpenAiError, ServiceConfig};
use crate::retry;

use std::io;

pub(crate) fn retry_config_from_config(config: &ServiceConfig) -> RetryConfig {
    RetryConfig::new(
        config.retry_max_attempts,
        config.retry_initial_backoff_ms,
        config.retry_max_backoff_ms,
    )
}

/// The nine parameters are all per-call context — no struct threads them all,
/// so an allowlist beat a parameter object here.
#[expect(clippy::too_many_arguments)]
pub(crate) fn retry_send(
    agent: &ureq::Agent,
    url: &str,
    api_key: &str,
    body: &serde_json::Value,
    config: &ServiceConfig,
    retry_cfg: &RetryConfig,
    ctx: &mut crate::retry::AttemptContext,
    // Gateway routing identity for the opencode zen/go providers: the turn's
    // (session_id, request_id). `None` on paths without a session (model
    // listing, prompt helpers) — those then route by the gateway's workspace
    // id / IP fallback, which is fine for auxiliary traffic.
    route: Option<(&str, &str)>,
    // Fires once when the request payload has been fully uploaded (see
    // [`RequestAcceptedCallback`]); `None` on paths that do not report it.
    on_accepted: Option<RequestAcceptedCallback>,
) -> Result<ureq::http::Response<ureq::Body>, OpenAiError> {
    let auth_header = zeroize::Zeroizing::new(format!("Bearer {}", api_key.trim()));
    // Provider-specific extra headers (the opencode gateway routing headers)
    // are derived from the config + route here, so every call site stays a
    // plain "send with retry" and cannot forget or duplicate them. Empty for
    // non-gateway providers, so the closure loop is a no-op there.
    let headers = route.map_or_default(|(session_id, request_id)| {
        crate::shared::opencode_gateway_headers(&config.provider_slug, session_id, request_id)
    });
    // Serialize once and shape the send: a compact body, an explicit
    // `Content-Length` (so ureq sends length-delimited rather than chunked and
    // the upload hook fires on the last payload byte), and the upload hook.
    let hooked = crate::retry::HookedJsonBody::new(body, on_accepted)
        .map_err(|e| OpenAiError::Io(io::Error::other(e)))?;
    let content_length = hooked.len().to_string();
    // The closure captures `auth_header`, `headers`, `hooked`, and
    // `content_length` by reference (it stays `Fn`); each attempt clones the
    // payload and the `Arc` upload hook. The Zeroizing wrapper ensures the
    // temporary `Bearer …` string is wiped when it goes out of scope.
    retry::retry_loop(
        || {
            let mut request = agent
                .post(url)
                .header("Authorization", auth_header.as_str())
                .header("Content-Type", "application/json; charset=utf-8")
                .header("Content-Length", content_length.as_str());
            for (name, value) in &headers {
                request = request.header(*name, value.as_str());
            }
            request.send(hooked.send_body())
        },
        retry_cfg,
        ctx,
    )
    .map_err(OpenAiError::from)
}

pub(crate) fn retry_send_get(
    agent: &ureq::Agent,
    url: &str,
    api_key: &str,
    _config: &ServiceConfig,
    retry_cfg: &RetryConfig,
    ctx: &mut crate::retry::AttemptContext,
) -> Result<ureq::http::Response<ureq::Body>, OpenAiError> {
    let auth_header = zeroize::Zeroizing::new(format!("Bearer {}", api_key.trim()));
    // Model listing carries no session identity, so no opencode gateway
    // routing headers are sent here — the gateway does not route model
    // listings to inference upstreams anyway.
    let headers: Vec<(&'static str, String)> = Vec::new();
    // The closure captures `auth_header` and `headers` by reference (it stays
    // `Fn`); the Zeroizing wrapper ensures the temporary `Bearer …` string is
    // wiped when it goes out of scope.
    retry::retry_loop(
        || {
            let mut request = agent.get(url).header("Authorization", auth_header.as_str());
            for (name, value) in &headers {
                request = request.header(*name, value.as_str());
            }
            request.call()
        },
        retry_cfg,
        ctx,
    )
    .map_err(OpenAiError::from)
}
