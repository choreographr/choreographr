//! Blocking provider OAuth **refresh** client.
//!
//! A provider OAuth grant is refreshed by presenting its refresh token to the
//! authorization server's token endpoint (RFC 6749 §6). This module owns that
//! one exchange: the request serialization (form vs JSON, per the catalog
//! profile's `refresh_encoding`), the response parse (rotate-if-present — the
//! returned `refresh_token` is adopted when present and omitted otherwise), and
//! the **failure classification** that drives the caller's terminal-vs-transient
//! decision. It stays synchronous and daemon-free: no sessions, no keystore, no
//! daemon types — the caller (the daemon's session thread) supplies the token
//! endpoint and the durable grant, and persists the result.
//!
//! The access token stays ephemeral (the caller caches it in memory only); the
//! returned `refresh_token` is the rotated durable root the caller re-encrypts
//! into the credential blob. `Display`/`Debug` are not derived on the token
//! response — the token fields are already wrapped in [`Zeroizing`] and the
//! caller never logs them.

use serde::Deserialize;
use zeroize::Zeroizing;

use crate::catalog::GrantEncoding;

/// The terminal OAuth error codes that mean the grant is dead: the refresh
/// token was rejected, revoked, or already spent. A caller must re-login (or
/// re-authorize); retrying cannot succeed. These are the codes the caller
/// classifies as terminal — any other rejection is still terminal, but these
/// are named for the documented taxonomy.
const TERMINAL_ERROR_CODES: &[&str] = &["invalid_grant", "invalid_token", "refresh_token_reused"];

/// A provider OAuth refresh request: everything the exchange needs, derived
/// from the stored credential and the provider's catalog `oauth` profile.
///
/// The `token_endpoint` is the absolute token URL (issuer + token path); the
/// `encoding` is the profile's `refresh_encoding`. `client_secret` is present
/// only for a confidential client.
#[derive(Clone)]
pub struct OAuthRefreshRequest<'a> {
    /// The absolute token endpoint URL.
    pub token_endpoint: &'a str,
    /// The request-body encoding the endpoint expects.
    pub encoding: GrantEncoding,
    /// The client id recorded at login.
    pub client_id: Option<&'a str>,
    /// The client secret, for a confidential client.
    pub client_secret: Option<&'a str>,
    /// The refresh token to present (the durable grant root).
    pub refresh_token: &'a str,
    /// The scopes to request; empty to omit the `scope` parameter.
    pub scopes: &'a [String],
}

/// Redacting `Debug`: the refresh token and any client secret are secret, so a
/// logged request must not expose them. Only the non-secret routing fields and
/// presence flags are printed.
impl std::fmt::Debug for OAuthRefreshRequest<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuthRefreshRequest")
            .field("token_endpoint", &self.token_endpoint)
            .field("encoding", &self.encoding)
            .field("has_client_id", &self.client_id.is_some())
            .field("has_client_secret", &self.client_secret.is_some())
            .field("has_refresh_token", &(!self.refresh_token.is_empty()))
            .field("scopes", &self.scopes.len())
            .finish()
    }
}

/// The token set a successful refresh returned.
///
/// Every secret field is wipe-on-drop. Only the access token is required by the
/// spec; a rotating provider also returns a fresh `refresh_token`, and an OIDC
/// provider may return a new `id_token` (from which the `ChatGPT` account claim
/// is re-extracted). Rotation is **rotate-if-present**: the caller adopts
/// `refresh_token` when `Some` and keeps the stored one when `None`.
pub struct OAuthTokenResponse {
    /// The new bearer access token.
    pub access_token: Zeroizing<String>,
    /// The rotated refresh token, when the server issued one.
    pub refresh_token: Option<Zeroizing<String>>,
    /// The raw OIDC `id_token`, when the server issued one. Not stored by the
    /// caller — only the extracted claim is — and wipe-on-drop like the other
    /// token fields.
    pub id_token: Option<Zeroizing<String>>,
    /// The access token's lifetime in seconds, when the server reported one.
    pub expires_in: Option<u64>,
    /// The bearer token type (almost always `Bearer`).
    pub token_type: Option<String>,
    /// The `ChatGPT` account id re-extracted from a new `id_token`, when one was
    /// returned and carried the claim. `None` when no `id_token` was returned,
    /// so the caller keeps the stored claim.
    pub account_id: Option<String>,
}

