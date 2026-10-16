//! Credential storage and cryptographic key management for the daemon, TUI,
//! and CLI.
//!
//! This crate owns the material every credentialed feature depends on: the
//! X25519 + AES-256-GCM envelope that moves a credential to a client
//! ([`crypto`]), the on-disk path layout under `{config}/choreographr`
//! ([`paths`]), and the Polkadot-JS keystore import and sr25519 validation
//! path ([`substrate`]).
//!
//! The credential value type itself — [`ServiceCredential`] with its
//! secret-redacting `Display` and `zeroize`-on-drop — lives in `choreo-shared`
//! (the one leaf crate both this crate and `choreo-proto` depend on), and is
//! re-exported here so the [`ServiceCredential`], [`XCredentialView`],
//! [`SubstrateCredentialView`], [`OAuthKind`], and [`OAuthCredentialView`]
//! paths under `choreo_keystore` keep working.

// Part of the ARCHITECTURE.md → rustdoc migration (see AGENTS.md → Documentation):
// every public item carries docs, enforced as a hard error by clippy-strict's
// `-D warnings`.
#![warn(missing_docs)]

pub mod crypto;
pub mod error;
pub mod paths;
pub mod substrate;

pub use error::KeystoreError;
// The credential value type and its borrowed views now live in `choreo-shared`;
// re-exported here so the long-standing `choreo_keystore::…` paths are stable.
pub use choreo_shared::{
    OAuthCredentialView, OAuthKind, ServiceCredential, SubstrateCredentialView, XCredentialView,
};

#[cfg(test)]
mod tests {
    use super::*;

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
