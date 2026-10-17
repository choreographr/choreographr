//! Blocking HTTPS calls the login flow makes: the authorization-code exchange
//! and the two device-code calls.
//!
//! The login library talks to the provider's token endpoint directly. Every
//! request goes over TLS to an `https` URL (loopback `http` is permitted for
//! the in-repo mock server and a local test issuer only), and the request body
//! is serialized as form-encoded or JSON according to the descriptor's
//! per-endpoint encoding — the code exchange and the refresh can differ (the
//! `ChatGPT`/Codex backend accepts a form-encoded code exchange but a JSON
//! refresh). No redirects are followed on these credential-bearing POSTs.

use serde::Deserialize;

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
#[derive(Debug)]
pub(crate) struct TokenSet {
    /// The bearer access token.
    pub(crate) access_token: String,
    /// The refresh token, when the provider issued one.
    pub(crate) refresh_token: Option<String>,
    /// The OIDC `id_token` (JWT), when the provider issued one.
    pub(crate) id_token: Option<String>,
    /// The bearer token type (almost always `Bearer`).
    pub(crate) token_type: Option<String>,
    /// The scopes the provider granted.
    pub(crate) scope: Option<String>,
}

/// The device-authorization (`usercode`) response.
#[derive(Debug, Deserialize)]
pub(crate) struct DeviceCode {
    /// The opaque device code the token poll exchanges.
    pub(crate) device_code: String,
    /// The short human code the user enters at `verification_uri`.
    pub(crate) user_code: String,
    /// The URL the user visits to enter `user_code`.
    pub(crate) verification_uri: String,
    /// The minimum seconds between token polls, when the server specifies one.
    pub(crate) interval: Option<u64>,
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

/// Join an issuer base URL and a path into one URL, tolerating either a
/// trailing slash on the issuer or a leading slash on the path.
#[must_use]
pub(crate) fn join_url(issuer: &str, path: &str) -> String {
    let issuer = issuer.trim_end_matches('/');
    if path.starts_with('/') {
        format!("{issuer}{path}")
    } else {
        format!("{issuer}/{path}")
    }
}

/// Append a form-encoded query to an authorization endpoint, choosing `&` when
/// the endpoint already carries a query string. The one place the
/// provider and MCP flows build an authorize URL, so the separator and encoding
/// rules cannot drift between them.
#[must_use]
pub(crate) fn authorize_url(endpoint: &str, params: &[(String, String)]) -> String {
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
fn agent() -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(30)))
        .http_status_as_error(false)
        .max_redirects(0)
        .build();
    ureq::Agent::new_with_config(config)
}

/// Percent-encode form parameters into `application/x-www-form-urlencoded`,
/// leaving the RFC 3986 unreserved set untouched and encoding a space as
/// `%20`.
#[must_use]
pub(crate) fn form_encode(params: &[(String, String)]) -> String {
    let mut out = String::new();
    for (i, (key, value)) in params.iter().enumerate() {
        if i > 0 {
            out.push('&');
        }
        encode_into(&mut out, key);
        out.push('=');
        encode_into(&mut out, value);
    }
    out
}

/// Append `value` percent-encoded per `application/x-www-form-urlencoded`.
fn encode_into(out: &mut String, value: &str) {
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(char::from(byte));
            }
            b' ' => out.push_str("%20"),
            _ => {
                out.push('%');
                out.push(hex_digit(byte >> 4));
                out.push(hex_digit(byte & 0x0f));
            }
        }
    }
}

/// The uppercase hex digit for a nibble.
fn hex_digit(nibble: u8) -> char {
    char::from_digit(u32::from(nibble & 0x0f), 16).map_or('0', |c| c.to_ascii_uppercase())
}

/// Send a POST body with the given content type, returning the raw response.
fn post(
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
        access_token,
        refresh_token: parsed.refresh_token,
        id_token: parsed.id_token,
        token_type: parsed.token_type,
        scope: parsed.scope,
    })
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
    let url = join_url(issuer, token_path);
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
            let pairs: Vec<(String, String)> = params
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect();
            post(
                &agent,
                &url,
                "application/x-www-form-urlencoded",
                &form_encode(&pairs),
            )?
        }
        choreo_proto::OAuthGrantEncoding::Json => {
            let object = serde_json::json!({
                "grant_type": "authorization_code",
                "code": code,
                "redirect_uri": redirect_uri,
                "client_id": client_id,
                "code_verifier": code_verifier,
            });
            post(&agent, &url, "application/json", &object.to_string())?
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
    let url = join_url(issuer, device_authorization_path);
    let pairs = vec![
        ("client_id".to_string(), client_id.to_string()),
        ("scope".to_string(), scope.to_string()),
    ];
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
        device_code,
        user_code,
        verification_uri,
        interval: parsed.interval,
    })
}

/// The device-authorization response body (success or error shape).
#[derive(Debug, Deserialize)]
struct DeviceCodeResponse {
    device_code: Option<String>,
    user_code: Option<String>,
    verification_uri: Option<String>,
    interval: Option<u64>,
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
    let url = join_url(issuer, device_token_path);
    let pairs = vec![
        (
            "grant_type".to_string(),
            "urn:ietf:params:oauth:grant-type:device_code".to_string(),
        ),
        ("device_code".to_string(), device_code.to_string()),
        ("client_id".to_string(), client_id.to_string()),
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
            access_token,
            refresh_token: parsed.refresh_token,
            id_token: parsed.id_token,
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
        let params = vec![("state".to_string(), "s".to_string())];
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
    fn join_url_handles_slashes() {
        assert_eq!(
            join_url("https://auth.openai.com", "/oauth/token"),
            "https://auth.openai.com/oauth/token"
        );
        assert_eq!(
            join_url("https://auth.openai.com/", "oauth/token"),
            "https://auth.openai.com/oauth/token"
        );
        assert_eq!(
            join_url("https://auth.openai.com/", "/oauth/token"),
            "https://auth.openai.com/oauth/token"
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
    fn form_encoding_matches_urlencoded_rules() {
        let pairs = vec![
            ("grant_type".to_string(), "authorization_code".to_string()),
            ("scope".to_string(), "openid offline_access".to_string()),
        ];
        assert_eq!(
            form_encode(&pairs),
            "grant_type=authorization_code&scope=openid%20offline_access"
        );
    }

    #[test]
    fn form_encoding_escapes_reserved_bytes() {
        let pairs = vec![("state".to_string(), "a+b/c=d".to_string())];
        assert_eq!(form_encode(&pairs), "state=a%2Bb%2Fc%3Dd");
    }
}
