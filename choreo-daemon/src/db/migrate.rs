//! Schema versioning: the persisted version, the migration chain, and the
//! runner that brings an opened database up to it.
//!
//! The 0 → 1 transition is initialization — the stamp [`open_db`](super::open_db)
//! writes when it creates the file — not a migration, so the chain's entries
//! start at 1 and a database still reporting 0 at startup is a pre-release
//! leftover. A migration is a function over the whole database so it can run in
//! exactly one `redb` write transaction, and the runner refuses a downgrade or
//! a non-contiguous chain before it writes anything.

use std::io;

use redb::ReadableDatabase;
use redb::ReadableTable;
use tracing::{debug, error, info, warn};

use super::backup::backup_db_file;
use super::codec::{ZSTD_FRAME_MAGIC, zstd_encode};
use super::paths::db_path;
use super::schema::{META, SCHEMA_VERSION_KEY, SESSION_TURNS};
use super::store::Store;

/// Persisted schema version. Bump on any *breaking* change to persisted
/// records: codec swap, key-type change, table split/merge, semantic change.
/// Additive fields (with `#[serde(default)]`) do NOT bump it — named
/// `MessagePack` tolerates those without a migration.
///
/// v2 (the current version): the `session_turns` value codec changed from raw
/// `MessagePack` to zstd-compressed `MessagePack`. This IS a breaking codec change
/// (an uncompressed legacy blob and a compressed one are mutually undecodable
/// through the opposite reader), so it owns the 1→2 migration that re-encodes
/// every existing row.
pub const SCHEMA_VERSION: u64 = 2;

/// The version [`open_db`](super::open_db) stamps on a database file it creates.
/// Fixed at 1: the 0 → 1 transition is *initialization* (a brand-new file,
/// stamped at creation), never a migration, so it must not drift.
/// [`run_migrations`] then brings the database from this version up to
/// [`SCHEMA_VERSION`].
/// Stamping here is what lets `run_migrations` treat any database still
/// reporting version 0 at startup as a *pre-existing* unversioned file
/// (pre-release leftovers) and refuse it once the chain grows past 1 — a
/// fresh install is never mistaken for one.
pub const INITIAL_SCHEMA_VERSION: u64 = 1;

/// A single schema migration: upgrades schema version `from` → `from + 1`.
///
/// The source version is carried *explicitly* — an entry's position in
/// [`MIGRATIONS`] is irrelevant, so a future contributor cannot silently
/// break the chain by placing the first migration at the wrong index (the
/// 0 → 1 transition is initialization, not a migration, so no entry has
/// `from == 0`; the first real migration is `from == 1`).
///
/// Each migration must:
/// - run in exactly one redb write transaction (a crash mid-migration leaves
///   the pre-migration state intact);
/// - decode historical record shapes with frozen local copies of the old
///   structs (current shapes drift over time);
/// - leave the database in the state `from + 1` describes; and
/// - be **idempotent under re-run**: the runner's crash recovery re-runs
///   migrations from the last persisted version (a migration that succeeded
///   but whose stamp was never committed would otherwise be re-applied), so
///   applying the same migration twice must produce the identical final state.
pub(super) struct Migration {
    pub(super) from: u64,
    run: fn(&Store) -> io::Result<()>,
}

/// Ordered migration chain. The first real entry (1 → 2, the `session_turns`
/// zstd codec change) lands here, upgrading FROM the initial stamped version.
/// A future breaking change adds the next entry (from == 2). [`run_migrations_to`]
/// validates that the chain is contiguous and covers every version from the
/// first migration up to the target before applying anything.
///
/// `pub(super)` because [`open`](super::open)'s pre-migration backup decision
/// reads this chain rather than keeping a second copy of it.
pub(super) const MIGRATIONS: &[Migration] = &[Migration {
    from: 1,
    run: migrate_turn_values_to_zstd,
}];

