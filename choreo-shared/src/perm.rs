//! Owner-only filesystem hardening, shared by every path the suite keeps
//! secret.
//!
//! Several on-disk locations are part of the security model rather than mere
//! storage: the Unix socket is the daemon's fully-trusted local domain, the
//! `redb` store holds plaintext transcripts and tool output, `known_servers.toml`
//! holds the per-daemon private unlock keys, and the log files carry diagnostics.
//! A peer that can read or replace any of these is inside the trust boundary, so
//! each is kept owner-only. This module is the ONE implementation of that
//! policy — the directory form ([`ensure_private_dir`]) and the file forms
//! ([`verify_owned_regular`], [`set_owner_only`], [`set_owner_only_path`]) — so
//! the mode, the ownership check, and the symlink refusal cannot drift between
//! callers (the daemon's socket/data dir, `choreo-client-core`'s known-servers
//! store, and the shared log opener).
//!
//! Two primitive shapes are provided because they close different races:
//!
//! - **By descriptor** ([`verify_owned_regular`] / [`set_owner_only`]): the
//!   caller has already opened the file (ideally `O_NOFOLLOW`), so the check and
//!   the `fchmod` act on the exact inode that will be used — the path cannot be
//!   re-pointed at a different file between them.
//! - **By path** ([`set_owner_only_path`]): for a file whose descriptor we do
//!   not own (a library such as `redb` keeps it open). This is inherently
//!   racy against a swap, so it is only safe where the parent directory already
//!   bounds reachability (an owner-only directory).
//!
//! Everything is Unix-only in effect: the non-Unix implementations create the
//! directory (Windows inherits the parent's ACLs and its socket is a named pipe,
//! not a filesystem path) and treat the file checks as no-ops.

use std::io;
use std::path::Path;

/// The current effective uid — the identity the ownership checks and the
/// per-uid fallback socket directory name are keyed on.
///
/// `pub(crate)` because it is an implementation detail of this hardening (the
/// public surface is the operations, not the id); [`crate::paths::temp_socket_dir`]
/// reuses it for the fallback directory name.
#[cfg(unix)]
pub(crate) fn current_euid() -> u32 {
    rustix::process::geteuid().as_raw()
}

/// Non-Unix platforms have no uid; the platform temp/state dirs are already
/// per-user there, so a constant is enough.
#[cfg(not(unix))]
pub(crate) fn current_euid() -> u32 {
    0
}

/// Create `dir` (and any missing parents) as an owner-only (`0700`) directory.
///
/// The directory is part of the access control: a `0600` file inside a
/// world-searchable directory is still reachable by every local user, so the
/// parent must be owner-only too. This is used for the Unix socket's parent
/// (the trusted local domain), the `redb` data directory, and the client config
/// directory that holds the private unlock keys.
///
/// On Unix the resolved directory must belong to the current effective uid; a
/// directory owned by somebody else is refused with `PermissionDenied` rather
/// than narrowed, because this helper is only for directories the instance
/// itself owns. The `0700` mode is then applied **through an `O_NOFOLLOW`
/// directory descriptor** (`fchmod`) — not by path — so the ownership check and
/// the mode change act on the same inode and a swapped entry cannot divert the
/// chmod. A `chmod` that fails for a filesystem reason only warns (the caller's
/// file mode is the remaining control); a foreign owner or an I/O failure
/// propagates.
///
/// Non-Unix: creates the directory only.
///
/// # Errors
///
/// Propagates a create/open/metadata failure, and returns `PermissionDenied`
/// when `dir` exists but is owned by another user.
pub fn ensure_private_dir(dir: &Path) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        // Open the directory itself with O_NOFOLLOW|O_DIRECTORY: a symlink
        // planted at the path is refused (ELOOP) and a non-directory is
        // rejected (ENOTDIR), so the ownership check and the fchmod below act
        // on the exact inode that will be used, closing the
        // metadata()->chmod() swap race a path-based chmod would leave open.
        let fd = rustix::fs::open(
            dir,
            rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )?;
        let file = std::fs::File::from(fd);
        let owner = file.metadata()?.uid();
        let euid = current_euid();
        if owner != euid {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "refusing to use {}: it is owned by uid {owner}, not the current user \
                     (uid {euid})",
                    dir.display()
                ),
            ));
        }
        if let Err(e) = file.set_permissions(std::fs::Permissions::from_mode(0o700)) {
            tracing::warn!(
                dir = %dir.display(),
                error = %e,
                "could not set the directory to mode 0700; the file's own mode is the remaining \
                 control"
            );
        }
    }
    Ok(())
}