/// Redacting `Debug`: the access/refresh tokens are secret and the account id
/// is a sensitive claim, so a logged token response must not expose them; only
/// presence flags are printed.
impl std::fmt::Debug for OAuthTokenResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuthTokenResponse")
            .field("access_token", &"***")
            .field("has_refresh_token", &self.refresh_token.is_some())
            .field("has_id_token", &self.id_token.is_some())
            .field("expires_in", &self.expires_in)
            .field("token_type", &self.token_type)
            .field("has_account_id", &self.account_id.is_some())
            .finish()
    }
}

/// A refresh failure, classified so the caller can act on it.
#[derive(Debug, thiserror::Error)]
pub enum OAuthRefreshError {
    /// A terminal failure (`invalid_grant` / `invalid_token` /
    /// `refresh_token_reused`, or any other rejection that retrying cannot
    /// clear): the grant is dead and the user must sign in again.
    #[error("oauth grant rejected: {detail}")]
    Terminal {
        /// The authorization server's error code and description, when
        /// supplied (never a token).
        detail: String,
    },
    /// A transient failure (transport error, `429`, or `5xx`): the caller
    /// should back off and retry the refresh.
    #[error("oauth refresh temporarily failed: {detail}")]
    Transient {
        /// A short description of the transient failure.
        detail: String,
    },
    /// The token endpoint is neither `https` nor a loopback `http` address, so
    /// the refresh token would be sent in cleartext. This is a permanent fault
    /// of the stored credential (retrying cannot fix the scheme), so the caller
    /// treats it as terminal: the user must re-login against a secure endpoint.
    #[error("refusing an insecure OAuth token endpoint: {endpoint}")]
    InsecureEndpoint {
        /// The offending endpoint URL (routing information, not a secret).
        endpoint: String,
    },
}

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
    let mut call = agent.post(request.token_endpoint);
    call = match request.encoding {
        GrantEncoding::Form => call.header("Content-Type", "application/x-www-form-urlencoded"),
        GrantEncoding::Json => call.header("Content-Type", "application/json"),
    };
    let response = call.send(body.as_slice()).map_err(|e| {
        // Transport errors (connect/read/timeout) are transient: retry.
        OAuthRefreshError::Transient {
            detail: e.to_string(),
        }
    })?;

    let status = response.status().as_u16();
    let body_text = response.into_body().read_to_string().unwrap_or_default();
    parse_token_response(status, body_text.as_str())
}

/// Serialize a refresh request body per the endpoint's encoding.
///
/// Kept separate from the HTTP call so the form-vs-JSON contract is unit
/// testable without a socket. The returned body holds the refresh token, so it
/// is wipe-on-drop.
fn encode_refresh_body(request: &OAuthRefreshRequest<'_>) -> Zeroizing<Vec<u8>> {
    // The spec's `scope` parameter is a space-delimited list; omitted when empty
    // (an empty scope asks for none and some servers reject it).
    let scope = request.scopes.join(" ");
    match request.encoding {
        GrantEncoding::Form => {
            let mut serializer = url::form_urlencoded::Serializer::new(String::new());
            serializer.append_pair("grant_type", "refresh_token");
            serializer.append_pair("refresh_token", request.refresh_token);
            if let Some(client_id) = request.client_id {
                serializer.append_pair("client_id", client_id);
            }
            if let Some(client_secret) = request.client_secret {
                serializer.append_pair("client_secret", client_secret);
            }
            if !scope.is_empty() {
                serializer.append_pair("scope", &scope);
            }
            Zeroizing::new(serializer.finish().into_bytes())
        }
        GrantEncoding::Json => {
            // Build the JSON object field-by-field; `serde_json::json!` would
            // copy the secrets into an intermediate `Value`, so a small manual
            // map keeps them in one place that zeroizes.
            let mut map = serde_json::Map::new();
            map.insert("grant_type".into(), "refresh_token".into());
            map.insert("refresh_token".into(), request.refresh_token.into());
            if let Some(client_id) = request.client_id {
                map.insert("client_id".into(), client_id.into());
            }
            if let Some(client_secret) = request.client_secret {
                map.insert("client_secret".into(), client_secret.into());
            }
            if !scope.is_empty() {
                map.insert("scope".into(), scope.into());
            }
            Zeroizing::new(
                serde_json::to_vec(&serde_json::Value::Object(map)).unwrap_or_else(|_| Vec::new()),
            )
        }
    }
}

