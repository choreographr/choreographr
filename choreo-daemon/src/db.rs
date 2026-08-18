use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::PathBuf;

use choreo_proto::{ContextConfig, ReasoningProducer, Turn};
use redb::ReadableDatabase;
use redb::ReadableTable;
use redb::TableDefinition;
use serde::{Deserialize, Serialize};
use tracing::{debug, error, info, warn};

const SESSIONS: TableDefinition<u64, &[u8]> = TableDefinition::new("sessions");
const SESSION_TURNS: TableDefinition<(u64, u32), &[u8]> = TableDefinition::new("session_turns");
const CREDENTIALS: TableDefinition<&str, &[u8]> = TableDefinition::new("credentials");
/// Production `meta` table: string keys, u64 values. Holds the persisted
/// schema version under [`SCHEMA_VERSION_KEY`]; the test-only
/// `next_session_id` counter shares the same table (test key, shared table).
const META: TableDefinition<&str, u64> = TableDefinition::new("meta");
/// Runtime catalog-refresh state (S4): the last models.dev fetch-attempt
/// timestamp and the current etag. One table for both so the refresh state is
/// a single coherent record. A new table is created lazily on first write, so
/// adding it is purely additive — no schema version bump (the migration chain
/// stays empty).
const CATALOG_STATE: TableDefinition<&str, &[u8]> = TableDefinition::new("catalog_state");
/// Key for the last models.dev fetch-attempt timestamp (Unix epoch millis, 8
/// bytes little-endian). Written BEFORE every fetch: the cooldown is armed at
/// attempt start, so a daemon that crashes mid-fetch and restarts immediately
/// reads a fresh timestamp and honors the remaining cooldown instead of
/// re-fetching.
const CATALOG_LAST_ATTEMPT_KEY: &str = "last_attempt_ms";
/// Key for the models.dev etag (UTF-8 bytes of the raw entity-tag). Written by
/// the daemon command loop AFTER the cache bin is persisted, so the etag
/// always describes content at least as new as what is on disk (bin-first
/// ordering keeps a crash between the two writes paired with the OLD content,
/// which self-heals via a 200 on the next fetch).
const CATALOG_ETAG_KEY: &str = "etag";
const SESSION_KV: TableDefinition<(u64, String), Vec<u8>> = TableDefinition::new("session_kv");
/// Tombstones for deleted sessions whose still-shutting-down thread may
/// re-create the record.  Keyed by session id; present means "deleted — purge
/// any record bearing this id at next startup" (see [`purge_tombstoned_sessions`]).
const DELETED_SESSIONS: TableDefinition<u64, ()> = TableDefinition::new("deleted_sessions");

/// Iterator type returned by redb range queries on SESSION_KV.
type KvRangeIter<'a> = Box<
    dyn Iterator<
            Item = Result<
                (
                    redb::AccessGuard<'a, (u64, String)>,
                    redb::AccessGuard<'a, Vec<u8>>,
                ),
                redb::StorageError,
            >,
        > + 'a,
>;

fn db_err(msg: String) -> io::Error {
    io::Error::other(msg)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRecord {
    pub title: Option<String>,
    pub selected_model: Option<String>,
    pub parent_session_id: Option<u64>,
    pub working_dir: Option<String>,
    pub turn_count: u32,
    /// Creation time, Unix-epoch-milliseconds.
    pub created_at: i64,
    /// Most recent modification time, Unix-epoch-milliseconds (status changes,
    /// turn completion, title/model edits).  Persisted so the sessions list
    /// keeps its "newest first" ordering across daemon restarts.
    pub last_modified: i64,
    pub active_tool_groups: Vec<String>,
    #[serde(default)]
    pub context_config: ContextConfig,
    #[serde(default)]
    pub account_name: Option<String>,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    /// Last provider response id, persisted so ResponseId-policy models
    /// (OpenAI/xAI Responses) can chain `previous_response_id` across user
    /// turns and daemon restarts (phase 4c). `#[serde(default)]` matches the
    /// convention of the sibling optional fields; the project is unreleased,
    /// so the postcard blobs holding records are rebuilt in lockstep and old
    /// blobs are not expected on disk (undecodable entries are skipped with a
    /// warning by `read_all_sessions`).
    #[serde(default)]
    pub last_response_id: Option<String>,
    /// Which provider+model produced `last_response_id`. The request builder
    /// restores the persisted id only when the current provider+model matches
    /// (same provenance rule as reasoning artifacts) — a stale id persisted
    /// under a different provider (e.g. a mid-session openai → xAI switch)
    /// must never be replayed into a service that does not recognize it.
    #[serde(default)]
    pub last_response_id_producer: Option<ReasoningProducer>,
}

pub fn db_path() -> io::Result<PathBuf> {
    if let Ok(override_path) = std::env::var("CHOREOGRAPHR_DB_PATH") {
        return Ok(PathBuf::from(override_path));
    }
    let data_dir = dirs::data_dir().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "could not determine data directory",
        )
    })?;
    Ok(data_dir.join("choreographr").join("state.redb"))
}

// ── Schema versioning & migrations ─────────────────────────────────────────────

/// Persisted schema version. Bump on any *breaking* change to persisted
/// records: codec swap, key-type change, table split/merge, semantic change.
/// Additive fields (with `#[serde(default)]`) do NOT bump it — named
/// MessagePack tolerates those without a migration.
///
/// v2 (the current version): the `session_turns` value codec changed from raw
/// MessagePack to zstd-compressed MessagePack. This IS a breaking codec change
/// (an uncompressed legacy blob and a compressed one are mutually undecodable
/// through the opposite reader), so it owns the 1→2 migration that re-encodes
/// every existing row.
pub const SCHEMA_VERSION: u64 = 2;

/// zstd compression level for `session_turns` values. Level 3 (the library
/// default) balances ratio against speed: turn text/tool-output/reasoning
/// compresses 4–10× while encode stays hundreds of MB/s and decode is
/// GB/s-scale. Tuning this is a constant, not a design change — the codec is
/// concrete (zstd) and the on-disk contract is "zstd-compressed MessagePack".
const COMPRESSION_LEVEL: i32 = 3;

/// The 4-byte little-endian magic that prefixes every zstd frame
/// (0xFD2FB528). It is zstd's *inherent* frame marker, not a wrapper tag we
/// add — we use it purely to tell an already-compressed row from a legacy
/// raw-MessagePack row, which is what makes the 1→2 migration safe to re-run
/// after a crash (idempotency the migration framework requires).
///
/// It is unambiguous: a legacy `Turn` always serializes to a named-MessagePack
/// map, so its first byte is a map header (0x80..0x8f, the 13-field marker is
/// 0x8D) — never 0x28 (a positive fixint). A zstd frame starts with 0x28.
const ZSTD_FRAME_MAGIC: [u8; 4] = [0x28, 0xB5, 0x2F, 0xFD];

/// The version [`open_db`] stamps on a database file it creates. Fixed at 1:
/// the 0 → 1 transition is *initialization* (a brand-new file, stamped at
/// creation), never a migration, so it must not drift. [`run_migrations`]
/// then brings the database from this version up to [`SCHEMA_VERSION`].
/// Stamping here is what lets `run_migrations` treat any database still
/// reporting version 0 at startup as a *pre-existing* unversioned file
/// (pre-release leftovers) and refuse it once the chain grows past 1 — a
/// fresh install is never mistaken for one.
pub const INITIAL_SCHEMA_VERSION: u64 = 1;

/// Key under which the current schema version is stored in [`META`].
const SCHEMA_VERSION_KEY: &str = "schema_version";

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
struct Migration {
    from: u64,
    run: fn(&redb::Database) -> io::Result<()>,
}

/// Ordered migration chain. The first real entry (1 → 2, the `session_turns`
/// zstd codec change) lands here, upgrading FROM the initial stamped version.
/// A future breaking change adds the next entry (from == 2). [`run_migrations_to`]
/// validates that the chain is contiguous and covers every version from the
/// first migration up to the target before applying anything.
const MIGRATIONS: &[Migration] = &[Migration {
    from: 1,
    run: migrate_turn_values_to_zstd,
}];

