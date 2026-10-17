//! Daemon-side account auth: the resolved grant the daemon hands to a provider
//! client, and its conversion to the client-side request material.
//!
//! The daemon is the sole holder of an account's durable credential, so it
//! resolves an account to a [`ProviderAuth`] — either an API key or an OAuth
//! grant plus any cached access token — and projects that into a
//! [`RequestAuth`] the provider client consumes. The projection is where the
//! grant's refresh token is dropped: the client sees only the already-derived
//! bearer material and headers, so `choreo-ai-protocols` never learns about the
//! durable grant.
//!
//! This module also owns [`derive_headers`], the one place the credential's
//! wire-specific request-header names live, so adding a provider later is a
//! change in one function rather than scattered across call sites.

use choreo_ai_protocols::RequestAuth;
use choreo_keystore::ServiceCredential;
use zeroize::Zeroizing;

/// The `originator` request-header value sent to the `ChatGPT`/Codex backend: a
/// stable, non-empty per-app identifier. The official Codex CLI sends
/// `codex_cli_rs`; Choreographr sends its own.
const ORIGINATOR: &str = "choreographr";

/// The auth material for an account, resolved by the daemon: the durable grant
/// plus any cached ephemeral access token. `OAuth` carries the whole
/// `ServiceCredential` so the refresh path has the grant; the client never sees
/// it.
pub enum ProviderAuth {
    /// A plain API-key credential.
    ApiKey {
        /// The secret API key, wipe-on-drop.
        key: Zeroizing<String>,
    },
    /// An OAuth grant plus its cached access token. The access token is `None`
    /// until the account has signed in and a token has been obtained. The grant
    /// is boxed: `ServiceCredential` is large (it carries every OAuth field),
    /// so inlining it would make this enum — and every `Option<ProviderAuth>`
    /// carried across the resolve reply — far larger than the API-key arm needs.
    OAuth {
        /// The durable grant (refresh token + client registration + claims).
        credential: Box<ServiceCredential>,
        /// The cached ephemeral access token, when one has been obtained.
        access_token: Option<Zeroizing<String>>,
    },
}

/// Redacting `Debug`: the API key and the carried grant are secret.
impl std::fmt::Debug for ProviderAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProviderAuth::ApiKey { .. } => f.write_str("ProviderAuth::ApiKey { key: *** }"),
            ProviderAuth::OAuth { access_token, .. } => f
                .debug_struct("ProviderAuth::OAuth")
                .field("credential", &"***")
                .field("has_access_token", &access_token.is_some())
                .finish(),
        }
    }
}

impl ProviderAuth {
    /// The API key when this is an API-key grant, cloned for a caller that needs
    /// its own copy (image-generation clients are API-key-only). `None` for
    /// OAuth.
    #[must_use]
    pub fn api_key_cloned(&self) -> Option<String> {
        match self {
            ProviderAuth::ApiKey { key } => Some(key.to_string()),
            ProviderAuth::OAuth { .. } => None,
        }
    }

    /// Project the daemon-side grant into the client-side request material.
    ///
    /// # Errors
    ///
    /// Returns an error for an OAuth grant that has no cached access token yet
    /// (the account has not signed in, or a refresh has not yet produced one):
    /// there is no bearer to send.
    pub fn into_request_auth(self) -> Result<RequestAuth, String> {
        match self {
            ProviderAuth::ApiKey { key } => Ok(RequestAuth::ApiKey(key)),
            ProviderAuth::OAuth {
                credential,
                access_token: Some(access_token),
            } => Ok(RequestAuth::OAuth {
                access_token,
                extra_headers: derive_headers(&credential),
            }),
            ProviderAuth::OAuth {
                access_token: None, ..
            } => Err("no access token; sign in first".to_string()),
        }
    }
}

