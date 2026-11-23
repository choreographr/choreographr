//! The `acl-add` subcommand: enroll one client's transport public key in the
//! daemon's `authorized_clients.toml`.
//!
//! The write goes through [`crate::server::acl::append_key_locked`], so the
//! advisory file lock serializes it against a concurrent daemon `/acl add`,
//! and the daemon's watcher + parse-compare reload makes the new entry live
//! without a restart.

use tracing::info;

/// Enroll `pubkey_b64` into the ACL file at `path`. The testable core of the
/// `acl-add` subcommand: the CLI resolves the path from the standard config
/// dir and delegates here.
///
/// # Errors
///
/// Returns an error when `pubkey_b64` is not base64 that decodes to exactly 32
/// bytes, or when the ACL file cannot be read or written.
pub(super) fn acl_add_to(path: &std::path::Path, pubkey_b64: &str) -> anyhow::Result<usize> {
    use base64::Engine as _;
    let key: [u8; 32] = base64::engine::general_purpose::STANDARD
        .decode(pubkey_b64.trim())
        .map_err(|e| anyhow::anyhow!("invalid pubkey: not valid base64: {e}"))?
        .try_into()
        .map_err(|_| anyhow::anyhow!("invalid pubkey: must decode to exactly 32 bytes"))?;

    // Idempotency: an already-present key is a no-op (no duplicate entry).
    let existing = crate::server::acl::Acl::load(path);
    if existing.contains(&key) {
        info!("pubkey is already authorized; nothing to do");
        return Ok(existing.len());
    }

    // The advisory file lock makes this safe against a concurrent daemon
    // /acl add; the daemon's watcher + parse-compare reload makes the new
    // entry live without a restart.
    crate::server::acl::append_key_locked(path, &key).map_err(|e| anyhow::anyhow!(e))?;
    let count = crate::server::acl::Acl::load(path).len();
    info!(clients = count, "ACL: client key enrolled");
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLI_KEY_A: [u8; 32] = [1u8; 32];
    const CLI_KEY_B: [u8; 32] = [2u8; 32];

    fn cli_b64(key: &[u8; 32]) -> String {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD.encode(key)
    }

    /// The CLI's idempotent enroll: a fresh file gains the entry; a second
    /// identical add is a no-op returning the SAME count; a different key
    /// appends alongside. These pin the behavior an operator relies on when
    /// scripting `choreographr acl-add` against a live daemon.
    #[test]
    fn acl_add_to_appends_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("authorized_clients.toml");

        // First add: file is created (with its parent dir), count = 1.
        let count = acl_add_to(&path, &cli_b64(&CLI_KEY_A)).unwrap();
        assert_eq!(count, 1);
        assert!(
            crate::server::acl::Acl::load(&path).contains(&CLI_KEY_A),
            "the enrolled key must authorize"
        );

        // Idempotent re-add: no duplicate entry.
        assert_eq!(acl_add_to(&path, &cli_b64(&CLI_KEY_A)).unwrap(), 1);
        let file = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            file.matches("pubkey").count(),
            1,
            "re-adding must not duplicate the entry"
        );

        // A second key appends alongside.
        assert_eq!(acl_add_to(&path, &cli_b64(&CLI_KEY_B)).unwrap(), 2);
        assert!(crate::server::acl::Acl::load(&path).contains(&CLI_KEY_B));
    }

    #[test]
    fn acl_add_to_rejects_bad_keys() {
        use base64::Engine as _;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("authorized_clients.toml");
        assert!(acl_add_to(&path, "not-base64!!!").is_err());
        // Valid base64, wrong decoded length.
        let short = base64::engine::general_purpose::STANDARD.encode([9u8; 16]);
        assert!(acl_add_to(&path, &short).is_err());
        // Nothing was written for the rejected keys.
        assert!(!path.exists(), "a rejected add must not create the file");
    }
}
