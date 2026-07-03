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

impl From<KeystoreError> for io::Error {
    fn from(error: KeystoreError) -> Self {
        match error {
            KeystoreError::Io(io) => io,
            KeystoreError::TooShort
            | KeystoreError::InvalidMagic
            | KeystoreError::InvalidData(_) => {
                io::Error::new(io::ErrorKind::InvalidData, error)
            }
            KeystoreError::DecryptionFailed => {
                io::Error::new(io::ErrorKind::PermissionDenied, error)
            }
            KeystoreError::AlreadyExists => {
                io::Error::new(io::ErrorKind::AlreadyExists, error)
            }
            KeystoreError::ConfigDirNotFound => {
                io::Error::new(io::ErrorKind::NotFound, error)
            }
            _ => io::Error::other(error),
        }
    }
}
