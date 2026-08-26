use thiserror::Error;

#[derive(Error, Debug)]
pub enum KeystoreError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("encryption failed")]
    EncryptionFailed,
    #[error("incorrect passphrase, corrupted data, or wrong key")]
    DecryptionFailed,
    #[error("invalid key length")]
    InvalidKeyLength,
    #[error("could not determine config directory")]
    ConfigDirNotFound,
    #[error("encrypted data too short")]
    TooShort,
    #[error("unsupported Polkadot-JS keystore encoding")]
    UnsupportedKeystoreFormat,
    #[error("malformed or corrupt keystore data")]
    InvalidKeystoreData,
}
