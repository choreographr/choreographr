use tai_keystore::ServiceCredential;
use tai_proto::ClientMessage;
use tracing::{debug, info, warn};

use crate::error::ClientError;
use crate::shell::UnlockMethod;

/// Resolve a private key from the given unlock method.
///
/// For `UnlockMethod::Raw`, reads the raw private key file.
/// For `UnlockMethod::Passphrase`, reads the encrypted key file and decrypts it.
pub fn resolve_private_key(method: &UnlockMethod) -> Result<Vec<u8>, ClientError> {
    match method {
        UnlockMethod::Raw => {
            info!("reading raw private key");
            read_raw_private_key()
        }
        UnlockMethod::Passphrase(passphrase) => {
            info!("reading encrypted private key");
            read_encrypted_private_key(passphrase)
        }
    }
}

/// Read and validate the raw private key file (`identity.pk`).
/// Returns 32-byte key data.
fn read_raw_private_key() -> Result<Vec<u8>, ClientError> {
    let path = tai_keystore::paths::private_key_path()
        .map_err(|e| ClientError::PrivateKeyRead(e.to_string()))?;
    let data = std::fs::read(&path).map_err(|e| ClientError::PrivateKeyRead(e.to_string()))?;
    if data.len() != 32 {
        return Err(ClientError::PrivateKeyInvalid);
    }
    Ok(data)
}

/// Read and decrypt the encrypted private key file (`identity.pk.enc`)
/// using the given passphrase.
fn read_encrypted_private_key(passphrase: &str) -> Result<Vec<u8>, ClientError> {
    let enc_path = tai_keystore::paths::private_key_enc_path()
        .map_err(|e| ClientError::PrivateKeyEncRead(e.to_string()))?;
    let data =
        std::fs::read(&enc_path).map_err(|e| ClientError::PrivateKeyEncRead(e.to_string()))?;
    let key = tai_keystore::crypto::decrypt_private_key(&data, passphrase)
        .map_err(|e| ClientError::PrivateKeyDecrypt(e.to_string()))?;
    Ok(key.to_vec())
}

/// Try to read the private key for automatic unlock on client connect.
///
/// This attempts:
/// 1. Reading `identity.pk` (raw 32-byte key) — the common case.
/// 2. Reading `identity.pk.enc` + decrypting with `TAI_PASSPHRASE` env var —
///    for setups where the private key is encrypted at rest.
///
/// Returns `None` if no key can be resolved, which is fine — the daemon
/// starts locked but all session operations (create, browse, delete) work
/// without unlocking.  Only inference (RunInput) requires credentials.
pub fn try_auto_unlock_key() -> Option<Vec<u8>> {
    match read_raw_private_key() {
        Ok(key) => {
            info!("auto-unlock: using raw private key");
            return Some(key);
        }
        Err(ClientError::PrivateKeyInvalid) => {
            warn!("auto-unlock: private key file exists but is not 32 bytes");
        }
        Err(_) => {
            // No raw key available — fall through to encrypted path.
        }
    }

    if let Ok(passphrase) = std::env::var("TAI_PASSPHRASE")
        && !passphrase.is_empty()
        && let Ok(key) = read_encrypted_private_key(&passphrase)
    {
        info!("auto-unlock: using encrypted private key with env passphrase");
        return Some(key);
    }

    debug!("auto-unlock: no private key available (daemon will start locked)");
    None
}

/// Read and validate the public key file. Returns the 32-byte public key.
pub fn read_public_key_bytes() -> Result<[u8; 32], ClientError> {
    info!("reading public key");
    let path = tai_keystore::paths::public_key_path()
        .map_err(|e| ClientError::PublicKeyRead(e.to_string()))?;
    let data = std::fs::read(&path).map_err(|e| ClientError::PublicKeyRead(e.to_string()))?;
    if data.len() != 32 {
        return Err(ClientError::PublicKeyInvalid);
    }
    let mut key = [0u8; 32];
    key.copy_from_slice(&data);
    Ok(key)
}

fn parse_credential(
    credential_type: &str,
    fields: &[String],
) -> Result<ServiceCredential, ClientError> {
    match credential_type {
        "api_key" => {
            if fields.is_empty() {
                return Err(ClientError::CredentialParse(
                    "missing api_key field".to_string(),
                ));
            }
            Ok(ServiceCredential::ApiKey {
                key: fields[0].clone(),
            })
        }
        "x" => {
            if fields.len() < 5 {
                return Err(ClientError::CredentialParse(
                    "missing X credential fields".to_string(),
                ));
            }
            let bearer = if fields[4] == "-" {
                None
            } else {
                Some(fields[4].clone())
            };
            Ok(ServiceCredential::X {
                api_key: fields[0].clone(),
                api_key_secret: fields[1].clone(),
                access_token: fields[2].clone(),
                access_token_secret: fields[3].clone(),
                bearer_token: bearer,
            })
        }
        other => Err(ClientError::CredentialParse(format!(
            "unknown credential type: {other}"
        ))),
    }
}

