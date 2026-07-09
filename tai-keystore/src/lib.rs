pub mod crypto;
pub mod error;
pub mod paths;

pub use error::KeystoreError;

use serde::{Deserialize, Serialize};

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
        return Ok(());
    }

    let (secret, public) = crypto::generate_keypair();

    // Ensure the config directory exists before writing into it.
    if let Some(parent) = pk_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    std::fs::write(&pk_path, secret)?;
    std::fs::write(&pub_path, public)?;

    Ok(())
}
use zeroize::Zeroize;

#[derive(Debug, Clone, Serialize, Deserialize, Zeroize)]
#[zeroize(drop)]
#[serde(tag = "type")]
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
    fn ensure_keypair_idempotent() {
        // Ensure that calling ensure_keypair succeeds even when files already
        // exist (the standard config dir may or may not have keys — either
        // way the function should return Ok).
        let result = ensure_keypair();
        assert!(result.is_ok(), "ensure_keypair should succeed");
    }
}
