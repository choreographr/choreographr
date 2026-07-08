use std::io;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum KeystoreError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("keystore file is too short")]
    TooShort,
    #[error("invalid keystore magic bytes")]
    InvalidMagic,
    #[error("unsupported keystore version: {0}")]
    UnsupportedVersion(u8),
    #[error("invalid key length")]
    InvalidKeyLength,
    #[error("encryption failed")]
    EncryptionFailed,
    #[error("incorrect passphrase or corrupted keystore")]
    DecryptionFailed,
    #[error("invalid keystore data: {0}")]
    InvalidData(#[from] serde_json::Error),
    #[error("keystore already exists, use load instead")]
    AlreadyExists,
    #[error("could not determine config directory")]
    ConfigDirNotFound,
}