/// Build an `AddCredential` message by reading the public key, constructing the
/// credential from the given type and fields, serialising, and encrypting.
///
/// If `unlock` is true, the raw private key is also bundled so the daemon can
/// decrypt immediately.
pub fn build_add_credential_message(
    service: String,
    credential_type: String,
    fields: Vec<String>,
    unlock: bool,
) -> Result<ClientMessage, ClientError> {
    debug!("building add credential message for service: {service}, type: {credential_type}");
    let pub_key = read_public_key_bytes()?;
    let credential = parse_credential(&credential_type, &fields)?;

    let plaintext =
        postcard::to_allocvec(&credential).map_err(|e| ClientError::Postcard(e.to_string()))?;

    let encrypted_payload = tai_keystore::crypto::encrypt_with_public_key(&pub_key, &plaintext)
        .map_err(|e| ClientError::Encryption(e.to_string()))?;

    let unlock_key = if unlock {
        tai_keystore::paths::private_key_path()
            .ok()
            .and_then(|p| std::fs::read(p).ok())
            .filter(|d| d.len() == 32)
    } else {
        None
    };

    Ok(ClientMessage::AddCredential {
        service,
        encrypted_payload,
        unlock_key,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_credential_api_key() {
        let cred = parse_credential("api_key", &["sk-test".into()]).unwrap();
        assert!(matches!(cred, ServiceCredential::ApiKey { ref key } if key == "sk-test"));
    }

    #[test]
    fn parse_credential_api_key_missing_field() {
        let result = parse_credential("api_key", &[]);
        assert!(result.is_err());
    }

    #[test]
    fn parse_credential_x() {
        let fields = vec![
            "ak".into(),
            "aks".into(),
            "at".into(),
            "ats".into(),
            "-".into(),
        ];
        let cred = parse_credential("x", &fields).unwrap();
        let view = cred.as_x().unwrap();
        assert_eq!(view.api_key, "ak");
        assert_eq!(view.api_key_secret, "aks");
        assert_eq!(view.access_token, "at");
        assert_eq!(view.access_token_secret, "ats");
        assert!(view.bearer_token.is_none());
    }

    #[test]
    fn parse_credential_x_with_bearer() {
        let fields = vec![
            "ak".into(),
            "aks".into(),
            "at".into(),
            "ats".into(),
            "bt".into(),
        ];
        let cred = parse_credential("x", &fields).unwrap();
        let view = cred.as_x().unwrap();
        assert_eq!(view.bearer_token.as_deref(), Some("bt"));
    }

    #[test]
    fn parse_credential_x_missing_fields() {
        let fields = vec!["ak".into(), "aks".into(), "at".into()];
        let result = parse_credential("x", &fields);
        assert!(result.is_err());
    }

    #[test]
    fn parse_credential_unknown_type() {
        let result = parse_credential("unknown", &[]);
        assert!(result.is_err());
    }

    // ── try_auto_unlock_key tests ──────────────────────────────────

    #[test]
    fn try_auto_unlock_key_with_raw_key() {
        let dir = tempfile::tempdir().unwrap();
        let _guard = tai_keystore::paths::TestConfigGuard::set_root(Some(dir.path().to_path_buf()));
        std::fs::create_dir_all(dir.path().join("tai-daemon")).unwrap();

        let (_, sk) = tai_keystore::crypto::generate_keypair();
        std::fs::write(dir.path().join("tai-daemon/identity.pk"), &sk).unwrap();

        assert_eq!(try_auto_unlock_key(), Some(sk.to_vec()));
    }

    #[test]
    fn try_auto_unlock_key_with_invalid_raw_key_length() {
        let dir = tempfile::tempdir().unwrap();
        let _guard = tai_keystore::paths::TestConfigGuard::set_root(Some(dir.path().to_path_buf()));
        std::fs::create_dir_all(dir.path().join("tai-daemon")).unwrap();

        // Write a file that isn't 32 bytes
        std::fs::write(dir.path().join("tai-daemon/identity.pk"), b"not 32 bytes").unwrap();

        assert!(try_auto_unlock_key().is_none());
    }

    #[test]
    fn try_auto_unlock_key_with_encrypted_key() {
        let dir = tempfile::tempdir().unwrap();
        let _guard = tai_keystore::paths::TestConfigGuard::set_root(Some(dir.path().to_path_buf()));
        std::fs::create_dir_all(dir.path().join("tai-daemon")).unwrap();

        let (_, sk) = tai_keystore::crypto::generate_keypair();
        let encrypted = tai_keystore::crypto::encrypt_private_key(&sk, "hunter2").unwrap();
        std::fs::write(dir.path().join("tai-daemon/identity.pk.enc"), &encrypted).unwrap();

        // Save and override the env var for the duration of this test.
        let old = std::env::var("TAI_PASSPHRASE").ok();
        // SAFETY: single-threaded test context; no other test reads TAI_PASSPHRASE.
        unsafe { std::env::set_var("TAI_PASSPHRASE", "hunter2") };
        let result = try_auto_unlock_key();
        // Restore the previous value (if any) or remove.
        // SAFETY: same single-threaded test context.
        match old {
            Some(v) => unsafe { std::env::set_var("TAI_PASSPHRASE", v) },
            None => unsafe { std::env::remove_var("TAI_PASSPHRASE") },
        }

        assert_eq!(result, Some(sk.to_vec()));
    }

    #[test]
    fn try_auto_unlock_key_with_no_keys() {
        let dir = tempfile::tempdir().unwrap();
        let _guard = tai_keystore::paths::TestConfigGuard::set_root(Some(dir.path().to_path_buf()));
        std::fs::create_dir_all(dir.path().join("tai-daemon")).unwrap();

        assert!(try_auto_unlock_key().is_none());
    }
}