/// The token-endpoint success and error response shape. A single struct with
/// every field optional so one deserialize covers both a token reply and an
/// error reply.
#[derive(Deserialize, Default)]
struct RawTokenResponse {
    access_token: Option<String>,
    refresh_token: Option<String>,
    id_token: Option<String>,
    expires_in: Option<u64>,
    token_type: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

/// Parse the token endpoint's response: a 2xx body yields a token set, anything
/// else a classified error. Split from the transport so the classify contract is
/// unit testable.
fn parse_token_response(status: u16, body: &str) -> Result<OAuthTokenResponse, OAuthRefreshError> {
    // A body that is not the expected JSON (an HTML error page, an empty body)
    // degrades to all-`None` rather than failing the parse: the status still
    // decides terminal-vs-transient, and a missing `access_token` on a 2xx is
    // classified transient below.
    let parsed: RawTokenResponse = serde_json::from_str(body).unwrap_or_default();

    if (200..300).contains(&status) {
        // A success status with a token: the access token is the one required
        // field. A "success" without one is odd but transient — the caller
        // retries rather than treating it as a dead grant.
        let Some(access_token) = parsed.access_token else {
            return Err(OAuthRefreshError::Transient {
                detail: format!("token endpoint returned {status} without an access_token"),
            });
        };
        let account_id = parsed
            .id_token
            .as_deref()
            .and_then(chatgpt_account_id_from_id_token);
        return Ok(OAuthTokenResponse {
            access_token: Zeroizing::new(access_token),
            refresh_token: parsed.refresh_token.map(Zeroizing::new),
            id_token: parsed.id_token.map(Zeroizing::new),
            expires_in: parsed.expires_in,
            token_type: parsed.token_type,
            account_id,
        });
    }

    // Non-2xx: 429 and 5xx are transient (back off and retry); any other 4xx is
    // terminal (retrying cannot clear it). The `error`/`error_description` pair
    // is the only part of the body retained — never a token or the full body.
    let detail =
        classify_error_detail(parsed.error.as_deref(), parsed.error_description.as_deref());
    if status == 429 || (500..600).contains(&status) {
        Err(OAuthRefreshError::Transient {
            detail: format!("status {status}: {detail}"),
        })
    } else {
        Err(OAuthRefreshError::Terminal {
            detail: format!("status {status}: {detail}"),
        })
    }
}

/// Build a short, token-free detail string from the error pair, naming the
/// terminal codes explicitly so the log/metric reads them.
fn classify_error_detail(error: Option<&str>, description: Option<&str>) -> String {
    let code = error.unwrap_or("unknown_error");
    let tag = if TERMINAL_ERROR_CODES.contains(&code) {
        "terminal"
    } else {
        "rejected"
    };
    match description {
        Some(desc) => format!("{tag} {code}: {desc}"),
        None => format!("{tag} {code}"),
    }
}

// The `ChatGPT` account-id decoder is shared with the login library, so a
// rotated `id_token` is read through the same namespace decode; re-exported
// here so callers keep using `choreo_ai_protocols::oauth::…`.
pub use choreo_shared::oauth_claims::chatgpt_account_id_from_id_token;

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;

