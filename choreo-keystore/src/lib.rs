//! Credential storage and cryptographic key management for the daemon, TUI,
//! and CLI.
//!
//! This crate owns the material every credentialed feature depends on: the
//! [`ServiceCredential`] enum (API keys, X/Twitter OAuth tokens, and Polkadot
//! sr25519 accounts) with secret-redacting `Display` and `zeroize`-on-drop, the
//! X25519 + AES-256-GCM envelope that moves a credential to a client
//! ([`crypto`]), the on-disk path layout under `{config}/choreographr`
//! ([`paths`]), and the Polkadot-JS keystore import and sr25519 validation
//! path ([`substrate`]).

// Part of the ARCHITECTURE.md → rustdoc migration (see AGENTS.md → Documentation):
// every public item carries docs, enforced as a hard error by clippy-strict's
// `-D warnings`.
#![warn(missing_docs)]

use std::fmt;

pub mod crypto;
pub mod error;
pub mod paths;
pub mod substrate;

pub use error::KeystoreError;
pub use substrate::SubstrateCredentialView;

use serde::{Deserialize, Serialize};
use zeroize::Zeroize;

/// A stored service credential.
///
/// The variants discriminate the per-service secret material the daemon holds
/// on a user's behalf. The whole enum is `zeroize`d on drop so secret bytes
/// are not left in freed memory, and its [`Display`](fmt::Display) redacts
/// every secret field so a credential can be logged without leaking.
#[derive(Debug, Clone, Serialize, Deserialize, Zeroize)]
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
}

/// Redacting display to prevent secret leakage in logs.
impl fmt::Display for ServiceCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
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
        }
    }
}

/// Borrowed view of X credential fields. Avoids allocating a separate struct.
#[derive(Debug, Clone, Copy)]
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
    fn service_credential_serialization_round_trip() {
        let cred = ServiceCredential::X {
            api_key: "ak".into(),
            api_key_secret: "aks".into(),
            access_token: "at".into(),
            access_token_secret: "ats".into(),
            bearer_token: None,
        };
        let json = serde_json::to_string(&cred).unwrap();
        let restored: ServiceCredential = serde_json::from_str(&json).unwrap();
        assert!(matches!(restored, ServiceCredential::X { .. }));
        let view = restored.as_x().unwrap();
        assert_eq!(view.api_key, "ak");
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

    #[test]
    fn service_credential_substrate_serialization_round_trip() {
        let cred = ServiceCredential::Substrate {
            name: "main".into(),
            account_id: "5GrwvaEF5zXb26Fz9rcQpDWS57CtERHpNehXCPcNoHGKutQY".into(),
            secret: vec![0x11; 64],
            public: vec![0x22; 32],
        };
        let json = serde_json::to_string(&cred).unwrap();
        let restored: ServiceCredential = serde_json::from_str(&json).unwrap();
        assert!(matches!(restored, ServiceCredential::Substrate { .. }));
        assert!(json.contains("\"substrate\""));
    }
}
