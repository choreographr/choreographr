//! Opening, hardening and backing up the database file.
//!
//! This module owns the file-level half of the store: where the database lives
//! (the `CHOREOGRAPHR_DB_PATH` override, otherwise the platform data
//! directory), how it is created or opened, the owner-only permission policy
//! for the data directory and the file, and the pre-migration snapshot that
//! makes a failed migration rollback-able. The versioning that decides *when* a
//! snapshot is due lives in [`super::migrate`].

use std::fs;
use std::io;
use std::path::PathBuf;

use tracing::{debug, info, warn};

use super::migrate::{MIGRATIONS, SCHEMA_VERSION, initialize_schema_version, schema_version};

/// Resolve the database file path: the `CHOREOGRAPHR_DB_PATH` override when
/// set, otherwise the standard data directory (platform `dirs::data_dir()` by
/// default, or `{base}/data` under `--base-dir`).
///
/// # Errors
///
/// Returns Err if the data directory cannot be determined.
pub fn db_path() -> io::Result<PathBuf> {
    if let Ok(override_path) = std::env::var("CHOREOGRAPHR_DB_PATH") {
        return Ok(PathBuf::from(override_path));
    }
    choreo_shared::paths::data_file("state.redb")
}

/// Force a database file — the store itself or one of its pre-migration
/// backups — to owner-only `0600`.
///
/// The store holds plaintext conversation history, tool output (which routinely
/// carries secrets), and attachments, so the file must not be readable by other
/// local users. `redb::Database::create` honours the process umask, so a typical
/// desktop create lands `0644`, and a file left from an earlier install keeps
/// whatever mode it already had: the mode is therefore forced on every open, not
/// merely trusted from creation.
///
/// Best-effort: an unusual filesystem that cannot express POSIX modes logs a
/// warning and the caller continues — refusing to start the daemon over a
/// permission quirk is worse than a visible log line, and the containing
/// directory's `0700` mode is the remaining control.
fn make_owner_only(path: &std::path::Path) {
    // Best-effort: an unusual filesystem that cannot express POSIX modes logs a
    // warning and the caller continues — refusing to start the daemon over a
    // permission quirk is worse than a visible log line, and the containing
    // directory's 0700 mode is the remaining control. The mode is applied via
    // the shared `choreo_shared::perm` policy so the store, its backups, and the
    // other secrets the suite keeps agree on 0600. `redb` owns the file's fd, so
    // this is the path form.
    if let Err(e) = choreo_shared::perm::set_owner_only_path(path) {
        warn!(
            path = %path.display(),
            error = %e,
            "could not set the database file to mode 0600; it may stay readable by other \
             local users"
        );
    }
}

