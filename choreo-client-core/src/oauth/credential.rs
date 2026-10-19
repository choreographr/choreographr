//! Build the durable [`ServiceCredential`] (and its `AddCredential` frame) from
//! the exchanged tokens.
//!
//! Only the irreducible durable subset is kept — the refresh token, the client
//! registration, and the extracted identity claims. The access token is derived
//! and ephemeral and is deliberately dropped here; the daemon mints a fresh one
//! when it needs it. This is the one place the credential's shape is assembled,
//! so the render crates never build an OAuth credential themselves.

use choreo_keystore::{OAuthKind, ServiceCredential};
use choreo_proto::OAuthProviderDescriptor;
use crossbeam_channel::Sender;

use super::{LoginEvent, LoginOptions, OAuthLoginError, chatgpt_account_id, decode_claims, http};
use crate::credentials::build_add_credential_from_credential;

/// Build the credential from the exchanged tokens and emit `Done`.
pub(super) fn finish(
    descriptor: &OAuthProviderDescriptor,
    options: &LoginOptions,
    tx: &Sender<LoginEvent>,
    tokens: &http::TokenSet,
    client_id: &str,
) -> Result<(), OAuthLoginError> {
    let credential = build_credential(descriptor, tokens, client_id);
    let message = build_add_credential_from_credential(options.account.clone(), credential)
        .map_err(|e| OAuthLoginError::Decode(e.to_string()))?;
    let _ = tx.send(LoginEvent::Done {
        service: options.account.clone(),
        message: Box::new(message),
    });
    Ok(())
}

/// Build the durable OAuth credential from the token response.
///
/// The identity claim is read from the `id_token` first, then the access token.
fn build_credential(
    descriptor: &OAuthProviderDescriptor,
    tokens: &http::TokenSet,
    client_id: &str,
) -> ServiceCredential {
    ServiceCredential::OAuth {
        kind: OAuthKind::Provider,
        issuer: Some(descriptor.issuer.clone()),
        resource: None,
        authorization_server: None,
        // Record the resolved token endpoint and refresh encoding at login, so a
        // runtime refresh reuses them instead of re-deriving from the catalog
        // profile (which an overlay edit or a catalog change could alter).
        token_endpoint: Some(choreo_shared::url::join_path(
            &descriptor.issuer,
            &descriptor.token_path,
        )),
        refresh_encoding: Some(descriptor.refresh_encoding),
        refresh_token: tokens.refresh_token.as_ref().map(|t| t.to_string()),
        // Providers refresh, so the access token is never persisted here.
        access_token: None,
        account_id: extract_account_id(tokens),
        client_id: Some(client_id.to_string()),
        client_secret: None,
        granted_scopes: granted_scopes(tokens.scope.as_deref(), &descriptor.scopes),
        token_type: tokens
            .token_type
            .clone()
            .or_else(|| Some("Bearer".to_string())),
    }
}

/// The account id, preferring the `id_token`'s claim, falling back to the
/// access token's (some providers carry it only in the access token).
fn extract_account_id(tokens: &http::TokenSet) -> Option<String> {
    if let Some(id_token) = tokens.id_token.as_deref()
        && let Some(claims) = decode_claims(id_token)
        && let Some(account_id) = chatgpt_account_id(&claims)
    {
        return Some(account_id);
    }
    let claims = decode_claims(tokens.access_token.as_str())?;
    chatgpt_account_id(&claims)
}

