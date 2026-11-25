//! Opening, creating and stamping the database file.
//!
//! This module owns the open-or-create sequence and nothing else: the directory
//! and file hardening lives in [`super::perm`], the pre-migration snapshots in
//! [`super::backup`], where the file lives in [`super::paths`], and the
//! versioning that decides *when* a snapshot is due in [`super::migrate`].
//! Keeping the sequence apart is what lets `migrate` name a backup path without
//! importing this module (and this module stamp an initial version without
//! importing the migration chain's helpers back).

use std::fs;
use std::io;
use std::path::Path;

use tracing::{info, warn};

use super::migrate::initialize_schema_version;
use super::perm::{ensure_db_dir, make_owner_only};
use super::store::Store;

/// Open (or create) the database file. The file is created when missing or
/// empty (the corpse of an interrupted create); a freshly created file is
/// stamped with
/// [`INITIAL_SCHEMA_VERSION`](super::migrate::INITIAL_SCHEMA_VERSION) here, but
/// the *migration chain* is deliberately NOT applied — callers run
/// [`run_migrations`](super::migrate::run_migrations) right after, before any
/// table access. Hard-errors on a database it cannot open rather than recreating
/// a potentially recoverable file (a "trying to recreate" catch-all could
/// silently clobber it).
///
/// # Errors
///
/// Returns Err if the database path cannot be resolved.
pub fn open_db() -> io::Result<Store> {
    open_db_at(&super::paths::db_path()?)
}

/// Build the hard error for a store whose redb file format is newer than this
/// binary can read.
///
/// Recreating the file would destroy the data, so the guidance names the real
/// database path and what to do instead. The recoveries are not equally
/// available: a newer binary always is, while a `.bak-v*` snapshot is written
/// only before a real schema migration — so an install that has never migrated
/// has none beside the file. The snapshot is therefore offered conditionally
/// ("if one was taken"), which keeps the message from sending the reader after
/// a file that cannot be there.
fn upgrade_required_message(actual: u8, path: &Path) -> String {
    format!(
        "database file format version {actual} is newer than this binary supports; \
         upgrade choreographr before continuing, or restore a .bak-v* snapshot from \
         before that upgrade, if one was taken (kept beside {}); deleting the file \
         loses every session",
        path.display()
    )
}

/// [`open_db`] parameterized by an explicit database file path, so non-CLI
/// embedders (the GUI's embedded daemon, tests) can open state without the
/// environment-variable override dance (`CHOREOGRAPHR_DB_PATH`). Same
/// create/stamp/upgrade semantics as `open_db` — this is the function; the
/// pathless version just resolves the standard location first.
///
/// # Errors
///
/// Returns Err if the file cannot be created/opened (corruption,
/// permissions, lock contention) or the initial schema version cannot be
/// stamped. A foreign-owned parent directory is refused; any other failure to
/// create or tighten the parent is only warned about (the subsequent
/// create/open reports the real failure).
pub fn open_db_at(path: &Path) -> io::Result<Store> {
    info!(path = %path.display(), "opening database");
    if let Some(parent) = path.parent() {
        ensure_db_dir(parent)?;
    }
    // A 0-byte file is the corpse of an interrupted `Database::create`
    // (crash between file creation and the first write): it holds no
    // recoverable data, so recreate it rather than hard-erroring like a
    // potentially-valuable corrupt file. As with a brand-new file, the
    // initial schema version is stamped immediately so the database is
    // versioned from the moment it exists.
    if let Ok(metadata) = fs::metadata(path)
        && metadata.len() == 0
    {
        warn!("database file exists but is empty (interrupted create?); recreating");
        return create_and_stamp(path);
    }
    match redb::Database::open(path) {
        Ok(db) => {
            // Force the mode even for an existing file: a store created before
            // the file was hardened carries the umask default and would stay
            // world-readable without this.
            make_owner_only(path);
            Ok(Store::from_db(db))
        }
        // File does not exist: fresh install. Create the database file and
        // stamp the initial schema version so `run_migrations` sees a versioned
        // database and migrates it from the initial version up to
        // SCHEMA_VERSION. Without this stamp a fresh file would report
        // version 0 — which the runner (correctly, for *pre-existing*
        // unversioned files) refuses once the migration chain grows past 1.
        Err(redb::DatabaseError::Storage(redb::StorageError::Io(io_err)))
            if io_err.kind() == io::ErrorKind::NotFound =>
        {
            info!("database file not found, creating new database");
            create_and_stamp(path)
        }
        // redb file-format bump: the file is a valid redb database in a newer
        // format than this binary can read — a newer choreographr wrote it and
        // this one is the downgrade. Recreating would destroy the data, so this
        // is a hard error whose recoveries are a newer binary, or a pre-upgrade
        // `.bak-v*` snapshot when one exists. A snapshot is written only before
        // a real schema migration, so an install that has never migrated has
        // none — hence the conditional wording, which must not send the reader
        // after a file that cannot be there.
        Err(redb::DatabaseError::UpgradeRequired(actual)) => {
            Err(io::Error::other(upgrade_required_message(actual, path)))
        }
        // Any other open failure (corruption, permissions, lock contention…)
        // is also a hard error: a "trying to recreate" catch-all could silently
        // clobber a potentially-recoverable file.
        Err(e) => Err(io::Error::other(format!(
            "failed to open database (refusing to recreate a potentially corrupt file): {e}"
        ))),
    }
}

