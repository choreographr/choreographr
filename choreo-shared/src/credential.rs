//! The workspace's shared serializable credential domain type.
//!
//! [`ServiceCredential`] is the per-service secret material the daemon holds on
//! a user's behalf. It lives here, in the one leaf crate both `choreo-proto`
//! (which types `ClientMessageType::AddCredential` with it) and `choreo-keystore`
//! (which stores it) already depend on, so the wire type can carry the
//! credential without either crate depending on the other or introducing a new
//! crate for it. The pure import/validation logic that produces a credential
//! from a Polkadot-JS export stays in `choreo-keystore`; only the value type and
//! its borrowed views live here.

use std::fmt;

use serde::{Deserialize, Serialize};
use zeroize::Zeroize;

/// Which OAuth profile an [`ServiceCredential::OAuth`] credential serves.
///
/// The kind selects the refresh client and the request-header policy: a
/// `Provider` grant refreshes through the daemon's own provider OAuth client,
/// while an `Mcp` grant refreshes through the MCP authorization manager. It is
/// a fieldless discriminator (no secret), so `Debug` is safely derived; the
/// `Zeroize` derive is a no-op here but keeps the whole credential enum
/// uniformly zeroizable.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize, Zeroize)]
pub enum OAuthKind {
    /// An inference-provider OAuth grant (e.g. `ChatGPT`/`Codex`).
    #[serde(rename = "provider")]
    Provider,
    /// A remote MCP server's OAuth grant.
    #[serde(rename = "mcp")]
    Mcp,
}

/// A stored service credential.
///
/// The variants discriminate the per-service secret material the daemon holds
/// on a user's behalf. The whole enum is `zeroize`d on drop so secret bytes
/// are not left in freed memory, and BOTH its [`Display`](fmt::Display) and
/// its hand-written [`Debug`](fmt::Debug) redact every secret field, so a
/// credential can be logged without leaking.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, Zeroize)]
#[zeroize(drop)]
pub enum ServiceCredential {
    /// A plain API-key service (the provider is identified out-of-band).
    #[serde(rename = "api_key")]
    ApiKey {
        /// The secret API key.
        key: String,
    },
    /// An X/Twitter OAuth 1.0a credential, optionally with an OAuth 2.0 bearer
    /// token for endpoints that require app-only auth.
    #[serde(rename = "x")]
    X {
        /// OAuth 1.0a consumer key.
        api_key: String,
        /// OAuth 1.0a consumer secret.
        api_key_secret: String,
        /// OAuth 1.0a access token.
        access_token: String,
        /// OAuth 1.0a access-token secret.
        access_token_secret: String,
        /// Optional OAuth 2.0 bearer token, when the app is bearer-configured.
        bearer_token: Option<String>,
    },
    /// A Polkadot sr25519 account imported from a Polkadot-JS keystore export.
    #[serde(rename = "substrate")]
    Substrate {
        /// Account name (matches the daemon's credential key, e.g. "main").
        name: String,
        /// SS58 address.
        account_id: String,
        /// Expanded ed25519 secret key (64 bytes) from a Polkadot-JS keystore export.
        secret: Vec<u8>,
        /// Raw 32-byte public key (=== account id bytes).
        public: Vec<u8>,
    },
    /// An OAuth credential: the durable grant a provider's or an MCP
    /// authorization server issued at login, refreshed by the daemon when the
    /// access token expires or a request is rejected with 401.
    ///
    /// It carries only the irreducible, durable subset — the refresh token,
    /// the client registration, and the extracted identity claims. The access
    /// token is derived, ephemeral, and lives in daemon memory only; the raw
    /// `id_token` is never stored (only the claim extracted from it).
    #[serde(rename = "oauth")]
    OAuth {
        /// Which OAuth profile this serves (provider versus MCP).
        kind: OAuthKind,
        /// Authorization-server issuer, validated against discovery.
        issuer: Option<String>,
        /// RFC 8707 resource indicator — the MCP server URL (MCP only).
        resource: Option<String>,
        /// Authorization-server base URL, so refresh needs no re-discovery
        /// (MCP).
        authorization_server: Option<String>,
        /// Long-lived token that mints a new access token: the durable grant
        /// root, rewritten when a refresh rotates it.
        refresh_token: Option<String>,
        /// Identity claim extracted from the `id_token` at login (for `ChatGPT`,
        /// its `chatgpt_account_id`). The raw JWT is not stored.
        account_id: Option<String>,
        /// The client id actually used at login, recorded so refresh does not
        /// depend on the catalog table matching.
        client_id: Option<String>,
        /// Client secret, for confidential clients (MCP).
        client_secret: Option<String>,
        /// The scopes granted at login.
        granted_scopes: Vec<String>,
        /// Bearer token type; almost always "Bearer".
        token_type: Option<String>,
    },
}

