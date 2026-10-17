//! Blocking HTTPS calls the login flow makes: the authorization-code exchange
//! and the two device-code calls.
//!
//! The login library talks to the provider's token endpoint directly. Every
//! request goes over TLS to an `https` URL (loopback `http` is permitted for
//! the in-repo mock server and a local test issuer only), and the request body
//! is serialized as form-encoded or JSON according to the descriptor's
//! per-endpoint encoding — the code exchange and the refresh can differ (the
//! `ChatGPT`/Codex backend accepts a form-encoded code exchange but a JSON
//! refresh). No redirects are followed on these credential-bearing POSTs; the
//! credential-free discovery GETs use a separate agent that follows a small,
//! bounded number of redirects (see [`discovery_agent`]).

use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

// The one OAuth form/query encoder lives in `choreo-shared` so the login path
// and the runtime refresh client escape request bodies identically; re-exported
// here so `http::form_encode` stays the call site for both this module and the
// MCP login submodules.
pub(crate) use choreo_shared::form::form_encode;

use super::OAuthLoginError;

/// The token endpoint's success/error body.
///
/// A success and an OAuth error are the same JSON shape discriminated by which
/// field is present, so one struct reads both: `access_token` on success, or
/// `error`/`error_description` on failure.
#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: Option<String>,
    refresh_token: Option<String>,
    id_token: Option<String>,
    token_type: Option<String>,
    scope: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

/// The tokens returned by an authorization-code or device-code exchange.
///
/// The access, refresh, and id tokens are wipe-on-drop ([`Zeroizing`]); the
/// `Debug` impl reports only their presence, never their value, so a token can
/// never reach a log line by way of a `{:?}`.
pub(crate) struct TokenSet {
    /// The bearer access token.
    pub(crate) access_token: Zeroizing<String>,
    /// The refresh token, when the provider issued one.
    pub(crate) refresh_token: Option<Zeroizing<String>>,
    /// The OIDC `id_token` (JWT), when the provider issued one.
    pub(crate) id_token: Option<Zeroizing<String>>,
    /// The bearer token type (almost always `Bearer`).
    pub(crate) token_type: Option<String>,
    /// The scopes the provider granted.
    pub(crate) scope: Option<String>,
}

impl std::fmt::Debug for TokenSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenSet")
            .field("access_token", &"<redacted>")
            .field("refresh_token", &self.refresh_token.is_some())
            .field("id_token", &self.id_token.is_some())
            .field("token_type", &self.token_type)
            .field("scope", &self.scope)
            .finish()
    }
}

/// The device-authorization (`usercode`) response.
///
/// The opaque `device_code` is a short-lived secret the token poll exchanges,
/// so it is wipe-on-drop ([`Zeroizing`]); the `Debug` impl reports only its
/// presence, never its value.
pub(crate) struct DeviceCode {
    /// The opaque device code the token poll exchanges.
    pub(crate) device_code: Zeroizing<String>,
    /// The short human code the user enters at `verification_uri`.
    pub(crate) user_code: String,
    /// The URL the user visits to enter `user_code`.
    pub(crate) verification_uri: String,
    /// The minimum seconds between token polls, when the server specifies one.
    pub(crate) interval: Option<u64>,
    /// The lifetime of `device_code` in seconds, when the server specifies one;
    /// the poll stops once it elapses even if the server never reports expiry.
    pub(crate) expires_in: Option<u64>,
}

impl std::fmt::Debug for DeviceCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceCode")
            .field("device_code", &"<redacted>")
            .field("user_code", &self.user_code)
            .field("verification_uri", &self.verification_uri)
            .field("interval", &self.interval)
            .field("expires_in", &self.expires_in)
            .finish()
    }
}

/// The outcome of one device token poll.
pub(crate) enum PollOutcome {
    /// The user has authorized; the tokens are ready.
    Tokens(TokenSet),
    /// The authorization is still pending; poll again after the interval.
    Pending,
    /// The server asked the client to poll less often (`slow_down`); the caller
    /// increases its interval and keeps polling.
    SlowDown,
    /// The device code expired (or was denied); the flow is terminal.
    Expired(String),
}

/// Append a form-encoded query to an authorization endpoint, choosing `&` when
/// the endpoint already carries a query string. The one place the
/// provider and MCP flows build an authorize URL, so the separator and encoding
/// rules cannot drift between them. The parameters are borrowed, so a secret
/// (the `state`) is never copied to a temporary owned string.
#[must_use]
pub(crate) fn authorize_url(endpoint: &str, params: &[(&str, &str)]) -> String {
    let query = form_encode(params);
    let separator = if endpoint.contains('?') { '&' } else { '?' };
    format!("{endpoint}{separator}{query}")
}