/// Verify that the already-open `file` is a regular file owned by the current
/// effective uid, refusing (`PermissionDenied`) anything else.
///
/// This checks the descriptor, not the path, so it cannot be fooled by a swap.
/// It does NOT prevent symlink-follow — that is the caller's job, by opening
/// with `O_NOFOLLOW`; a symlink that reached here (some other open mode) would
/// have been resolved to its target, whose ownership is checked instead.
///
/// Non-Unix: a no-op (no POSIX uid), matching [`set_owner_only`].
///
/// # Errors
///
/// Returns the metadata error, or `PermissionDenied` for a non-regular or
/// foreign-owned file.
pub fn verify_owned_regular(file: &std::fs::File, path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        let meta = file.metadata()?;
        if !meta.is_file() || meta.uid() != current_euid() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "refusing {}: not a regular file owned by the current user",
                    path.display()
                ),
            ));
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (file, path);
    }
    Ok(())
}

/// Tighten the open `file` to owner-only `0600` through its descriptor.
///
/// `fchmod` acts on the exact inode that was opened, so this is race-free
/// against a path swap. The failure is returned, not swallowed: a caller that
/// must not lose the write (the private-key store) propagates it, while a caller
/// that can degrade (a log file) may treat it as best-effort.
///
/// Non-Unix: a no-op (Windows inherits the parent's ACLs).
///
/// # Errors
///
/// Returns the underlying permission error.
pub fn set_owner_only(file: &std::fs::File) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(not(unix))]
    {
        let _ = file;
    }
    Ok(())
}

/// Tighten the file at `path` to owner-only `0600` by path.
///
/// For a file whose descriptor we do not own — a library such as `redb` keeps
/// the store open — so [`set_owner_only`] (the descriptor form) is out of reach.
/// A path-based chmod can race a swap, so this is only safe where the parent
/// directory already bounds reachability (an owner-only directory); prefer the
/// descriptor form everywhere an fd is available.
///
/// Non-Unix: a no-op.
///
/// # Errors
///
/// Returns the underlying permission error.
pub fn set_owner_only_path(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_dir(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "choreo-perm-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ))
    }

    #[test]
    fn ensure_private_dir_creates_missing_parents() {
        let root = unique_dir("parents");
        let nested = root.join("a").join("b");
        ensure_private_dir(&nested).expect("create the nested dir");
        assert!(nested.is_dir());
        // Idempotent: a second call over an existing directory succeeds.
        ensure_private_dir(&nested).expect("re-ensure an existing dir");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn ensure_private_dir_sets_mode_0700() {
        use std::os::unix::fs::PermissionsExt;
        let root = unique_dir("mode");
        ensure_private_dir(&root).expect("create the dir");
        let mode = std::fs::metadata(&root)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700, "the directory must be owner-only");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn ensure_private_dir_refuses_a_symlink() {
        // A symlink planted where the private directory should be is refused
        // (O_NOFOLLOW), not followed — so the owner-only mode is never applied
        // through it to the target.
        let root = unique_dir("symlink");
        std::fs::create_dir_all(&root).expect("mkdir root");
        let target = root.join("target");
        std::fs::create_dir_all(&target).expect("mkdir target");
        let link = root.join("link");
        std::os::unix::fs::symlink(&target, &link).expect("symlink");
        assert!(
            ensure_private_dir(&link).is_err(),
            "a symlink at the directory path must be refused"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn verify_owned_regular_accepts_a_regular_file_and_rejects_a_directory() {
        let root = unique_dir("verify");
        std::fs::create_dir_all(&root).expect("mkdir");
        let file_path = root.join("store");
        std::fs::write(&file_path, b"x").expect("write");

        let file = std::fs::File::open(&file_path).expect("open file");
        verify_owned_regular(&file, &file_path).expect("a regular file we own is accepted");

        // A directory opened as a File is not a regular file -> refused.
        let dir = std::fs::File::open(&root).expect("open dir");
        assert!(
            verify_owned_regular(&dir, &root).is_err(),
            "a directory is not a regular file"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn set_owner_only_tightens_to_0600() {
        use std::os::unix::fs::PermissionsExt;
        let root = unique_dir("chmod");
        std::fs::create_dir_all(&root).expect("mkdir");
        let path = root.join("store");
        std::fs::write(&path, b"x").expect("write");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
            .expect("chmod 0644");

        let file = std::fs::File::open(&path).expect("open");
        set_owner_only(&file).expect("fchmod");
        let mode = std::fs::metadata(&path)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);

        let _ = std::fs::remove_dir_all(&root);
    }
}