/// Read the persisted schema version of an open database, or `0` for an
/// unversioned one (no `meta` table yet, or the `schema_version` key absent).
///
/// Public so callers outside this module (the startup sequence's
/// open → version → drop → backup → reopen path) can read the version without
/// depending on the internal table layout.
///
/// # Errors
///
/// Returns Err if the read transaction or the `meta` table read fails.
pub fn schema_version(db: &Store) -> io::Result<u64> {
    let read_txn = db
        .begin_read()
        .map_err(|e| db.err(format!("redb read txn: {e}")))?;
    let table = match read_txn.open_table(META) {
        Ok(table) => table,
        // No meta table ⇒ either a freshly created DB (never stamped) or a
        // pre-release leftover. Both report 0 (unversioned).
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(0),
        Err(e) => return Err(db.err(format!("redb open meta: {e}"))),
    };
    Ok(table
        .get(SCHEMA_VERSION_KEY)
        .map_err(|e| db.err(format!("redb get meta: {e}")))?
        .map_or(0, |guard| guard.value()))
}

/// Persist `version` under `SCHEMA_VERSION_KEY` in [`META`]. Opening the
/// table inside a write transaction creates it on first use, so this also
/// initializes the `meta` table on a fresh database.
pub(super) fn stamp_schema_version(db: &Store, version: u64) -> io::Result<()> {
    let write_txn = db
        .begin_write()
        .map_err(|e| db.err(format!("redb write txn: {e}")))?;
    {
        let mut table = write_txn
            .open_table(META)
            .map_err(|e| db.err(format!("redb open meta: {e}")))?;
        table
            .insert(SCHEMA_VERSION_KEY, version)
            .map_err(|e| db.err(format!("redb set schema_version: {e}")))?;
    }
    write_txn
        .commit()
        .map_err(|e| db.err(format!("redb commit schema_version: {e}")))?;
    info!(version, "stamped database schema version");
    Ok(())
}

/// Bring the database up to [`SCHEMA_VERSION`]. Idempotent; safe to call on
/// every startup, right after [`open_db`](super::open_db). Delegates to `run_migrations_to`
/// with the production version and chain, resolving the database file path
/// once so the pre-migration backup targets the file that is actually being
/// migrated (never injected from a test's tempdir).
///
/// # Errors
///
/// Returns Err if the database path cannot be resolved.
pub fn run_migrations(db: &Store) -> io::Result<()> {
    run_migrations_at(db, &db_path()?)
}

/// [`run_migrations`] parameterized by the database file path (the pre-migration
/// backup targets this file), mirroring [`open_db_at`](super::open_db_at) for non-CLI embedders.
///
/// # Errors
///
/// Returns Err if the current version cannot be read, the database is
/// newer than the target (downgrade protection), the migration chain is
/// non-contiguous, or any migration step or backup write fails.
pub fn run_migrations_at(db: &Store, path: &std::path::Path) -> io::Result<()> {
    run_migrations_to(db, SCHEMA_VERSION, MIGRATIONS, path)
}

