use std::fmt;

pub mod crypto;
pub mod error;
pub mod paths;

pub use error::KeystoreError;

use serde::{Deserialize, Serialize};
use tracing::{debug, info};
use zeroize::Zeroize;

/// Ensure that a keypair exists at the standard paths.
///
/// Checks whether `identity.pk` and `public.pk` exist in the config directory.
/// If either file is missing, generates a new X25519 keypair and writes both
/// files (32 bytes each, raw binary).  Existing files are left untouched.
///
/// This is safe to call on every startup — it only writes when necessary.
pub fn ensure_keypair() -> Result<(), KeystoreError> {
    let pk_path = paths::private_key_path()?;
    let pub_path = paths::public_key_path()?;

    // If both files already exist, there is nothing to do.
    if pk_path.exists() && pub_path.exists() {
        debug!("keypair files already exist, skipping generation");
        return Ok(());
    }

    let (secret, public) = crypto::generate_keypair();

    // Ensure the config directory exists before writing into it.
    if let Some(parent) = pk_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    std::fs::write(&pk_path, secret)?;
    std::fs::write(&pub_path, public)?;
    info!(
        "generated and wrote new X25519 keypair to {}",
        pk_path.display()
    );

    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize, Zeroize)]
#[zeroize(drop)]
pub enum ServiceCredential {
    #[serde(rename = "api_key")]
    ApiKey { key: String },
    #[serde(rename = "x")]
    X {
        api_key: String,
        api_key_secret: String,
        access_token: String,
        access_token_secret: String,
        bearer_token: Option<String>,
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
        }
    }
}

/// Borrowed view of X credential fields. Avoids allocating a separate struct.
#[derive(Debug, Clone, Copy)]
pub struct XCredentialView<'a> {
    pub api_key: &'a str,
    pub api_key_secret: &'a str,
    pub access_token: &'a str,
    pub access_token_secret: &'a str,
    pub bearer_token: Option<&'a str>,
}

impl ServiceCredential {
    /// Returns a view of the X credential fields if this is the X variant.
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
}