    fn request(encoding: GrantEncoding) -> OAuthRefreshRequest<'static> {
        OAuthRefreshRequest {
            token_endpoint: "https://auth.example/token",
            encoding,
            client_id: Some("client-abc"),
            client_secret: None,
            refresh_token: "refresh-xyz",
            scopes: &[],
        }
    }

    #[test]
    fn form_encoding_serializes_a_url_encoded_body() {
        let body = encode_refresh_body(&request(GrantEncoding::Form));
        let text = std::str::from_utf8(&body).expect("utf-8 body");
        assert!(text.contains("grant_type=refresh_token"), "{text}");
        assert!(text.contains("refresh_token=refresh-xyz"), "{text}");
        assert!(text.contains("client_id=client-abc"), "{text}");
        // No scope parameter when the scope list is empty.
        assert!(!text.contains("scope="), "{text}");
    }

    #[test]
    fn form_encoding_includes_scope_and_secret_when_present() {
        let scopes = vec!["openid".to_string(), "offline_access".to_string()];
        let req = OAuthRefreshRequest {
            client_secret: Some("shh"),
            scopes: &scopes,
            ..request(GrantEncoding::Form)
        };
        let body = encode_refresh_body(&req);
        let text = std::str::from_utf8(&body).expect("utf-8 body");
        assert!(text.contains("client_secret=shh"), "{text}");
        // Scope is space-delimited and URL-encoded ('+' or '%20').
        assert!(
            text.contains("scope=openid+offline_access")
                || text.contains("scope=openid%20offline_access"),
            "{text}"
        );
    }

    #[test]
    fn json_encoding_serializes_a_json_object() {
        let body = encode_refresh_body(&request(GrantEncoding::Json));
        let value: serde_json::Value = serde_json::from_slice(&body).expect("json body");
        assert_eq!(value["grant_type"], "refresh_token");
        assert_eq!(value["refresh_token"], "refresh-xyz");
        assert_eq!(value["client_id"], "client-abc");
        assert!(value.get("scope").is_none());
    }

    #[test]
    fn success_response_parses_and_rotates_if_present() {
        let body = r#"{
            "access_token": "at-1",
            "refresh_token": "rt-2",
            "expires_in": 3600,
            "token_type": "Bearer"
        }"#;
        let tokens = parse_token_response(200, body).expect("parses");
        assert_eq!(tokens.access_token.as_str(), "at-1");
        assert_eq!(
            tokens.refresh_token.as_deref().map(String::as_str),
            Some("rt-2")
        );
        assert_eq!(tokens.expires_in, Some(3600));
        assert_eq!(tokens.token_type.as_deref(), Some("Bearer"));
    }

    #[test]
    fn success_response_without_a_refresh_token_keeps_none() {
        let body = r#"{"access_token": "at-1", "expires_in": 60}"#;
        let tokens = parse_token_response(200, body).expect("parses");
        assert!(tokens.refresh_token.is_none());
    }

    #[test]
    fn terminal_error_codes_are_terminal() {
        for code in ["invalid_grant", "invalid_token", "refresh_token_reused"] {
            let body = format!(r#"{{"error": "{code}", "error_description": "nope"}}"#);
            let err = parse_token_response(400, &body).expect_err("must fail");
            assert!(
                matches!(err, OAuthRefreshError::Terminal { .. }),
                "{code} must be terminal, got {err:?}"
            );
        }
    }

    #[test]
    fn rate_limit_and_server_errors_are_transient() {
        for status in [429, 500, 502, 503] {
            let err = parse_token_response(status, "{}").expect_err("must fail");
            assert!(
                matches!(err, OAuthRefreshError::Transient { .. }),
                "status {status} must be transient, got {err:?}"
            );
        }
    }

    #[test]
    fn other_client_errors_are_terminal() {
        let body = r#"{"error": "invalid_request"}"#;
        let err = parse_token_response(400, body).expect_err("must fail");
        assert!(matches!(err, OAuthRefreshError::Terminal { .. }));
    }

    #[test]
    fn success_without_access_token_is_transient() {
        let err = parse_token_response(200, r#"{"token_type":"Bearer"}"#).expect_err("must fail");
        assert!(matches!(err, OAuthRefreshError::Transient { .. }));
    }

    #[test]
    fn account_id_is_extracted_from_a_new_id_token() {
        let claims = serde_json::json!({
            "https://api.openai.com/auth": { "chatgpt_account_id": "acct_9" }
        });
        let id_token = jwt(&claims);
        let body =
            format!(r#"{{"access_token":"at","refresh_token":"rt","id_token":"{id_token}"}}"#);
        let tokens = parse_token_response(200, &body).expect("parses");
        assert_eq!(tokens.account_id.as_deref(), Some("acct_9"));
    }

    #[test]
    fn account_id_is_none_without_an_id_token() {
        let tokens = parse_token_response(200, r#"{"access_token":"at"}"#).expect("parses");
        assert!(tokens.account_id.is_none());
    }

    #[test]
    fn malformed_id_token_yields_no_account_id() {
        assert!(chatgpt_account_id_from_id_token("not-a-jwt").is_none());
        assert!(chatgpt_account_id_from_id_token("a.b").is_none());
        assert!(chatgpt_account_id_from_id_token("a.b.c.d").is_none());
    }

    fn jwt(claims: &serde_json::Value) -> String {
        let header = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#);
        let payload =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(claims.to_string().as_bytes());
        format!("{header}.{payload}.sig")
    }
}
