//! Blocking provider OAuth **refresh** client.
//!
//! A provider OAuth grant is refreshed by presenting its refresh token to the
//! authorization server's token endpoint (RFC 6749 §6). This module owns that
//! one exchange, split by concern: the `request` submodule carries the
//! request parameters and their serialization (form vs JSON, per the
//! credential's recorded `refresh_encoding`), the `response` submodule carries
//! the token set and the parse plus the **failure classification** that drives the
//! caller's terminal-vs-transient decision, and this file holds
//! [`refresh_oauth_token`] — the `ureq` agent, the endpoint vetting, the POST,
//! and the call into the parse — plus the public re-exports. It stays
//! synchronous and daemon-free: no sessions, no keystore, no daemon types — the
//! caller (the daemon's session thread) supplies the token endpoint and the
//! durable grant, and persists the result.
//!
//! The access token stays ephemeral (the caller caches it in memory only); the
//! returned `refresh_token` is the rotated durable root the caller re-encrypts
//! into the credential blob.

use crate::catalog::GrantEncoding;

mod request;
mod response;

pub use request::OAuthRefreshRequest;
pub use response::{OAuthRefreshError, OAuthTokenResponse};

// The `ChatGPT` account-id decoder is shared with the login library, so a
// rotated `id_token` is read through the same namespace decode; re-exported
// here so callers keep using `choreo_ai_protocols::oauth::…`.
pub use choreo_shared::oauth_claims::chatgpt_account_id_from_id_token;

use request::encode_refresh_body;
use response::parse_token_response;

/// Refresh an OAuth access token against `request.token_endpoint`.
///
/// # Errors
///
/// Returns [`OAuthRefreshError::Terminal`] when the authorization server
/// rejects the grant permanently, and [`OAuthRefreshError::Transient`] on a
/// transport error, `429`, or `5xx`.
pub fn refresh_oauth_token(
    request: &OAuthRefreshRequest<'_>,
) -> Result<OAuthTokenResponse, OAuthRefreshError> {
    // Never send the durable refresh token over cleartext. The login library
    // enforces the same rule when it first obtains the grant; both call the one
    // shared policy (`choreo_shared::oauth_endpoint`) so they cannot drift, so a
    // credential whose stored issuer is a non-https URL (the daemon derives the
    // endpoint from it, and `AddCredential` does not vet the issuer) cannot
    // silently leak the token on every runtime refresh.
    if !choreo_shared::oauth_endpoint::is_secure_endpoint(request.token_endpoint) {
        return Err(OAuthRefreshError::InsecureEndpoint {
            endpoint: request.token_endpoint.to_string(),
        });
    }
    // A dedicated agent: the refresh is a one-shot blocking POST with a
    // bounded global timeout, and statuses are handled here (not by ureq) so a
    // 4xx body can be classified without an exception path. No socket registry:
    // this crate stays free of the daemon's cancellation plumbing.
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(30)))
        .http_status_as_error(false)
        .user_agent(format!("choreographr/{}", env!("CARGO_PKG_VERSION")))
        // A credential-bearing POST must never follow a redirect: the refresh
        // token in the body would be replayed at a server the authorization
        // server chose. ureq refuses to replay a POST body on a 307/308 and
        // downgrades a 301/302/303 to a body-less GET, but this is spelled out
        // explicitly so the guarantee does not rest on ureq's internals.
        .max_redirects(0)
        .build()
        .into();

    let body = encode_refresh_body(request);
    // `Accept` mirrors the login path; `Cache-Control: no-store` keeps a proxy
    // from caching a body that carries the durable refresh token (and whose
    // response carries a fresh one).
    let call = agent.post(request.token_endpoint);
    let call = match request.encoding {
        GrantEncoding::Form => call.header("Content-Type", "application/x-www-form-urlencoded"),
        GrantEncoding::Json => call.header("Content-Type", "application/json"),
    };
    let response = call
        .header("Accept", "application/json")
        .header("Cache-Control", "no-store")
        .send(body.as_slice())
        .map_err(|e| {
            // Transport errors (connect/read/timeout) are transient: retry.
            OAuthRefreshError::Transient {
                detail: e.to_string(),
            }
        })?;

    let status = response.status().as_u16();
    let body_text = response.into_body().read_to_string().unwrap_or_default();
    parse_token_response(status, body_text.as_str())
}
