/// Integration tests for `choreo-keystore` that perform filesystem I/O.
///
/// These tests create real directories and files, so they are marked
/// `#[ignore]` — they only run under `cargo test -- --ignored`.

#[test]
#[ignore]
fn test_override_used_when_set_integration() {
    let temp = std::env::temp_dir().join("choreo-keystore-test-override-int");
    let _guard = choreo_keystore::paths::TestConfigGuard::set_root(Some(temp.clone()));
    let cfg = choreo_keystore::paths::config_dir().unwrap();
    assert!(cfg.starts_with(&temp));
    assert!(cfg.ends_with("choreographr"));
}
