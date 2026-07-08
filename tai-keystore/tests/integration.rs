use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use tai_keystore::{Keystore, KeystoreError, ServiceCredential};

static TEST_COUNTER: AtomicU32 = AtomicU32::new(0);

fn temp_path() -> PathBuf {
    let id = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("tai-keystore-test-{id}.enc"))
}

#[test]
#[ignore]
fn test_init_and_load() {
    let path = temp_path();
    let passphrase = "initial-passphrase";

    let ks = Keystore::init(&path, passphrase).expect("init should succeed");
    assert!(
        ks.service_names().count() == 0,
        "new keystore should be empty"
    );

    let loaded = Keystore::load(&path, passphrase).expect("load should succeed");
    assert!(
        loaded.service_names().count() == 0,
        "loaded keystore should be empty"
    );

    let _ = fs::remove_file(&path);
}

#[test]
#[ignore]
fn test_save_and_load_round_trip() {
    let path = temp_path();
    let passphrase = "round-trip-pw";

    let mut ks = Keystore::new();
    ks.add(
        "openai".into(),
        ServiceCredential::ApiKey {
            key: "sk-test-openai-key-123".into(),
        },
    );
    ks.add(
        "twitter".into(),
        ServiceCredential::X {
            api_key: "x-api-key".into(),
            api_key_secret: "x-api-secret".into(),
            access_token: "x-access-token".into(),
            access_token_secret: "x-access-token-secret".into(),
            bearer_token: Some("x-bearer-token".into()),
        },
    );

    ks.save(&path, passphrase).expect("save should succeed");

    let loaded = Keystore::load(&path, passphrase).expect("load should succeed");

    let openai = loaded.get("openai").expect("openai service should exist");
    match openai {
        ServiceCredential::ApiKey { key } => assert_eq!(key, "sk-test-openai-key-123"),
        _ => panic!("expected ApiKey variant"),
    }

    let twitter = loaded.get("twitter").expect("twitter service should exist");
    match twitter {
        ServiceCredential::X {
            api_key,
            api_key_secret,
            access_token,
            access_token_secret,
            bearer_token,
        } => {
            assert_eq!(api_key, "x-api-key");
            assert_eq!(api_key_secret, "x-api-secret");
            assert_eq!(access_token, "x-access-token");
            assert_eq!(access_token_secret, "x-access-token-secret");
            assert_eq!(bearer_token.as_deref(), Some("x-bearer-token"));
        }
        _ => panic!("expected X variant"),
    }

    let _ = fs::remove_file(&path);
}

#[test]
#[ignore]
fn test_wrong_passphrase() {
    let path = temp_path();
    let passphrase = "correct-pass";

    let ks = Keystore::new();
    ks.save(&path, passphrase).expect("save should succeed");

    let result = Keystore::load(&path, "wrong-pass");
    match result {
        Err(KeystoreError::DecryptionFailed) => {}
        other => panic!("expected DecryptionFailed, got {other:?}"),
    }

    let _ = fs::remove_file(&path);
}

#[test]
#[ignore]
fn test_corrupted_file() {
    let path = temp_path();
    let passphrase = "corrupt-test-pass";

    let mut ks = Keystore::new();
    ks.add("svc".into(), ServiceCredential::ApiKey { key: "k".into() });
    ks.save(&path, passphrase).expect("save should succeed");

    let mut data = fs::read(&path).expect("should read file");
    let ciphertext_start = 4 + 1 + 32 + 12;
    assert!(
        data.len() > ciphertext_start + 10,
        "file should be large enough for ciphertext corruption"
    );
    data[ciphertext_start] ^= 0xFF;
    fs::write(&path, &data).expect("should write corrupted file");

    let result = Keystore::load(&path, passphrase);
    match result {
        Err(KeystoreError::DecryptionFailed) => {}
        other => panic!("expected DecryptionFailed, got {other:?}"),
    }

    let _ = fs::remove_file(&path);
}

#[test]
#[ignore]
fn test_invalid_magic() {
    let path = temp_path();

    let mut data = vec![0u8; 65];
    data[..4].copy_from_slice(b"BADC");
    fs::write(&path, &data).expect("should write bad-magic file");

    let result = Keystore::load(&path, "irrelevant");
    match result {
        Err(KeystoreError::InvalidMagic) => {}
        other => panic!("expected InvalidMagic, got {other:?}"),
    }

    let _ = fs::remove_file(&path);
}