/// Directories this instance must never narrow, because other programs and
/// users legitimately live in them and a `0700` there would lock them all out.
///
/// Only the DB's *immediate* parent is ours to tighten; the platform temp dir
/// and the data *root* (`~/.local/share`, the parent of the standard
/// `choreographr` data dir) are shared by construction. A `CHOREOGRAPHR_DB_PATH`
/// pointing straight at one of them leaves the mode alone — the file's own
/// `0600` is the control there.
fn is_shared_root(dir: &std::path::Path) -> bool {
    if dir == std::env::temp_dir() {
        return true;
    }
    choreo_shared::paths::data_dir()
        .ok()
        .and_then(|data| data.parent().map(std::path::Path::to_path_buf))
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
fn ensure_db_dir(dir: &std::path::Path) -> io::Result<()> {
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

/// Compute the backup path [`backup_db_file`] and [`backup_database`] share:
/// `path.bak-v{from}`, named after the version being migrated FROM. Extracted
/// so the naming cannot drift between the pre-lock copy (CLI startup) and the
/// in-runner copy (fallback).
fn backup_path_for(path: &std::path::Path, from: u64) -> std::path::PathBuf {
    let file_name = path.file_name().map_or_else(
        || "state.redb".to_string(),
        |name| name.to_string_lossy().into_owned(),
    );
    path.with_file_name(format!("{file_name}.bak-v{from}"))
}

/// Snapshot the database file at `path` to `{file_name}.bak-v{from_version}`
/// (the same name `backup_db_file` uses), where `from_version` is the schema
/// version being migrated away from.
///
/// # Why this exists as a separate pre-lock entry point
///
/// redb takes a whole-file exclusive lock (`fs4`/`LockFileEx`) on the database
/// file while a `redb::Database` handle is open. On Windows a byte-range
/// exclusive lock blocks `ReadFile` from ANY handle — including another handle
/// in the SAME process — so `fs::copy` of the open file fails with os error 33
/// ("The process cannot access the file because another process has locked a
/// portion of the file"). Linux POSIX locks are advisory, so the copy only
/// ever failed on Windows (first observed in windows-latest CI). The copy
/// therefore has to happen BEFORE redb opens/locks the file: the caller must
/// read [`schema_version`], DROP the `Database` handle, call this, and only
/// then reopen and run the migrations. Never call this while holding an open
/// `redb::Database` on `path` — that is precisely the bug this function exists
/// to avoid.
///
/// Note that copying an unopened file is still safe and consistent: redb only
/// extends/rewrites the file on committed transactions, and this runs at
/// startup, single-threaded, before any migration writes — the on-disk image
/// reflects the last committed transaction.
///
/// # Errors
///
/// Returns Err if the file copy fails (I/O error on source or destination).
pub fn backup_database(path: &std::path::Path, from_version: u64) -> io::Result<()> {
    let backup_path = backup_path_for(path, from_version);
    fs::copy(path, &backup_path)?;
    // The backup is a byte-for-byte copy of the store — same transcripts, same
    // tool output — so it gets the same owner-only mode as the store itself.
    make_owner_only(&backup_path);
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
/// The production startup path (cli.rs) now takes the backup BEFORE redb
/// opens/locks the file, because on Windows redb's whole-file exclusive lock
/// makes reading the open file fail outright (os error 33 — see
/// [`backup_database`]). By the time `run_migrations_to` runs, the backup
/// already exists; overwriting it here would read the locked file and fail on
/// Windows. So if the backup is already present, it is treated as the
/// authoritative pre-migration snapshot and left untouched. Direct callers of
/// `run_migrations_to` (the unit tests) that did NOT pre-copy still get the
/// `fs::copy` fallback, so the copy path stays exercised.
pub(super) fn backup_db_file(path: &std::path::Path, from: u64) -> io::Result<()> {
    let backup_path = backup_path_for(path, from);
    if backup_path.exists() {
        info!(
            path = %backup_path.display(),
            "pre-migration backup already exists (created before the database lock was taken); \
             skipping the copy and leaving it untouched"
        );
        return Ok(());
    }
    fs::copy(path, &backup_path)?;
    // A byte-for-byte copy of the store — same transcripts, same tool output —
    // so it gets the same owner-only mode as the store itself.
    make_owner_only(&backup_path);
    info!(
        from = %path.display(),
        to = %backup_path.display(),
        "backed up database before applying migrations"
    );
    Ok(())
}

/// Decide whether the pending migration would create a pre-migration backup,
/// and if so return the schema version the backup must be named after (the
/// source version). Returns `Ok(Some(version))` in EXACTLY the situations
/// where [`run_migrations_to`](super::migrate::run_migrations_to) reaches `backup_db_file` for the production
/// chain (`MIGRATIONS`, target [`SCHEMA_VERSION`]), and `Ok(None)` otherwise:
///
/// - version 0: the 0 → 1 (or 0 → stamp) path is pure initialization — no
///   migration writes, no backup;
/// - version == [`SCHEMA_VERSION`]: the idempotent fast path, nothing runs;
/// - version > [`SCHEMA_VERSION`]: `run_migrations_to` refuses the database
///   BEFORE any write, so no backup is taken;
/// - the migration chain is empty or not contiguous over `1..SCHEMA_VERSION`:
///   `run_migrations_to` hard-errors before the backup in that case.
///
/// This is the single source of truth shared by the CLI's pre-lock backup
/// decision and (via the skip-if-exists check in `backup_db_file`) the
/// runner's own backup step, so the two can never disagree about whether a
/// backup exists before the migration runs.
pub(crate) fn migration_backup_version(db: &redb::Database) -> io::Result<Option<u64>> {
    let current = schema_version(db)?;
    debug!(current, "checking whether a pre-migration backup is needed");
    if current == 0 || current >= SCHEMA_VERSION || MIGRATIONS.is_empty() {
        return Ok(None);
    }
    // Mirror run_migrations_to's chain validation: a broken/empty chain makes
    // the runner error out BEFORE the backup, so no pre-copy must happen
    // either (the gates above already cover the empty case; this covers
    // non-contiguity).
    let expected: Vec<u64> = (1..SCHEMA_VERSION).collect();
    let provided: Vec<u64> = MIGRATIONS.iter().map(|m| m.from).collect();
    if provided != expected {
        return Ok(None);
    }
    Ok(Some(current))
}

/// Open (or create) the database file. The file is created when missing or
/// empty (the corpse of an interrupted create); a freshly created file is
/// stamped with
/// [`INITIAL_SCHEMA_VERSION`](super::migrate::INITIAL_SCHEMA_VERSION) here, but
/// the *migration chain* is deliberately NOT applied — callers run
/// [`run_migrations`](super::migrate::run_migrations) right after, before any
/// table access (see `main.rs`). Hard-errors on a database it cannot open
/// rather than recreating a potentially recoverable file (the old "trying to
/// recreate" catch-all could silently clobber it).
///
/// # Errors
///
/// Returns Err if the database path cannot be resolved.
pub fn open_db() -> io::Result<redb::Database> {
    open_db_at(&db_path()?)
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
pub fn open_db_at(path: &std::path::Path) -> io::Result<redb::Database> {
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
        let db = redb::Database::create(path)
            .map_err(|e| io::Error::other(format!("failed to create database: {e}")))?;
        make_owner_only(path);
        initialize_schema_version(&db)?;
        return Ok(db);
    }
    match redb::Database::open(path) {
        Ok(db) => {
            // Force the mode even for an existing file: a store created before
            // the file was hardened carries the umask default and would stay
            // world-readable without this.
            make_owner_only(path);
            Ok(db)
        }
        // File does not exist: fresh install. Create the database file and
        // stamp the initial schema version so `run_migrations` (called by
        // the daemon right after `open_db`) sees a versioned database and
        // migrates it from the initial version up to SCHEMA_VERSION. Without
        // this stamp a fresh file would report version 0 — which the runner
        // (correctly, for *pre-existing* unversioned files) refuses once the
        // migration chain grows past 1.
        Err(redb::DatabaseError::Storage(redb::StorageError::Io(io_err)))
            if io_err.kind() == io::ErrorKind::NotFound =>
        {
            info!("database file not found, creating new database");
            let db = redb::Database::create(path)
                .map_err(|e| io::Error::other(format!("failed to create database: {e}")))?;
            make_owner_only(path);
            initialize_schema_version(&db)?;
            Ok(db)
        }
        // redb file-format bump: the file is a valid redb database but in a
        // newer file format than this binary can read — the file was written by
        // a newer choreographr and this one is the downgrade. Hard error with
        // recovery guidance that names only paths that actually exist:
        // recreating would destroy the data, and the two real options are a
        // newer binary or a pre-upgrade snapshot.
        Err(redb::DatabaseError::UpgradeRequired(actual)) => Err(io::Error::other(format!(
            "database file format version {actual} is newer than this binary supports; \
             upgrade choreographr before continuing, or restore a .bak-v* snapshot from \
             before that upgrade, kept beside {} (deleting the file loses every session)",
            path.display()
        ))),
        // Any other open failure (corruption, permissions, lock contention…)
        // is also a hard error: the old "trying to recreate" catch-all could
        // silently clobber a potentially-recoverable file.
        Err(e) => Err(io::Error::other(format!(
            "failed to open database (refusing to recreate a potentially corrupt file): {e}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::migrate::{current_schema_version, run_migrations_to, stamp_schema_version};

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
    fn backup_db_file_names_backup_after_source_version() {
        // The pre-migration snapshot must be named after the version being
        // migrated FROM (`bak-v1` for a v1 database), so restoring it rolls
        // back to exactly the pre-migration state — never after the target
        // (a target-named `bak-v2` for a 1 → 2 migration would be ambiguous:
        // is it the pre-migration v1 file or a post-migration v2 file?).
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("state.redb");
        fs::write(&db_path, b"database contents").unwrap();

        backup_db_file(&db_path, 1).unwrap();
        assert!(db_path.with_file_name("state.redb.bak-v1").exists());

        // A different source version produces a differently named backup —
        // both can coexist without colliding.
        backup_db_file(&db_path, 2).unwrap();
        assert!(db_path.with_file_name("state.redb.bak-v2").exists());
    }

    #[test]
    #[cfg(unix)]
    fn backup_files_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        // A backup is a byte-for-byte copy of the store, so it holds the same
        // transcripts and tool output and must be no more readable than the
        // store itself — `fs::copy` alone would carry the source's 0644 mode.
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
    fn backup_database_produces_versioned_name_and_identical_content() {
        // The public pre-lock backup entry point must use the SAME naming
        // scheme as the in-runner backup (bak-v{source}) and copy the file
        // byte-for-byte — the CLI's pre-copy is only equivalent to the old
        // in-runner copy if the artifacts are indistinguishable.
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("state.redb");
        fs::write(&db_path, b"database bytes v1").unwrap();

        backup_database(&db_path, 1).unwrap();
        let backup_path = db_path.with_file_name("state.redb.bak-v1");
        assert!(backup_path.exists());
        assert_eq!(fs::read(&backup_path).unwrap(), b"database bytes v1");
    }

    #[test]
    fn migration_backup_version_matches_run_migrations_to_backup_conditions() {
        // The version-gating helper must return Some exactly when
        // run_migrations_to would take a backup for the production chain:
        // Some(1) for a pending real migration from v1; None for version 0
        // (pure initialization stamp, no backup), for version ==
        // SCHEMA_VERSION (fast path), and for a newer-than-supported version
        // (the runner refuses before any write). Keeping both sides pinned to
        // one helper is what prevents the CLI's pre-lock backup decision from
        // drifting away from the runner's own backup step.
        let dir = tempfile::tempdir().unwrap();

        let make_db_at = |version: u64| {
            let db =
                redb::Database::create(dir.path().join(format!("test-{version}.redb"))).unwrap();
            if version > 0 {
                stamp_schema_version(&db, version).unwrap();
            }
            db
        };

        assert_eq!(migration_backup_version(&make_db_at(1)).unwrap(), Some(1));
        assert_eq!(migration_backup_version(&make_db_at(0)).unwrap(), None);
        assert_eq!(
            migration_backup_version(&make_db_at(SCHEMA_VERSION)).unwrap(),
            None
        );
        assert_eq!(
            migration_backup_version(&make_db_at(SCHEMA_VERSION + 1)).unwrap(),
            None
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

        let opened = redb::Database::create(&db_path).unwrap();
        stamp_schema_version(&opened, 1).unwrap();
        let version = migration_backup_version(&opened)
            .unwrap()
            .expect("a pending v1 migration must be reported as needing a backup");
        drop(opened); // release redb's lock before the copy (the Windows fix)

        backup_database(&db_path, version).unwrap();
        let backup_path = db_path.with_file_name("state.redb.bak-v1");
        let backup_before = fs::read(&backup_path).unwrap();

        let reopened = redb::Database::open(&db_path).unwrap();
        run_migrations_to(&reopened, SCHEMA_VERSION, MIGRATIONS, &db_path).unwrap();

        assert_eq!(current_schema_version(&reopened).unwrap(), SCHEMA_VERSION);
        assert_eq!(
            fs::read(&backup_path).unwrap(),
            backup_before,
            "the pre-lock backup must survive the migration untouched"
        );
    }
}