/// Create the database file at `path`, harden it, and stamp the initial schema
/// version. Shared by the fresh-install and interrupted-create paths, which
/// differ only in why the file is not usable as it stands.
fn create_and_stamp(path: &Path) -> io::Result<Store> {
    let db = redb::Database::create(path)
        .map_err(|e| io::Error::other(format!("failed to create database: {e}")))?;
    make_owner_only(path);
    let store = Store::from_db(db);
    initialize_schema_version(&store)?;
    Ok(store)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::backup::backup_database;
    use crate::db::migrate::{MIGRATIONS, SCHEMA_VERSION, run_migrations_to, stamp_schema_version};

    #[test]
    #[cfg(unix)]
    fn open_db_at_creates_private_dir_and_file() {
        use std::os::unix::fs::PermissionsExt;
        // The store holds plaintext transcripts and tool output, so both the
        // immediate parent (0700) and the file (0600) must be owner-only from
        // the very first open — a 0600 file inside a world-searchable
        // directory is still reachable by every local user.
        let root = tempfile::tempdir().unwrap();
        let data_dir = root.path().join("choreographr");
        let db_path = data_dir.join("state.redb");

        let db = open_db_at(&db_path).unwrap();
        drop(db);

        let dir_mode = fs::metadata(&data_dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700, "the data directory must be owner-only");
        let file_mode = fs::metadata(&db_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(file_mode, 0o600, "the database file must be owner-only");
    }

    #[test]
    #[cfg(unix)]
    fn open_db_at_tightens_a_world_readable_existing_file() {
        use std::os::unix::fs::PermissionsExt;
        // An install created before the file was hardened carries the umask
        // default (typically 0644); reopening must tighten it rather than
        // trusting the mode the file arrived with.
        let root = tempfile::tempdir().unwrap();
        let data_dir = root.path().join("choreographr");
        let db_path = data_dir.join("state.redb");

        let db = open_db_at(&db_path).unwrap();
        drop(db);
        fs::set_permissions(&db_path, fs::Permissions::from_mode(0o644)).unwrap();

        let db = open_db_at(&db_path).unwrap();
        drop(db);

        let file_mode = fs::metadata(&db_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(file_mode, 0o600, "an existing 0644 file must be tightened");
    }

    #[test]
    fn upgrade_required_message_offers_the_snapshot_only_conditionally() {
        // A `.bak-v*` snapshot is written only when a real schema migration is
        // about to run, so an install that has never migrated has none. The
        // refusal shown when this binary cannot read the store at all must
        // therefore offer the snapshot conditionally, while still naming the
        // real database path and the cost of deleting it.
        let msg = upgrade_required_message(7, Path::new("/data/state.redb"));
        assert!(
            msg.contains("version 7 is newer than this binary supports"),
            "{msg}"
        );
        assert!(
            msg.contains("upgrade choreographr before continuing"),
            "{msg}"
        );
        assert!(msg.contains("if one was taken"), "{msg}");
        assert!(msg.contains("/data/state.redb"), "{msg}");
        assert!(
            msg.contains("deleting the file loses every session"),
            "{msg}"
        );
    }

    #[test]
    fn production_orchestration_pre_backs_up_then_migrates() {
        // The full production sequence (open → read version → drop the handle
        // → backup_database → reopen → run migrations), exercised with an
        // injected path instead of open_db()'s real db_path(). The runner's
        // skip-if-exists check must leave the pre-created backup untouched
        // while the migration still succeeds — the contract the Windows-safe
        // startup path depends on.
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("state.redb");

        let opened = Store::from_db(redb::Database::create(&db_path).unwrap());
        stamp_schema_version(&opened, 1).unwrap();
        let version = crate::db::migration_backup_version(&opened)
            .unwrap()
            .expect("a pending v1 migration must be reported as needing a backup");
        drop(opened); // release redb's lock before the copy (the Windows fix)

        backup_database(&db_path, version).unwrap();
        let backup_path = db_path.with_file_name("state.redb.bak-v1");
        let backup_before = fs::read(&backup_path).unwrap();

        let reopened = Store::from_db(redb::Database::open(&db_path).unwrap());
        run_migrations_to(&reopened, SCHEMA_VERSION, MIGRATIONS, &db_path).unwrap();

        assert_eq!(
            crate::db::schema_version(&reopened).unwrap(),
            SCHEMA_VERSION
        );
        assert_eq!(
            fs::read(&backup_path).unwrap(),
            backup_before,
            "the pre-lock backup must survive the migration untouched"
        );
    }
}
