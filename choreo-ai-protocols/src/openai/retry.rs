pub(crate) use crate::retry::AttemptContext;
pub(crate) use crate::retry::AttemptDeadline;
pub use crate::retry::{RequestUploadedCallback, RetryCallback, RetryConfig};

use super::{OpenAiError, ServiceConfig};
use crate::retry;

pub(crate) fn retry_config_from_config(config: &ServiceConfig) -> RetryConfig {
    RetryConfig::new(
        config.retry_max_attempts,
        config.retry_initial_backoff_ms,
        config.retry_max_backoff_ms,
    )
}

/// One retried JSON send: the transport target, the serialized request body, the
/// provider/retry config, and the per-call extras (the gateway route and the
/// upload hook).  Bundled into a struct so `retry_send` reads as
/// `(request, ctx)`: the call carries enough per-call context that a positional
/// list is unreadable, and a parameter object keeps a future field a one-line
/// addition.
pub(crate) struct RetrySend<'a> {
    pub agent: &'a ureq::Agent,
    pub url: &'a str,
    pub api_key: &'a str,
    pub body: &'a serde_json::Value,
    pub config: &'a ServiceConfig,
    pub retry_cfg: &'a RetryConfig,
    /// Gateway routing identity for the opencode zen/go providers: the turn's
    /// (`session_id`, `request_id`). `None` on paths without a session (model
    /// listing, prompt helpers) — those then route by the gateway's workspace
    /// id / IP fallback, which is fine for auxiliary traffic.
    pub route: Option<(&'a str, &'a str)>,
    /// Fires once when the request payload has been fully uploaded (see
    /// [`RequestUploadedCallback`]); `None` on paths that do not report it.
    pub on_uploaded: Option<RequestUploadedCallback>,
}

pub(crate) fn retry_send(
    request: RetrySend<'_>,
    ctx: &mut crate::retry::AttemptContext,
) -> Result<ureq::http::Response<ureq::Body>, OpenAiError> {
    let RetrySend {
        agent,
        url,
        api_key,
        body,
        config,
        retry_cfg,
        route,
        on_uploaded,
    } = request;
    let auth_header = zeroize::Zeroizing::new(format!("Bearer {}", api_key.trim()));
    // Provider-specific extra headers (the opencode gateway routing headers)
    // are derived from the config + route here, so every call site stays a
    // plain "send with retry" and cannot forget or duplicate them. Empty for
    // non-gateway providers, so the closure loop is a no-op there.
    let headers = route.map_or_default(|(session_id, request_id)| {
        crate::shared::opencode_gateway_headers(&config.provider_slug, session_id, request_id)
    });
    // Serialize once and shape the send: a compact body plus the
    // `Content-Type`/`Content-Length` headers (and `Content-Encoding` when the
    // provider opts into request compression) and the upload hook (see
    // `retry::HookedJsonBody`).
    let compression =
        retry::effective_request_compression(&config.provider_slug, config.request_compression);
    let hooked = crate::retry::HookedJsonBody::new(body, on_uploaded, compression)?;
    // The closure captures `auth_header`, `headers`, and `hooked` by reference
    // (it stays `Fn`); each attempt shares the payload (`Arc<[u8]>`) and clones
    // the `Arc` upload hook. The Zeroizing wrapper ensures the temporary
    // `Bearer …` string is wiped when it goes out of scope.
    retry::retry_loop_with_compression(
        &hooked,
        &config.provider_slug,
        || {
            let mut request = hooked.apply_headers(
                agent
                    .post(url)
                    .header("Authorization", auth_header.as_str()),
            );
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
