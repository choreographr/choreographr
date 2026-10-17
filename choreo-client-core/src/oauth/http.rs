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
//! credential-free discovery GETs use the same agent (which follows **no**
//! redirects itself), and the discovery caller follows a bounded number of hops,
//! re-vetting each with the endpoint policy (see `mcp::discovery::get_json`).
//!
//! Every credential-bearing endpoint is vetted with the shared `https`-or-
//! loopback rule, and the MCP flow additionally refuses a private/link-local
//! IP-literal host whenever its resource was remote (see [`HopPolicy`]), so a
//! fetched document cannot steer a hop onto `http://127.0.0.1` or an internal
//! service. The provider flow and the mock-server tests keep the loopback
//! allowance.

use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

// The one OAuth form/query encoder lives in `choreo-shared` so the login path
// and the runtime refresh client escape request bodies identically; re-exported
// here so `http::form_encode` stays the call site for both this module and the
// MCP login submodules.
pub(crate) use choreo_shared::form::form_encode;
// The token-endpoint success/error body is the one shape `choreo-shared` parses,
// so a token reply is read the same way by this login library and the runtime
// refresh client (`choreo-ai-protocols`).
use choreo_shared::oauth_token::{TokenResponse, parse};

use super::OAuthLoginError;

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

impl TokenSet {
    /// Build a token set from the shared token-response parse, or `None` when
    /// the reply carried no access token (an OAuth error body, or an empty
    /// one). The one `TokenResponse`→`TokenSet` projection, so the code
    /// exchange and the device poll read the shared parse identically.
    fn from_response(parsed: TokenResponse) -> Option<Self> {
        let access_token = parsed.access_token?;
        Some(Self {
            access_token: Zeroizing::new(access_token),
            refresh_token: parsed.refresh_token.map(Zeroizing::new),
            id_token: parsed.id_token.map(Zeroizing::new),
            token_type: parsed.token_type,
            scope: parsed.scope,
        })
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
    ensure_secure_with(url, true)
}

/// As [`ensure_secure`], under an explicit loopback policy.
///
/// The MCP discovery flow passes `allow_loopback_http = false` when it did not
/// begin at a loopback resource, so a fetched metadata document or a redirect
/// cannot steer a hop onto `http://127.0.0.1` (an SSRF against the host's local
/// services). The provider flow keeps the permissive default.
pub(crate) fn ensure_secure_with(
    url: &str,
    allow_loopback_http: bool,
) -> Result<(), OAuthLoginError> {
    if choreo_shared::oauth_endpoint::is_secure_endpoint_with(url, allow_loopback_http) {
        Ok(())
    } else {
        Err(OAuthLoginError::InsecureEndpoint(url.to_string()))
    }
}

/// The endpoint policy one login hop is vetted under.
///
/// The provider flow's endpoints come from the trusted catalog descriptor, so
/// its policy allows a loopback `http` endpoint and imposes no IP-literal
/// restriction. The MCP flow's endpoints come from a document a remote server
/// served, so its policy additionally refuses a private/link-local IP literal
/// unless the flow itself began at such a resource (see [`Self::for_resource`]).
#[derive(Clone, Copy, Debug)]
pub(crate) struct HopPolicy {
    /// Allow a loopback `http` endpoint (the flow began at a loopback resource,
    /// or the provider flow's trusted descriptor).
    pub(crate) allow_loopback_http: bool,
    /// Allow an endpoint whose host is a private/link-local IP literal (the
    /// flow's own resource is one, so it is expected to discover internal
    /// endpoints).
    pub(crate) allow_private_hosts: bool,
}

impl HopPolicy {
    /// The provider flow's policy: endpoints come from the trusted descriptor,
    /// so neither a loopback `http` endpoint nor a private host is restricted.
    pub(crate) fn provider() -> Self {
        Self {
            allow_loopback_http: true,
            allow_private_hosts: true,
        }
    }

    /// The MCP flow's policy for a starting `resource` URL.
    ///
    /// A resource reached by hostname (the common remote case) is treated as
    /// untrusted, so a document it served may not steer a hop onto a loopback
    /// `http` endpoint or a private/link-local IP literal. A resource that is
    /// itself a loopback or private IP literal — an in-repo mock, or a LAN
    /// self-hosted server — keeps the local allowance, because its own
    /// documents legitimately name internal endpoints.
    pub(crate) fn for_resource(resource: &str) -> Self {
        let loopback = choreo_shared::oauth_endpoint::is_loopback_endpoint(resource);
        Self {
            allow_loopback_http: loopback,
            allow_private_hosts: loopback || host_is_blocked_ip(resource),
        }
    }