/// Read the persisted schema version, or `0` for an unversioned database
/// (no `meta` table yet, or the `schema_version` key absent).
fn current_schema_version(db: &redb::Database) -> io::Result<u64> {
    let read_txn = db
        .begin_read()
        .map_err(|e| db_err(format!("redb read txn: {e}")))?;
    let table = match read_txn.open_table(META) {
        Ok(table) => table,
        // No meta table ⇒ either a freshly created DB (never stamped) or a
        // pre-release leftover. Both report 0 (unversioned).
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(0),
        Err(e) => return Err(db_err(format!("redb open meta: {e}"))),
    };
    Ok(table
        .get(SCHEMA_VERSION_KEY)
        .map_err(|e| db_err(format!("redb get meta: {e}")))?
        .map(|guard| guard.value())
        .unwrap_or(0))
}

/// Persist `version` under `SCHEMA_VERSION_KEY` in [`META`]. Opening the
/// table inside a write transaction creates it on first use, so this also
/// initializes the `meta` table on a fresh database.
fn stamp_schema_version(db: &redb::Database, version: u64) -> io::Result<()> {
    let write_txn = db
        .begin_write()
        .map_err(|e| db_err(format!("redb write txn: {e}")))?;
    {
        let mut table = write_txn
            .open_table(META)
            .map_err(|e| db_err(format!("redb open meta: {e}")))?;
        table
            .insert(SCHEMA_VERSION_KEY, version)
            .map_err(|e| db_err(format!("redb set schema_version: {e}")))?;
    }
    write_txn
        .commit()
        .map_err(|e| db_err(format!("redb commit schema_version: {e}")))?;
    info!(version, "stamped database schema version");
    Ok(())
}

/// Snapshot the database file before a migration rewrites it:
/// `path` → `path.bak-v{from}`, where `from` is the schema version of the
/// file being snapshotted (the version being migrated away from). Naming the
/// backup after its *source* version — not the migration target — keeps
/// restore semantics unambiguous: a `bak-v2` file IS a v2 database, so
/// restoring it rolls back to exactly the state the migration started from.
///
/// The path is injected (the caller resolves [`db_path`]) so the naming
/// behavior is unit-testable without touching the real data directory.
/// Fires only before a real migration writes (never for the pure 0 → 1
/// initialization stamp): one backup per source schema version, taken before
/// any write, so a failed migration can always be rolled back from disk. The
/// active 1 → 2 migration therefore snapshots a v1 file to `bak-v1`. Safe to
/// `fs::copy` the open file because this runs at startup, single-threaded,
/// before any migration writes — the database is quiescent, so the on-disk
/// image reflects the last committed transaction.
fn backup_db_file(path: &std::path::Path, from: u64) -> io::Result<()> {
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "state.redb".to_string());
    let backup_path = path.with_file_name(format!("{file_name}.bak-v{from}"));
    fs::copy(path, &backup_path)?;
    info!(
        from = %path.display(),
        to = %backup_path.display(),
        "backed up database before applying migrations"
    );
    Ok(())
}

/// Bring the database up to [`SCHEMA_VERSION`]. Idempotent; safe to call on
/// every startup, right after [`open_db`]. Delegates to [`run_migrations_to`]
/// with the production version and chain, resolving the database file path
/// once so the pre-migration backup targets the file that is actually being
/// migrated (never injected from a test's tempdir).
pub fn run_migrations(db: &redb::Database) -> io::Result<()> {
    run_migrations_to(db, SCHEMA_VERSION, MIGRATIONS, &db_path()?)
}

