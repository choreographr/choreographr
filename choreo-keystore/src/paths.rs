use crate::error::KeystoreError;
use std::path::PathBuf;
use tracing::debug;

#[doc(hidden)]
thread_local! {
    /// Test-only override for config_dir. When set, config_dir() returns
    /// a choreographr subdirectory inside this path instead of the real
    /// user config directory.
    static TEST_CONFIG_ROOT: std::cell::RefCell<Option<PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

/// Set the config root directory for testing.
/// When `root` is `Some(path)`, `config_dir()` returns `path/choreographr`.
#[doc(hidden)]
pub fn set_test_config_root(root: Option<PathBuf>) {
    TEST_CONFIG_ROOT.with(|cell| cell.replace(root));
}

/// Guard that resets the test config root on drop, even if the test panics.
#[doc(hidden)]
pub struct TestConfigGuard;

#[doc(hidden)]
impl TestConfigGuard {
    /// Set a new test config root, returning a guard that resets it to `None`.
    pub fn set_root(root: Option<PathBuf>) -> Self {
        set_test_config_root(root);
        TestConfigGuard
    }
}

#[doc(hidden)]
impl Drop for TestConfigGuard {
    fn drop(&mut self) {
        set_test_config_root(None);
    }
}

/// Returns the config directory (`{config}/choreographr`).
///
/// Under test, this can be overridden with [`set_test_config_root`].
pub fn config_dir() -> Result<PathBuf, KeystoreError> {
    if let Some(root) = TEST_CONFIG_ROOT.with(|cell| cell.borrow().clone()) {
        let path = root.join("choreographr");
        debug!(?path, "config_dir (test override)");
        return Ok(path);
    }
    let config_dir = dirs::config_dir().ok_or(KeystoreError::ConfigDirNotFound)?;
    let path = config_dir.join("choreographr");
    debug!(?path, "resolved config directory");
    Ok(path)
}

pub fn private_key_path() -> Result<PathBuf, KeystoreError> {
    Ok(config_dir()?.join("identity.pk"))
}

pub fn private_key_enc_path() -> Result<PathBuf, KeystoreError> {
    Ok(config_dir()?.join("identity.pk.enc"))
}

pub fn public_key_path() -> Result<PathBuf, KeystoreError> {
    Ok(config_dir()?.join("public.pk"))
}

/// Path to the authorized clients ACL file (~/.config/choreographr/authorized_clients.toml)
pub fn authorized_clients_path() -> Result<PathBuf, KeystoreError> {
    Ok(config_dir()?.join("authorized_clients.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_dir_returns_choreographr_subdir() {
        let path = config_dir().unwrap();
        assert!(path.ends_with("choreographr"));
    }

    #[test]
    fn private_key_path_ends_with_identity_pk() {
        let path = private_key_path().unwrap();
        assert!(path.ends_with("identity.pk"));
        assert!(path.to_string_lossy().contains("choreographr"));
    }

    #[test]
    fn private_key_enc_path_ends_with_identity_pk_enc() {
        let path = private_key_enc_path().unwrap();
        assert!(path.ends_with("identity.pk.enc"));
    }

    #[test]
    fn public_key_path_ends_with_public_pk() {
        let path = public_key_path().unwrap();
        assert!(path.ends_with("public.pk"));
    }

    #[test]
    fn all_paths_share_same_config_dir() {
        let pk = private_key_path().unwrap();
        let enc = private_key_enc_path().unwrap();
        let pubk = public_key_path().unwrap();
        let cfg = config_dir().unwrap();

        assert!(pk.starts_with(&cfg));
        assert!(enc.starts_with(&cfg));
        assert!(pubk.starts_with(&cfg));
    }

    #[test]
    fn test_override_used_when_set() {
        let temp = std::env::temp_dir().join("choreo-keystore-test-override");
        // Use the drop-guard so the override is reset even on panic.
        let _guard = TestConfigGuard::set_root(Some(temp.clone()));
        let cfg = config_dir().unwrap();
        assert!(cfg.starts_with(&temp));
        assert!(cfg.ends_with("choreographr"));
    }
}