    /// Vet one hop under this policy: the shared `https`-or-loopback rule,
    /// plus the SSRF IP-literal refusal when a private host is not allowed.
    pub(crate) fn check(self, url: &str) -> Result<(), OAuthLoginError> {
        ensure_secure_with(url, self.allow_loopback_http)?;
        if !self.allow_private_hosts && host_is_blocked_ip(url) {
            return Err(OAuthLoginError::InsecureEndpoint(url.to_string()));
        }
        Ok(())
    }
}

/// Whether `url`'s host is a loopback/private/link-local IP literal. The
/// classifier itself is shared (`choreo_shared::net`); this is only the
/// URL-to-host projection, kept here because `choreo-shared` is a leaf with no
/// URL dependency.
fn host_is_blocked_ip(url: &str) -> bool {
    url::Url::parse(url)
        .ok()
        .and_then(|parsed| parsed.host_str().map(str::to_owned))
        .is_some_and(|host| choreo_shared::net::is_blocked_ip_literal(&host))
}

/// Build the shared agent: status responses are returned verbatim (a 400 token
/// response is a normal, readable outcome), a global timeout bounds a hung
/// provider, and redirects are never followed — on the credential-bearing POSTs
/// (so a code or device code is never replayed at another host) or on the
/// credential-free discovery GETs (whose caller follows a small, bounded number
/// of hops itself, re-vetting each with the endpoint policy; see
/// `mcp::discovery::get_json`).
pub(crate) fn agent() -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(30)))
        .http_status_as_error(false)
        .max_redirects(0)
        .build();
    ureq::Agent::new_with_config(config)
}

/// Send a POST body with the given content type, returning the raw response.
///
/// The shared POST for every credential-bearing call: it vets the endpoint,
/// sends `Accept: application/json`, and sets `Cache-Control: no-store` so a
/// proxy never caches a body carrying a code or token. The client is not
/// authenticated; use [`post_with_basic_auth`] for a confidential client's
/// token-endpoint exchange.
pub(crate) fn post(
    agent: &ureq::Agent,
    url: &str,
    content_type: &str,
    body: &str,
    policy: HopPolicy,
) -> Result<ureq::http::Response<ureq::Body>, OAuthLoginError> {
    post_with_basic_auth(agent, url, content_type, body, policy, None)
}

/// As [`post`], additionally authenticating the client with the RFC 6749 §2.3.1
/// HTTP Basic credential (`client_secret_basic`) when `basic` is set.
///
/// The credential is built from the client id and secret — url-encoded
/// separately, then `id:secret`, then base64 — and set as the `Authorization`
/// header; the caller omits `client_id`/`client_secret` from `body` in that
/// case (RFC 6749 forbids two client-auth methods in one request).
pub(crate) fn post_with_basic_auth(
    agent: &ureq::Agent,
    url: &str,
    content_type: &str,
    body: &str,
    policy: HopPolicy,
    basic: Option<(&str, &str)>,
) -> Result<ureq::http::Response<ureq::Body>, OAuthLoginError> {
    policy.check(url)?;
    let mut request = agent
        .post(url)
        .header("Content-Type", content_type)
        .header("Accept", "application/json")
        // These POSTs carry a code or device code; never let a redirect replay
        // them at another host.
        .header("Cache-Control", "no-store");
    if let Some((client_id, client_secret)) = basic {
        let authorization = basic_authorization(client_id, client_secret);
        request = request.header("Authorization", authorization.as_str());
    }
    request
        .send(body.as_bytes())
        .map_err(|e| OAuthLoginError::Network(e.to_string()))
}

/// The RFC 6749 §2.3.1 HTTP Basic credential for a confidential client: the
/// client id and secret url-encoded separately, joined with `:`, and base64.
///
/// The intermediate `id:secret` buffer and the returned header value are
/// wipe-on-drop ([`Zeroizing`]), so the client secret is not left in a plain
/// `String` in freed memory.
fn basic_authorization(client_id: &str, client_secret: &str) -> Zeroizing<String> {
    use base64::Engine as _;
    let credential = Zeroizing::new(format!(
        "{}:{}",
        choreo_shared::form::percent_encode(client_id),
        choreo_shared::form::percent_encode(client_secret)
    ));
    let encoded = base64::engine::general_purpose::STANDARD.encode(credential.as_bytes());
    Zeroizing::new(format!("Basic {encoded}"))
}