/// Redacting display to prevent secret leakage in logs.
impl fmt::Display for ServiceCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // `Display` and `Debug` share one redaction shape (`fmt_redacted`).
        fmt_redacted(self, f)
    }
}

/// Redacting `Debug` to prevent secret leakage in logs. Hand-written (not
/// derived) because the type travels inside protocol messages such as
/// `ClientMessageType::AddCredential`: a derived `Debug` would print the raw
/// secret whenever a containing message is `{:?}`-formatted, so this prints
/// the same redacted shape as [`Display`](fmt::Display).
impl fmt::Debug for ServiceCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt_redacted(self, f)
    }
}

/// The single redaction shape shared by [`Display`](fmt::Display) and
/// [`Debug`](fmt::Debug) for [`ServiceCredential`], so the two projections of
/// a secret cannot drift into one of them leaking.
fn fmt_redacted(credential: &ServiceCredential, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match credential {
        ServiceCredential::ApiKey { .. } => {
            write!(f, "ApiKey {{ key: *** }}")
        }
        ServiceCredential::X { .. } => {
            write!(
                f,
                "X {{ api_key: ***, api_key_secret: ***, \
                 access_token: ***, access_token_secret: ***, \
                 bearer_token: *** }}"
            )
        }
        ServiceCredential::Substrate { .. } => {
            write!(f, "Substrate {{ name: ***, account_id: ***, seed: *** }}")
        }
        ServiceCredential::OAuth { kind, .. } => {
            // Every OAuth field is redacted — the refresh token, the client
            // secret, and the identity/issuer strings are all sensitive. Only
            // the fieldless `kind` discriminator is printed plainly.
            write!(
                f,
                "OAuth {{ kind: {kind:?}, issuer: ***, resource: ***, \
                 authorization_server: ***, refresh_token: ***, account_id: ***, \
                 client_id: ***, client_secret: ***, granted_scopes: ***, \
                 token_type: *** }}"
            )
        }
    }
}

/// Borrowed view of X credential fields. Avoids allocating a separate struct.
#[derive(Clone, Copy)]
pub struct XCredentialView<'a> {
    /// OAuth 1.0a consumer key.
    pub api_key: &'a str,
    /// OAuth 1.0a consumer secret.
    pub api_key_secret: &'a str,
    /// OAuth 1.0a access token.
    pub access_token: &'a str,
    /// OAuth 1.0a access-token secret.
    pub access_token_secret: &'a str,
    /// Optional OAuth 2.0 bearer token.
    pub bearer_token: Option<&'a str>,
}

/// Redacting `Debug` so a logged view cannot expose the secret fields it
/// borrows (mirrors [`ServiceCredential`]'s hand-written `Debug`).
impl fmt::Debug for XCredentialView<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "XCredentialView {{ api_key: ***, api_key_secret: ***, \
             access_token: ***, access_token_secret: ***, bearer_token: *** }}"
        )
    }
}

/// Borrowed view of a Substrate credential's fields. Mirrors the borrowed
/// view used for the `X` variant; avoids allocating a separate struct.
#[derive(Clone, Copy)]
pub struct SubstrateCredentialView<'a> {
    /// Account name (matches the daemon's credential key).
    pub name: &'a str,
    /// SS58 address of the account.
    pub account_id: &'a str,
    /// Expanded ed25519 secret key (64 bytes).
    pub secret: &'a [u8],
    /// Raw 32-byte public key (the account id bytes).
    pub public: &'a [u8],
}

/// Redacting `Debug` so a logged view cannot expose the secret fields it
/// borrows (mirrors [`ServiceCredential`]'s hand-written `Debug`).
impl fmt::Debug for SubstrateCredentialView<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "SubstrateCredentialView {{ name: ***, account_id: ***, \
             secret: ***, public: *** }}"
        )
    }
}

