mod error;

pub use error::KeystoreError;

use aes_gcm::{
    Aes256Gcm, Nonce,
    aead::{Aead, KeyInit},
};
use argon2::Argon2;
use rand::RngExt;
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

    #[allow(deprecated)]
    pub fn save(&self, path: &Path, passphrase: &str) -> Result<(), KeystoreError> {
        let store = self.to_store();
        let plaintext = serde_json::to_vec(&store)?;

        let salt: [u8; SALT_LEN] = {
            let mut buf = [0u8; SALT_LEN];
            rand::rng().fill(&mut buf);
            buf
        };
        let key = derive_key(passphrase, &salt);
        let nonce_bytes: [u8; NONCE_LEN] = {
            let mut buf = [0u8; NONCE_LEN];
            rand::rng().fill(&mut buf);
            buf
        };
        let nonce = Nonce::from_slice(&nonce_bytes);
        let cipher =
            Aes256Gcm::new_from_slice(&key).map_err(|_| KeystoreError::InvalidKeyLength)?;
        let ciphertext = cipher
            .encrypt(nonce, plaintext.as_ref())
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

    #[allow(deprecated)]
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

        let key = derive_key(passphrase, &salt);
        let nonce = Nonce::from_slice(&nonce_bytes);
        let cipher =
            Aes256Gcm::new_from_slice(&key).map_err(|_| KeystoreError::InvalidKeyLength)?;
        let plaintext = cipher
            .decrypt(nonce, ciphertext)
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

fn derive_key(passphrase: &str, salt: &[u8; SALT_LEN]) -> [u8; KEY_LEN] {
    let mut output = [0u8; KEY_LEN];
    Argon2::default()
        .hash_password_into(passphrase.as_bytes(), salt, &mut output)
        .expect("argon2 key derivation failed");
    output
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