#[test]
#[ignore]
fn test_file_too_short() {
    let path = temp_path();

    fs::write(&path, b"12345").expect("should write tiny file");

    let result = Keystore::load(&path, "irrelevant");
    match result {
        Err(KeystoreError::TooShort) => {}
        other => panic!("expected TooShort, got {other:?}"),
    }

    let _ = fs::remove_file(&path);
}

#[test]
#[ignore]
fn test_already_exists() {
    let path = temp_path();
    let passphrase = "already-exists-pass";

    Keystore::init(&path, passphrase).expect("first init should succeed");

    let result = Keystore::init(&path, passphrase);
    match result {
        Err(KeystoreError::AlreadyExists) => {}
        other => panic!("expected AlreadyExists, got {other:?}"),
    }

    let _ = fs::remove_file(&path);
}

#[test]
#[ignore]
fn test_add_remove_credentials() {
    let mut ks = Keystore::new();

    ks.add(
        "myservice".into(),
        ServiceCredential::ApiKey {
            key: "my-api-key".into(),
        },
    );
    assert!(ks.get("myservice").is_some());

    let removed = ks.remove("myservice");
    assert!(removed, "should return true when removing existing service");
    assert!(ks.get("myservice").is_none());

    let removed_again = ks.remove("myservice");
    assert!(
        !removed_again,
        "should return false when removing nonexistent service"
    );
}

#[test]
#[ignore]
fn test_get_api_key_vs_x() {
    let mut ks = Keystore::new();

    ks.add(
        "openai".into(),
        ServiceCredential::ApiKey {
            key: "sk-key".into(),
        },
    );
    ks.add(
        "twitter".into(),
        ServiceCredential::X {
            api_key: "x-api".into(),
            api_key_secret: "x-secret".into(),
            access_token: "x-token".into(),
            access_token_secret: "x-token-secret".into(),
            bearer_token: None,
        },
    );

    assert_eq!(ks.get_api_key("openai"), Some("sk-key"));
    assert!(ks.get_api_key("twitter").is_none());

    let x_creds = ks.get("twitter")
        .and_then(ServiceCredential::as_x)
        .expect("twitter should have X creds");
    assert_eq!(x_creds.api_key, "x-api");
    assert_eq!(x_creds.api_key_secret, "x-secret");
    assert_eq!(x_creds.access_token, "x-token");
    assert_eq!(x_creds.access_token_secret, "x-token-secret");
    assert!(x_creds.bearer_token.is_none());

    assert!(ks.get("openai").and_then(ServiceCredential::as_x).is_none());
}

#[test]
#[ignore]
fn test_service_names() {
    let mut ks = Keystore::new();

    ks.add("a".into(), ServiceCredential::ApiKey { key: "k1".into() });
    ks.add("b".into(), ServiceCredential::ApiKey { key: "k2".into() });
    ks.add("c".into(), ServiceCredential::ApiKey { key: "k3".into() });

    let mut names: Vec<String> = ks.service_names().cloned().collect();
    names.sort();
    assert_eq!(names, vec!["a", "b", "c"]);
}

#[test]
#[ignore]
fn test_unsupported_version() {
    let path = temp_path();
    let passphrase = "version-test-pass";

    let ks = Keystore::new();
    ks.save(&path, passphrase).expect("save should succeed");

    let mut data = fs::read(&path).expect("should read file");
    assert_eq!(data[4], 1, "saved version should be 1");
    data[4] = 99;
    fs::write(&path, &data).expect("should write version-modified file");

    let result = Keystore::load(&path, passphrase);
    match result {
        Err(KeystoreError::UnsupportedVersion(99)) => {}
        other => panic!("expected UnsupportedVersion(99), got {other:?}"),
    }

    let _ = fs::remove_file(&path);
}

#[test]
#[ignore]
fn test_missing_file() {
    let path = temp_path();

    let result = Keystore::load(&path, "irrelevant");
    match result {
        Err(KeystoreError::Io(_)) => {}
        other => panic!("expected Io error, got {other:?}"),
    }
}