/// The full migration runner, parameterized by the target version and the
/// migration chain so the future (non-empty-chain) behavior is unit-testable
/// today. Production entry point: [`run_migrations`].
///
/// The path of the database file being migrated is a parameter, so a unit test
/// can point the pre-migration backup at its own tempdir instead of leaking a
/// copy of the *real* data-directory file (the pre-existing design called
/// `db_path()` here, which made any run_migrations_to test with a non-empty
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
///   [`open_db`] stamps [`INITIAL_SCHEMA_VERSION`] at creation.)
/// - The chain must be contiguous: the entries' `from` values must cover
///   exactly `1..target` (the 0 → 1 transition is initialization, so no
///   entry has `from == 0`). A gap — or a misplaced entry — is a hard error
///   BEFORE anything is written: silently stamping a version whose data was
///   never migrated would corrupt reads far worse than failing startup.
/// - The 0 → 1 transition is pure initialization, performed once at database
///   creation ([`open_db`] stamps [`INITIAL_SCHEMA_VERSION`]): no backup, no
///   migration (see [`MIGRATIONS`]). A database still at 0 at startup is a
///   pre-existing unversioned file: stamped to 1 with a warning while the
///   target is 1, refused once the chain grows past 1.
fn run_migrations_to(
    db: &redb::Database,
    target: u64,
    migrations: &[Migration],
    db_path: &std::path::Path,
) -> io::Result<()> {
    let current = current_schema_version(db)?;
    if current > target {
        error!(
            current,
            supported = target,
            "refusing to open database: schema version newer than this binary supports"
        );
        return Err(db_err(format!(
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
        return Err(db_err(msg.to_string()));
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
        return Err(db_err(msg));
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

/// Stamp [`INITIAL_SCHEMA_VERSION`] on a database file that was just
/// created. A fresh file has no `meta` table and would otherwise report
/// version 0 — which [`run_migrations`] treats as a *pre-existing*
/// unversioned file and refuses once the chain grows past 1. Performing the
/// 0 → 1 initialization here, at creation, keeps every later startup on the
/// migrate-from-`current` path regardless of [`SCHEMA_VERSION`].
fn initialize_schema_version(db: &redb::Database) -> io::Result<()> {
    stamp_schema_version(db, INITIAL_SCHEMA_VERSION)
        .map_err(|e| io::Error::other(format!("failed to initialize schema version: {e}")))
}

/// Open (or create) the database file. The file is created when missing or
/// empty (the corpse of an interrupted create); a freshly created file is
/// stamped with [`INITIAL_SCHEMA_VERSION`] here, but the *migration chain*
/// is deliberately NOT applied — callers run [`run_migrations`] right after,
/// before any table access (see `main.rs`). Hard-errors on a database it
/// cannot open rather than recreating a potentially recoverable file (the
/// old "trying to recreate" catch-all could silently clobber it).
pub fn open_db() -> io::Result<redb::Database> {
    let path = db_path()?;
    info!(path = %path.display(), "opening database");
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    // A 0-byte file is the corpse of an interrupted `Database::create`
    // (crash between file creation and the first write): it holds no
    // recoverable data, so recreate it rather than hard-erroring like a
    // potentially-valuable corrupt file. As with a brand-new file, the
    // initial schema version is stamped immediately so the database is
    // versioned from the moment it exists.
    if let Ok(metadata) = fs::metadata(&path)
        && metadata.len() == 0
    {
        warn!("database file exists but is empty (interrupted create?); recreating");
        let db = redb::Database::create(&path)
            .map_err(|e| io::Error::other(format!("failed to create database: {e}")))?;
        initialize_schema_version(&db)?;
        return Ok(db);
    }
    match redb::Database::open(&path) {
        Ok(db) => Ok(db),
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
            let db = redb::Database::create(&path)
                .map_err(|e| io::Error::other(format!("failed to create database: {e}")))?;
            initialize_schema_version(&db)?;
            Ok(db)
        }
        // redb file-format bump: the file is a valid redb database but in a
        // newer file format than this binary can read. Hard error with
        // recovery guidance — recreating would destroy the data.
        Err(redb::DatabaseError::UpgradeRequired(actual)) => Err(io::Error::other(format!(
            "database file format version {actual} is not supported by this binary; \
             restore a backup (state.redb.bak-v*) or use the documented dump/restore path"
        ))),
        // Any other open failure (corruption, permissions, lock contention…)
        // is also a hard error: the old "trying to recreate" catch-all could
        // silently clobber a potentially-recoverable file.
        Err(e) => Err(io::Error::other(format!(
            "failed to open database (refusing to recreate a potentially corrupt file): {e}"
        ))),
    }
}

pub fn write_session(
    db: &redb::Database,
    session_id: u64,
    record: &SessionRecord,
) -> io::Result<()> {
    let payload = rmp_serde::to_vec_named(record)
        .map_err(|e| db_err(format!("codec encode session: {e}")))?;
    let write_txn = db
        .begin_write()
        .map_err(|e| db_err(format!("redb write txn: {e}")))?;
    {
        let mut table = write_txn
            .open_table(SESSIONS)
            .map_err(|e| db_err(format!("redb open sessions: {e}")))?;
        table
            .insert(session_id, payload.as_slice())
            .map_err(|e| db_err(format!("redb insert session: {e}")))?;
    }
    write_txn
        .commit()
        .map_err(|e| db_err(format!("redb commit session: {e}")))?;
    debug!("write_session: id={} ok", session_id);
    Ok(())
}

/// Read a single session record. Returns `Ok(None)` both when the session
/// does not exist and when the stored record cannot be decoded — an
/// undecodable record is skipped with a warning and treated as absent, the
/// same policy as `read_all_sessions`/`read_turns`. A corrupt record is
/// unrecoverable, so it must never fail the caller (or the daemon); the
/// warning keeps the loss loud-but-non-fatal.
pub fn read_session(db: &redb::Database, session_id: u64) -> io::Result<Option<SessionRecord>> {
    debug!("read_session: id={}", session_id);
    let read_txn = db
        .begin_read()
        .map_err(|e| db_err(format!("redb read txn: {e}")))?;
    let table = read_txn
        .open_table(SESSIONS)
        .map_err(|e| db_err(format!("redb open sessions: {e}")))?;
    match table
        .get(session_id)
        .map_err(|e| db_err(format!("redb get session: {e}")))?
    {
        Some(guard) => match rmp_serde::from_slice::<SessionRecord>(guard.value()) {
            Ok(record) => Ok(Some(record)),
            Err(e) => {
                warn!(
                    session_id,
                    error = %e,
                    "undecodable session record, treating as absent"
                );
                Ok(None)
            }
        },
        None => Ok(None),
    }
}

pub fn read_all_sessions(db: &redb::Database) -> io::Result<Vec<(u64, SessionRecord)>> {
    debug!("read_all_sessions");
    let read_txn = db.begin_read().map_err(|e| {
        let msg = format!("redb read txn: {e}");
        error!("read_all_sessions: {msg}");
        db_err(msg)
    })?;
    let table = match read_txn.open_table(SESSIONS) {
        Ok(t) => t,
        Err(e) => {
            warn!("read_all_sessions: table 'sessions' not found (first run?): {e}");
            return Ok(Vec::new());
        }
    };
    let mut sessions: Vec<(u64, SessionRecord)> = Vec::new();
    let iter = match table.iter() {
        Ok(it) => it,
        Err(e) => {
            let msg = format!("redb iter sessions: {e}");
            error!("read_all_sessions: {msg}");
            return Err(db_err(msg));
        }
    };
    for result in iter {
        let (key, value) = match result {
            Ok(kv) => kv,
            Err(e) => {
                warn!("read_all_sessions: skipping bad entry: {e}");
                continue;
            }
        };
        match rmp_serde::from_slice::<SessionRecord>(value.value()) {
            Ok(record) => {
                sessions.push((key.value(), record));
            }
            Err(e) => {
                warn!(
                    "read_all_sessions: skipping session {} (decode failed: {e})",
                    key.value()
                );
                continue;
            }
        }
    }
    debug!("read_all_sessions: {} records", sessions.len());
    sessions.sort_by_key(|(id, _)| *id);
    Ok(sessions)
}

/// Exclusive upper-bound session id for the range queries that span a single
/// session's keys: `(session_id, …)..(session_range_end(session_id), …)`
/// covers every key whose first tuple element is `session_id`.
///
/// `saturating_add` keeps the bound total even at the theoretical
/// `session_id == u64::MAX` (which the daemon's monotonic id counter can
/// never reach in practice): the range would simply be empty for that id
/// instead of overflowing (debug) or wrapping (release).
fn session_range_end(session_id: u64) -> u64 {
    session_id.saturating_add(1)
}

pub fn delete_session(db: &redb::Database, session_id: u64) -> io::Result<()> {
    debug!("delete_session: id={}", session_id);
    let write_txn = db
        .begin_write()
        .map_err(|e| db_err(format!("redb write txn: {e}")))?;
    {
        let mut sessions = write_txn
            .open_table(SESSIONS)
            .map_err(|e| db_err(format!("redb open sessions: {e}")))?;
        sessions
            .remove(session_id)
            .map_err(|e| db_err(format!("redb remove session: {e}")))?;
    }
    {
        let mut turns = write_txn
            .open_table(SESSION_TURNS)
            .map_err(|e| db_err(format!("redb open turns: {e}")))?;
        // Bounded range scan over just this session's turn ids instead of
        // iterating the whole table (the old full-table scan made each delete
        // O(total turns) — costly for the largest sessions).
        let keys_to_remove: Vec<(u64, u32)> = turns
            .range::<(u64, u32)>((session_id, 0u32)..(session_range_end(session_id), 0u32))
            .map_err(|e| db_err(format!("redb range turns: {e}")))?
            .filter_map(|result| result.ok())
            .map(|(key, _)| key.value())
            .collect();
        for key in keys_to_remove {
            turns
                .remove(key)
                .map_err(|e| db_err(format!("redb remove turn: {e}")))?;
        }
    }
    {
        let mut kv_table = write_txn
            .open_table(SESSION_KV)
            .map_err(|e| db_err(format!("redb open session_kv: {e}")))?;
        let kv_keys: Vec<(u64, String)> = kv_table
            .range::<(u64, String)>(
                (session_id, String::new())..(session_range_end(session_id), String::new()),
            )
            .map_err(|e| db_err(format!("redb range session_kv: {e}")))?
            .filter_map(|result| result.ok())
            .map(|(k, _)| k.value())
            .collect();
        for key in kv_keys {
            kv_table
                .remove(key)
                .map_err(|e| db_err(format!("redb remove session_kv: {e}")))?;
        }
    }
    write_txn
        .commit()
        .map_err(|e| db_err(format!("redb commit delete: {e}")))?;
    Ok(())
}

/// Write a deletion tombstone for `session_id`.
///
/// Called by the daemon when deleting a session whose thread is still alive.
/// If that thread re-creates the record (via `persist_and_exit`) and the
/// daemon crashes before `handle_session_exited` finalizes the delete, the
/// tombstone survives so [`purge_tombstoned_sessions`] removes the record at
/// the next startup instead of letting a deleted session reappear.
pub fn mark_session_deleted(db: &redb::Database, session_id: u64) -> io::Result<()> {
    debug!("mark_session_deleted: id={}", session_id);
    let write_txn = db
        .begin_write()
        .map_err(|e| db_err(format!("redb write txn: {e}")))?;
    {
        let mut table = write_txn
            .open_table(DELETED_SESSIONS)
            .map_err(|e| db_err(format!("redb open deleted_sessions: {e}")))?;
        table
            .insert(session_id, ())
            .map_err(|e| db_err(format!("redb insert tombstone: {e}")))?;
    }
    write_txn
        .commit()
        .map_err(|e| db_err(format!("redb commit tombstone: {e}")))?;
    Ok(())
}

/// Remove the deletion tombstone for `session_id`.
///
/// Called once `handle_session_exited` has deleted the record the
/// still-shutting-down thread re-created, so the tombstone does not
/// accumulate.
pub fn clear_session_tombstone(db: &redb::Database, session_id: u64) -> io::Result<()> {
    debug!("clear_session_tombstone: id={}", session_id);
    let write_txn = db
        .begin_write()
        .map_err(|e| db_err(format!("redb write txn: {e}")))?;
    {
        let mut table = write_txn
            .open_table(DELETED_SESSIONS)
            .map_err(|e| db_err(format!("redb open deleted_sessions: {e}")))?;
        table
            .remove(session_id)
            .map_err(|e| db_err(format!("redb remove tombstone: {e}")))?;
    }
    write_txn
        .commit()
        .map_err(|e| db_err(format!("redb commit tombstone: {e}")))?;
    Ok(())
}

/// Delete every session that carries a deletion tombstone and clear the
/// tombstones.  Returns the number of sessions purged.
///
/// Called once at daemon startup, before the session index is loaded: a
/// deleted session whose still-shutting-down thread re-created the record,
/// then died with a crashed daemon before the delete could be finalized,
/// must not resurface.  Deleting a record that is already gone is a harmless
/// no-op.
pub fn purge_tombstoned_sessions(db: &redb::Database) -> io::Result<usize> {
    let read_txn = db
        .begin_read()
        .map_err(|e| db_err(format!("redb read txn: {e}")))?;
    let table = match read_txn.open_table(DELETED_SESSIONS) {
        Ok(table) => table,
        // No tombstone table yet (e.g. a pre-upgrade database): nothing to purge.
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(0),
        Err(e) => return Err(db_err(format!("redb open deleted_sessions: {e}"))),
    };
    let ids: Vec<u64> = table
        .iter()
        .map_err(|e| db_err(format!("redb iter deleted_sessions: {e}")))?
        .filter_map(|result| result.ok())
        .map(|(key, _)| key.value())
        .collect();
    drop(read_txn);

    let mut purged = 0usize;
    for id in ids {
        if let Err(e) = delete_session(db, id) {
            warn!(session_id = id, error = %e, "purge: failed to delete tombstoned session");
            continue;
        }
        if let Err(e) = clear_session_tombstone(db, id) {
            warn!(session_id = id, error = %e, "purge: failed to clear tombstone");
        }
        purged += 1;
        info!(
            session_id = id,
            "purged session record left behind by a deleted-session shutdown"
        );
    }
    Ok(purged)
}

// ── Turn value compression ────────────────────────────────────────────────────

/// Encode a MessagePack-encoded `Turn` with zstd at [`COMPRESSION_LEVEL`].
/// Compression is applied to the WHOLE serialized blob (never per-field): zstd
/// matches redundancy across the entire buffer, so it also compresses the
/// MessagePack framing overhead (field keys, headers) on top of the string
/// payloads. `encode_all` wraps the slice in a `Cursor` and returns one zstd
/// frame.
fn zstd_encode(payload: &[u8]) -> io::Result<Vec<u8>> {
    zstd::stream::encode_all(std::io::Cursor::new(payload), COMPRESSION_LEVEL)
        .map_err(|e| db_err(format!("zstd encode turn: {e}")))
}

/// Recover the original MessagePack bytes from a zstd frame. The zstd frame is
/// self-terminating (it carries the uncompressed length), so `decode_all` needs
/// no size hint.
fn zstd_decode(blob: &[u8]) -> io::Result<Vec<u8>> {
    zstd::stream::decode_all(std::io::Cursor::new(blob))
        .map_err(|e| db_err(format!("zstd decode turn: {e}")))
}

/// The 1 → 2 schema migration: re-encode every `session_turns` value from raw
/// MessagePack (the v1 codec) to zstd-compressed MessagePack (the v2 codec).
/// Compression is codec-orthogonal to serialization, so a legacy raw blob is
/// re-encoded by simply wrapping the SAME MessagePack bytes in a zstd frame —
/// no deserialize/re-serialize of the `Turn` is needed.
///
/// Idempotency (required by the migration framework, whose crash recovery may
/// re-run this after the stamp was never committed): an already-compressed row
/// is recognized by [`ZSTD_FRAME_MAGIC`] and left untouched, so re-running
/// cannot double-compress a row.
///
/// Runs the rewrite in exactly one redb write transaction (the atomic-change
/// contract: a crash mid-rewrite leaves the pre-migration state intact).
fn migrate_turn_values_to_zstd(db: &redb::Database) -> io::Result<()> {
    info!("applying 1→2 migration: re-encoding session_turns values with zstd");
    // Read every value and compute the compressed replacement first (we cannot
    // mutate the table while iterating it under redb's borrow rules, and
    // doing the CPU-bound compression without holding the write lock keeps
    // the write transaction minimal).
    let mut rewrites: Vec<((u64, u32), Vec<u8>)> = Vec::new();
    let mut skipped = 0usize;
    {
        let read_txn = db
            .begin_read()
            .map_err(|e| db_err(format!("redb read txn: {e}")))?;
        let table = match read_txn.open_table(SESSION_TURNS) {
            // A fresh database with no turns has no table yet — nothing to migrate.
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(()),
            Ok(t) => t,
            Err(e) => return Err(db_err(format!("redb open turns (migration): {e}"))),
        };
        let iter = table
            .iter()
            .map_err(|e| db_err(format!("redb iter turns (migration): {e}")))?;
        for result in iter {
            let (key, value) =
                result.map_err(|e| db_err(format!("redb iter item (migration): {e}")))?;
            let (sid, idx) = key.value();
            let bytes = value.value();
            // Already a zstd frame ⇒ this row survived an earlier (stamp-less)
            // run of this same migration; leave it byte-for-byte intact.
            if bytes.starts_with(&ZSTD_FRAME_MAGIC) {
                debug!(
                    session_id = sid,
                    turn_id = idx,
                    "turn already zstd-compressed; skipping"
                );
                skipped += 1;
                continue;
            }
            let compressed = zstd_encode(bytes)?;
            rewrites.push(((sid, idx), compressed));
        }
    }
    if rewrites.is_empty() {
        info!(skipped, "no raw session_turns values to re-encode");
        return Ok(());
    }
    let write_txn = db
        .begin_write()
        .map_err(|e| db_err(format!("redb write txn (migration): {e}")))?;
    {
        let mut table = write_txn
            .open_table(SESSION_TURNS)
            .map_err(|e| db_err(format!("redb open turns (migration): {e}")))?;
        for (key, compressed) in &rewrites {
            table
                .insert(*key, compressed.as_slice())
                .map_err(|e| db_err(format!("redb insert turn (migration): {e}")))?;
        }
    }
    write_txn
        .commit()
        .map_err(|e| db_err(format!("redb commit turn migration: {e}")))?;
    info!(
        encoded = rewrites.len(),
        skipped, "re-encoded session_turns values with zstd"
    );
    Ok(())
}

pub fn write_turn(
    db: &redb::Database,
    session_id: u64,
    turn_id: u32,
    turn: &Turn,
) -> io::Result<()> {
    let payload =
        rmp_serde::to_vec_named(turn).map_err(|e| db_err(format!("codec encode turn: {e}")))?;
    // Serialize first, then compress the whole blob (see [`zstd_encode`]).
    let compressed = zstd_encode(&payload)?;
    let write_txn = db
        .begin_write()
        .map_err(|e| db_err(format!("redb write txn: {e}")))?;
    {
        let mut table = write_txn
            .open_table(SESSION_TURNS)
            .map_err(|e| db_err(format!("redb open turns: {e}")))?;
        table
            .insert((session_id, turn_id), compressed.as_slice())
            .map_err(|e| db_err(format!("redb insert turn: {e}")))?;
    }
    write_txn
        .commit()
        .map_err(|e| db_err(format!("redb commit turn: {e}")))?;
    Ok(())
}

pub fn read_turns(db: &redb::Database, session_id: u64) -> io::Result<Vec<(u32, Turn)>> {
    let read_txn = db
        .begin_read()
        .map_err(|e| db_err(format!("redb read txn: {e}")))?;
    let table = read_txn
        .open_table(SESSION_TURNS)
        .map_err(|e| db_err(format!("redb open turns: {e}")))?;
    let mut turns: Vec<(u32, Turn)> = Vec::new();
    for result in table
        .iter()
        .map_err(|e| db_err(format!("redb iter turns: {e}")))?
    {
        let (key, value) = result.map_err(|e| db_err(format!("redb iter item: {e}")))?;
        let (sid, idx) = key.value();
        if sid == session_id {
            match zstd_decode(value.value())
                .and_then(|buf| rmp_serde::from_slice::<Turn>(&buf).map_err(io::Error::other))
            {
                Ok(turn) => turns.push((idx, turn)),
                Err(e) => {
                    tracing::warn!(session_id, turn_id = idx, error = %e, "undecodable turn, skipping");
                }
            }
        }
    }
    turns.sort_by_key(|(idx, _)| *idx);
    Ok(turns)
}

/// Retry a write_turn on transient storage errors (e.g. I/O contention)
/// with up to 3 retries and a 1ms backoff.
pub fn write_turn_retry(
    db: &redb::Database,
    session_id: u64,
    turn_id: u32,
    turn: &Turn,
) -> io::Result<()> {
    let mut attempts = 0;
    loop {
        match write_turn(db, session_id, turn_id, turn) {
            Ok(()) => return Ok(()),
            Err(_e) if attempts < 3 => {
                attempts += 1;
                std::thread::sleep(std::time::Duration::from_millis(1));
                continue;
            }
            Err(e) => return Err(e),
        }
    }
}

pub fn delete_session_turns(db: &redb::Database, session_id: u64) -> io::Result<()> {
    let write_txn = db
        .begin_write()
        .map_err(|e| db_err(format!("redb write txn: {e}")))?;
    {
        let mut table = write_txn
            .open_table(SESSION_TURNS)
            .map_err(|e| db_err(format!("redb open turns: {e}")))?;
        let keys_to_remove: Vec<(u64, u32)> = table
            .iter()
            .map_err(|e| db_err(format!("redb iter turns: {e}")))?
            .filter_map(|result| match result {
                Ok((key, _)) => {
                    if key.value().0 == session_id {
                        Some(key.value())
                    } else {
                        None
                    }
                }
                Err(e) => {
                    warn!("undecodable turn entry in session {session_id}: {e}");
                    None
                }
            })
            .collect();
        for key in keys_to_remove {
            table
                .remove(key)
                .map_err(|e| db_err(format!("redb remove turn: {e}")))?;
        }
    }
    write_txn
        .commit()
        .map_err(|e| db_err(format!("redb commit delete turns: {e}")))?;
    Ok(())
}

pub fn delete_session_turns_retry(db: &redb::Database, session_id: u64) -> io::Result<()> {
    let mut attempts = 0;
    loop {
        match delete_session_turns(db, session_id) {
            Ok(()) => return Ok(()),
            Err(_e) if attempts < 3 => {
                attempts += 1;
                std::thread::sleep(std::time::Duration::from_millis(1));
                continue;
            }
            Err(e) => return Err(e),
        }
    }
}

// ── Credential table ────────────────────────────────────────────────────────────

pub fn set_credential_blob(
    db: &redb::Database,
    service: &str,
    blob: &[u8],
) -> Result<(), redb::Error> {
    let write_txn = db.begin_write()?;
    {
        let mut table = write_txn.open_table(CREDENTIALS)?;
        table.insert(service, blob)?;
    }
    write_txn.commit()?;
    Ok(())
}

pub fn get_all_credential_blobs(
    db: &redb::Database,
) -> Result<HashMap<String, Vec<u8>>, redb::Error> {
    let read_txn = db.begin_read()?;
    // The credentials table may not exist yet (no credentials have ever been
    // saved).  Return an empty map instead of propagating the error so that
    // unlock can proceed without credentials.
    let table = match read_txn.open_table(CREDENTIALS) {
        Ok(table) => table,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(HashMap::new()),
        Err(e) => return Err(e.into()),
    };
    let mut map = HashMap::new();
    for result in table.iter()? {
        let (key, value) = result?;
        map.insert(key.value().to_string(), value.value().to_vec());
    }
    Ok(map)
}

pub fn remove_credential_blob(db: &redb::Database, service: &str) -> Result<(), redb::Error> {
    let write_txn = db.begin_write()?;
    {
        let mut table = write_txn.open_table(CREDENTIALS)?;
        table.remove(service)?;
    }
    write_txn.commit()?;
    Ok(())
}

// ── Catalog state table ───────────────────────────────────────────────────────

/// Read a raw value out of the `catalog_state` table.  `Ok(None)` when the
/// table does not exist yet (it is created lazily on the first write) or the
/// key is absent — the shared existence-tolerant read both getters use, so
/// the redb boilerplate lives in one place.
fn catalog_state_get(db: &redb::Database, key: &str) -> io::Result<Option<Vec<u8>>> {
    let read_txn = db
        .begin_read()
        .map_err(|e| db_err(format!("redb read txn: {e}")))?;
    let table = match read_txn.open_table(CATALOG_STATE) {
        Ok(table) => table,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
        Err(e) => return Err(db_err(format!("redb open catalog_state: {e}"))),
    };
    match table
        .get(key)
        .map_err(|e| db_err(format!("redb get catalog_state {key}: {e}")))?
    {
        Some(guard) => Ok(Some(guard.value().to_vec())),
        None => Ok(None),
    }
}

/// Set or clear one `catalog_state` key in a single write transaction.
/// `Some` inserts the value (creating the table on the first write); `None`
/// removes the key.  Both writers (the maintenance thread's attempt
/// timestamp, the command loop's etag) go through this.
fn catalog_state_write(db: &redb::Database, key: &str, value: Option<&[u8]>) -> io::Result<()> {
    let write_txn = db
        .begin_write()
        .map_err(|e| db_err(format!("redb write txn: {e}")))?;
    {
        let mut table = write_txn
            .open_table(CATALOG_STATE)
            .map_err(|e| db_err(format!("redb open catalog_state: {e}")))?;
        match value {
            Some(value) => {
                table
                    .insert(key, value)
                    .map_err(|e| db_err(format!("redb set catalog_state {key}: {e}")))?;
            }
            None => {
                table
                    .remove(key)
                    .map_err(|e| db_err(format!("redb remove catalog_state {key}: {e}")))?;
            }
        }
    }
    write_txn
        .commit()
        .map_err(|e| db_err(format!("redb commit catalog_state {key}: {e}")))?;
    Ok(())
}

/// Record the wall-clock time (Unix epoch millis) at which a models.dev fetch
/// attempt STARTED. The S4 pacing anchor: every attempt (startup refresh,
/// timer revalidation, `/refresh-models`, a coalesced burst — always one
/// write) goes through this, and the outcome (200/304/failure) is irrelevant
/// to the recorded value — the 25h no-reattempt rule is anchored on "when we
/// last tried", not "when we last succeeded".
pub fn set_catalog_last_attempt_ms(db: &redb::Database, ms: u64) -> io::Result<()> {
    catalog_state_write(db, CATALOG_LAST_ATTEMPT_KEY, Some(&ms.to_le_bytes()))
}

/// Read the recorded catalog fetch-attempt timestamp. `None` when no attempt
/// has ever been recorded (first run, or an upgrade from a build without the
/// key) — callers treat that as "stale, fetch now". A stored value with an
/// unexpected length is logged and treated as absent, the same policy as
/// undecodable session records: the timestamp is advisory pacing, so a corrupt
/// value must never fail the caller (or the daemon).
pub fn get_catalog_last_attempt_ms(db: &redb::Database) -> io::Result<Option<u64>> {
    let Some(bytes) = catalog_state_get(db, CATALOG_LAST_ATTEMPT_KEY)? else {
        return Ok(None);
    };
    match <[u8; 8]>::try_from(bytes.as_slice()) {
        Ok(bytes) => Ok(Some(u64::from_le_bytes(bytes))),
        Err(_) => {
            warn!("catalog last_attempt_ms has an invalid length; treating as absent");
            Ok(None)
        }
    }
}

/// Store or clear the models.dev etag. `Some` inserts the raw entity-tag
/// (replacing any previous value); `None` removes the key — a fetch that came
/// back without an etag must not leave a stale one behind (it would be served
/// as `If-None-Match` forever).
pub fn set_catalog_etag(db: &redb::Database, etag: Option<&str>) -> io::Result<()> {
    catalog_state_write(db, CATALOG_ETAG_KEY, etag.map(str::as_bytes))
}

/// Read the stored models.dev etag. `None` when absent or blank (an empty
/// stored value is treated as absent — it could never be a valid entity-tag).
pub fn get_catalog_etag(db: &redb::Database) -> io::Result<Option<String>> {
    let Some(bytes) = catalog_state_get(db, CATALOG_ETAG_KEY)? else {
        return Ok(None);
    };
    let trimmed = String::from_utf8_lossy(&bytes).trim().to_string();
    if trimmed.is_empty() {
        Ok(None)
    } else {
        Ok(Some(trimmed))
    }
}

// ── Session KV table ───────────────────────────────────────────────────────────

/// Insert or overwrite a key-value pair for the given session.
pub fn kv_set(db: &redb::Database, session_id: u64, key: &str, value: &[u8]) -> io::Result<()> {
    let write_txn = db
        .begin_write()
        .map_err(|e| db_err(format!("redb write txn: {e}")))?;
    {
        let mut table = write_txn
            .open_table(SESSION_KV)
            .map_err(|e| db_err(format!("redb open session_kv: {e}")))?;
        table
            .insert((session_id, key.to_string()), value.to_vec())
            .map_err(|e| db_err(format!("redb kv_set: {e}")))?;
    }
    write_txn
        .commit()
        .map_err(|e| db_err(format!("redb commit kv_set: {e}")))?;
    debug!("kv_set: session={} key=\"{}\" ok", session_id, key);
    Ok(())
}

/// Retrieve a value by session and key. Returns `None` if the key does not exist.
pub fn kv_get(db: &redb::Database, session_id: u64, key: &str) -> io::Result<Option<Vec<u8>>> {
    let read_txn = db
        .begin_read()
        .map_err(|e| db_err(format!("redb read txn: {e}")))?;
    let table = read_txn
        .open_table(SESSION_KV)
        .map_err(|e| db_err(format!("redb open session_kv: {e}")))?;
    match table
        .get((session_id, key.to_string()))
        .map_err(|e| db_err(format!("redb kv_get: {e}")))?
    {
        Some(guard) => Ok(Some(guard.value().to_vec())),
        None => Ok(None),
    }
}

/// Remove a single key. Returns `true` if the key existed, `false` otherwise.
pub fn kv_delete(db: &redb::Database, session_id: u64, key: &str) -> io::Result<bool> {
    let write_txn = db
        .begin_write()
        .map_err(|e| db_err(format!("redb write txn: {e}")))?;
    let removed = {
        let mut table = write_txn
            .open_table(SESSION_KV)
            .map_err(|e| db_err(format!("redb open session_kv: {e}")))?;
        table
            .remove((session_id, key.to_string()))
            .map_err(|e| db_err(format!("redb kv_delete: {e}")))?
            .is_some()
    };
    write_txn
        .commit()
        .map_err(|e| db_err(format!("redb commit kv_delete: {e}")))?;
    debug!(
        "kv_delete: session={} key=\"{}\" found={}",
        session_id, key, removed
    );
    Ok(removed)
}

/// Remove all keys in the range [`start`, `end`) for the given session.
///
/// If `end` is `None`, removes from `start` to the end of the session's keys.
/// Returns the number of keys removed.
pub fn kv_delete_range(
    db: &redb::Database,
    session_id: u64,
    start: &str,
    end: Option<&str>,
) -> io::Result<u64> {
    let write_txn = db
        .begin_write()
        .map_err(|e| db_err(format!("redb write txn: {e}")))?;
    let count = {
        let mut table = write_txn
            .open_table(SESSION_KV)
            .map_err(|e| db_err(format!("redb open session_kv: {e}")))?;
        let range = match end {
            Some(end) => {
                let range_start = (session_id, start.to_string());
                let range_end = (session_id, end.to_string());
                table
                    .range::<(u64, String)>((range_start)..(range_end))
                    .map_err(|e| db_err(format!("redb range kv_delete_range: {e}")))?
            }
            None => {
                let range_start = (session_id, start.to_string());
                let range_end = (session_range_end(session_id), String::new());
                table
                    .range::<(u64, String)>((range_start)..(range_end))
                    .map_err(|e| db_err(format!("redb range kv_delete_range: {e}")))?
            }
        };
        let keys: Vec<(u64, String)> = range
            .filter_map(|r| r.ok())
            .map(|(k, _)| k.value())
            .collect();
        let count = keys.len() as u64;
        for key in keys {
            table
                .remove(key)
                .map_err(|e| db_err(format!("redb kv_delete_range remove: {e}")))?;
        }
        count
    };
    write_txn
        .commit()
        .map_err(|e| db_err(format!("redb commit kv_delete_range: {e}")))?;
    debug!(
        "kv_delete_range: session={} start=\"{}\" end={:?} removed={}",
        session_id, start, end, count
    );
    Ok(count)
}

/// Retrieve all key-value pairs in the range [`start`, `end`) for the given session.
///
/// If `end` is `None`, retrieves from `start` to the end of the session's keys.
pub fn kv_get_range(
    db: &redb::Database,
    session_id: u64,
    start: &str,
    end: Option<&str>,
) -> io::Result<Vec<(String, Vec<u8>)>> {
    let read_txn = db
        .begin_read()
        .map_err(|e| db_err(format!("redb read txn: {e}")))?;
    let table = read_txn
        .open_table(SESSION_KV)
        .map_err(|e| db_err(format!("redb open session_kv: {e}")))?;
    let range = match end {
        Some(end) => {
            let range_start = (session_id, start.to_string());
            let range_end = (session_id, end.to_string());
            table
                .range::<(u64, String)>((range_start)..(range_end))
                .map_err(|e| db_err(format!("redb range kv_get_range: {e}")))?
        }
        None => {
            let range_start = (session_id, start.to_string());
            let range_end = (session_range_end(session_id), String::new());
            table
                .range::<(u64, String)>((range_start)..(range_end))
                .map_err(|e| db_err(format!("redb range kv_get_range: {e}")))?
        }
    };
    let mut results = Vec::new();
    for result in range {
        let (key, value) = result.map_err(|e| db_err(format!("redb iter kv_get_range: {e}")))?;
        results.push((key.value().1, value.value().to_vec()));
    }
    Ok(results)
}

/// List all keys in the range [`start`, `end`) for the given session.
///
/// Returns only key names (not values). If `start` is `None`, starts from
/// the beginning of the session's keys. If `end` is `None`, goes to the end.
pub fn kv_list(
    db: &redb::Database,
    session_id: u64,
    start: Option<&str>,
    end: Option<&str>,
) -> io::Result<Vec<String>> {
    let read_txn = db
        .begin_read()
        .map_err(|e| db_err(format!("redb read txn: {e}")))?;
    let table = read_txn
        .open_table(SESSION_KV)
        .map_err(|e| db_err(format!("redb open session_kv: {e}")))?;
    let range: KvRangeIter<'_> = match (start, end) {
        (Some(start), Some(end)) => {
            let range_start = (session_id, start.to_string());
            let range_end = (session_id, end.to_string());
            Box::new(
                table
                    .range::<(u64, String)>((range_start)..(range_end))
                    .map_err(|e| db_err(format!("redb range kv_list: {e}")))?,
            )
        }
        (Some(start), None) => {
            let range_start = (session_id, start.to_string());
            let range_end = (session_range_end(session_id), String::new());
            Box::new(
                table
                    .range::<(u64, String)>((range_start)..(range_end))
                    .map_err(|e| db_err(format!("redb range kv_list: {e}")))?,
            )
        }
        (None, Some(end)) => {
            let range_start = (session_id, String::new());
            let range_end = (session_id, end.to_string());
            Box::new(
                table
                    .range::<(u64, String)>((range_start)..(range_end))
                    .map_err(|e| db_err(format!("redb range kv_list: {e}")))?,
            )
        }
        (None, None) => {
            let range_start = (session_id, String::new());
            let range_end = (session_range_end(session_id), String::new());
            Box::new(
                table
                    .range::<(u64, String)>((range_start)..(range_end))
                    .map_err(|e| db_err(format!("redb range kv_list: {e}")))?,
            )
        }
    };
    let mut keys = Vec::new();
    for result in range {
        let (key, _) = result.map_err(|e| db_err(format!("redb iter kv_list: {e}")))?;
        keys.push(key.value().1);
    }
    Ok(keys)
}

/// Count keys in the given session, optionally filtered by prefix.
///
/// When `prefix` is `Some(p)`, counts keys in [`p`, `p` + max_char).
/// When `prefix` is `None`, counts all keys for the session.
pub fn kv_count(db: &redb::Database, session_id: u64, prefix: Option<&str>) -> io::Result<u64> {
    let read_txn = db
        .begin_read()
        .map_err(|e| db_err(format!("redb read txn: {e}")))?;
    let table = read_txn
        .open_table(SESSION_KV)
        .map_err(|e| db_err(format!("redb open session_kv: {e}")))?;
    let range = match prefix {
        Some(prefix) => {
            let range_start = (session_id, prefix.to_string());
            // We need an upper bound for the prefix scan.  Appending 0xFF and feeding
            // the result through String::from_utf8_lossy replaces the 0xFF with the
            // Unicode replacement character U+FFFD (UTF-8: EF BF BD), so the actual
            // end bound is prefix + "\u{FFFD}".  Every valid UTF-8 key that shares the
            // prefix has a byte sequence strictly less than EF BF BD at the first
            // differing position, so this bound correctly terminates the range — the
            // bound value itself is never returned, only used for range termination.
            let mut end_bytes = prefix.as_bytes().to_vec();
            end_bytes.push(0xFF);
            let range_end_str = String::from_utf8_lossy(&end_bytes).into_owned();
            let range_end = (session_id, range_end_str);
            table
                .range::<(u64, String)>((range_start)..(range_end))
                .map_err(|e| db_err(format!("redb range kv_count: {e}")))?
        }
        None => {
            let range_start = (session_id, String::new());
            let range_end = (session_range_end(session_id), String::new());
            table
                .range::<(u64, String)>((range_start)..(range_end))
                .map_err(|e| db_err(format!("redb range kv_count: {e}")))?
        }
    };
    let mut count: u64 = 0;
    for result in range {
        result.map_err(|e| db_err(format!("redb iter kv_count: {e}")))?;
        count += 1;
    }
    Ok(count)
}

/// Retry a write_session on transient storage errors with up to 3 retries.
pub fn write_session_retry(
    db: &redb::Database,
    session_id: u64,
    record: &SessionRecord,
) -> io::Result<()> {
    let mut attempts = 0;
    loop {
        match write_session(db, session_id, record) {
            Ok(()) => return Ok(()),
            Err(_e) if attempts < 3 => {
                attempts += 1;
                std::thread::sleep(std::time::Duration::from_millis(1));
                continue;
            }
            Err(e) => return Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use choreo_proto::Turn;

    /// Read the current `next_session_id` from the DB and atomically
    /// increment it.  Only used by tests — production code derives the
    /// next ID from max(existing keys) + 1 at startup.
    fn next_session_id(db: &redb::Database) -> io::Result<u64> {
        let write_txn = db
            .begin_write()
            .map_err(|e| db_err(format!("redb write txn: {e}")))?;
        let current = {
            let mut table = write_txn
                .open_table(META)
                .map_err(|e| db_err(format!("redb open meta: {e}")))?;
            let current = table
                .get("next_session_id")
                .map_err(|e| db_err(format!("redb get meta: {e}")))?
                .map(|g| g.value())
                .unwrap_or(1);
            table
                .insert("next_session_id", current.wrapping_add(1))
                .map_err(|e| db_err(format!("redb set meta: {e}")))?;
            current
        };
        write_txn
            .commit()
            .map_err(|e| db_err(format!("redb commit meta: {e}")))?;
        Ok(current)
    }

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
    fn round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let db = redb::Database::create(dir.path().join("test.redb")).unwrap();

        let id = next_session_id(&db).unwrap();
        assert_eq!(id, 1);

        let record = SessionRecord {
            title: Some("test session".into()),
            selected_model: Some("gpt-4".into()),
            reasoning_effort: None,
            parent_session_id: None,
            working_dir: Some("/tmp".into()),
            turn_count: 1,
            created_at: 1234567890000,
            last_modified: 1234567890000,
            active_tool_groups: vec!["core".into(), "git".into()],
            context_config: ContextConfig::default(),
            account_name: None,
            last_response_id: None,
            last_response_id_producer: None,
        };
        write_session(&db, id, &record).unwrap();

        let read = read_session(&db, id).unwrap().unwrap();
        assert_eq!(read.title, record.title);
        assert_eq!(read.turn_count, record.turn_count);

        let all = read_all_sessions(&db).unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].0, id);

        let turn = dummy_turn();
        write_turn(&db, id, 0, &turn).unwrap();

        // The v2 codec stores the turn as a zstd-compressed MessagePack frame,
        // not raw MessagePack — the whole point of the schema-2 change.
        {
            let read_txn = db.begin_read().unwrap();
            let table = read_txn.open_table(SESSION_TURNS).unwrap();
            let guard = table.get((id, 0u32)).unwrap().unwrap();
            let v = guard.value();
            assert!(
                v.starts_with(&ZSTD_FRAME_MAGIC),
                "turns must be stored as zstd frames in schema 2"
            );
        }

        let turns = read_turns(&db, id).unwrap();
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].1, turn);

        let id2 = next_session_id(&db).unwrap();
        assert_eq!(id2, 2);

        delete_session(&db, id).unwrap();
        assert!(read_session(&db, id).unwrap().is_none());
        assert!(read_turns(&db, id).unwrap().is_empty());

        drop(db);
    }

    #[test]
    fn session_record_last_response_id_round_trips() {
        // Phase 4c persistence: the response id written to the record must
        // survive a write/read cycle so ResponseId-policy models chain across
        // user turns even after a daemon restart.
        let dir = tempfile::tempdir().unwrap();
        let db = redb::Database::create(dir.path().join("test.redb")).unwrap();
        let id = 1u64;
        let record = SessionRecord {
            title: Some("t".into()),
            selected_model: None,
            reasoning_effort: None,
            parent_session_id: None,
            working_dir: None,
            turn_count: 0,
            created_at: 1,
            last_modified: 1,
            active_tool_groups: vec![],
            context_config: ContextConfig::default(),
            account_name: None,
            last_response_id: Some("resp_1".into()),
            last_response_id_producer: Some(ReasoningProducer {
                provider_slug: "openai".into(),
                model: "gpt-5.4".into(),
            }),
        };
        write_session(&db, id, &record).unwrap();

        let read = read_session(&db, id).unwrap().unwrap();
        assert_eq!(read.last_response_id.as_deref(), Some("resp_1"));
        assert_eq!(
            read.last_response_id_producer
                .as_ref()
                .map(|p| p.model.as_str()),
            Some("gpt-5.4"),
            "response id provenance must survive the write/read cycle",
        );
        assert_eq!(read.title.as_deref(), Some("t"));
    }

    #[test]
    fn read_turns_skips_corrupt_entries() {
        let dir = tempfile::tempdir().unwrap();
        let db = redb::Database::create(dir.path().join("test.redb")).unwrap();
        let id = 1u64;

        // Write a valid turn at index 0
        let valid_turn = dummy_turn();
        write_turn(&db, id, 0, &valid_turn).unwrap();

        // Manually insert a corrupt blob at index 1 (neither a valid zstd
        // frame nor valid MessagePack) to exercise the skip-and-warn path.
        {
            let write_txn = db.begin_write().unwrap();
            {
                let mut table = write_txn.open_table(SESSION_TURNS).unwrap();
                table
                    .insert((id, 1u32), b"not a zstd frame".as_slice())
                    .unwrap();
            }
            write_txn.commit().unwrap();
        }

        // Write another valid turn at index 2
        let valid_turn2 = dummy_turn();
        write_turn(&db, id, 2, &valid_turn2).unwrap();

        // read_turns should skip the corrupt entry
        let turns = read_turns(&db, id).unwrap();
        assert_eq!(turns.len(), 2, "corrupt turn should be skipped");
        assert_eq!(turns[0].1, valid_turn);
        assert_eq!(turns[1].1, valid_turn2);
    }

    #[test]
    fn read_session_skips_corrupt_record_with_warning() {
        // A corrupt/legacy session record must not fail the read (or the
        // daemon): read_session treats undecodable data as absent — warn and
        // return None — the same policy as the batch reads.
        let dir = tempfile::tempdir().unwrap();
        let db = redb::Database::create(dir.path().join("test.redb")).unwrap();
        {
            let write_txn = db.begin_write().unwrap();
            {
                let mut table = write_txn.open_table(SESSIONS).unwrap();
                table
                    .insert(42u64, b"not a session record".as_slice())
                    .unwrap();
            }
            write_txn.commit().unwrap();
        }
        assert!(
            read_session(&db, 42).unwrap().is_none(),
            "undecodable record must read as absent, not error"
        );
        // A genuinely missing session is indistinguishable (also None).
        assert!(read_session(&db, 99).unwrap().is_none());
    }

    #[test]
    fn purge_removes_tombstoned_resurrected_record() {
        // Simulates the crash window: a session is deleted (tombstone
        // written), its still-shutting-down thread re-creates the record, and
        // the daemon dies before the delete is finalized.  The startup purge
        // must remove the record so the deleted session cannot resurface.
        let dir = tempfile::tempdir().unwrap();
        let db = redb::Database::create(dir.path().join("test.redb")).unwrap();
        let record = SessionRecord {
            title: Some("ghost".into()),
            selected_model: None,
            reasoning_effort: None,
            parent_session_id: None,
            working_dir: None,
            turn_count: 0,
            created_at: 1000,
            last_modified: 1000,
            active_tool_groups: vec![],
            context_config: ContextConfig::default(),
            account_name: None,
            last_response_id: None,
            last_response_id_producer: None,
        };

        write_session(&db, 5, &record).unwrap();
        mark_session_deleted(&db, 5).unwrap();
        // The still-shutting-down thread re-creates the record after the delete…
        write_session(&db, 5, &record).unwrap();

        let purged = purge_tombstoned_sessions(&db).unwrap();
        assert_eq!(purged, 1, "the resurrected record must be purged");
        assert!(
            read_session(&db, 5).unwrap().is_none(),
            "tombstoned session must not survive the purge"
        );
        // Purge is idempotent: the tombstone was cleared, so a second run
        // has nothing to do.
        assert_eq!(purge_tombstoned_sessions(&db).unwrap(), 0);
    }

    #[test]
    fn clear_tombstone_prevents_purge_of_live_record() {
        // A tombstone that is cleared (the exit finalize finished) must not
        // cause a still-valid record to be purged.
        let dir = tempfile::tempdir().unwrap();
        let db = redb::Database::create(dir.path().join("test.redb")).unwrap();
        let record = SessionRecord {
            title: Some("live".into()),
            selected_model: None,
            reasoning_effort: None,
            parent_session_id: None,
            working_dir: None,
            turn_count: 0,
            created_at: 1000,
            last_modified: 1000,
            active_tool_groups: vec![],
            context_config: ContextConfig::default(),
            account_name: None,
            last_response_id: None,
            last_response_id_producer: None,
        };
        write_session(&db, 6, &record).unwrap();
        mark_session_deleted(&db, 6).unwrap();
        clear_session_tombstone(&db, 6).unwrap();

        let purged = purge_tombstoned_sessions(&db).unwrap();
        assert_eq!(purged, 0);
        assert!(read_session(&db, 6).unwrap().is_some());
    }

    #[test]
    fn purge_empty_database_is_zero() {
        let dir = tempfile::tempdir().unwrap();
        let db = redb::Database::create(dir.path().join("test.redb")).unwrap();
        assert_eq!(purge_tombstoned_sessions(&db).unwrap(), 0);
    }

    #[test]
    fn catalog_last_attempt_ms_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let db = redb::Database::create(dir.path().join("test.redb")).unwrap();

        // A fresh database has no catalog_state table yet → None (the caller
        // treats that as "stale, fetch now").
        assert_eq!(get_catalog_last_attempt_ms(&db).unwrap(), None);

        set_catalog_last_attempt_ms(&db, 1_700_000_123_456).unwrap();
        assert_eq!(
            get_catalog_last_attempt_ms(&db).unwrap(),
            Some(1_700_000_123_456)
        );

        // Overwrite: a later attempt replaces the earlier one (one attempt
        // timestamp, always the most recent).
        set_catalog_last_attempt_ms(&db, 1_700_000_500_000).unwrap();
        assert_eq!(
            get_catalog_last_attempt_ms(&db).unwrap(),
            Some(1_700_000_500_000)
        );
    }

    #[test]
    fn catalog_last_attempt_ms_corrupt_length_treated_as_absent() {
        // A stored value with the wrong length (e.g. an interrupted/foreign
        // write) must be treated as absent with a warning, never an error —
        // the timestamp is advisory pacing and a corrupt value must not fail
        // the daemon.
        let dir = tempfile::tempdir().unwrap();
        let db = redb::Database::create(dir.path().join("test.redb")).unwrap();
        {
            let write_txn = db.begin_write().unwrap();
            {
                let mut table = write_txn.open_table(CATALOG_STATE).unwrap();
                table
                    .insert(CATALOG_LAST_ATTEMPT_KEY, b"too short".as_slice())
                    .unwrap();
            }
            write_txn.commit().unwrap();
        }
        assert_eq!(get_catalog_last_attempt_ms(&db).unwrap(), None);
    }

    #[test]
    fn catalog_etag_round_trips_and_clears() {
        let dir = tempfile::tempdir().unwrap();
        let db = redb::Database::create(dir.path().join("test.redb")).unwrap();

        assert_eq!(get_catalog_etag(&db).unwrap(), None);

        set_catalog_etag(&db, Some("\"v1\"")).unwrap();
        assert_eq!(get_catalog_etag(&db).unwrap().as_deref(), Some("\"v1\""));

        // Replacing an etag stores the new one.
        set_catalog_etag(&db, Some("W/\"v2\"")).unwrap();
        assert_eq!(get_catalog_etag(&db).unwrap().as_deref(), Some("W/\"v2\""));

        // A fetch that returned no etag must clear the stored one, otherwise
        // the stale etag would be served as If-None-Match forever.
        set_catalog_etag(&db, None).unwrap();
        assert_eq!(get_catalog_etag(&db).unwrap(), None);
    }

    #[test]
    fn catalog_etag_blank_value_reads_as_absent() {
        let dir = tempfile::tempdir().unwrap();
        let db = redb::Database::create(dir.path().join("test.redb")).unwrap();
        {
            let write_txn = db.begin_write().unwrap();
            {
                let mut table = write_txn.open_table(CATALOG_STATE).unwrap();
                table.insert(CATALOG_ETAG_KEY, b"   ".as_slice()).unwrap();
            }
            write_txn.commit().unwrap();
        }
        // Blank (whitespace-only) reads as absent — it could never be a valid
        // entity-tag.
        assert_eq!(get_catalog_etag(&db).unwrap(), None);
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
        let db = redb::Database::create(dir.path().join("test.redb")).unwrap();
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
        let db = redb::Database::create(dir.path().join("test.redb")).unwrap();
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
        let db = redb::Database::create(dir.path().join("test.redb")).unwrap();
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
        let db = redb::Database::create(&db_path).unwrap();
        stamp_schema_version(&db, 1).unwrap(); // a v1 database, pre-migration

        run_migrations_to(&db, SCHEMA_VERSION, MIGRATIONS, &db_path).unwrap();

        assert!(
            db_path.with_file_name("state.redb.bak-v1").exists(),
            "the 1→2 migration must back up the source-version file"
        );
        assert_eq!(
            current_schema_version(&db).unwrap(),
            SCHEMA_VERSION,
            "the migration must reach the current schema version"
        );
    }

    /// A stand-in for a real future migration: records a marker in `meta` so a
    /// test can assert the migration actually ran.
    fn dummy_migrate_1_to_2(db: &redb::Database) -> io::Result<()> {
        let write_txn = db
            .begin_write()
            .map_err(|e| db_err(format!("redb write txn: {e}")))?;
        {
            let mut table = write_txn
                .open_table(META)
                .map_err(|e| db_err(format!("redb open meta: {e}")))?;
            table
                .insert("migrated", 1u64)
                .map_err(|e| db_err(format!("redb set migrated marker: {e}")))?;
        }
        write_txn
            .commit()
            .map_err(|e| db_err(format!("redb commit migrated marker: {e}")))?;
        Ok(())
    }

    #[test]
    fn run_migrations_applies_contiguous_chain_from_current_version() {
        // Simulates the first real migration landing (1 → 2): a database
        // stamped at v1 plus a dummy migration entry. Pins the runner's
        // indexing — the entry's explicit `from` field (not its position)
        // determines what runs.
        let dir = tempfile::tempdir().unwrap();
        let db = redb::Database::create(dir.path().join("test.redb")).unwrap();
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

        assert_eq!(current_schema_version(&db).unwrap(), 2);
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
    fn run_migrations_rejects_non_contiguous_chain_before_writing() {
        // The natural mistake a contributor would make: writing the first
        // migration with `from == 0` (thinking of the array index) when the
        // database is at v1. The runner must refuse loudly BEFORE writing
        // anything — stamping v2 over data that was never migrated would
        // corrupt every subsequent read.
        let dir = tempfile::tempdir().unwrap();
        let db = redb::Database::create(dir.path().join("test.redb")).unwrap();
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
        assert_eq!(current_schema_version(&db).unwrap(), 1);
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
    fn run_migrations_refuses_unversioned_db_when_target_above_initial() {
        // The flip side of the fresh-install fix: a database that is STILL
        // unversioned (current == 0) at startup never went through open_db's
        // creation-time initialization — it is a pre-existing file (pre-
        // release leftovers). Once the chain grows past the initial version
        // (target > 1) the runner must refuse it rather than stamp over data
        // that was never migrated. Fresh installs never hit this branch
        // because open_db stamps INITIAL_SCHEMA_VERSION at creation.
        let dir = tempfile::tempdir().unwrap();
        let db = redb::Database::create(dir.path().join("test.redb")).unwrap();

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
        assert_eq!(current_schema_version(&db).unwrap(), 0);
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
