//! The OAuth token-endpoint response shape: the one read of a token reply.
//!
//! Both the frontend login library (`choreo-client-core`) and the runtime refresh
//! client (`choreo-ai-protocols`) POST to an authorization server's token endpoint
//! and must read back the same reply — a token set on success, or an
//! `error`/`error_description` pair on failure. This module is that read, so the
//! two paths cannot drift into different interpretations of the same body. It
//! also owns the owned projection of that read ([`TokenSet`]), so which fields
//! are wipe-on-drop and which the redacting `Debug` withholds is decided here
//! once, not per caller. It sits beside [`oauth_claims`](crate::oauth_claims) (the
//! `id_token` claim decode) and [`oauth_endpoint`](crate::oauth_endpoint) (the
//! endpoint-security policy) as the third shared OAuth helper.
//!
//! The shape is deliberately lenient: every field is optional and a body that is
//! not the expected JSON degrades to all-`None` rather than failing the parse, so
//! the caller's status-based classification still decides terminal-vs-transient.
//! The `expires_in` field additionally tolerates a provider that emits it as a
//! string, because a parse failure there would drop the freshly minted tokens and
//! force a re-refresh on every request.

use serde::Deserialize;
use zeroize::Zeroizing;

/// An OAuth token-endpoint response body.
///
/// Every field is optional so one deserialize covers both a success reply (the
/// token set) and an `error`/`error_description` reply: the caller keys its
/// terminal-vs-transient decision on the HTTP status and only reads the fields
/// the status makes meaningful.
#[derive(Default, Deserialize)]
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

/// Redacting `Debug`: the struct holds the freshly minted access/refresh/id
/// tokens, so a logged response must not expose them. Hand-written (not derived)
/// because this type sits on the shared login/refresh boundary — one stray
/// `{:?}` would leak a live token — under the same rule [`TokenSet`] applies.
/// Only presence and the non-secret provider-error fields are printed.
impl std::fmt::Debug for TokenResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenResponse")
            .field(
                "access_token",
                &self.access_token.as_ref().map(|_| "<redacted>"),
            )
            .field("refresh_token", &self.refresh_token.is_some())
            .field("id_token", &self.id_token.is_some())
            .field("token_type", &self.token_type)
            .field("scope", &self.scope)
            .field("expires_in", &self.expires_in)
            .field("error", &self.error)
            .field("error_description", &self.error_description)
            .finish()
    }
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

/// The owned token set a successful token-endpoint reply projects to.
///
/// This is the one projection of a [`TokenResponse`] into the values a caller
/// keeps: the login library and the runtime refresh client both build it via
/// [`TokenSet::from_response`], so the two paths cannot disagree about which
/// fields a reply yields. The access, refresh, and id tokens are wipe-on-drop
/// ([`Zeroizing`]) and the hand-written `Debug` reports only their presence,
/// never their value, so a token can never reach a log line by way of a `{:?}`.
pub struct TokenSet {
    /// The new bearer access token.
    pub access_token: Zeroizing<String>,
    /// A rotated refresh token, when the server issued one. Rotation is
    /// rotate-if-present: an absent value means the caller keeps its stored one.
    pub refresh_token: Option<Zeroizing<String>>,
    /// The raw OIDC `id_token` (a JWT), when the server issued one. The identity
    /// claim is decoded from it by whichever caller persists that claim.
    pub id_token: Option<Zeroizing<String>>,
    /// The bearer token type (almost always `Bearer`).
    pub token_type: Option<String>,
    /// The granted scopes, when the server echoed them.
    pub scope: Option<String>,
    /// The access token's lifetime in seconds, when the server reported one.
    pub expires_in: Option<u64>,
}