/// The credential-derived extra request headers for an OAuth account.
///
/// The single place the wire-specific header names live, so a later provider is
/// added here as data. A non-OAuth credential (an API key) derives none of
/// these and yields an empty list.
#[must_use]
pub fn derive_headers(credential: &ServiceCredential) -> Vec<(String, String)> {
    // These headers are OAuth-only: an API-key credential never sends them.
    let Some(oauth) = credential.as_oauth() else {
        return Vec::new();
    };
    let mut headers = Vec::new();
    // The `chatgpt-account-id` header is the identity claim extracted from the
    // id_token at login; only sent when the credential carries one.
    if let Some(account_id) = oauth.account_id {
        headers.push(("chatgpt-account-id".to_string(), account_id.to_string()));
    }
    // The backend requires a stable, non-empty originator id; ours never
    // changes, so it is a constant rather than any per-request value.
    headers.push(("originator".to_string(), ORIGINATOR.to_string()));
    headers
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oauth_credential(account_id: Option<&str>) -> ServiceCredential {
        ServiceCredential::OAuth {
            kind: choreo_keystore::OAuthKind::Provider,
            issuer: Some("https://auth.openai.com".into()),
            resource: None,
            authorization_server: None,
            refresh_token: Some("refresh-token".into()),
            account_id: account_id.map(str::to_string),
            client_id: Some("client-id".into()),
            client_secret: None,
            granted_scopes: vec!["openid".into()],
            token_type: Some("Bearer".into()),
        }
    }

    #[test]
    fn api_key_projects_to_request_auth_api_key() {
        let auth = ProviderAuth::ApiKey {
            key: Zeroizing::new("sk-test".into()),
        };
        let request = auth.into_request_auth().expect("api key projects");
        assert!(matches!(&request, RequestAuth::ApiKey(key) if key.as_str() == "sk-test"));
        assert_eq!(request.extra_headers(), [] as [(String, String); 0]);
    }

    #[test]
    fn oauth_with_token_projects_and_derives_headers() {
        let auth = ProviderAuth::OAuth {
            credential: Box::new(oauth_credential(Some("acct-123"))),
            access_token: Some(Zeroizing::new("access-token".into())),
        };
        let request = auth.into_request_auth().expect("oauth with token projects");
        match &request {
            RequestAuth::OAuth {
                access_token,
                extra_headers,
            } => {
                assert_eq!(access_token.as_str(), "access-token");
                assert_eq!(
                    extra_headers,
                    &[
                        ("chatgpt-account-id".to_string(), "acct-123".to_string()),
                        ("originator".to_string(), ORIGINATOR.to_string()),
                    ]
                );
            }
            RequestAuth::ApiKey(_) => panic!("expected OAuth request auth"),
        }
    }

    #[test]
    fn oauth_without_token_is_the_not_signed_in_error() {
        let auth = ProviderAuth::OAuth {
            credential: Box::new(oauth_credential(None)),
            access_token: None,
        };
        let err = auth
            .into_request_auth()
            .expect_err("no token must not project");
        assert!(err.contains("sign in first"), "{err}");
    }

    #[test]
    fn derive_headers_omits_account_id_when_absent() {
        let headers = derive_headers(&oauth_credential(None));
        assert_eq!(
            headers,
            vec![("originator".to_string(), ORIGINATOR.to_string())]
        );
    }

    #[test]
    fn derive_headers_empty_for_api_key_credential() {
        let credential = ServiceCredential::ApiKey {
            key: "sk-test".into(),
        };
        // An API-key credential is not OAuth, so no credential-derived headers
        // are produced — the originator header is OAuth-only.
        assert_eq!(derive_headers(&credential), [] as [(String, String); 0]);
    }

    #[test]
    fn api_key_cloned_only_for_api_key_grant() {
        let auth = ProviderAuth::ApiKey {
            key: Zeroizing::new("sk-test".into()),
        };
        assert_eq!(auth.api_key_cloned().as_deref(), Some("sk-test"));
        let auth = ProviderAuth::OAuth {
            credential: Box::new(oauth_credential(None)),
            access_token: None,
        };
        assert!(auth.api_key_cloned().is_none());
    }

    #[test]
    fn debug_redacts_secrets() {
        let auth = ProviderAuth::OAuth {
            credential: Box::new(oauth_credential(Some("acct-123"))),
            access_token: Some(Zeroizing::new("access-token".into())),
        };
        let debug = format!("{auth:?}");
        assert!(!debug.contains("access-token"), "Debug leaked the token");
        assert!(!debug.contains("refresh-token"), "Debug leaked the grant");
        assert!(debug.contains("has_access_token: true"));
    }
}