/// Borrowed view of an OAuth credential's fields. Mirrors the borrowed views
/// used for the `X` and `Substrate` variants; avoids allocating a separate
/// struct. Like them it redacts every secret in [`Debug`](fmt::Debug).
#[derive(Clone, Copy)]
pub struct OAuthCredentialView<'a> {
    /// Which OAuth profile this serves.
    pub kind: OAuthKind,
    /// Authorization-server issuer.
    pub issuer: Option<&'a str>,
    /// RFC 8707 resource indicator (MCP only).
    pub resource: Option<&'a str>,
    /// Authorization-server base URL (MCP).
    pub authorization_server: Option<&'a str>,
    /// Long-lived refresh token (the durable grant root).
    pub refresh_token: Option<&'a str>,
    /// Identity claim extracted from the `id_token` at login.
    pub account_id: Option<&'a str>,
    /// The client id used at login.
    pub client_id: Option<&'a str>,
    /// Client secret, for confidential clients.
    pub client_secret: Option<&'a str>,
    /// The scopes granted at login.
    pub granted_scopes: &'a [String],
    /// Bearer token type.
    pub token_type: Option<&'a str>,
}

/// Redacting `Debug` so a logged view cannot expose the secret fields it
/// borrows (mirrors [`ServiceCredential`]'s hand-written `Debug`).
impl fmt::Debug for OAuthCredentialView<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "OAuthCredentialView {{ kind: {:?}, issuer: ***, resource: ***, \
             authorization_server: ***, refresh_token: ***, account_id: ***, \
             client_id: ***, client_secret: ***, granted_scopes: ***, \
             token_type: *** }}",
            self.kind
        )
    }
}

