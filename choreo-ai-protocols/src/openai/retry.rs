pub(crate) use crate::retry::AttemptContext;
pub(crate) use crate::retry::AttemptDeadline;
pub use crate::retry::{RetryCallback, RetryConfig};

use super::{OpenAiError, ServiceConfig};
use crate::retry;

pub(crate) fn retry_config_from_config(config: &ServiceConfig) -> RetryConfig {
    RetryConfig::new(
        config.retry_max_attempts,
        config.retry_initial_backoff_ms,
        config.retry_max_backoff_ms,
    )
}

/// The eight parameters are all per-call context — no struct threads them all,
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
) -> Result<ureq::http::Response<ureq::Body>, OpenAiError> {
    let auth_header = zeroize::Zeroizing::new(format!("Bearer {}", api_key.trim()));
    // Provider-specific extra headers (the opencode gateway routing headers)
    // are derived from the config + route here, so every call site stays a
    // plain "send with retry" and cannot forget or duplicate them. Empty for
    // non-gateway providers, so the closure loop is a no-op there.
    let headers = route
        .map(|(session_id, request_id)| {
            crate::shared::opencode_gateway_headers(&config.provider_slug, session_id, request_id)
        })
        .unwrap_or_default();
    // The closure captures `auth_header` and `headers` by reference (it stays
    // `Fn`); the Zeroizing wrapper ensures the temporary `Bearer …` string is
    // wiped when it goes out of scope.
    retry::retry_loop(
        || {
            let mut request = agent
                .post(url)
                .header("Authorization", auth_header.as_str());
            for (name, value) in &headers {
                request = request.header(*name, value.as_str());
            }
            request.send_json(body.clone())
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
