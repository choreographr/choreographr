//! Pre-migration snapshots of the store.
//!
//! A schema migration rewrites rows, so it runs only after the file has been
//! copied aside: `path` → `path.bak-v{from}`, named after the version being
//! migrated FROM (a target-named `bak-v2` for a 1 → 2 migration would be
//! ambiguous — is it the pre-migration v1 file or a post-migration v2 file?).
//! A failed migration can then always be rolled back from disk.
//!
//! ## Why the copy has to happen before the file is opened
//!
//! `redb` takes a whole-file exclusive lock (`fs4`/`LockFileEx`) on the database
//! while a `redb::Database` handle is open, and on Windows a byte-range
//! exclusive lock blocks `ReadFile` from ANY handle — including another handle
//! in the SAME process — so copying the open file fails with os error 33 ("The
//! process cannot access the file because another process has locked a portion
//! of the file"). Linux POSIX locks are advisory, so the copy only ever failed
//! on Windows (first observed in windows-latest CI). The production startup
//! sequence therefore reads the schema version, DROPS the handle, takes this
//! copy, and only then reopens and migrates — [`backup_database`] is that
//! pre-lock entry point, and [`backup_db_file`] is the runner's in-transaction
//! fallback for a caller that did not pre-copy (see its skip-if-exists rule).
//!
//! Copying a closed file is still consistent: `redb` only extends or rewrites
//! the file on committed transactions, and this runs at startup, single
//! threaded, before any migration writes — the on-disk image reflects the last
//! committed transaction.
//!
//! ## Permissions
//!
//! A backup is a byte-for-byte copy of the store, so it holds the same
//! transcripts and tool output. [`copy_private`] is the only way bytes land
//! here: the destination is created NEW at `0600` without following a symlink,
//! so a stale entry is refused rather than overwritten and the copy is never
//! briefly readable by another local user.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use tracing::info;

/// Compute the backup path both entry points share: `path.bak-v{from}`, named
/// after the version being migrated FROM. Extracted so the naming cannot drift
/// between the pre-lock copy (startup) and the in-runner copy (fallback).
fn backup_path_for(path: &Path, from: u64) -> PathBuf {
    let file_name = path.file_name().map_or_else(
        || "state.redb".to_string(),
        |name| name.to_string_lossy().into_owned(),
    );
    path.with_file_name(format!("{file_name}.bak-v{from}"))
}

/// Copy `src` to a brand-new owner-only `dst`.
///
/// `fs::copy` is the wrong primitive for a store snapshot on two counts: it
/// follows a symlink planted at the destination (writing the whole transcript
/// over the link's target, which is a data-destruction primitive for anyone who
/// can write in the directory — a `CHOREOGRAPHR_DB_PATH` under a shared root
/// makes that possible), and it reproduces the source's mode, leaving a
/// readable window before a later `chmod` can close it.
///
/// `create_new` is `O_CREAT | O_EXCL`, which refuses an entry that already
/// exists — a symlink included, which is why no separate `O_NOFOLLOW` is
/// needed — and `mode(0o600)` applies at creation, so there is no window.
///
/// The copy is fsynced before returning: it is the only rollback point for a
/// migration that is about to rewrite the store.
fn copy_private(src: &Path, dst: &Path) -> io::Result<()> {
    let mut reader = fs::File::open(src)?;
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut writer = options.open(dst)?;
    io::copy(&mut reader, &mut writer)?;
    writer.sync_all()
}

/// Snapshot the database file at `path` to `{file_name}.bak-v{from_version}`,
/// where `from_version` is the schema version being migrated away from.
///
/// This is the pre-lock entry point (see the module docs): the caller must read
/// the schema version, DROP the `Database` handle, call this, and only then
/// reopen and run the migrations. Never call this while holding an open
/// `redb::Database` on `path` — that is precisely the Windows failure this
/// function exists to avoid.
///
/// # Errors
///
/// Returns Err if the copy fails (an I/O error on the source, or an entry
/// already sitting at the backup path).
pub fn backup_database(path: &Path, from_version: u64) -> io::Result<()> {
    let backup_path = backup_path_for(path, from_version);
    copy_private(path, &backup_path)?;
    info!(
        from = %path.display(),
        to = %backup_path.display(),
        "backed up database before applying migrations (taken before the database lock)"
    );
    Ok(())
}

