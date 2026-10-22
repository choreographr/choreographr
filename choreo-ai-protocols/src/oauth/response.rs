//! The OAuth refresh **response** side: the token set a successful exchange
//! returns, the classified failure type, and the parse/classify contract.
//!
//! A 2xx body yields an [`OAuthTokenResponse`]; anything else yields an
//! [`OAuthRefreshError`] classified terminal-vs-transient. The token set is the
//! one shared projection (`choreo_shared::oauth_token::TokenSet`), so its
//! wipe-on-drop token fields and its redacting `Debug` are defined in one place
//! rather than here. The parse is split from the transport so the classify
//! contract is unit testable.

// The token-endpoint response shape and its owned projection are the one read
// both the login library and this refresh client perform, so a token reply
// cannot drift in interpretation between the two paths.
use choreo_shared::oauth_token::{TokenResponse, parse};

/// The terminal OAuth error codes the catalog documents as "the grant is dead":
/// the refresh token was rejected, revoked, or already spent.
///
/// This constant does NOT decide the terminal-vs-transient classification — the
/// caller's decision keys on the HTTP status (a `3xx`, `429`, or `5xx` is
/// transient, and any other `4xx` is terminal). It is used only by
/// [`classify_error_detail`] to pick the `terminal` vs `rejected` tag in the
/// log-only detail string, so a documented terminal code reads distinctly from
/// an arbitrary rejection.
const TERMINAL_ERROR_CODES: &[&str] = &["invalid_grant", "invalid_token", "refresh_token_reused"];

/// The token set a successful refresh returned.
///
/// This is the shared projection (`choreo_shared::oauth_token::TokenSet`) the
/// login library also builds, so which fields a reply yields — and their
/// wipe-on-drop and redacting-`Debug` rules — is defined in one place. Only the
/// access token is required by the spec; a rotating provider also returns a fresh
/// `refresh_token`, and an OIDC provider may return a new `id_token` (from which
/// the identity claim is re-extracted by the caller that persists it). Rotation
/// is **rotate-if-present**: the caller adopts `refresh_token` when `Some` and
/// keeps the stored one when `None`.
pub type OAuthTokenResponse = choreo_shared::oauth_token::TokenSet;

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

/// Parse the token endpoint's response: a 2xx body yields a token set, anything
/// else a classified error. Split from the transport so the classify contract is
/// unit testable. Visible to the parent module so `refresh_oauth_token` can hand
/// it the status and body.
pub(super) fn parse_token_response(
    status: u16,
    body: &str,
) -> Result<OAuthTokenResponse, OAuthRefreshError> {
    // The shared `choreo_shared::oauth_token` parse: every field is optional, a
    // body that is not the expected JSON degrades to all-`None` (the status still
    // decides terminal-vs-transient), and a `expires_in` emitted as a string is
    // read leniently rather than dropping the freshly minted tokens.
    let parsed: TokenResponse = parse(body);

    if (200..300).contains(&status) {
        // A success status with a token: the access token is the one required
        // field. A "success" without one is odd but transient — the caller
        // retries rather than treating it as a dead grant. The identity claim is
        // deliberately not extracted here: whoever persists the claim derives it
        // from the returned `id_token` through the shared decode, so this parse
        // stays a pure status → token-set projection.
        return OAuthTokenResponse::from_response(parsed).ok_or_else(|| {
            OAuthRefreshError::Transient {
                detail: format!("token endpoint returned {status} without an access_token"),
            }
        });
    }

    // Non-2xx: a 3xx (the agent follows no redirects, so a redirect is returned
    // here), 429, and 5xx are transient (back off and retry); any other 4xx is
    // terminal (retrying cannot clear it). The `error`/`error_description` pair
    // is the only part of the body retained — never a token or the full body.
    let detail =
        classify_error_detail(parsed.error.as_deref(), parsed.error_description.as_deref());
    if (300..400).contains(&status) || status == 429 || (500..600).contains(&status) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;
    // The identity-claim decoder the persisting caller uses; the tests exercise
    // the same derivation from the returned token set.
    use choreo_shared::oauth_claims::chatgpt_account_id_from_id_token;

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
    fn success_response_with_a_string_expires_in_still_yields_tokens() {
        // A provider that emits `expires_in` as a string must not drop the token
        // set (a strict parse would discard the freshly minted tokens and force a
        // re-refresh on every request). The shared lenient parse reads it as a
        // number.
        let body = r#"{"access_token":"at-1","expires_in":"3600"}"#;
        let tokens = parse_token_response(200, body).expect("parses");
        assert_eq!(tokens.access_token.as_str(), "at-1");
        assert_eq!(tokens.expires_in, Some(3600));
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
    fn a_redirect_is_transient_not_terminal() {
        // The agent follows no redirects, so a 3xx reaches the classifier. It is
        // transient — a misconfigured/re-pointed endpoint must not force the user
        // through a needless re-login.
        for status in [301, 302, 307, 308] {
            let err = parse_token_response(status, "").expect_err("must fail");
            assert!(
                matches!(err, OAuthRefreshError::Transient { .. }),
                "status {status} must be transient, got {err:?}"
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
        // The parse carries the raw `id_token`; the claim is derived from it by
        // the caller that persists it, through the one shared decode.
        let account_id = tokens
            .id_token
            .as_deref()
            .and_then(|id_token| chatgpt_account_id_from_id_token(id_token.as_str()));
        assert_eq!(account_id.as_deref(), Some("acct_9"));
    }

    #[test]
    fn account_id_is_none_without_an_id_token() {
        let tokens = parse_token_response(200, r#"{"access_token":"at"}"#).expect("parses");
        assert!(tokens.id_token.is_none());
        let account_id = tokens
            .id_token
            .as_deref()
            .and_then(|id_token| chatgpt_account_id_from_id_token(id_token.as_str()));
        assert!(account_id.is_none());
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