impl ServiceCredential {
    /// Returns a view of the X credential fields if this is the X variant.
    #[must_use]
    pub fn as_x(&self) -> Option<XCredentialView<'_>> {
        match self {
            ServiceCredential::X {
                api_key,
                api_key_secret,
                access_token,
                access_token_secret,
                bearer_token,
            } => Some(XCredentialView {
                api_key,
                api_key_secret,
                access_token,
                access_token_secret,
                bearer_token: bearer_token.as_deref(),
            }),
            _ => None,
        }
    }

    /// Returns a view of the Substrate credential fields if this is the
    /// Substrate variant.
    #[must_use]
    pub fn as_substrate(&self) -> Option<SubstrateCredentialView<'_>> {
        match self {
            ServiceCredential::Substrate {
                name,
                account_id,
                secret,
                public,
            } => Some(SubstrateCredentialView {
                name,
                account_id,
                secret,
                public,
            }),
            _ => None,
        }
    }

    /// Returns a view of the OAuth credential fields if this is the OAuth
    /// variant.
    #[must_use]
    pub fn as_oauth(&self) -> Option<OAuthCredentialView<'_>> {
        match self {
            ServiceCredential::OAuth {
                kind,
                issuer,
                resource,
                authorization_server,
                refresh_token,
                account_id,
                client_id,
                client_secret,
                granted_scopes,
                token_type,
            } => Some(OAuthCredentialView {
                kind: *kind,
                issuer: issuer.as_deref(),
                resource: resource.as_deref(),
                authorization_server: authorization_server.as_deref(),
                refresh_token: refresh_token.as_deref(),
                account_id: account_id.as_deref(),
                client_id: client_id.as_deref(),
                client_secret: client_secret.as_deref(),
                granted_scopes,
                token_type: token_type.as_deref(),
            }),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_credential_as_x_returns_none_for_api_key() {
        let cred = ServiceCredential::ApiKey {
            key: "sk-test".to_string(),
        };
        assert!(cred.as_x().is_none());
    }

    #[test]
    fn service_credential_as_x_returns_view() {
        let cred = ServiceCredential::X {
            api_key: "ak".into(),
            api_key_secret: "aks".into(),
            access_token: "at".into(),
            access_token_secret: "ats".into(),
            bearer_token: Some("bt".into()),
        };
        let view = cred.as_x().unwrap();
        assert_eq!(view.api_key, "ak");
        assert_eq!(view.api_key_secret, "aks");
        assert_eq!(view.access_token, "at");
        assert_eq!(view.access_token_secret, "ats");
        assert_eq!(view.bearer_token, Some("bt"));
    }

    #[test]
    fn service_credential_as_x_none_bearer() {
        let cred = ServiceCredential::X {
            api_key: "ak".into(),
            api_key_secret: "aks".into(),
            access_token: "at".into(),
            access_token_secret: "ats".into(),
            bearer_token: None,
        };
        let view = cred.as_x().unwrap();
        assert!(view.bearer_token.is_none());
    }

    #[test]
    fn service_credential_display_redacts_secrets() {
        let cred = ServiceCredential::ApiKey {
            key: "sk-supersecret".into(),
        };
        let display = format!("{cred}");
        assert!(!display.contains("sk-supersecret"));
        assert!(display.contains("***"));

        let cred_x = ServiceCredential::X {
            api_key: "ak".into(),
            api_key_secret: "aks".into(),
            access_token: "at".into(),
            access_token_secret: "ats".into(),
            bearer_token: Some("bt".into()),
        };
        let display_x = format!("{cred_x}");
        assert!(!display_x.contains("ak"));
        assert!(!display_x.contains("ats"));
        assert!(!display_x.contains("bt"));
        assert_eq!(display_x.matches("***").count(), 5);
    }

    #[test]
    fn service_credential_debug_redacts_secrets() {
        // `Debug` is hand-written to redact: the type now travels inside
        // protocol messages, so a `{:?}` on a containing message must not
        // expose the secret. Pin that the derive was not reintroduced.
        let cred = ServiceCredential::ApiKey {
            key: "sk-supersecret".into(),
        };
        let debug = format!("{cred:?}");
        assert!(!debug.contains("sk-supersecret"), "Debug leaked the key");
        assert!(debug.contains("***"));

        let cred_x = ServiceCredential::X {
            api_key: "ak".into(),
            api_key_secret: "aks".into(),
            access_token: "at".into(),
            access_token_secret: "ats".into(),
            bearer_token: Some("bt".into()),
        };
        let debug_x = format!("{cred_x:?}");
        assert!(!debug_x.contains("ak"));
        assert!(!debug_x.contains("ats"));
        assert!(!debug_x.contains("bt"));
        assert_eq!(debug_x.matches("***").count(), 5);

        let cred_substrate = ServiceCredential::Substrate {
            name: "main".into(),
            account_id: "5GrwvaEF5zXb26Fz9rcQpDWS57CtERHpNehXCPcNoHGKutQY".into(),
            secret: vec![0x11; 64],
            public: vec![0x22; 32],
        };
        let debug_substrate = format!("{cred_substrate:?}");
        assert!(!debug_substrate.contains("main"));
        assert!(!debug_substrate.contains("5Grwva"));
        assert_eq!(debug_substrate.matches("***").count(), 3);
    }

    #[test]
    fn borrowed_credential_views_debug_redacts_secrets() {
        let cred = ServiceCredential::X {
            api_key: "ak".into(),
            api_key_secret: "aks".into(),
            access_token: "at".into(),
            access_token_secret: "ats".into(),
            bearer_token: Some("bt".into()),
        };
        let view = cred.as_x().expect("expected X view");
        let debug = format!("{view:?}");
        assert!(!debug.contains("ak"));
        assert!(!debug.contains("ats"));
        assert!(!debug.contains("bt"));

        let substrate = ServiceCredential::Substrate {
            name: "main".into(),
            account_id: "5GrwvaEF5zXb26Fz9rcQpDWS57CtERHpNehXCPcNoHGKutQY".into(),
            secret: vec![0x11; 64],
            public: vec![0x22; 32],
        };
        let sub_view = substrate.as_substrate().expect("expected Substrate view");
        let sub_debug = format!("{sub_view:?}");
        assert!(!sub_debug.contains("main"));
        assert!(!sub_debug.contains("5Grwva"));
    }

    #[test]
    fn service_credential_zeroize_clears_api_key() {
        let mut cred = ServiceCredential::ApiKey {
            key: "sk-test".to_string(),
        };
        cred.zeroize();
        match &cred {
            ServiceCredential::ApiKey { key } => {
                assert!(
                    key.as_bytes().iter().all(|&b| b == 0),
                    "key bytes should be zeroed after zeroize"
                );
            }
            _ => panic!("expected ApiKey variant"),
        }
    }

    #[test]
    fn service_credential_zeroize_clears_x_fields() {
        let mut cred = ServiceCredential::X {
            api_key: "ak".into(),
            api_key_secret: "aks".into(),
            access_token: "at".into(),
            access_token_secret: "ats".into(),
            bearer_token: Some("bt".into()),
        };
        cred.zeroize();
        match &cred {
            ServiceCredential::X {
                api_key,
                api_key_secret,
                access_token,
                access_token_secret,
                bearer_token,
            } => {
                assert!(api_key.as_bytes().iter().all(|&b| b == 0));
                assert!(api_key_secret.as_bytes().iter().all(|&b| b == 0));
                assert!(access_token.as_bytes().iter().all(|&b| b == 0));
                assert!(access_token_secret.as_bytes().iter().all(|&b| b == 0));
                assert!(
                    bearer_token
                        .as_ref()
                        .is_none_or(|s| s.as_bytes().iter().all(|&b| b == 0))
                );
            }
            _ => panic!("expected X variant"),
        }
    }

    #[test]
    fn service_credential_substrate_as_substrate_returns_view() {
        let cred = ServiceCredential::Substrate {
            name: "main".into(),
            account_id: "5GrwvaEF5zXb26Fz9rcQpDWS57CtERHpNehXCPcNoHGKutQY".into(),
            secret: vec![0x11; 64],
            public: vec![0x22; 32],
        };
        let view = cred.as_substrate().expect("expected Substrate view");
        assert_eq!(view.name, "main");
        assert_eq!(
            view.account_id,
            "5GrwvaEF5zXb26Fz9rcQpDWS57CtERHpNehXCPcNoHGKutQY"
        );
        assert_eq!(view.secret.len(), 64);
        assert_eq!(view.public.len(), 32);
    }

    #[test]
    fn service_credential_substrate_as_substrate_none_for_other_variant() {
        let cred = ServiceCredential::ApiKey {
            key: "sk-test".into(),
        };
        assert!(cred.as_substrate().is_none());
    }

    #[test]
    fn service_credential_substrate_display_redacts_secrets() {
        let cred = ServiceCredential::Substrate {
            name: "main".into(),
            account_id: "5GrwvaEF5zXb26Fz9rcQpDWS57CtERHpNehXCPcNoHGKutQY".into(),
            secret: vec![0x11; 64],
            public: vec![0x22; 32],
        };
        let display = format!("{cred}");
        assert_eq!(
            display,
            "Substrate { name: ***, account_id: ***, seed: *** }"
        );
        assert!(!display.contains("main"));
        assert!(!display.contains("5Grwva"));
        assert_eq!(display.matches("***").count(), 3);
    }

    #[test]
    fn service_credential_substrate_zeroize_clears_fields() {
        let mut cred = ServiceCredential::Substrate {
            name: "main".into(),
            account_id: "5GrwvaEF5zXb26Fz9rcQpDWS57CtERHpNehXCPcNoHGKutQY".into(),
            secret: vec![0x11; 64],
            public: vec![0x22; 32],
        };
        cred.zeroize();
        match &cred {
            ServiceCredential::Substrate {
                name,
                account_id,
                secret,
                public,
            } => {
                assert!(name.as_bytes().iter().all(|&b| b == 0));
                assert!(account_id.as_bytes().iter().all(|&b| b == 0));
                assert!(secret.iter().all(|&b| b == 0));
                assert!(public.iter().all(|&b| b == 0));
            }
            _ => panic!("expected Substrate variant"),
        }
    }

    /// A fully-populated OAuth credential, for the redaction/zeroize tests.
    fn oauth_credential() -> ServiceCredential {
        ServiceCredential::OAuth {
            kind: OAuthKind::Provider,
            issuer: Some("https://auth.openai.com".into()),
            resource: Some("https://api.example/v1".into()),
            authorization_server: Some("https://as.example".into()),
            refresh_token: Some("refresh-supersecret".into()),
            account_id: Some("acct-supersecret".into()),
            client_id: Some("client-supersecret".into()),
            client_secret: Some("clientsecret-supersecret".into()),
            granted_scopes: vec!["openid".into(), "offline_access".into()],
            token_type: Some("Bearer".into()),
        }
    }

    #[test]
    fn service_credential_as_oauth_returns_view() {
        let cred = oauth_credential();
        let view = cred.as_oauth().expect("expected OAuth view");
        assert_eq!(view.kind, OAuthKind::Provider);
        assert_eq!(view.issuer, Some("https://auth.openai.com"));
        assert_eq!(view.resource, Some("https://api.example/v1"));
        assert_eq!(view.authorization_server, Some("https://as.example"));
        assert_eq!(view.refresh_token, Some("refresh-supersecret"));
        assert_eq!(view.account_id, Some("acct-supersecret"));
        assert_eq!(view.client_id, Some("client-supersecret"));
        assert_eq!(view.client_secret, Some("clientsecret-supersecret"));
        assert_eq!(view.granted_scopes, ["openid", "offline_access"]);
        assert_eq!(view.token_type, Some("Bearer"));
    }

    #[test]
    fn service_credential_as_oauth_returns_mcp_kind() {
        let mut cred = oauth_credential();
        if let ServiceCredential::OAuth { kind, .. } = &mut cred {
            *kind = OAuthKind::Mcp;
        }
        let view = cred.as_oauth().expect("expected OAuth view");
        assert_eq!(view.kind, OAuthKind::Mcp);
    }

    #[test]
    fn service_credential_as_oauth_none_for_other_variant() {
        let cred = ServiceCredential::ApiKey {
            key: "sk-test".into(),
        };
        assert!(cred.as_oauth().is_none());
    }

    #[test]
    fn service_credential_oauth_display_redacts_secrets() {
        let display = format!("{}", oauth_credential());
        for secret in [
            "auth.openai.com",
            "api.example",
            "as.example",
            "refresh-supersecret",
            "acct-supersecret",
            "client-supersecret",
            "clientsecret-supersecret",
            "openid",
            "offline_access",
            "Bearer",
        ] {
            assert!(!display.contains(secret), "Display leaked {secret}");
        }
        // Nine redacted fields; the fieldless `kind` is printed plainly.
        assert_eq!(display.matches("***").count(), 9);
        assert!(display.contains("kind: Provider"));
    }

    #[test]
    fn service_credential_oauth_debug_redacts_secrets() {
        let debug = format!("{:?}", oauth_credential());
        for secret in [
            "auth.openai.com",
            "refresh-supersecret",
            "clientsecret-supersecret",
            "offline_access",
        ] {
            assert!(!debug.contains(secret), "Debug leaked {secret}");
        }
        assert_eq!(debug.matches("***").count(), 9);
    }

    #[test]
    fn oauth_credential_view_debug_redacts_secrets() {
        let cred = oauth_credential();
        let view = cred.as_oauth().expect("expected OAuth view");
        let debug = format!("{view:?}");
        for secret in [
            "auth.openai.com",
            "refresh-supersecret",
            "clientsecret-supersecret",
            "openid",
        ] {
            assert!(!debug.contains(secret), "view Debug leaked {secret}");
        }
        assert_eq!(debug.matches("***").count(), 9);
    }

    #[test]
    fn service_credential_oauth_zeroize_clears_fields() {
        let mut cred = oauth_credential();
        cred.zeroize();
        match &cred {
            ServiceCredential::OAuth {
                issuer,
                resource,
                authorization_server,
                refresh_token,
                account_id,
                client_id,
                client_secret,
                granted_scopes,
                token_type,
                // The fieldless discriminator carries no bytes to wipe.
                kind: _,
            } => {
                let cleared = |v: &Option<String>| {
                    v.as_ref()
                        .is_none_or(|s| s.as_bytes().iter().all(|&b| b == 0))
                };
                assert!(cleared(issuer));
                assert!(cleared(resource));
                assert!(cleared(authorization_server));
                assert!(cleared(refresh_token));
                assert!(cleared(account_id));
                assert!(cleared(client_id));
                assert!(cleared(client_secret));
                assert!(cleared(token_type));
                assert!(
                    granted_scopes
                        .iter()
                        .all(|s| s.as_bytes().iter().all(|&b| b == 0))
                );
            }
            _ => panic!("expected OAuth variant"),
        }
    }
}
