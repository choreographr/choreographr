//! The OAuth token-endpoint response shape: the one read of a token reply.
//!
//! Both the frontend login library (`choreo-client-core`) and the runtime refresh
//! client (`choreo-ai-protocols`) POST to an authorization server's token endpoint
//! and must read back the same reply — a token set on success, or an
//! `error`/`error_description` pair on failure. This module is that read, so the
//! two paths cannot drift into different interpretations of the same body. It
//! sits beside [`oauth_claims`](crate::oauth_claims) (the `id_token` claim decode)
//! and [`oauth_endpoint`](crate::oauth_endpoint) (the endpoint-security policy) as
//! the third shared OAuth helper.
//!
//! The shape is deliberately lenient: every field is optional and a body that is
//! not the expected JSON degrades to all-`None` rather than failing the parse, so
//! the caller's status-based classification still decides terminal-vs-transient.
//! The `expires_in` field additionally tolerates a provider that emits it as a
//! string, because a parse failure there would drop the freshly minted tokens and
//! force a re-refresh on every request.

use serde::Deserialize;

/// An OAuth token-endpoint response body.
///
/// Every field is optional so one deserialize covers both a success reply (the
/// token set) and an `error`/`error_description` reply: the caller keys its
/// terminal-vs-transient decision on the HTTP status and only reads the fields
/// the status makes meaningful.
#[derive(Debug, Default, Deserialize)]
pub struct TokenResponse {
    /// The new bearer access token, present on a success reply.
    pub access_token: Option<String>,
    /// A rotated refresh token, when the server issued one.
    pub refresh_token: Option<String>,
    /// The raw OIDC `id_token`, when the server issued one.
    pub id_token: Option<String>,
    /// The bearer token type (almost always `Bearer`).
    pub token_type: Option<String>,
    /// The granted scopes, when the server echoed them.
    pub scope: Option<String>,
    /// The access token's lifetime in seconds, when the server reported one. Read
    /// leniently — as a number or a numeric string — so a deviant provider does
    /// not fail the whole parse.
    #[serde(default, deserialize_with = "deserialize_expires_in")]
    pub expires_in: Option<u64>,
    /// The RFC 6749 error code on a failure reply.
    pub error: Option<String>,
    /// The human-readable failure description, when supplied.
    pub error_description: Option<String>,
}

/// Parse a token-endpoint response body.
///
/// A body that is not the expected JSON (an HTML error page, an empty body)
/// degrades to all-`None` rather than failing the parse: the status still decides
/// terminal-vs-transient, and a missing `access_token` on a `2xx` is treated as
/// transient by the caller.
#[must_use]
pub fn parse(body: &str) -> TokenResponse {
    serde_json::from_str(body).unwrap_or_default()
}

/// Deserialize `expires_in` from either a JSON number or a numeric string.
///
/// A provider that emits `expires_in` as a string (e.g. `"3600"`) must not make
/// the whole struct fail to deserialize — a failure would drop the freshly minted
/// tokens and force a re-refresh on every request. Any other shape (`null`, a
/// bool, a non-numeric string, an object) yields `None` so the token set still
/// parses.
fn deserialize_expires_in<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    Ok(match value {
        Some(serde_json::Value::Number(n)) => n.as_u64(),
        Some(serde_json::Value::String(s)) => s.trim().parse::<u64>().ok(),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_json_success_body_parses() {
        let body = r#"{
            "access_token": "at-1",
            "refresh_token": "rt-2",
            "id_token": "idt",
            "token_type": "Bearer",
            "scope": "openid profile",
            "expires_in": 3600
        }"#;
        let parsed = parse(body);
        assert_eq!(parsed.access_token.as_deref(), Some("at-1"));
        assert_eq!(parsed.refresh_token.as_deref(), Some("rt-2"));
        assert_eq!(parsed.id_token.as_deref(), Some("idt"));
        assert_eq!(parsed.token_type.as_deref(), Some("Bearer"));
        assert_eq!(parsed.scope.as_deref(), Some("openid profile"));
        assert_eq!(parsed.expires_in, Some(3600));
    }

    #[test]
    fn expires_in_given_as_a_string_still_parses() {
        let body = r#"{"access_token": "at-1", "expires_in": "3600"}"#;
        let parsed = parse(body);
        // The tokens survive; only a strict number parse would have dropped them.
        assert_eq!(parsed.access_token.as_deref(), Some("at-1"));
        assert_eq!(parsed.expires_in, Some(3600));
    }

    #[test]
    fn garbage_bodies_yield_the_default() {
        for body in ["", "not json", "null", r#"{"expires_in": null}"#] {
            let parsed = parse(body);
            assert!(parsed.access_token.is_none(), "body: {body:?}");
            assert!(parsed.expires_in.is_none(), "body: {body:?}");
        }
    }

    #[test]
    fn error_reply_parses_the_error_pair() {
        let body = r#"{"error": "invalid_grant", "error_description": "the grant is dead"}"#;
        let parsed = parse(body);
        assert_eq!(parsed.error.as_deref(), Some("invalid_grant"));
        assert_eq!(
            parsed.error_description.as_deref(),
            Some("the grant is dead")
        );
        assert!(parsed.access_token.is_none());
    }

    #[test]
    fn non_numeric_expires_in_shapes_yield_none() {
        for body in [
            r#"{"access_token":"at","expires_in":true}"#,
            r#"{"access_token":"at","expires_in":"soon"}"#,
            r#"{"access_token":"at","expires_in":{}}"#,
        ] {
            let parsed = parse(body);
            assert_eq!(parsed.access_token.as_deref(), Some("at"), "body: {body:?}");
            assert!(parsed.expires_in.is_none(), "body: {body:?}");
        }
    }
}
