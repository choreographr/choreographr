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

    // Fast path: both files already exist — nothing to generate or write,
    // so no lock is needed (nothing is written).
    if pk_path.exists() && pub_path.exists() {
        debug!("keypair files already exist, skipping generation");
        return Ok(());
    }

    let (secret, public) = crypto::generate_keypair();

    // Ensure the config directory exists before writing into it.
    if let Some(parent) = pk_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Serialize keypair generation across processes. Advisory exclusive lock
    // on the private-key file (std::fs::File::lock, stable since Rust 1.89);
    // on Unix new files are created with owner-only 0o600 permissions because
    // they hold secret material.
    let mut pk_file = open_locked_for_write(&pk_path)?;

    // Re-check under the lock: a concurrent process may have written a valid
    // pair while we waited for the lock.
    let write_result = (|| -> Result<(), KeystoreError> {
        if pk_path.exists() && pub_path.exists() {
            debug!("keypair files appeared while waiting for lock; skipping write");
            return Ok(());
        }
        use std::io::{Seek, SeekFrom, Write};
        pk_file.set_len(0)?; // truncate any stale content
        pk_file.seek(SeekFrom::Start(0))?;
        pk_file.write_all(&secret)?;
        pk_file.sync_all()?; // persist secret before releasing the lock
        std::fs::write(&pub_path, public)?;
        info!(
            "generated and wrote new X25519 keypair to {}",
            pk_path.display()
        );
        Ok(())
    })();

    // Release the advisory lock, then surface write errors.
    let unlock_result = pk_file.unlock();
    write_result?;
    unlock_result?;
    Ok(())
}

/// Open `path` for writing (creating it if needed) and take an advisory
/// exclusive lock so concurrent processes cannot race the keypair write.
/// On Unix, newly-created files get owner-only permissions (0o600) since
/// they contain secret key material.
fn open_locked_for_write(path: &std::path::Path) -> Result<std::fs::File, KeystoreError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .mode(0o600)
            .open(path)?;
        file.lock()?;
        Ok(file)
    }
    #[cfg(not(unix))]
    {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(path)?;
        file.lock()?;
        Ok(file)
    }
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

    #[test]
    fn ensure_keypair_is_race_safe() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let prev = std::env::var("XDG_CONFIG_HOME").ok();
        // SAFETY: the env var is process-wide and restored after both threads
        // finish. In spawned threads the paths::TEST_CONFIG_ROOT thread-local
        // is unset, so config_dir() falls back to dirs::config_dir(), which
        // honours XDG_CONFIG_HOME — both threads therefore target the same
        // temp config dir while the two racing calls are in flight.
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", temp.path());
        }

        // Both threads race to generate/write the keypair against the same
        // paths; the advisory file lock must serialize them.
        let handle_a = std::thread::spawn(ensure_keypair);
        let handle_b = std::thread::spawn(ensure_keypair);
        let result_a = handle_a.join();
        let result_b = handle_b.join();

        match prev {
            Some(val) => unsafe { std::env::set_var("XDG_CONFIG_HOME", val) },
            None => unsafe { std::env::remove_var("XDG_CONFIG_HOME") },
        }

        result_a
            .expect("thread A panicked")
            .expect("thread A ensure_keypair failed");
        result_b
            .expect("thread B panicked")
            .expect("thread B ensure_keypair failed");

        // The on-disk pair must be a single complete keypair: no torn write
        // (both files are full 32 bytes) and the secret/public halves must
        // match each other (they would not if writes interleaved).
        let secret = std::fs::read(temp.path().join("choreographr").join("identity.pk"))
            .expect("read identity.pk");
        let public = std::fs::read(temp.path().join("choreographr").join("public.pk"))
            .expect("read public.pk");
        assert_eq!(secret.len(), 32, "secret must be a full 32-byte key");
        assert_eq!(public.len(), 32, "public must be a full 32-byte key");

        // Derive the public key from the stored secret and check it matches
        // the stored public key — a mismatched pair would mean the writes
        // from the two threads interleaved.
        let secret_arr: [u8; 32] = secret.try_into().expect("32-byte secret");
        let derived = x25519_dalek::PublicKey::from(&x25519_dalek::StaticSecret::from(secret_arr));
        assert_eq!(derived.as_bytes().as_slice(), public.as_slice());
    }
}
