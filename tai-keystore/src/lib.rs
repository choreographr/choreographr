mod error;

pub use error::KeystoreError;

use aes_gcm::{
    Aes256Gcm, Nonce,
    aead::{Aead, KeyInit},
};
use argon2::Argon2;
use rand::Rng;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
};
use zeroize::Zeroize;

const MAGIC: &[u8; 4] = b"TAIK";
const VERSION: u8 = 1;
const SALT_LEN: usize = 32;
const NONCE_LEN: usize = 12;
const KEY_LEN: usize = 32;

#[derive(Debug, Clone, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CredentialStore {
    version: u32,
    services: HashMap<String, ServiceCredential>,
}

#[derive(Debug, Clone)]
pub struct Keystore {
    services: HashMap<String, ServiceCredential>,
}

impl Default for Keystore {
    fn default() -> Self {
        Self::new()
    }
}

impl Keystore {
    pub fn new() -> Self {
        Self {
            services: HashMap::new(),
        }
    }

    pub fn add(&mut self, service: String, credential: ServiceCredential) {
        self.services.insert(service, credential);
    }

    pub fn remove(&mut self, service: &str) -> bool {
        self.services.remove(service).is_some()
    }

    pub fn get(&self, service: &str) -> Option<&ServiceCredential> {
        self.services.get(service)
    }

    pub fn get_api_key(&self, service: &str) -> Option<&str> {
        match self.services.get(service)? {
            ServiceCredential::ApiKey { key } => Some(key.as_str()),
            _ => None,
        }
    }

    pub fn get_x_credentials(&self, service: &str) -> Option<XCredentials> {
        match self.services.get(service)? {
            ServiceCredential::X {
                api_key,
                api_key_secret,
                access_token,
                access_token_secret,
                bearer_token,
            } => Some(XCredentials {
                api_key: api_key.clone(),
                api_key_secret: api_key_secret.clone(),
                access_token: access_token.clone(),
                access_token_secret: access_token_secret.clone(),
                bearer_token: bearer_token.clone(),
            }),
            _ => None,
        }
    }

    pub fn service_names(&self) -> impl Iterator<Item = &String> {
        self.services.keys()
    }

    fn from_store(store: CredentialStore) -> Self {
        Self {
            services: store.services,
        }
    }

    fn to_store(&self) -> CredentialStore {
        CredentialStore {
            version: 1,
            services: self.services.clone(),
        }
    }

    pub fn save(&self, path: &Path, passphrase: &str) -> Result<(), KeystoreError> {
        let store = self.to_store();
        let plaintext = serde_json::to_vec(&store)?;

        let salt: [u8; SALT_LEN] = {
            let mut buf = [0u8; SALT_LEN];
            rand::rng().fill_bytes(&mut buf);
            buf
        };
        let key = derive_key(passphrase, &salt)?;
        let nonce_bytes: [u8; NONCE_LEN] = {
            let mut buf = [0u8; NONCE_LEN];
            rand::rng().fill_bytes(&mut buf);
            buf
        };
        let nonce =
            Nonce::try_from(&nonce_bytes[..]).map_err(|_| KeystoreError::EncryptionFailed)?;
        let cipher =
            Aes256Gcm::new_from_slice(&key).map_err(|_| KeystoreError::InvalidKeyLength)?;
        let ciphertext = cipher
            .encrypt(&nonce, plaintext.as_ref())
            .map_err(|_| KeystoreError::EncryptionFailed)?;

        let mut data = Vec::with_capacity(4 + 1 + SALT_LEN + NONCE_LEN + ciphertext.len());
        data.extend_from_slice(MAGIC);
        data.push(VERSION);
        data.extend_from_slice(&salt);
        data.extend_from_slice(&nonce_bytes);
        data.extend_from_slice(&ciphertext);

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("tmp");
        fs::write(&tmp, &data)?;
        fs::rename(&tmp, path)?;

        Ok(())
    }

