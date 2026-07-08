use crate::error::KeystoreError;

use aes_gcm::{
    Aes256Gcm, Nonce,
    aead::{Aead, KeyInit},
};
use argon2::Argon2;
use hkdf::Hkdf;
use rand::Rng;
use rand_core::OsRng;
use sha2::Sha256;
use x25519_dalek::{PublicKey, StaticSecret};

const NONCE_LEN: usize = 12;
const SALT_LEN: usize = 32;
const KEY_LEN: usize = 32;

/// Generate a new X25519 keypair. Returns (secret, public).
pub fn generate_keypair() -> ([u8; 32], [u8; 32]) {
    let secret = StaticSecret::random_from_rng(OsRng);
    let public = PublicKey::from(&secret);
    (secret.to_bytes(), public.to_bytes())
}

/// Encrypt `plaintext` with `pub_key` using ECDH + HKDF + AES-256-GCM.
///
/// Output format:
///   eph_public(32) || nonce(12) || ciphertext(rest)
pub fn encrypt_with_public_key(
    pub_key: &[u8; 32],
    plaintext: &[u8],
) -> Result<Vec<u8>, KeystoreError> {
    let eph_secret = StaticSecret::random_from_rng(OsRng);
    let eph_public = PublicKey::from(&eph_secret);
    let recipient = PublicKey::from(*pub_key);

    // ECDH
    let shared_secret = eph_secret.diffie_hellman(&recipient);
    let shared_bytes = shared_secret.as_bytes();

    // Derive AES key via HKDF
    let nonce_bytes: [u8; NONCE_LEN] = {
        let mut buf = [0u8; NONCE_LEN];
        rand::rng().fill_bytes(&mut buf);
        buf
    };
    let hkdf = Hkdf::<Sha256>::new(Some(&nonce_bytes), shared_bytes);
    let mut aes_key = [0u8; KEY_LEN];
    hkdf.expand(b"tai-credential-v2", &mut aes_key)
        .map_err(|_| KeystoreError::EncryptionFailed)?;

    // Encrypt
    let nonce = Nonce::try_from(&nonce_bytes[..]).map_err(|_| KeystoreError::EncryptionFailed)?;
    let cipher =
        Aes256Gcm::new_from_slice(&aes_key).map_err(|_| KeystoreError::InvalidKeyLength)?;
    let ciphertext = cipher
        .encrypt(&nonce, plaintext)
        .map_err(|_| KeystoreError::EncryptionFailed)?;

    // Assemble: eph_pub(32) || nonce(12) || ciphertext
    let mut output = Vec::with_capacity(32 + NONCE_LEN + ciphertext.len());
    output.extend_from_slice(eph_public.as_bytes());
    output.extend_from_slice(&nonce_bytes);
    output.extend_from_slice(&ciphertext);
    Ok(output)
}

/// Decrypt data that was encrypted with `encrypt_with_public_key`.
pub fn decrypt_with_private_key(
    priv_key: &[u8; 32],
    data: &[u8],
) -> Result<Vec<u8>, KeystoreError> {
    if data.len() < 32 + NONCE_LEN {
        return Err(KeystoreError::TooShort);
    }

    let eph_pub_bytes: [u8; 32] = data[..32].try_into().map_err(|_| KeystoreError::TooShort)?;
    let nonce_bytes: [u8; NONCE_LEN] = data[32..44]
        .try_into()
        .map_err(|_| KeystoreError::TooShort)?;
    let ciphertext = &data[44..];

    let eph_public = PublicKey::from(eph_pub_bytes);
    let secret = StaticSecret::from(*priv_key);

    // ECDH
    let shared_secret = secret.diffie_hellman(&eph_public);
    let shared_bytes = shared_secret.as_bytes();

    // Derive AES key via HKDF
    let hkdf = Hkdf::<Sha256>::new(Some(&nonce_bytes), shared_bytes);
    let mut aes_key = [0u8; KEY_LEN];
    hkdf.expand(b"tai-credential-v2", &mut aes_key)
        .map_err(|_| KeystoreError::DecryptionFailed)?;

    // Decrypt
    let nonce = Nonce::try_from(&nonce_bytes[..]).map_err(|_| KeystoreError::DecryptionFailed)?;
    let cipher =
        Aes256Gcm::new_from_slice(&aes_key).map_err(|_| KeystoreError::InvalidKeyLength)?;
    cipher
        .decrypt(&nonce, ciphertext)
        .map_err(|_| KeystoreError::DecryptionFailed)
}