/// Snapshot the database file before a migration rewrites it:
/// `path` → `path.bak-v{from}` (naming rationale: see [`backup_path_for`]).
///
/// Fires only before a real migration writes (never for the pure 0 → 1
/// initialization stamp): one backup per source schema version, taken before
/// any write, so a failed migration can always be rolled back from disk. The
/// active 1 → 2 migration therefore snapshots a v1 file to `bak-v1`.
///
/// # Skip-if-exists
///
/// The production startup path takes the backup BEFORE `redb` opens/locks the
/// file (see the module docs), so by the time the runner reaches this, the
/// backup already exists; overwriting it here would read the locked file and
/// fail on Windows. An existing backup is therefore treated as the
/// authoritative pre-migration snapshot and left untouched. Direct callers of
/// the runner that did NOT pre-copy (the unit tests) still get the copy, so that
/// path stays exercised.
///
/// # Errors
///
/// Returns Err if the copy fails.
pub(super) fn backup_db_file(path: &Path, from: u64) -> io::Result<()> {
    let backup_path = backup_path_for(path, from);
    if backup_path.exists() {
        info!(
            path = %backup_path.display(),
            "pre-migration backup already exists (created before the database lock was taken); \
             skipping the copy and leaving it untouched"
        );
        return Ok(());
    }
    copy_private(path, &backup_path)?;
    info!(
        from = %path.display(),
        to = %backup_path.display(),
        "backed up database before applying migrations"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backup_db_file_names_backup_after_source_version() {
        // The pre-migration snapshot must be named after the version being
        // migrated FROM (`bak-v1` for a v1 database), so restoring it rolls
        // back to exactly the pre-migration state. A different source version
        // produces a differently named backup — both can coexist.
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("state.redb");
        fs::write(&db_path, b"database contents").unwrap();

        backup_db_file(&db_path, 1).unwrap();
        assert!(db_path.with_file_name("state.redb.bak-v1").exists());

        backup_db_file(&db_path, 2).unwrap();
        assert!(db_path.with_file_name("state.redb.bak-v2").exists());
    }

    #[test]
    fn backup_database_produces_versioned_name_and_identical_content() {
        // The public pre-lock entry point must use the SAME naming scheme as
        // the in-runner backup (bak-v{source}) and copy the file byte-for-byte
        // — the pre-copy is only equivalent to the in-runner copy if the
        // artifacts are indistinguishable.
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("state.redb");
        fs::write(&db_path, b"database bytes v1").unwrap();

        backup_database(&db_path, 1).unwrap();
        let backup_path = db_path.with_file_name("state.redb.bak-v1");
        assert!(backup_path.exists());
        assert_eq!(fs::read(&backup_path).unwrap(), b"database bytes v1");
    }

    #[test]
    #[cfg(unix)]
    fn backups_are_owner_only_from_creation() {
        use std::os::unix::fs::PermissionsExt as _;
        // A backup is a byte-for-byte copy of the store, so it holds the same
        // transcripts and tool output and must be no more readable than the
        // store itself. The mode comes from the open, not from a chmod after
        // the copy, so there is no readable window.
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("state.redb");
        fs::write(&db_path, b"database contents").unwrap();
        fs::set_permissions(&db_path, fs::Permissions::from_mode(0o644)).unwrap();

        backup_db_file(&db_path, 1).unwrap();
        backup_database(&db_path, 2).unwrap();

        for backup in ["state.redb.bak-v1", "state.redb.bak-v2"] {
            let mode = fs::metadata(db_path.with_file_name(backup))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600, "{backup} must be owner-only");
        }
    }

    #[test]
    #[cfg(unix)]
    fn a_symlink_at_the_backup_path_is_refused_not_followed() {
        // Anyone who can write in the store's directory could otherwise plant
        // `state.redb.bak-v1` as a symlink and have the whole transcript
        // written over the link's target.
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("state.redb");
        fs::write(&db_path, b"database contents").unwrap();
        let victim = dir.path().join("victim");
        fs::write(&victim, b"untouched").unwrap();
        std::os::unix::fs::symlink(&victim, dir.path().join("state.redb.bak-v1")).unwrap();

        // `exists()` follows the link, so the skip-if-exists rule reports it as
        // an existing snapshot and touches nothing.
        backup_db_file(&db_path, 1).unwrap();
        assert_eq!(fs::read(&victim).unwrap(), b"untouched");

        // A dangling link is not "existing", so the copy is attempted — and
        // must refuse rather than create the target through the link.
        std::os::unix::fs::symlink(
            dir.path().join("missing"),
            dir.path().join("state.redb.bak-v2"),
        )
        .unwrap();
        assert!(
            backup_db_file(&db_path, 2).is_err(),
            "a dangling symlink at the backup path must be refused, not followed"
        );
        assert!(
            !dir.path().join("missing").exists(),
            "the copy must not have created the link's target"
        );
    }
}