/// Interpret a token endpoint's response body, mapping an OAuth `error` to a
/// categorized failure and an absent `access_token` to a protocol error.
pub(crate) fn read_token_response(
    response: ureq::http::Response<ureq::Body>,
) -> Result<TokenSet, OAuthLoginError> {
    let status = response.status();
    // The body carries the freshly minted access/refresh/id tokens, so it is
    // wipe-on-drop rather than left in a plain `String` in freed memory.
    let body = Zeroizing::new(
        response
            .into_body()
            .read_to_string()
            .map_err(|e| OAuthLoginError::Decode(e.to_string()))?,
    );
    let mut parsed: TokenResponse = parse(body.as_str());
    // Pull the non-secret error fields out with `take` so `parsed` is not
    // partially moved and no secret `String` (the tokens) is cloned.
    let error = parsed.error.take();
    let error_description = parsed.error_description.take();
    if let Some(error) = error {
        return Err(OAuthLoginError::TokenEndpoint {
            error,
            description: error_description.unwrap_or_default(),
        });
    }
    TokenSet::from_response(parsed).ok_or_else(|| {
        OAuthLoginError::Decode(format!(
            "token endpoint returned no access_token (status {status})"
        ))
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
    // The body carries the authorization `code` and PKCE `code_verifier`, so
    // both branches build it wipe-on-drop; the pairs are borrowed straight from
    // the caller, so no non-zeroizing copy of a secret survives the request.
    let (content_type, body) = match encoding {
        choreo_proto::OAuthGrantEncoding::Form => (
            "application/x-www-form-urlencoded",
            Zeroizing::new(form_encode(&params)),
        ),
        choreo_proto::OAuthGrantEncoding::Json => {
            let body = CodeExchangeBody {
                grant_type: "authorization_code",
                code,
                redirect_uri,
                client_id,
                code_verifier,
            };
            (
                "application/json",
                Zeroizing::new(
                    serde_json::to_string(&body)
                        .map_err(|e| OAuthLoginError::Decode(e.to_string()))?,
                ),
            )
        }
    };
    let response = post(&agent, &url, content_type, &body, HopPolicy::provider())?;
    read_token_response(response)
}

/// Request a device code (`usercode`) for the RFC 8628 headless flow.
///
/// Takes the already-built `agent` so the device-code wait reuses one
/// connection pool across every poll instead of rebuilding an agent per
/// iteration.
///
/// # Errors
///
/// Returns [`OAuthLoginError`] on an insecure endpoint, a network failure, a
/// malformed body, or an OAuth error from the provider.
pub(crate) fn device_authorize(
    agent: &ureq::Agent,
    issuer: &str,
    device_authorization_path: &str,
    client_id: &str,
    scope: &str,
) -> Result<DeviceCode, OAuthLoginError> {
    let url = choreo_shared::url::join_path(issuer, device_authorization_path);
    let pairs = [("client_id", client_id), ("scope", scope)];
    let body = Zeroizing::new(form_encode(&pairs));
    let response = post(
        agent,
        &url,
        "application/x-www-form-urlencoded",
        &body,
        HopPolicy::provider(),
    )?;
    let status = response.status();
    // The response carries the opaque `device_code` (a short-lived secret), so
    // its body is wipe-on-drop.
    let body = Zeroizing::new(
        response
            .into_body()
            .read_to_string()
            .map_err(|e| OAuthLoginError::Decode(e.to_string()))?,
    );
    let parsed: DeviceCodeResponse =
        serde_json::from_str(body.as_str()).map_err(|e| OAuthLoginError::Decode(e.to_string()))?;
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
/// Takes the already-built `agent` so the poll loop reuses one connection pool.
///
/// # Errors
///
/// Returns [`OAuthLoginError`] only on an insecure endpoint, a network failure,
/// or a malformed body — the "still pending" case is a normal
/// [`PollOutcome::Pending`], not an error.
pub(crate) fn device_poll(
    agent: &ureq::Agent,
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
    let body = Zeroizing::new(form_encode(&pairs));
    let response = post(
        agent,
        &url,
        "application/x-www-form-urlencoded",
        &body,
        HopPolicy::provider(),
    )?;
    let status = response.status().as_u16();
    // A refused redirect (the agent follows none), 429, or 5xx is TRANSIENT: the
    // caller tolerates a short run of transient failures before giving up, so a
    // one-off server error does not abort the (minutes-long) device flow. This
    // mirrors the runtime refresh classifier's terminal-vs-transient rule.
    if (300..400).contains(&status) || status == 429 || (500..600).contains(&status) {
        return Err(OAuthLoginError::Network(format!(
            "device token endpoint returned transient status {status}"
        )));
    }
    // The response body carries the freshly minted tokens on the authorized
    // poll, so it is wipe-on-drop.
    let body = Zeroizing::new(
        response
            .into_body()
            .read_to_string()
            .map_err(|e| OAuthLoginError::Decode(e.to_string()))?,
    );
    let mut parsed: TokenResponse = parse(body.as_str());
    // Pull the non-secret error fields out with `take` so `parsed` is not
    // partially moved and no secret `String` (the tokens) is cloned.
    let error = parsed.error.take();
    let error_description = parsed.error_description.take();
    if let Some(tokens) = TokenSet::from_response(parsed) {
        return Ok(PollOutcome::Tokens(tokens));
    }
    match error.as_deref() {
        Some("authorization_pending") => Ok(PollOutcome::Pending),
        Some("slow_down") => Ok(PollOutcome::SlowDown),
        Some(other) => Ok(PollOutcome::Expired(format!(
            "{other}: {}",
            error_description.unwrap_or_default()
        ))),
        None => Err(OAuthLoginError::Decode(
            "device poll returned neither tokens nor an error".to_string(),
        )),
    }
}

/// Probe the MCP resource for a `WWW-Authenticate` challenge (RFC 9728 §5.1):
/// send one unauthenticated `initialize` and return the challenge header value
/// from a `401` (authorization required) or a `403` (`insufficient_scope`, RFC
/// 6750 §3.1 — the same challenge shape). A missing endpoint, any other status,
/// an insecure URL, or a network error yields `None`, so the caller falls back
/// to well-known discovery.
pub(crate) fn probe_www_authenticate(
    agent: &ureq::Agent,
    resource: &str,
    policy: HopPolicy,
) -> Option<String> {
    if policy.check(resource).is_err() {
        return None;
    }
    // A minimal, well-formed MCP `initialize`. An authorization-requiring server
    // answers 401 before it inspects the body, so the contents do not matter
    // beyond being valid JSON-RPC.
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"Choreographr","version":"0"}}}"#;
    let response = agent
        .post(resource)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .send(body.as_bytes())
        .ok()?;
    if response.status() != 401 && response.status() != 403 {
        return None;
    }
    response
        .headers()
        .get("www-authenticate")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
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

    #[test]
    fn token_set_from_response_projects_the_shared_parse() {
        // A reply with an access token projects to a token set carrying every
        // field the shared parse read.
        let parsed = TokenResponse {
            access_token: Some("access".to_string()),
            refresh_token: Some("refresh".to_string()),
            id_token: Some("id".to_string()),
            token_type: Some("Bearer".to_string()),
            scope: Some("openid".to_string()),
            ..TokenResponse::default()
        };
        let tokens = TokenSet::from_response(parsed).expect("access token present");
        assert_eq!(tokens.access_token.as_str(), "access");
        assert_eq!(
            tokens.refresh_token.as_deref().map(String::as_str),
            Some("refresh")
        );
        assert_eq!(tokens.id_token.as_deref().map(String::as_str), Some("id"));
        assert_eq!(tokens.token_type.as_deref(), Some("Bearer"));
        assert_eq!(tokens.scope.as_deref(), Some("openid"));
        // A reply with no access token (an OAuth error body, or an empty one)
        // yields no token set.
        let error_only = TokenResponse {
            error: Some("invalid_grant".to_string()),
            ..TokenResponse::default()
        };
        assert!(TokenSet::from_response(error_only).is_none());
        assert!(TokenSet::from_response(TokenResponse::default()).is_none());
    }

    #[test]
    fn basic_authorization_encodes_the_rfc_6749_credential() {
        use base64::Engine as _;
        // RFC 6749 §2.3.1: the id and secret are url-encoded separately, joined
        // with ':', then base64. `app-1:right-secret` carries no reserved bytes,
        // so it is the raw concatenation.
        assert_eq!(
            basic_authorization("app-1", "right-secret").as_str(),
            "Basic YXBwLTE6cmlnaHQtc2VjcmV0"
        );
        // A secret with reserved bytes is url-encoded first, so a ':' in it can
        // never be read as the id/secret separator.
        let expected = base64::engine::general_purpose::STANDARD.encode("id:a%3Ab%20c");
        assert_eq!(
            basic_authorization("id", "a:b c").as_str(),
            format!("Basic {expected}")
        );
    }
}
