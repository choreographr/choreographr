//! The crate's error type.

use thiserror::Error;

/// Errors surfaced by keystore operations.
#[derive(Error, Debug)]
pub enum KeystoreError {
    /// An underlying filesystem or socket I/O failure.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// The AEAD seal or key derivation failed while encrypting.
    #[error("encryption failed")]
    EncryptionFailed,
    /// The password is wrong, the ciphertext corrupt, or the key mismatched.
    #[error("incorrect passphrase, corrupted data, or wrong key")]
    DecryptionFailed,
    /// A key was not exactly the length the cipher requires.
    #[error("invalid key length")]
    InvalidKeyLength,
    /// The OS exposed no configuration directory to place the keystore in.
    #[error("could not determine config directory")]
    ConfigDirNotFound,
    /// The encrypted payload is shorter than its fixed header requires.
    #[error("encrypted data too short")]
    TooShort,
    /// The keystore encoding is not one this crate can decrypt.
    #[error("unsupported Polkadot-JS keystore encoding")]
    UnsupportedKeystoreFormat,
    /// The keystore payload is malformed or failed its consistency checks.
    #[error("malformed or corrupt keystore data")]
    InvalidKeystoreData,
}