/// Reject any authorization-server URL that is not `https`, except a loopback
/// `http` URL (the in-repo mock server and a local test issuer), so a
/// misconfigured descriptor can never leak a code or token over cleartext to a
/// remote host. The predicate itself is the one shared policy
/// (`choreo_shared::oauth_endpoint`), the same rule the runtime refresh client
/// applies, so login and refresh cannot drift.
pub(crate) fn ensure_secure(url: &str) -> Result<(), OAuthLoginError> {
    if choreo_shared::oauth_endpoint::is_secure_endpoint(url) {
        Ok(())
    } else {
        Err(OAuthLoginError::InsecureEndpoint(url.to_string()))
    }
}

/// Build the shared agent: status responses are returned verbatim (a 400 token
/// response is a normal, readable outcome), a global timeout bounds a hung
/// provider, and redirects are never followed on these POSTs.
pub(crate) fn agent() -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(30)))
        .http_status_as_error(false)
        .max_redirects(0)
        .build();
    ureq::Agent::new_with_config(config)
}

/// Build the agent for the credential-free **discovery** GETs (the RFC 9728 /
/// 8414 well-known metadata fetches). Unlike [`agent`] it follows a small,
/// bounded number of redirects, because an authorization server may redirect a
/// well-known document (e.g. to a tenant host) and no secret rides a GET. Only
/// the initial URL is vetted with the endpoint policy; redirect targets are
/// bounded but not re-vetted, which is acceptable because a metadata document
/// carries no credential.
pub(crate) fn discovery_agent() -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(30)))
        .http_status_as_error(false)
        .max_redirects(5)
        .build();
    ureq::Agent::new_with_config(config)
}

/// Send a POST body with the given content type, returning the raw response.
///
/// The shared POST for every credential-bearing call: it vets the endpoint,
/// sends `Accept: application/json`, and sets `Cache-Control: no-store` so a
/// proxy never caches a body carrying a code or token.
pub(crate) fn post(
    agent: &ureq::Agent,
    url: &str,
    content_type: &str,
    body: &str,
) -> Result<ureq::http::Response<ureq::Body>, OAuthLoginError> {
    ensure_secure(url)?;
    agent
        .post(url)
        .header("Content-Type", content_type)
        .header("Accept", "application/json")
        // These POSTs carry a code or device code; never let a redirect replay
        // them at another host.
        .header("Cache-Control", "no-store")
        .send(body.as_bytes())
        .map_err(|e| OAuthLoginError::Network(e.to_string()))
}

/// Interpret a token endpoint's response body, mapping an OAuth `error` to a
/// categorized failure and an absent `access_token` to a protocol error.
pub(crate) fn read_token_response(
    response: ureq::http::Response<ureq::Body>,
) -> Result<TokenSet, OAuthLoginError> {
    let status = response.status();
    let parsed: TokenResponse = response
        .into_body()
        .read_json()
        .map_err(|e| OAuthLoginError::Decode(e.to_string()))?;
    if let Some(error) = parsed.error {
        let description = parsed.error_description.unwrap_or_default();
        return Err(OAuthLoginError::TokenEndpoint { error, description });
    }
    let Some(access_token) = parsed.access_token else {
        return Err(OAuthLoginError::Decode(format!(
            "token endpoint returned no access_token (status {status})"
        )));
    };
    Ok(TokenSet {
        access_token: Zeroizing::new(access_token),
        refresh_token: parsed.refresh_token.map(Zeroizing::new),
        id_token: parsed.id_token.map(Zeroizing::new),
        token_type: parsed.token_type,
        scope: parsed.scope,
    })
}

/// The JSON authorization-code-exchange body.
///
/// A typed struct (rather than a `serde_json::Value`) serializes straight to
/// the request body, so the authorization `code` and the `code_verifier` are
/// never copied into an intermediate owned map that would hold a second copy of
/// the secrets in memory.
#[derive(Serialize)]
struct CodeExchangeBody<'a> {
    grant_type: &'static str,
    code: &'a str,
    redirect_uri: &'a str,
    client_id: &'a str,
    code_verifier: &'a str,
}

/// Exchange an authorization `code` for tokens (the PKCE code exchange).
///
/// # Errors
///
/// Returns [`OAuthLoginError`] on an insecure endpoint, a network failure, a
/// malformed body, or an OAuth error from the provider.
pub(crate) fn exchange_code(
    issuer: &str,
    token_path: &str,
    encoding: choreo_proto::OAuthGrantEncoding,
    client_id: &str,
    code: &str,
    redirect_uri: &str,
    code_verifier: &str,
) -> Result<TokenSet, OAuthLoginError> {
    let url = choreo_shared::url::join_path(issuer, token_path);
    let params = [
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", redirect_uri),
        ("client_id", client_id),
        ("code_verifier", code_verifier),
    ];
    let agent = agent();
    let response = match encoding {
        choreo_proto::OAuthGrantEncoding::Form => {
            // The pairs are borrowed straight from the caller, so the `code` and
            // `code_verifier` are never copied into a temporary owned string.
            post(
                &agent,
                &url,
                "application/x-www-form-urlencoded",
                &form_encode(&params),
            )?
        }
        choreo_proto::OAuthGrantEncoding::Json => {
            let body = CodeExchangeBody {
                grant_type: "authorization_code",
                code,
                redirect_uri,
                client_id,
                code_verifier,
            };
            let body =
                serde_json::to_string(&body).map_err(|e| OAuthLoginError::Decode(e.to_string()))?;
            post(&agent, &url, "application/json", &body)?
        }
    };
    read_token_response(response)
}