    pub fn load(path: &Path, passphrase: &str) -> Result<Self, KeystoreError> {
        let data = fs::read(path)?;
        if data.len() < 4 + 1 + SALT_LEN + NONCE_LEN + 16 {
            return Err(KeystoreError::TooShort);
        }

        if &data[..4] != MAGIC {
            return Err(KeystoreError::InvalidMagic);
        }

        let version = data[4];
        if version != VERSION {
            return Err(KeystoreError::UnsupportedVersion(version));
        }

        let salt: [u8; SALT_LEN] = data[5..5 + SALT_LEN]
            .try_into()
            .map_err(|_| KeystoreError::TooShort)?;
        let nonce_bytes: [u8; NONCE_LEN] = data[5 + SALT_LEN..5 + SALT_LEN + NONCE_LEN]
            .try_into()
            .map_err(|_| KeystoreError::TooShort)?;
        let ciphertext = &data[5 + SALT_LEN + NONCE_LEN..];

        let key = derive_key(passphrase, &salt)?;
        let nonce =
            Nonce::try_from(&nonce_bytes[..]).map_err(|_| KeystoreError::DecryptionFailed)?;
        let cipher =
            Aes256Gcm::new_from_slice(&key).map_err(|_| KeystoreError::InvalidKeyLength)?;
        let plaintext = cipher
            .decrypt(&nonce, ciphertext)
            .map_err(|_| KeystoreError::DecryptionFailed)?;

        let store: CredentialStore = serde_json::from_slice(&plaintext)?;

        Ok(Self::from_store(store))
    }

    pub fn init(path: &Path, passphrase: &str) -> Result<Self, KeystoreError> {
        if path.exists() {
            return Err(KeystoreError::AlreadyExists);
        }
        let keystore = Self::new();
        keystore.save(path, passphrase)?;
        Ok(keystore)
    }
}

fn derive_key(passphrase: &str, salt: &[u8; SALT_LEN]) -> Result<[u8; KEY_LEN], KeystoreError> {
    let mut output = [0u8; KEY_LEN];
    Argon2::default()
        .hash_password_into(passphrase.as_bytes(), salt, &mut output)
        .map_err(|_| KeystoreError::EncryptionFailed)?;
    Ok(output)
}

pub fn keystore_path() -> Result<PathBuf, KeystoreError> {
    if let Ok(override_path) = std::env::var("TAI_KEYSTORE_PATH") {
        return Ok(PathBuf::from(override_path));
    }
    let config_dir = dirs::config_dir().ok_or(KeystoreError::ConfigDirNotFound)?;
    Ok(config_dir.join("tai-daemon").join("credentials.enc"))
}

#[derive(Debug, Clone, Zeroize)]
pub struct XCredentials {
    pub api_key: String,
    pub api_key_secret: String,
    pub access_token: String,
    pub access_token_secret: String,
    pub bearer_token: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_keystore_is_empty() {
        let ks = Keystore::new();
        assert!(ks.get("any").is_none());
        assert_eq!(ks.service_names().count(), 0);
    }

    #[test]
    fn add_and_get_api_key() {
        let mut ks = Keystore::new();
        ks.add(
            "openai".to_string(),
            ServiceCredential::ApiKey {
                key: "sk-test".to_string(),
            },
        );
        assert_eq!(ks.service_names().count(), 1);
        assert_eq!(ks.get_api_key("openai"), Some("sk-test"));
        assert!(ks.get_x_credentials("openai").is_none());
    }

    #[test]
    fn add_and_get_x_credentials() {
        let mut ks = Keystore::new();
        ks.add(
            "twitter".to_string(),
            ServiceCredential::X {
                api_key: "ak".into(),
                api_key_secret: "aks".into(),
                access_token: "at".into(),
                access_token_secret: "ats".into(),
                bearer_token: Some("bt".into()),
            },
        );
        assert!(ks.get_api_key("twitter").is_none());
        let creds = ks.get_x_credentials("twitter").unwrap();
        assert_eq!(creds.api_key, "ak");
        assert_eq!(creds.api_key_secret, "aks");
        assert_eq!(creds.access_token, "at");
        assert_eq!(creds.access_token_secret, "ats");
        assert_eq!(creds.bearer_token, Some("bt".to_string()));
    }

