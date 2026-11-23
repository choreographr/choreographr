//! Atomic file replacement: a sibling temp file, fsync, then rename.
//!
//! [`write_private_atomic`] is the one implementation of the workspace's
//! crash-safe write pattern. A reader of the target — a later daemon start, a
//! concurrent process under the same advisory lock, a config watcher — sees
//! either the whole old file or the whole new one, never a truncated or
//! half-written one. That property is what makes a partial write of a
//! **secret** survivable: an identity file written in place can be left a
//! prefix of itself by a crash, and a prefix of a key is not a key.

use std::io::{self, Write as _};
use std::path::Path;

/// Atomically replace `path`'s contents with `bytes`, owner-only
/// (`0600` on Unix).
///
/// The temp file is created in the target's own directory, so the final rename
/// never crosses a filesystem boundary (a cross-device rename is not atomic and
/// fails outright on most platforms). The bytes are flushed and fsynced before
/// the rename, so the rename publishes durable content — a crash can lose the
/// write, but cannot publish a torn one. The parent directory is created if
/// missing, and fsynced after the rename so the rename itself is durable.
///
/// Permissions come from the temp file, not the previous target: on Unix
/// `tempfile` creates it `0600` and [`crate::perm::set_owner_only`] makes that
/// explicit, so a file written through this helper is always owner-only
/// regardless of what the target's mode used to be. Callers that must preserve
/// an arbitrary pre-existing mode (a script being rewritten) are not served by
/// this helper — it is for private state, not for editing user files.
///
/// # Errors
///
/// Returns the underlying `io::Error` when the parent cannot be created, the
/// temp file cannot be written or synced, or the rename fails. A failure to
/// fsync the *directory* after a successful rename is not an error: it only
/// weakens crash durability of the rename on platforms/filesystems that refuse
/// to sync a directory handle, so it is logged and ignored.
pub fn write_private_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    // A parentless path (a bare file name) has an empty parent, which is not a
    // usable directory argument — fall back to "." for the temp file and skip
    // the directory fsync (there is no identifiable directory to sync).
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty());
    let dir = parent.unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir)?;

    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    // Explicit rather than assumed: the mode must not depend on the temp-file
    // library's default, and on non-Unix this is a no-op that inherits the
    // parent's ACLs.
    crate::perm::set_owner_only(tmp.as_file())?;
    tmp.write_all(bytes)?;
    tmp.as_file().sync_all()?;
    tmp.persist(path).map_err(|e| e.error)?;

    if let Some(dir) = parent
        && let Err(e) = std::fs::File::open(dir).and_then(|d| d.sync_all())
    {
        tracing::debug!(
            path = %dir.display(),
            error = %e,
            "failed to fsync the directory after an atomic rename",
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The write replaces the target's contents wholesale and leaves no temp
    /// file behind — the two things a caller (and a directory listing) can
    /// actually observe.
    #[test]
    fn replaces_contents_and_leaves_no_temp_behind() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state.txt");

        write_private_atomic(&path, b"first").expect("initial write");
        assert_eq!(std::fs::read(&path).expect("read"), b"first");

        write_private_atomic(&path, b"second, longer").expect("replace");
        assert_eq!(std::fs::read(&path).expect("read"), b"second, longer");

        // Only the target remains: the temp file was renamed, not copied.
        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .expect("read dir")
            .map(|e| e.expect("entry").file_name())
            .collect();
        assert_eq!(entries, vec![std::ffi::OsString::from("state.txt")]);
    }

    /// The file lands owner-only regardless of the mode the target had before,
    /// so a secret written through this helper is never left group-readable.
    #[cfg(unix)]
    #[test]
    fn result_is_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("secret");

        // Plant a deliberately loose target, then write over it.
        std::fs::write(&path, b"old").expect("plant");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("chmod");
        write_private_atomic(&path, b"new").expect("write");

        let mode = std::fs::metadata(&path)
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "the result must be owner-only");
    }

    /// A missing parent directory is created, so a caller does not have to
    /// create the config tree before its first write.
    #[test]
    fn creates_the_parent_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nested").join("deep").join("file");

        write_private_atomic(&path, b"value").expect("write into a missing parent");
        assert_eq!(std::fs::read(&path).expect("read"), b"value");
    }
}