/// Redacting `Debug`: the set holds the freshly minted access/refresh/id tokens,
/// so a logged token set must not expose them. Hand-written (not derived) because
/// one stray `{:?}` would leak a live token; only presence flags and the
/// non-secret fields are printed.
impl std::fmt::Debug for TokenSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenSet")
            .field("access_token", &"<redacted>")
            .field("refresh_token", &self.refresh_token.is_some())
            .field("id_token", &self.id_token.is_some())
            .field("token_type", &self.token_type)
            .field("scope", &self.scope)
            .field("expires_in", &self.expires_in)
            .finish()
    }
}

impl TokenSet {
    /// Project a parsed token reply into an owned token set, or `None` when the
    /// reply carried no access token (an OAuth error body, or an empty one).
    ///
    /// The three token fields are wrapped in [`Zeroizing`] so they are wiped on
    /// drop; the non-secret fields are carried verbatim.
    #[must_use]
    pub fn from_response(parsed: TokenResponse) -> Option<Self> {
        let access_token = parsed.access_token?;
        Some(Self {
            access_token: Zeroizing::new(access_token),
            refresh_token: parsed.refresh_token.map(Zeroizing::new),
            id_token: parsed.id_token.map(Zeroizing::new),
            token_type: parsed.token_type,
            scope: parsed.scope,
            expires_in: parsed.expires_in,
        })
    }
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
    fn debug_never_prints_a_token_value() {
        let body = r#"{
            "access_token": "at-secret",
            "refresh_token": "rt-secret",
            "id_token": "idt-secret",
            "token_type": "Bearer",
            "error": "invalid_grant"
        }"#;
        let debug = format!("{:?}", parse(body));
        for secret in ["at-secret", "rt-secret", "idt-secret"] {
            assert!(!debug.contains(secret), "Debug leaked {secret}: {debug}");
        }
        // Presence flags and the non-secret fields still render.
        assert!(debug.contains("<redacted>"), "{debug}");
        assert!(debug.contains("refresh_token: true"), "{debug}");
        assert!(debug.contains("invalid_grant"), "{debug}");
    }

    #[test]
    fn token_set_projects_every_field_of_a_success_reply() {
        let body = r#"{
            "access_token": "at-1",
            "refresh_token": "rt-2",
            "id_token": "idt",
            "token_type": "Bearer",
            "scope": "openid profile",
            "expires_in": 3600
        }"#;
        let tokens = TokenSet::from_response(parse(body)).expect("access token present");
        assert_eq!(tokens.access_token.as_str(), "at-1");
        assert_eq!(
            tokens.refresh_token.as_deref().map(String::as_str),
            Some("rt-2")
        );
        assert_eq!(tokens.id_token.as_deref().map(String::as_str), Some("idt"));
        assert_eq!(tokens.token_type.as_deref(), Some("Bearer"));
        assert_eq!(tokens.scope.as_deref(), Some("openid profile"));
        assert_eq!(tokens.expires_in, Some(3600));
    }

    #[test]
    fn token_set_is_none_without_an_access_token() {
        // An OAuth error body carries no token set at all.
        assert!(TokenSet::from_response(parse(r#"{"error":"invalid_grant"}"#)).is_none());
        assert!(TokenSet::from_response(parse("")).is_none());
    }

    #[test]
    fn token_set_debug_never_prints_a_token_value() {
        let body = r#"{
            "access_token": "at-secret",
            "refresh_token": "rt-secret",
            "id_token": "idt-secret",
            "token_type": "Bearer",
            "scope": "openid",
            "expires_in": 60
        }"#;
        let tokens = TokenSet::from_response(parse(body)).expect("access token present");
        let debug = format!("{tokens:?}");
        for secret in ["at-secret", "rt-secret", "idt-secret"] {
            assert!(!debug.contains(secret), "Debug leaked {secret}: {debug}");
        }
        // Presence flags and the non-secret fields still render.
        assert!(debug.contains("<redacted>"), "{debug}");
        assert!(debug.contains("refresh_token: true"), "{debug}");
        assert!(debug.contains("id_token: true"), "{debug}");
        assert!(debug.contains("expires_in: Some(60)"), "{debug}");
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
