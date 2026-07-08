use tai_keystore::ServiceCredential;
use tai_proto::ClientMessage;

use crate::shell::UnlockMethod;

/// Resolve a private key from the given unlock method.
///
/// For `UnlockMethod::Raw`, reads the raw private key file.
/// For `UnlockMethod::Passphrase`, reads the encrypted key file and decrypts it.
pub fn resolve_private_key(method: &UnlockMethod) -> Result<Vec<u8>, String> {
    match method {
        UnlockMethod::Raw => {
            let path = tai_keystore::paths::private_key_path().map_err(|e| e.to_string())?;
            let data =
                std::fs::read(&path).map_err(|e| format!("failed to read private key: {e}"))?;
            if data.len() != 32 {
                return Err("invalid private key file: expected 32 bytes".to_string());
            }
            Ok(data)
        }
        UnlockMethod::Passphrase(passphrase) => {
            let enc_path =
                tai_keystore::paths::private_key_enc_path().map_err(|e| e.to_string())?;
            let data = std::fs::read(&enc_path)
                .map_err(|e| format!("failed to read encrypted private key: {e}"))?;
            let key = tai_keystore::crypto::decrypt_private_key(&data, passphrase)
                .map_err(|e| format!("failed to decrypt private key: {e}"))?;
            Ok(key.to_vec())
        }
    }
}

/// Read and validate the public key file. Returns the 32-byte public key.
pub fn read_public_key_bytes() -> Result<[u8; 32], String> {
    let path = tai_keystore::paths::public_key_path().map_err(|e| e.to_string())?;
    let data = std::fs::read(&path).map_err(|e| format!("failed to read public key: {e}"))?;
    if data.len() != 32 {
        return Err("invalid public key file".to_string());
    }
    let mut key = [0u8; 32];
    key.copy_from_slice(&data);
    Ok(key)
}

fn parse_credential(credential_type: &str, fields: &[String]) -> Result<ServiceCredential, String> {
    match credential_type {
        "api_key" => {
            if fields.is_empty() {
                return Err("missing api_key field".to_string());
            }
            Ok(ServiceCredential::ApiKey {
                key: fields[0].clone(),
            })
        }
        "x" => {
            if fields.len() < 5 {
                return Err("missing X credential fields".to_string());
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
        other => Err(format!("unknown credential type: {other}")),
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
) -> Result<ClientMessage, String> {
    let pub_key = read_public_key_bytes()?;
    let credential = parse_credential(&credential_type, &fields)?;

    let plaintext = bincode::serde::encode_to_vec(&credential, bincode::config::standard())
        .map_err(|e| format!("serialize failed: {e}"))?;

    let encrypted_payload = tai_keystore::crypto::encrypt_with_public_key(&pub_key, &plaintext)
        .map_err(|e| format!("encryption failed: {e}"))?;

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
}
