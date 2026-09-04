//! Test-only helpers shared by other crates' unit/integration tests.
//!
//! Enabled by the opt-in `test-support` feature. The feature is FOR
//! TESTS ONLY — production consumers must never enable it, since it
//! drags in `tempfile` and exposes fixtures that make no sense at
//! runtime.

/// Isolate known_servers writes (bind pre-send recording and the Bound
/// confirmation record) in a temp config root. Returns the TempDir AND the
/// override guard — the guard must stay alive for the whole test or the
/// thread-local override resets and writes hit the real config dir.
pub fn isolate_config() -> (tempfile::TempDir, choreo_keystore::paths::TestConfigGuard) {
    let dir = tempfile::tempdir().unwrap();
    let guard = choreo_keystore::paths::TestConfigGuard::set_root(Some(dir.path().to_path_buf()));
    std::fs::create_dir_all(dir.path().join("choreographr")).unwrap();
    (dir, guard)
}
