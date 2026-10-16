pub(crate) use crate::retry::AttemptContext;
pub(crate) use crate::retry::AttemptDeadline;
pub use crate::retry::{RequestUploadedCallback, RetryCallback, RetryConfig};

use super::{OpenAiError, ServiceConfig};
use crate::retry;
use crate::shared::RequestAuth;

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
    /// The request auth: an API key or an OAuth access token plus its derived
    /// extra headers. The bearer value and the extra headers are both applied
    /// here, so every call site stays a plain "send with retry".
    pub auth: &'a RequestAuth,
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
        auth,
        body,
        config,
        retry_cfg,
        route,
        on_uploaded,
    } = request;
    // The bearer value is built from the request auth (an API key or an OAuth
    // access token); `Zeroizing` wipes the temporary `Bearer …` string on drop.
    let auth_header = auth.authorization_header();
    // Provider-specific extra headers are derived here: the credential-derived
    // headers (e.g. the `ChatGPT` account id and originator) come from the auth
    // itself, and the opencode gateway routing headers from the config + route.
    // Both apply to the same request, so they are merged into one list; every
    // call site stays a plain "send with retry" and cannot forget or duplicate
    // either set. Empty lists on both sides make the loop a no-op (the merged
    // order does not matter unless a name collides, which they never do).
    let mut headers: Vec<(String, String)> = route.map_or_default(|(session_id, request_id)| {
        crate::shared::opencode_gateway_headers(&config.provider_slug, session_id, request_id)
            .into_iter()
            .map(|(name, value)| (name.to_string(), value))
            .collect()
    });
    headers.extend(auth.extra_headers().iter().cloned());
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
                request = request.header(name.as_str(), value.as_str());
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
    auth: &RequestAuth,
    _config: &ServiceConfig,
    retry_cfg: &RetryConfig,
    ctx: &mut crate::retry::AttemptContext,
) -> Result<ureq::http::Response<ureq::Body>, OpenAiError> {
    let auth_header = auth.authorization_header();
    // Model listing carries no session identity, so no opencode gateway
    // routing headers are sent here — the gateway does not route model
    // listings to inference upstreams anyway. Credential-derived headers still
    // apply, because they belong to the account, not the route.
    let headers: Vec<(String, String)> = auth.extra_headers().to_vec();
    // The closure captures `auth_header` and `headers` by reference (it stays
    // `Fn`); the Zeroizing wrapper ensures the temporary `Bearer …` string is
    // wiped when it goes out of scope.
    retry::retry_loop(
        || {
            let mut request = agent.get(url).header("Authorization", auth_header.as_str());
            for (name, value) in &headers {
                request = request.header(name.as_str(), value.as_str());
            }
            request.call()
        },
        retry_cfg,
        ctx,
    )
    .map_err(OpenAiError::from)
}
