use crate::error::KeystoreError;

use aes_gcm::{
    Aes256Gcm, Nonce,
    aead::{Aead, KeyInit},
};
use hkdf::Hkdf;
use rand::Rng;
use sha2::Sha256;
use tracing::{debug, trace};
use x25519_dalek::{PublicKey, StaticSecret};

const NONCE_LEN: usize = 12;
const SALT_LEN: usize = 32;
const KEY_LEN: usize = 32;

/// Generate a new X25519 keypair. Returns (secret, public).
pub fn generate_keypair() -> ([u8; 32], [u8; 32]) {
    let secret = StaticSecret::random_from_rng(&mut rand::rng());
    let public = PublicKey::from(&secret);
    let pk_bytes = public.to_bytes();
    trace!(?pk_bytes, "generated new X25519 keypair");
    (secret.to_bytes(), pk_bytes)
}

/// Private helper: create an AES-256-GCM cipher from a 32-byte key.
fn aead(key: &[u8; 32]) -> Result<Aes256Gcm, KeystoreError> {
    Aes256Gcm::new_from_slice(key).map_err(|_| KeystoreError::InvalidKeyLength)
}

/// Private helper: encrypt with AES-256-GCM.
fn aes_encrypt(
    key: &[u8; 32],
    nonce: &[u8; NONCE_LEN],
    plaintext: &[u8],
) -> Result<Vec<u8>, KeystoreError> {
    trace!(pt_len = plaintext.len(), "AES-256-GCM encrypt");
    let nonce = Nonce::from(*nonce);
    aead(key)?
        .encrypt(&nonce, plaintext)
        .map_err(|_| KeystoreError::EncryptionFailed)
}

/// Private helper: decrypt with AES-256-GCM.
fn aes_decrypt(
    key: &[u8; 32],
    nonce: &[u8; NONCE_LEN],
    ciphertext: &[u8],
) -> Result<Vec<u8>, KeystoreError> {
    trace!(ct_len = ciphertext.len(), "AES-256-GCM decrypt");
    let nonce = Nonce::from(*nonce);
    aead(key)?
        .decrypt(&nonce, ciphertext)
        .map_err(|_| KeystoreError::DecryptionFailed)
}

/// Encrypt `plaintext` with `pub_key` using ECDH + HKDF + AES-256-GCM.
///
/// Output format:
///   eph_public(32) || salt(32) || nonce(12) || ciphertext(rest)
pub fn encrypt_with_public_key(
    pub_key: &[u8; 32],
    plaintext: &[u8],
) -> Result<Vec<u8>, KeystoreError> {
    debug!(pt_len = plaintext.len(), "encrypting with public key");
    let eph_secret = StaticSecret::random_from_rng(&mut rand::rng());
    let eph_public = PublicKey::from(&eph_secret);
    let recipient = PublicKey::from(*pub_key);

    // ECDH
    let shared_secret = eph_secret.diffie_hellman(&recipient);
    let shared_bytes = shared_secret.as_bytes();

    // Generate independent salt (32B) and nonce (12B)
    let salt: [u8; SALT_LEN] = {
        let mut buf = [0u8; SALT_LEN];
        rand::rng().fill_bytes(&mut buf);
        buf
    };
    let nonce_bytes: [u8; NONCE_LEN] = {
        let mut buf = [0u8; NONCE_LEN];
        rand::rng().fill_bytes(&mut buf);
        buf
    };

    // Derive AES key via HKDF with a dedicated salt
    let hkdf = Hkdf::<Sha256>::new(Some(&salt), shared_bytes);
    let mut aes_key = [0u8; KEY_LEN];
    hkdf.expand(b"choreographr-credential-v2", &mut aes_key)
        .map_err(|_| KeystoreError::EncryptionFailed)?;

    // Encrypt
    let ciphertext = aes_encrypt(&aes_key, &nonce_bytes, plaintext)?;

    // Assemble: eph_pub(32) || salt(32) || nonce(12) || ciphertext
    let mut output = Vec::with_capacity(32 + SALT_LEN + NONCE_LEN + ciphertext.len());
    output.extend_from_slice(eph_public.as_bytes());
    output.extend_from_slice(&salt);
    output.extend_from_slice(&nonce_bytes);
    output.extend_from_slice(&ciphertext);
    Ok(output)
}

/// Decrypt data that was encrypted with `encrypt_with_public_key`.
pub fn decrypt_with_private_key(
    priv_key: &[u8; 32],
    data: &[u8],
) -> Result<Vec<u8>, KeystoreError> {
    debug!(data_len = data.len(), "decrypting with private key");
    if data.len() < 32 + SALT_LEN + NONCE_LEN {
        return Err(KeystoreError::TooShort);
    }

    let eph_pub_bytes: [u8; 32] = data[..32].try_into().map_err(|_| KeystoreError::TooShort)?;
    let salt: [u8; SALT_LEN] = data[32..64]
        .try_into()
        .map_err(|_| KeystoreError::TooShort)?;
    let nonce_bytes: [u8; NONCE_LEN] = data[64..76]
        .try_into()
        .map_err(|_| KeystoreError::TooShort)?;
    let ciphertext = &data[76..];

    let eph_public = PublicKey::from(eph_pub_bytes);
    let secret = StaticSecret::from(*priv_key);

    // ECDH
    let shared_secret = secret.diffie_hellman(&eph_public);
    let shared_bytes = shared_secret.as_bytes();

    // Derive AES key via HKDF with the stored salt
    let hkdf = Hkdf::<Sha256>::new(Some(&salt), shared_bytes);
    let mut aes_key = [0u8; KEY_LEN];
    hkdf.expand(b"choreographr-credential-v2", &mut aes_key)
        .map_err(|_| KeystoreError::DecryptionFailed)?;

    // Decrypt
    aes_decrypt(&aes_key, &nonce_bytes, ciphertext)
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
    fn encrypt_decrypt_various_plaintexts() {
        // Property-style: round trip for many plaintext lengths
        let (secret, public) = generate_keypair();
        for len in [0, 1, 16, 256, 1024, 4096] {
            let mut plaintext = vec![0u8; len];
            rand::rng().fill_bytes(&mut plaintext);

            let encrypted =
                encrypt_with_public_key(&public, &plaintext).expect("encryption should succeed");
            let decrypted =
                decrypt_with_private_key(&secret, &encrypted).expect("decryption should succeed");

            assert_eq!(decrypted, plaintext, "round trip failed for length {len}");
        }
    }
}