/// Request a device code (`usercode`) for the RFC 8628 headless flow.
///
/// # Errors
///
/// Returns [`OAuthLoginError`] on an insecure endpoint, a network failure, a
/// malformed body, or an OAuth error from the provider.
pub(crate) fn device_authorize(
    issuer: &str,
    device_authorization_path: &str,
    client_id: &str,
    scope: &str,
) -> Result<DeviceCode, OAuthLoginError> {
    let url = choreo_shared::url::join_path(issuer, device_authorization_path);
    let pairs = [("client_id", client_id), ("scope", scope)];
    let agent = agent();
    let response = post(
        &agent,
        &url,
        "application/x-www-form-urlencoded",
        &form_encode(&pairs),
    )?;
    let status = response.status();
    let parsed: DeviceCodeResponse = response
        .into_body()
        .read_json()
        .map_err(|e| OAuthLoginError::Decode(e.to_string()))?;
    if let Some(error) = parsed.error {
        return Err(OAuthLoginError::TokenEndpoint {
            error,
            description: parsed.error_description.unwrap_or_default(),
        });
    }
    let (Some(device_code), Some(user_code), Some(verification_uri)) = (
        parsed.device_code,
        parsed.user_code,
        parsed.verification_uri,
    ) else {
        return Err(OAuthLoginError::Decode(format!(
            "device endpoint returned no code (status {status})"
        )));
    };
    Ok(DeviceCode {
        device_code: Zeroizing::new(device_code),
        user_code,
        verification_uri,
        interval: parsed.interval,
        expires_in: parsed.expires_in,
    })
}

/// The device-authorization response body (success or error shape).
#[derive(Debug, Deserialize)]
struct DeviceCodeResponse {
    device_code: Option<String>,
    user_code: Option<String>,
    verification_uri: Option<String>,
    interval: Option<u64>,
    expires_in: Option<u64>,
    error: Option<String>,
    error_description: Option<String>,
}

/// Poll the device token endpoint once. `authorization_pending` (still waiting)
/// and `slow_down` (poll less often) are the two non-terminal OAuth errors and
/// are returned as [`PollOutcome::Pending`] / [`PollOutcome::SlowDown`]; any
/// other OAuth error is terminal.
///
/// # Errors
///
/// Returns [`OAuthLoginError`] only on an insecure endpoint, a network failure,
/// or a malformed body — the "still pending" case is a normal
/// [`PollOutcome::Pending`], not an error.
pub(crate) fn device_poll(
    issuer: &str,
    device_token_path: &str,
    client_id: &str,
    device_code: &str,
) -> Result<PollOutcome, OAuthLoginError> {
    let url = choreo_shared::url::join_path(issuer, device_token_path);
    let pairs = [
        ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
        ("device_code", device_code),
        ("client_id", client_id),
    ];
    let agent = agent();
    let response = post(
        &agent,
        &url,
        "application/x-www-form-urlencoded",
        &form_encode(&pairs),
    )?;
    let parsed: TokenResponse = response
        .into_body()
        .read_json()
        .map_err(|e| OAuthLoginError::Decode(e.to_string()))?;
    if let Some(access_token) = parsed.access_token {
        return Ok(PollOutcome::Tokens(TokenSet {
            access_token: Zeroizing::new(access_token),
            refresh_token: parsed.refresh_token.map(Zeroizing::new),
            id_token: parsed.id_token.map(Zeroizing::new),
            token_type: parsed.token_type,
            scope: parsed.scope,
        }));
    }
    match parsed.error.as_deref() {
        Some("authorization_pending") => Ok(PollOutcome::Pending),
        Some("slow_down") => Ok(PollOutcome::SlowDown),
        Some(other) => Ok(PollOutcome::Expired(format!(
            "{other}: {}",
            parsed.error_description.unwrap_or_default()
        ))),
        None => Err(OAuthLoginError::Decode(
            "device poll returned neither tokens nor an error".to_string(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authorize_url_appends_query_with_the_right_separator() {
        let params = [("state", "s")];
        assert_eq!(
            authorize_url("https://as.example/authorize", &params),
            "https://as.example/authorize?state=s"
        );
        assert_eq!(
            authorize_url("https://as.example/authorize?keep=1", &params),
            "https://as.example/authorize?keep=1&state=s"
        );
    }

    #[test]
    fn secure_endpoint_rule_allows_https_and_loopback_http() {
        assert!(ensure_secure("https://auth.openai.com/oauth/token").is_ok());
        assert!(ensure_secure("http://127.0.0.1:8080/token").is_ok());
        assert!(ensure_secure("http://localhost:8080/token").is_ok());
        assert!(ensure_secure("http://[::1]:8080/token").is_ok());
        assert!(ensure_secure("http://evil.example/token").is_err());
    }
}
