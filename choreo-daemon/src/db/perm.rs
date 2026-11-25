//! The store's filesystem access control.
//!
//! The `redb` file holds plaintext conversation history, tool output (which
//! routinely carries secrets), and attachments, so its permissions are part of
//! the security model rather than mere storage: the data directory is kept
//! owner-only, and every file the store owns — the database itself and each
//! pre-migration backup — is forced to `0600`.
//!
//! Both halves go through the shared [`choreo_shared::perm`] policy so the mode,
//! the ownership check, and the symlink refusal cannot drift between the store,
//! the Unix socket, and the other secrets the suite keeps. The descriptor forms
//! ([`choreo_shared::perm::verify_owned_regular`] /
//! [`choreo_shared::perm::set_owner_only`]) are what this module prefers: the
//! check and the `fchmod` then act on the exact inode that was opened, so the
//! path cannot be re-pointed between them.
//!
//! Everything here is Unix-only in effect; on Windows the directory is created
//! with the parent's ACLs and the file checks are no-ops, matching
//! `choreo_shared::perm`.

use std::fs;
use std::io;
use std::path::Path;

use tracing::warn;

/// Force the database file to owner-only `0600`.
///
/// `redb::Database::create` honours the process umask, so a typical desktop
/// create lands `0644`, and a file left from an earlier install keeps whatever
/// mode it already had: the mode is therefore forced on every open, not merely
/// trusted from creation.
///
/// The change is applied through a descriptor of our own — opened `O_NOFOLLOW`,
/// proved to be an owner-owned regular file, then `fchmod`-ed — because
/// `redb` owns the handle's descriptor and a path-form `chmod` would follow a
/// symlink planted at the store's path and could race a swap.
///
/// Best-effort: an unusual filesystem that cannot express POSIX modes logs a
/// warning and the caller continues — refusing to start the daemon over a
/// permission quirk is worse than a visible log line, and the containing
/// directory's `0700` mode is the remaining control.
pub(super) fn make_owner_only(path: &Path) {
    if let Err(e) = harden_file(path) {
        warn!(
            path = %path.display(),
            error = %e,
            "could not set the database file to mode 0600; it may stay readable by other \
             local users"
        );
    }
}

/// The fd-form harden behind [`make_owner_only`]: open `path` without following
/// a symlink, verify it is ours, and set `0600` on that descriptor.
///
/// Kept separate from the warn-and-continue wrapper so the failure is a real
/// error here (and therefore testable) while the open path stays tolerant.
#[cfg(unix)]
fn harden_file(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt as _;
    let file = fs::OpenOptions::new()
        .read(true)
        // O_NOFOLLOW: fail rather than follow a symlink at the store's path.
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits().cast_signed())
        .open(path)?;
    choreo_shared::perm::verify_owned_regular(&file, path)?;
    choreo_shared::perm::set_owner_only(&file)
}

/// Non-Unix: no POSIX mode to set, so nothing to do. Windows inherits the parent
/// directory's ACLs, which is the control there.
#[cfg(not(unix))]
fn harden_file(path: &Path) -> io::Result<()> {
    choreo_shared::perm::set_owner_only_path(path)
}

/// Directories this instance must never narrow, because other programs and
/// users legitimately live in them and a `0700` there would lock them all out.
///
/// Only the DB's *immediate* parent is ours to tighten; the platform temp dir
/// and the data *root* (`~/.local/share`, the parent of the standard
/// `choreographr` data dir) are shared by construction. A `CHOREOGRAPHR_DB_PATH`
/// pointing straight at one of them leaves the mode alone — the file's own
/// `0600` is the control there.
fn is_shared_root(dir: &Path) -> bool {
    if dir == std::env::temp_dir() {
        return true;
    }
    choreo_shared::paths::data_dir()
        .ok()
        .and_then(|data| data.parent().map(Path::to_path_buf))
        .is_some_and(|root| dir == root)
}

/// Create the database's parent directory and make it owner-only when that is
/// the right thing to do.
///
/// The data directory is part of the store's access control: a `0600` file
/// inside a world-searchable directory is still enumerable and openable by
/// every local user, so the immediate parent gets the shared
/// [`ensure_private_dir`](choreo_shared::perm::ensure_private_dir) treatment
/// (create, refuse a foreign owner, set `0700`). A shared ancestor is created
/// with a plain `create_dir_all` instead.
///
/// A **foreign-owned** immediate parent is refused outright: another user could
/// then enumerate or swap the store's files, so that is a real problem, not a
/// quirk to log and ignore. Any other failure to tighten only warns — startup
/// proceeds and the file mode is the remaining control.
pub(super) fn ensure_db_dir(dir: &Path) -> io::Result<()> {
    if !is_shared_root(dir) {
        return match choreo_shared::perm::ensure_private_dir(dir) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => Err(e),
            Err(e) => {
                warn!(
                    dir = %dir.display(),
                    error = %e,
                    "database directory could not be made owner-only; the database file's own \
                     0600 mode is the remaining control"
                );
                Ok(())
            }
        };
    }
    if let Err(e) = fs::create_dir_all(dir) {
        warn!(
            dir = %dir.display(),
            error = %e,
            "could not create the database directory"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(unix)]
    fn hardening_tightens_through_a_descriptor() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.redb");
        fs::write(&path, b"store").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();

        harden_file(&path).expect("an owner-owned regular file must harden");

        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "the descriptor form must force mode 0600");
    }

    #[test]
    #[cfg(unix)]
    fn hardening_refuses_a_symlink_instead_of_following_it() {
        // A symlink at the store's path must not be followed: the chmod would
        // land on the link's target (a file outside the store entirely).
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("victim");
        fs::write(&target, b"not the store").unwrap();
        let link = dir.path().join("state.redb");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        assert!(
            harden_file(&link).is_err(),
            "O_NOFOLLOW must refuse a symlink at the database path"
        );
    }

    #[test]
    fn the_platform_temp_dir_is_a_shared_root() {
        // The temp dir is shared by every local user, so the store must never
        // narrow it — only the file's own mode is the control there.
        assert!(is_shared_root(&std::env::temp_dir()));
    }
}