/// The granted scopes: the token response's `scope` when present and non-empty,
/// else the requested scopes. The one rule shared by the provider and MCP flows,
/// so their credentials cannot disagree on how a response scope is read.
pub(super) fn granted_scopes(scope: Option<&str>, fallback: &[String]) -> Vec<String> {
    match scope {
        Some(scope) if !scope.trim().is_empty() => {
            scope.split_whitespace().map(str::to_string).collect()
        }
        _ => fallback.to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use choreo_proto::{OAuthClientIdentity, OAuthGrantEncoding};

    fn descriptor() -> OAuthProviderDescriptor {
        OAuthProviderDescriptor {
            provider: "mock".into(),
            issuer: "https://auth.example".into(),
            authorize_path: "/authorize".into(),
            token_path: "/token".into(),
            client_id: Some("client-abc".into()),
            scopes: vec!["openid".into(), "offline_access".into()],
            redirect_ports: vec![1455],
            code_exchange_encoding: OAuthGrantEncoding::Form,
            refresh_encoding: OAuthGrantEncoding::Json,
            client_identity: OAuthClientIdentity::Registered,
            device_code: false,
            device_authorization_path: None,
            device_token_path: None,
        }
    }

    fn tokens(id_token: Option<&str>, access_token: &str, scope: Option<&str>) -> http::TokenSet {
        http::TokenSet {
            access_token: zeroize::Zeroizing::new(access_token.to_string()),
            refresh_token: Some(zeroize::Zeroizing::new("refresh".to_string())),
            id_token: id_token.map(|t| zeroize::Zeroizing::new(t.to_string())),
            token_type: Some("Bearer".into()),
            scope: scope.map(str::to_string),
            expires_in: None,
        }
    }

    fn jwt(claims: &serde_json::Value) -> String {
        use base64::Engine as _;
        let header = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b"{}");
        let payload =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(claims.to_string().as_bytes());
        format!("{header}.{payload}.sig")
    }

    #[test]
    fn account_id_prefers_the_id_token_then_falls_back_to_the_access_token() {
        let acct = serde_json::json!({
            "https://api.openai.com/auth": { "chatgpt_account_id": "from-id" }
        });
        let from_id = tokens(Some(&jwt(&acct)), "opaque-access", None);
        assert_eq!(extract_account_id(&from_id).as_deref(), Some("from-id"));

        // No id_token: the access token itself is a JWT carrying the claim.
        let from_access = tokens(None, &jwt(&acct), None);
        assert_eq!(extract_account_id(&from_access).as_deref(), Some("from-id"));

        // Neither carries the claim.
        let none = tokens(None, "opaque", None);
        assert!(extract_account_id(&none).is_none());
    }

    #[test]
    fn granted_scopes_prefers_the_response_then_the_descriptor() {
        let with_scope = tokens(None, "opaque", Some("a b"));
        assert_eq!(
            granted_scopes(with_scope.scope.as_deref(), &descriptor().scopes),
            ["a", "b"]
        );

        let without = tokens(None, "opaque", None);
        assert_eq!(
            granted_scopes(without.scope.as_deref(), &descriptor().scopes),
            ["openid", "offline_access"]
        );

        // An empty/whitespace response scope falls back to the requested set.
        assert_eq!(
            granted_scopes(Some("  "), &descriptor().scopes),
            ["openid", "offline_access"]
        );
    }

    #[test]
    fn build_credential_drops_the_access_token_and_keeps_the_grant() {
        let cred = build_credential(
            &descriptor(),
            &tokens(None, "opaque-access", Some("openid")),
            "client-abc",
        );
        let oauth = cred.as_oauth().expect("OAuth credential");
        assert_eq!(oauth.kind, OAuthKind::Provider);
        assert_eq!(oauth.refresh_token, Some("refresh"));
        assert_eq!(oauth.client_id, Some("client-abc"));
        assert_eq!(oauth.issuer, Some("https://auth.example"));
        assert_eq!(
            oauth.token_endpoint,
            Some("https://auth.example/token"),
            "the resolved token endpoint is recorded at login"
        );
        assert_eq!(
            oauth.refresh_encoding,
            Some(choreo_shared::OAuthGrantEncoding::Json),
            "the refresh encoding is recorded at login"
        );
        // The access token is not a field of the credential at all.
        assert_eq!(format!("{cred:?}").matches("opaque-access").count(), 0);
    }
}