/// Decide whether the pending migration would create a pre-migration backup,
/// and if so return the schema version the backup must be named after (the
/// source version). Returns `Ok(Some(version))` in EXACTLY the situations where
/// [`run_migrations_to`] reaches [`backup_db_file`] for the production chain
/// ([`MIGRATIONS`], target [`SCHEMA_VERSION`]), and `Ok(None)` otherwise:
///
/// - version 0: the 0 → 1 (or 0 → stamp) path is pure initialization — no
///   migration writes, no backup;
/// - version == [`SCHEMA_VERSION`]: the idempotent fast path, nothing runs;
/// - version > [`SCHEMA_VERSION`]: the runner refuses the database BEFORE any
///   write, so no backup is taken;
/// - the migration chain is empty or not contiguous over `1..SCHEMA_VERSION`:
///   the runner hard-errors before the backup in that case.
///
/// This is the single source of truth shared by the startup sequence's pre-lock
/// backup decision and (via the skip-if-exists check in [`backup_db_file`]) the
/// runner's own backup step, so the two can never disagree about whether a
/// backup exists before the migration runs. It lives beside the runner it
/// mirrors rather than beside the copy it decides about, so the decision reads
/// the chain and the version constants without making the backup module depend
/// on the migration machinery.
pub(crate) fn migration_backup_version(db: &Store) -> io::Result<Option<u64>> {
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

/// The full migration runner, parameterized by the target version and the
/// migration chain so the future (non-empty-chain) behavior is unit-testable
/// today. Production entry point: [`run_migrations`].
///
/// The path of the database file being migrated is a parameter, so a unit test
/// can point the pre-migration backup at its own tempdir instead of leaking a
/// copy of the *real* data-directory file (the pre-existing design called
/// `db_path()` here, which made any `run_migrations_to` test with a non-empty
/// chain silently snapshot the real `state.redb`). Production resolves it once
/// in [`run_migrations`]; tests inject their own path.
///
/// - A database at a *newer* version than the target is rejected outright
///   (downgrade protection — a future binary's writes would be misread by
///   this one).
/// - An unversioned database (version 0) is accepted only while the target
///   is 1, i.e. as the initial state. Once the chain grows past 1, a
///   no-meta database means pre-release leftovers and is refused with
///   recreate/restore guidance. (Fresh installs never reach this state:
///   [`open_db`](super::open_db) stamps [`INITIAL_SCHEMA_VERSION`] at creation.)
/// - The chain must be contiguous: the entries' `from` values must cover
///   exactly `1..target` (the 0 → 1 transition is initialization, so no
///   entry has `from == 0`). A gap — or a misplaced entry — is a hard error
///   BEFORE anything is written: silently stamping a version whose data was
///   never migrated would corrupt reads far worse than failing startup.
/// - The 0 → 1 transition is pure initialization, performed once at database
///   creation ([`open_db`](super::open_db) stamps [`INITIAL_SCHEMA_VERSION`]): no backup, no
///   migration (see [`MIGRATIONS`]). A database still at 0 at startup is a
///   pre-existing unversioned file: stamped to 1 with a warning while the
///   target is 1, refused once the chain grows past 1.
pub(super) fn run_migrations_to(
    db: &Store,
    target: u64,
    migrations: &[Migration],
    db_path: &std::path::Path,
) -> io::Result<()> {
    let current = schema_version(db)?;
    if current > target {
        error!(
            current,
            supported = target,
            "refusing to open database: schema version newer than this binary supports"
        );
        return Err(db.err(format!(
            "database schema version {current} is newer than this binary supports ({target}); \
             upgrade choreographr before continuing"
        )));
    }
    // An unversioned DB is only ever acceptable as the *initial* state (v1).
    // Once the chain grows, a no-meta DB means pre-release leftovers.
    if current == 0 && target > 1 {
        let msg =
            "database has no schema version (pre-release data); recreate it or restore a backup";
        error!("{msg}");
        return Err(db.err(msg.to_string()));
    }
    if current == target {
        return Ok(()); // idempotent fast path
    }
    // Validate the chain BEFORE any write: the entries' `from` values must
    // form the exact contiguous sequence 1..target. This catches a misplaced
    // entry — e.g. the first real migration written with `from == 0` when the
    // database is at v1 — before it can silently stamp a version whose data
    // was never migrated. Entries below `current` have already run on disk
    // and are skipped by the filter in the apply loop below.
    let expected: Vec<u64> = (1..target).collect();
    let provided: Vec<u64> = migrations.iter().map(|m| m.from).collect();
    if provided != expected {
        let msg =
            format!("migration chain is not contiguous: has {provided:?}, needs {expected:?}");
        error!("{msg}");
        return Err(db.err(msg));
    }
    if current == 0 {
        // A fresh DB or a pre-release dev DB. Both are stamped the same way —
        // the leftover postcard-era blobs are deliberately not migrated (no
        // v0 → v1 migration by design) and will be skipped with a warning by
        // read_all_sessions/read_turns on first read.
        warn!(
            "database was unversioned; stamping schema version {target} \
             (pre-release blobs, if any, are not migrated)"
        );
    }
    // Snapshot only before an actual migration writes. With an empty chain
    // (current release) this never fires — the 0 → 1 transition is pure
    // initialization (stamping), and nothing was rewritten. The backup is
    // named after the version being migrated FROM: `current` is the schema
    // version of the file on disk right now.
    if !migrations.is_empty() {
        backup_db_file(db_path, current)?; // state.redb → state.redb.bak-v{current}
    }
    for migration in migrations.iter().filter(|m| m.from >= current) {
        info!(
            from = migration.from,
            to = migration.from + 1,
            "applying database migration"
        );
        // Parenthesized call: `run` is a field holding a function pointer, but
        // trait methods named `run` are in scope (e.g. flate2's `Ops`), so an
        // unparenthesized `migration.run(db)` is parsed as a method call and
        // fails to resolve. The explicit `(…)` disambiguates field access.
        (migration.run)(db)?;
    }
    // Final stamping: initializes a fresh/legacy DB (0 → 1) and is a no-op
    // when the last migration already stamped its target.
    stamp_schema_version(db, target)
}

/// The 1 → 2 schema migration: re-encode every `session_turns` value from raw
/// `MessagePack` (the v1 codec) to zstd-compressed `MessagePack` (the v2 codec).
/// Compression is codec-orthogonal to serialization, so a legacy raw blob is
/// re-encoded by simply wrapping the SAME `MessagePack` bytes in a zstd frame —
/// no deserialize/re-serialize of the `Turn` is needed.
///
/// Idempotency (required by the migration framework, whose crash recovery may
/// re-run this after the stamp was never committed): an already-compressed row
/// is recognized by [`ZSTD_FRAME_MAGIC`] and left untouched, so re-running
/// cannot double-compress a row.
///
/// Runs the rewrite in exactly one redb write transaction (the atomic-change
/// contract: a crash mid-rewrite leaves the pre-migration state intact).
fn migrate_turn_values_to_zstd(db: &Store) -> io::Result<()> {
    info!("applying 1→2 migration: re-encoding session_turns values with zstd");
    // Pass 1 (read txn): identify which rows need re-encoding. We buffer only
    // the (session_id, turn_id) keys — never the values — so memory stays flat
    // no matter how many turns, or how large, the database holds (turn history
    // is the bulk of the DB, and buffering every blob would spike startup RAM).
    let mut keys: Vec<(u64, u32)> = Vec::new();
    let mut skipped = 0usize;
    {
        let read_txn = db
            .begin_read()
            .map_err(|e| db.err(format!("redb read txn: {e}")))?;
        let table = match read_txn.open_table(SESSION_TURNS) {
            // A fresh database with no turns has no table yet — nothing to migrate.
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(()),
            Ok(t) => t,
            Err(e) => return Err(db.err(format!("redb open turns (migration): {e}"))),
        };
        let iter = table
            .iter()
            .map_err(|e| db.err(format!("redb iter turns (migration): {e}")))?;
        for result in iter {
            let (key, value) =
                result.map_err(|e| db.err(format!("redb iter item (migration): {e}")))?;
            let (sid, idx) = key.value();
            // Already a zstd frame ⇒ this row survived an earlier (stamp-less)
            // run of this same migration; leave it byte-for-byte intact.
            if value.value().starts_with(&ZSTD_FRAME_MAGIC) {
                debug!(
                    session_id = sid,
                    turn_id = idx,
                    "turn already zstd-compressed; skipping"
                );
                skipped += 1;
                continue;
            }
            keys.push((sid, idx));
        }
    }
    if keys.is_empty() {
        info!(skipped, "no raw session_turns values to re-encode");
        return Ok(());
    }
    // Pass 2 (exactly one redb write transaction, the atomic-change contract: a
    // crash mid-rewrite leaves the pre-migration state intact). We re-read each
    // value inside the write snapshot and re-encode it in place — migration runs
    // single-threaded at startup with no competing writers, so holding the write
    // lock while compressing is safe and keeps memory flat (only keys buffered).
    let write_txn = db
        .begin_write()
        .map_err(|e| db.err(format!("redb write txn (migration): {e}")))?;
    {
        let mut table = write_txn
            .open_table(SESSION_TURNS)
            .map_err(|e| db.err(format!("redb open turns (migration): {e}")))?;
        for key in &keys {
            // The write snapshot reflects the pre-write state for every key here
            // (keys are distinct, each read before its write), so get() returns
            // the legacy raw-MessagePack blob that Pass 1 identified.
            let raw = table
                .get(*key)
                .map_err(|e| db.err(format!("redb get turn (migration): {e}")))?
                .ok_or_else(|| db.err(format!("turn vanished during migration: {key:?}")))
                .map(|g| g.value().to_vec())?;
            let compressed = zstd_encode(&raw);
            table
                .insert(*key, compressed.as_slice())
                .map_err(|e| db.err(format!("redb insert turn (migration): {e}")))?;
        }
    }
    write_txn
        .commit()
        .map_err(|e| db.err(format!("redb commit turn migration: {e}")))?;
    info!(
        encoded = keys.len(),
        skipped, "re-encoded session_turns values with zstd"
    );
    Ok(())
}

/// Stamp [`INITIAL_SCHEMA_VERSION`] on a database file that was just
/// created. A fresh file has no `meta` table and would otherwise report
/// version 0 — which [`run_migrations`] treats as a *pre-existing*
/// unversioned file and refuses once the chain grows past 1. Performing the
/// 0 → 1 initialization here, at creation, keeps every later startup on the
/// migrate-from-`current` path regardless of [`SCHEMA_VERSION`].
pub(super) fn initialize_schema_version(db: &Store) -> io::Result<()> {
    stamp_schema_version(db, INITIAL_SCHEMA_VERSION)
        .map_err(|e| io::Error::other(format!("failed to initialize schema version: {e}")))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use choreo_proto::Turn;

    use super::*;
    use crate::db::read_turns;

    fn dummy_turn() -> Turn {
        Turn {
            created_at: choreo_proto::TimestampMs::now(),
            undone: false,
            error: None,
            user_text: Some("hello".into()),
            assistant_text: None,
            assistant_reasoning: None,
            tool_calls: Vec::new(),
            token_usage: None,
            tool_results: Vec::new(),
            displayed_images: Vec::new(),
            reasoning_artifact: None,
            reasoning_producer: None,
        }
    }

    #[test]
    fn migrate_turn_values_to_zstd_rewrites_legacy_rows() {
        // A v1 database stores turns as raw MessagePack. The 1→2 migration must
        // re-encode every row to a zstd frame so `read_turns` (which now always
        // decompresses) can read them after the upgrade. Before the migration
        // the same rows are the OPPOSITE codec, so `read_turns` cannot decode
        // them — that "breaking codec change" is exactly why the migration owns
        // the 1→2 schema bump (see SCHEMA_VERSION).
        let dir = tempfile::tempdir().unwrap();
        let db = Store::from_db(redb::Database::create(dir.path().join("test.redb")).unwrap());
        let sid = 1u64;

        // Write legacy raw-MessagePack turn blobs directly into SESSION_TURNS,
        // bypassing write_turn (which now compresses) — exactly what a v1 DB
        // has on disk.
        let turns: Vec<Turn> = (0..3).map(|_| dummy_turn()).collect();
        {
            let write_txn = db.begin_write().unwrap();
            {
                let mut table = write_txn.open_table(SESSION_TURNS).unwrap();
                for (i, turn) in turns.iter().enumerate() {
                    let raw = rmp_serde::to_vec_named(turn).unwrap();
                    // usize→u32 table key: the fixture writes 3 turns.
                    #[expect(clippy::cast_possible_truncation)]
                    table.insert((sid, i as u32), raw.as_slice()).unwrap();
                }
            }
            write_txn.commit().unwrap();
        }

        // Raw (uncompressed) blobs are undecodable through the now-
        // decompressing reader: nothing is read back yet.
        assert_eq!(read_turns(&db, sid).unwrap().len(), 0);

        migrate_turn_values_to_zstd(&db).unwrap();

        // After migration every row is a zstd frame that decodes to the turn.
        let decoded = read_turns(&db, sid).unwrap();
        assert_eq!(decoded.len(), 3);
        for (i, (idx, turn)) in decoded.iter().enumerate() {
            assert_eq!(*idx as usize, i);
            assert_eq!(turn, &turns[i]);
        }
        // And the stored bytes are genuinely compressed (zstd frame magic).
        {
            let read_txn = db.begin_read().unwrap();
            let table = read_txn.open_table(SESSION_TURNS).unwrap();
            for i in 0..3 {
                let guard = table.get((sid, i)).unwrap().unwrap();
                let v = guard.value();
                assert!(
                    v.starts_with(&ZSTD_FRAME_MAGIC),
                    "row {i} must be stored as a zstd frame"
                );
            }
        }
    }

    #[test]
    fn production_migration_chain_matches_schema_version() {
        // The chain's `from` values must cover exactly 1..SCHEMA_VERSION
        // (the 0 → 1 transition is initialization, not a migration, so no
        // entry has `from == 0`). Pinning this in a test makes a misplaced
        // entry fail CI immediately — the runner's runtime guard is skipped
        // on the `current == target` fast path, so without this canary a
        // broken chain would only error at the next schema bump.
        let provided: Vec<u64> = MIGRATIONS.iter().map(|m| m.from).collect();
        let expected: Vec<u64> = (1..SCHEMA_VERSION).collect();
        assert_eq!(provided, expected);
    }

    #[test]
    fn migrate_turn_values_to_zstd_is_idempotent() {
        // The migration framework may RE-RUN a migration when its schema stamp
        // was never committed (crash window). Re-running/MIRRORSing must not
        // double-compress: a row already holding a zstd frame must be left
        // byte-for-byte intact.
        let dir = tempfile::tempdir().unwrap();
        let db = Store::from_db(redb::Database::create(dir.path().join("test.redb")).unwrap());
        let sid = 1u64;
        let turn = dummy_turn();
        {
            let raw = rmp_serde::to_vec_named(&turn).unwrap();
            let write_txn = db.begin_write().unwrap();
            {
                let mut table = write_txn.open_table(SESSION_TURNS).unwrap();
                table.insert((sid, 0u32), raw.as_slice()).unwrap();
            }
            write_txn.commit().unwrap();
        }

        migrate_turn_values_to_zstd(&db).unwrap();
        // Capture the compressed bytes after the first run.
        let after_first: Vec<u8> = {
            let read_txn = db.begin_read().unwrap();
            let table = read_txn.open_table(SESSION_TURNS).unwrap();
            table.get((sid, 0)).unwrap().unwrap().value().to_vec()
        };
        assert!(after_first.starts_with(&ZSTD_FRAME_MAGIC));

        // Re-run (simulates crash recovery): the row must be unchanged (not
        // double-compressed) and still decode to the original turn.
        migrate_turn_values_to_zstd(&db).unwrap();
        {
            let read_txn = db.begin_read().unwrap();
            let table = read_txn.open_table(SESSION_TURNS).unwrap();
            let guard = table.get((sid, 0)).unwrap().unwrap();
            let v = guard.value();
            assert_eq!(
                v,
                after_first.as_slice(),
                "re-run must not rewrite an already-compressed row"
            );
        }
        assert_eq!(read_turns(&db, sid).unwrap()[0].1, turn);
    }

    #[test]
    fn run_migrations_rejects_newer_schema_version() {
        // Simulate a database written by a future binary by stamping a
        // version above SCHEMA_VERSION directly into meta.
        let dir = tempfile::tempdir().unwrap();
        let db = Store::from_db(redb::Database::create(dir.path().join("test.redb")).unwrap());
        {
            let write_txn = db.begin_write().unwrap();
            {
                let mut table = write_txn.open_table(META).unwrap();
                table.insert(SCHEMA_VERSION_KEY, 5u64).unwrap();
            }
            write_txn.commit().unwrap();
        }
        let err = run_migrations(&db).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("newer") && msg.contains('5'),
            "error must name the newer version: {msg}"
        );
    }

    #[test]
    fn run_migrations_to_backs_up_and_stamps_for_first_real_migration() {
        // The first real migration (1→2, the zstd codec change) must snapshot
        // the database BEFORE the rewrite — backup named after the SOURCE
        // version (bak-v1) — and stamp the current schema version afterwards.
        // Exercises the real production MIGRATIONS chain with an injected temp
        // path so no real data-directory file is ever touched (see the
        // run_migrations_to `db_path` parameter — the pre-existing design
        // called db_path() here and silently snapshotted the real state.redb).
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("state.redb");
        let db = Store::from_db(redb::Database::create(&db_path).unwrap());
        stamp_schema_version(&db, 1).unwrap(); // a v1 database, pre-migration

        run_migrations_to(&db, SCHEMA_VERSION, MIGRATIONS, &db_path).unwrap();

        assert!(
            db_path.with_file_name("state.redb.bak-v1").exists(),
            "the 1→2 migration must back up the source-version file"
        );
        assert_eq!(
            schema_version(&db).unwrap(),
            SCHEMA_VERSION,
            "the migration must reach the current schema version"
        );
    }

    /// A stand-in for a real future migration: records a marker in `meta` so a
    /// test can assert the migration actually ran.
    fn dummy_migrate_1_to_2(db: &Store) -> io::Result<()> {
        let write_txn = db
            .begin_write()
            .map_err(|e| db.err(format!("redb write txn: {e}")))?;
        {
            let mut table = write_txn
                .open_table(META)
                .map_err(|e| db.err(format!("redb open meta: {e}")))?;
            table
                .insert("migrated", 1u64)
                .map_err(|e| db.err(format!("redb set migrated marker: {e}")))?;
        }
        write_txn
            .commit()
            .map_err(|e| db.err(format!("redb commit migrated marker: {e}")))?;
        Ok(())
    }

    #[test]
    fn run_migrations_applies_contiguous_chain_from_current_version() {
        // Simulates the first real migration landing (1 → 2): a database
        // stamped at v1 plus a dummy migration entry. Pins the runner's
        // indexing — the entry's explicit `from` field (not its position)
        // determines what runs.
        let dir = tempfile::tempdir().unwrap();
        let db = Store::from_db(redb::Database::create(dir.path().join("test.redb")).unwrap());
        stamp_schema_version(&db, 1).unwrap();

        run_migrations_to(
            &db,
            2,
            &[Migration {
                from: 1,
                run: dummy_migrate_1_to_2,
            }],
            &dir.path().join("test.redb"),
        )
        .unwrap();

        assert_eq!(schema_version(&db).unwrap(), 2);
        {
            let read_txn = db.begin_read().unwrap();
            let table = read_txn.open_table(META).unwrap();
            assert_eq!(
                table.get("migrated").unwrap().unwrap().value(),
                1,
                "the dummy migration must have run"
            );
        }
    }

    #[test]
    fn run_migrations_rejects_non_contiguous_chain_before_writing() {
        // The natural mistake a contributor would make: writing the first
        // migration with `from == 0` (thinking of the array index) when the
        // database is at v1. The runner must refuse loudly BEFORE writing
        // anything — stamping v2 over data that was never migrated would
        // corrupt every subsequent read.
        let dir = tempfile::tempdir().unwrap();
        let db = Store::from_db(redb::Database::create(dir.path().join("test.redb")).unwrap());
        stamp_schema_version(&db, 1).unwrap();

        let err = run_migrations_to(
            &db,
            2,
            &[Migration {
                from: 0,
                run: dummy_migrate_1_to_2,
            }],
            &dir.path().join("test.redb"),
        )
        .unwrap_err();

        let msg = err.to_string();
        assert!(
            msg.contains("not contiguous") && msg.contains('0') && msg.contains('1'),
            "error must describe the chain mismatch: {msg}"
        );
        // Nothing was applied or stamped: still at v1, marker absent.
        assert_eq!(schema_version(&db).unwrap(), 1);
        {
            let read_txn = db.begin_read().unwrap();
            let table = read_txn.open_table(META).unwrap();
            assert!(
                table.get("migrated").unwrap().is_none(),
                "no migration may run when the chain is rejected"
            );
        }
    }

    #[test]
    fn run_migrations_to_does_not_overwrite_pre_existing_backup() {
        // The production path (cli.rs) now takes the backup BEFORE the
        // database file is opened/locked (redb's whole-file exclusive lock
        // blocks same-process reads on Windows). run_migrations_to must
        // therefore treat an existing backup as authoritative and never
        // overwrite it — this test pre-creates the backup with sentinel
        // content and asserts the sentinel survives the migration.
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("state.redb");
        let db = Store::from_db(redb::Database::create(&db_path).unwrap());
        stamp_schema_version(&db, 1).unwrap();
        let backup_path = db_path.with_file_name("state.redb.bak-v1");
        fs::write(&backup_path, b"pre-lock sentinel").unwrap();

        run_migrations_to(&db, SCHEMA_VERSION, MIGRATIONS, &db_path).unwrap();

        assert_eq!(
            fs::read(&backup_path).unwrap(),
            b"pre-lock sentinel",
            "the pre-existing backup must not be overwritten by the runner"
        );
        assert_eq!(schema_version(&db).unwrap(), SCHEMA_VERSION);
    }

    #[test]
    fn schema_version_accessor_returns_stamped_version() {
        // The public accessor the startup sequence relies on must report the
        // stamped version of a fresh database (open_db stamps
        // INITIAL_SCHEMA_VERSION at creation; here a v1 stamp is applied
        // directly to keep the test independent of db_path()).
        let dir = tempfile::tempdir().unwrap();
        let db = Store::from_db(redb::Database::create(dir.path().join("test.redb")).unwrap());
        stamp_schema_version(&db, 1).unwrap();
        assert_eq!(schema_version(&db).unwrap(), 1);
    }

    #[test]
    fn migration_backup_version_matches_run_migrations_to_backup_conditions() {
        // The version-gating helper must return Some exactly when
        // run_migrations_to would take a backup for the production chain:
        // Some(1) for a pending real migration from v1; None for version 0
        // (pure initialization stamp, no backup), for version ==
        // SCHEMA_VERSION (fast path), and for a newer-than-supported version
        // (the runner refuses before any write). Keeping both sides pinned to
        // one helper is what prevents the startup sequence's pre-lock backup
        // decision from drifting away from the runner's own backup step.
        let dir = tempfile::tempdir().unwrap();

        let make_db_at = |version: u64| {
            let db = Store::from_db(
                redb::Database::create(dir.path().join(format!("test-{version}.redb"))).unwrap(),
            );
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
    fn run_migrations_refuses_unversioned_db_when_target_above_initial() {
        // The flip side of the fresh-install fix: a database that is STILL
        // unversioned (current == 0) at startup never went through open_db's
        // creation-time initialization — it is a pre-existing file (pre-
        // release leftovers). Once the chain grows past the initial version
        // (target > 1) the runner must refuse it rather than stamp over data
        // that was never migrated. Fresh installs never hit this branch
        // because open_db stamps INITIAL_SCHEMA_VERSION at creation.
        let dir = tempfile::tempdir().unwrap();
        let db = Store::from_db(redb::Database::create(dir.path().join("test.redb")).unwrap());

        let err = run_migrations_to(
            &db,
            2,
            &[Migration {
                from: 1,
                run: dummy_migrate_1_to_2,
            }],
            &dir.path().join("test.redb"),
        )
        .unwrap_err();

        let msg = err.to_string();
        assert!(
            msg.contains("no schema version"),
            "error must name the pre-release refusal: {msg}"
        );
        // Nothing was written: still unversioned, marker absent.
        assert_eq!(schema_version(&db).unwrap(), 0);
        {
            let read_txn = db.begin_read().unwrap();
            // The meta table may not exist at all (nothing was ever written)
            // — that itself proves no migration ran.
            match read_txn.open_table(META) {
                Ok(table) => assert!(
                    table.get("migrated").unwrap().is_none(),
                    "no migration may run when a pre-existing unversioned DB is refused"
                ),
                Err(redb::TableError::TableDoesNotExist(_)) => {}
                Err(e) => panic!("unexpected table error: {e}"),
            }
        }
    }
}