    #[test]
    fn remove_credential() {
        let mut ks = Keystore::new();
        ks.add(
            "svc".to_string(),
            ServiceCredential::ApiKey { key: "k".into() },
        );
        assert!(ks.remove("svc"));
        assert!(!ks.remove("svc"));
        assert!(ks.get("svc").is_none());
    }

    #[test]
    fn service_names_iterates_all_services() {
        let mut ks = Keystore::new();
        ks.add(
            "a".to_string(),
            ServiceCredential::ApiKey { key: "1".into() },
        );
        ks.add(
            "b".to_string(),
            ServiceCredential::ApiKey { key: "2".into() },
        );
        let mut names: Vec<_> = ks.service_names().collect();
        names.sort();
        assert_eq!(names, vec![&"a".to_string(), &"b".to_string()]);
    }

    #[test]
    fn to_store_from_store_round_trip() {
        let mut ks = Keystore::new();
        ks.add(
            "s1".to_string(),
            ServiceCredential::ApiKey {
                key: "api-key".into(),
            },
        );
        ks.add(
            "s2".to_string(),
            ServiceCredential::X {
                api_key: "a".into(),
                api_key_secret: "as".into(),
                access_token: "at".into(),
                access_token_secret: "ats".into(),
                bearer_token: None,
            },
        );

        let store = ks.to_store();
        let restored = Keystore::from_store(store);

        assert_eq!(restored.get_api_key("s1"), Some("api-key"));
        let x = restored.get_x_credentials("s2").unwrap();
        assert_eq!(x.api_key, "a");
        assert_eq!(x.bearer_token, None);
        assert_eq!(restored.service_names().count(), 2);
    }

    #[test]
    fn derive_key_is_deterministic() {
        let salt = [0xAAu8; SALT_LEN];
        let k1 = derive_key("passphrase", &salt).unwrap();
        let k2 = derive_key("passphrase", &salt).unwrap();
        assert_eq!(k1, k2);
    }

    #[test]
    fn derive_key_different_salt_produces_different_output() {
        let salt1 = [0x01u8; SALT_LEN];
        let salt2 = [0x02u8; SALT_LEN];
        let k1 = derive_key("passphrase", &salt1).unwrap();
        let k2 = derive_key("passphrase", &salt2).unwrap();
        assert_ne!(k1, k2);
    }

    #[test]
    fn derive_key_different_passphrase_produces_different_output() {
        let salt = [0xFFu8; SALT_LEN];
        let k1 = derive_key("alpha", &salt).unwrap();
        let k2 = derive_key("beta", &salt).unwrap();
        assert_ne!(k1, k2);
    }

    #[test]
    fn credential_store_serialization_round_trip() {
        let store = CredentialStore {
            version: 1,
            services: {
                let mut m = std::collections::HashMap::new();
                m.insert(
                    "test".to_string(),
                    ServiceCredential::ApiKey { key: "val".into() },
                );
                m
            },
        };
        let json = serde_json::to_vec(&store).unwrap();
        let restored: CredentialStore = serde_json::from_slice(&json).unwrap();
        assert_eq!(restored.version, 1);
        let key = restored.services.get("test").unwrap();
        assert!(matches!(key, ServiceCredential::ApiKey { key } if key == "val"));
    }

    #[test]
    fn credential_store_json_round_trip() {
        let json = r#"{"version":1,"services":{"x":{"type":"api_key","key":"secret"}}}"#;
        let store: CredentialStore = serde_json::from_str(json).unwrap();
        assert_eq!(store.version, 1);
        assert!(matches!(
            store.services.get("x"),
            Some(ServiceCredential::ApiKey { key }) if key == "secret"
        ));
        let re_serialized = serde_json::to_string(&store).unwrap();
        let re_deserialized: CredentialStore = serde_json::from_str(&re_serialized).unwrap();
        assert_eq!(re_deserialized.version, 1);
        assert!(matches!(
            re_deserialized.services.get("x"),
            Some(ServiceCredential::ApiKey { key }) if key == "secret"
        ));
    }
}
