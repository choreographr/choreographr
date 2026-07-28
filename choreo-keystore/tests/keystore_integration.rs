/// Integration tests for `choreo-keystore` that perform filesystem I/O.
///
/// These tests create real directories and files, so they are marked
/// `#[ignore]` — they only run under `cargo test -- --ignored`.

#[test]
#[ignore]
fn ensure_keypair_creates_files_in_override_dir() {
    let dir = std::env::temp_dir().join("choreo-keystore-test-ensure-keypair");
    let _ = std::fs::remove_dir_all(&dir);

    // Use the drop-guard so the override is reset even on panic.
    let _guard = choreo_keystore::paths::TestConfigGuard::set_root(Some(dir.clone()));

    // First call should create both files
    let result = choreo_keystore::ensure_keypair();
    assert!(result.is_ok(), "ensure_keypair should succeed");

    let pk = dir.join("choreographr").join("identity.pk");
    let pubk = dir.join("choreographr").join("public.pk");
    assert!(pk.exists(), "private key file should exist");
    assert!(pubk.exists(), "public key file should exist");

    // Second call should be idempotent
    let result2 = choreo_keystore::ensure_keypair();
    assert!(
        result2.is_ok(),
        "ensure_keypair should succeed on second call"
    );

    // Clean up temp directory
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
#[ignore]
fn test_override_used_when_set_integration() {
    let temp = std::env::temp_dir().join("choreo-keystore-test-override-int");
    let _guard = choreo_keystore::paths::TestConfigGuard::set_root(Some(temp.clone()));
    let cfg = choreo_keystore::paths::config_dir().unwrap();
    assert!(cfg.starts_with(&temp));
    assert!(cfg.ends_with("choreographr"));
}
