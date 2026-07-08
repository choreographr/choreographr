use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use tai_keystore::KeystoreError;
use tai_keystore::crypto::{
    decrypt_private_key, decrypt_with_private_key, encrypt_private_key, encrypt_with_public_key,
    generate_keypair,
};

static TEST_COUNTER: AtomicU32 = AtomicU32::new(0);

fn test_dir() -> PathBuf {
    let id = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("tai-keystore-crypto-test-{id}"))
}

#[test]
#[ignore]
fn encrypt_decrypt_credential_round_trip_with_key_files() {
    let dir = test_dir();
    fs::create_dir_all(&dir).expect("create test dir");

    let (secret, public) = generate_keypair();

    // Write key files as the system would
    let pk_path = dir.join("identity.pk");
    let pub_path = dir.join("public.pk");
    fs::write(&pk_path, &secret).expect("write private key");
    fs::write(&pub_path, &public).expect("write public key");

    // Read them back through the paths module (with overridden config)
    let stored_private = fs::read(&pk_path).expect("read private key");
    let stored_public = fs::read(&pub_path).expect("read public key");
    assert_eq!(stored_private.len(), 32);
    assert_eq!(stored_public.len(), 32);

    let mut priv_arr = [0u8; 32];
    priv_arr.copy_from_slice(&stored_private);
    let mut pub_arr = [0u8; 32];
    pub_arr.copy_from_slice(&stored_public);

    // Credential plaintext
    let plaintext = b"openai:sk-test-credential-value";

    // Encrypt with public key
    let encrypted =
        encrypt_with_public_key(&pub_arr, plaintext).expect("encryption should succeed");

    // Decrypt with private key
    let decrypted =
        decrypt_with_private_key(&priv_arr, &encrypted).expect("decryption should succeed");

    assert_eq!(decrypted, plaintext, "decrypted must match original");

    let _ = fs::remove_dir_all(&dir);
}

#[test]
#[ignore]
fn encrypt_decrypt_private_key_with_passphrase_round_trip() {
    let mut key = [0u8; 32];
    for i in 0..32 {
        key[i] = i as u8;
    }
    let passphrase = "test-passphrase-for-integration-test";

    let encrypted =
        encrypt_private_key(&key, passphrase).expect("private key encryption should succeed");
    assert!(
        encrypted.len() > 32 + 12,
        "encrypted output should have salt + nonce + ciphertext"
    );

    let decrypted =
        decrypt_private_key(&encrypted, passphrase).expect("private key decryption should succeed");
    assert_eq!(decrypted, key, "decrypted key must match original");
}

#[test]
#[ignore]
fn decrypt_private_key_with_wrong_passphrase_fails() {
    let mut key = [0u8; 32];
    for i in 0..32 {
        key[i] = i as u8;
    }

    let encrypted =
        encrypt_private_key(&key, "correct-passphrase").expect("encryption should succeed");
    let result = decrypt_private_key(&encrypted, "wrong-passphrase");

    assert!(
        matches!(result, Err(KeystoreError::DecryptionFailed)),
        "expected DecryptionFailed, got {:?}",
        result
    );
}

#[test]
#[ignore]
fn decrypt_credential_with_wrong_key_fails() {
    let (_secret, public) = generate_keypair();
    let (wrong_secret, _) = generate_keypair();
    let plaintext = b"secret data";

    let encrypted = encrypt_with_public_key(&public, plaintext).expect("encryption should succeed");
    let result = decrypt_with_private_key(&wrong_secret, &encrypted);

    assert!(
        matches!(result, Err(KeystoreError::DecryptionFailed)),
        "expected DecryptionFailed, got {:?}",
        result
    );
}

#[test]
#[ignore]
fn decrypt_too_short_data_fails() {
    let secret = [0u8; 32];
    let result = decrypt_with_private_key(&secret, &[0u8; 10]);
    assert!(
        matches!(result, Err(KeystoreError::TooShort)),
        "expected TooShort, got {:?}",
        result
    );
}