/// Encrypt a private key bytes with a passphrase (argon2 + AES-256-GCM).
///
/// Output format:
///   salt(32) || nonce(12) || ciphertext(32) = 76 bytes
pub fn encrypt_private_key(
    priv_key: &[u8; 32],
    passphrase: &str,
) -> Result<Vec<u8>, KeystoreError> {
    let salt: [u8; SALT_LEN] = {
        let mut buf = [0u8; SALT_LEN];
        rand::rng().fill_bytes(&mut buf);
        buf
    };

    let mut key = [0u8; KEY_LEN];
    Argon2::default()
        .hash_password_into(passphrase.as_bytes(), &salt, &mut key)
        .map_err(|_| KeystoreError::EncryptionFailed)?;

    let nonce_bytes: [u8; NONCE_LEN] = {
        let mut buf = [0u8; NONCE_LEN];
        rand::rng().fill_bytes(&mut buf);
        buf
    };

    let nonce = Nonce::try_from(&nonce_bytes[..]).map_err(|_| KeystoreError::EncryptionFailed)?;
    let cipher = Aes256Gcm::new_from_slice(&key).map_err(|_| KeystoreError::InvalidKeyLength)?;
    let ciphertext = cipher
        .encrypt(&nonce, priv_key.as_ref())
        .map_err(|_| KeystoreError::EncryptionFailed)?;

    let mut output = Vec::with_capacity(SALT_LEN + NONCE_LEN + ciphertext.len());
    output.extend_from_slice(&salt);
    output.extend_from_slice(&nonce_bytes);
    output.extend_from_slice(&ciphertext);
    Ok(output)
}

/// Decrypt a private key that was encrypted with `encrypt_private_key`.
pub fn decrypt_private_key(data: &[u8], passphrase: &str) -> Result<[u8; 32], KeystoreError> {
    if data.len() < SALT_LEN + NONCE_LEN + KEY_LEN {
        return Err(KeystoreError::TooShort);
    }

    let salt: [u8; SALT_LEN] = data[..SALT_LEN]
        .try_into()
        .map_err(|_| KeystoreError::TooShort)?;
    let nonce_bytes: [u8; NONCE_LEN] = data[SALT_LEN..SALT_LEN + NONCE_LEN]
        .try_into()
        .map_err(|_| KeystoreError::TooShort)?;
    let ciphertext = &data[SALT_LEN + NONCE_LEN..];

    let mut key = [0u8; KEY_LEN];
    Argon2::default()
        .hash_password_into(passphrase.as_bytes(), &salt, &mut key)
        .map_err(|_| KeystoreError::EncryptionFailed)?;

    let nonce = Nonce::try_from(&nonce_bytes[..]).map_err(|_| KeystoreError::DecryptionFailed)?;
    let cipher = Aes256Gcm::new_from_slice(&key).map_err(|_| KeystoreError::InvalidKeyLength)?;
    let plaintext = cipher
        .decrypt(&nonce, ciphertext)
        .map_err(|_| KeystoreError::DecryptionFailed)?;

    plaintext
        .try_into()
        .map_err(|_| KeystoreError::DecryptionFailed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_keypair_returns_non_zero() {
        let (_secret, public) = generate_keypair();
        assert!(
            public.iter().any(|&b| b != 0),
            "public should not be all zeros"
        );
    }

    #[test]
    fn encrypt_decrypt_round_trip() {
        let (secret, public) = generate_keypair();
        let plaintext = b"hello world, this is a secret credential payload";

        let encrypted =
            encrypt_with_public_key(&public, plaintext).expect("encryption should succeed");
        let decrypted =
            decrypt_with_private_key(&secret, &encrypted).expect("decryption should succeed");

        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn decrypt_with_wrong_key_fails() {
        let (_secret, public) = generate_keypair();
        let (wrong_secret, _) = generate_keypair();
        let plaintext = b"secret data";

        let encrypted =
            encrypt_with_public_key(&public, plaintext).expect("encryption should succeed");
        let result = decrypt_with_private_key(&wrong_secret, &encrypted);

        assert!(
            matches!(result, Err(KeystoreError::DecryptionFailed)),
            "expected DecryptionFailed, got {:?}",
            result
        );
    }

    #[test]
    fn encrypt_private_key_decrypt_private_key_round_trip() {
        let mut key = [0u8; 32];
        rand::rng().fill_bytes(&mut key);
        let passphrase = "correct horse battery staple";

        let encrypted = encrypt_private_key(&key, passphrase).expect("encryption should succeed");
        let decrypted =
            decrypt_private_key(&encrypted, passphrase).expect("decrypt should succeed");

        assert_eq!(decrypted, key);
    }

    #[test]
    fn decrypt_private_key_wrong_passphrase_fails() {
        let mut key = [0u8; 32];
        rand::rng().fill_bytes(&mut key);
        let passphrase = "correct passphrase";

        let encrypted = encrypt_private_key(&key, passphrase).expect("encryption should succeed");
        let result = decrypt_private_key(&encrypted, "wrong passphrase");

        assert!(
            matches!(result, Err(KeystoreError::DecryptionFailed)),
            "expected DecryptionFailed, got {:?}",
            result
        );
    }
}
