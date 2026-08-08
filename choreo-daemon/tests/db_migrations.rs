//! Integration tests for the versioned DB migration framework: the full
//! `open_db` → `run_migrations` startup sequence against a real redb database
//! on the filesystem.
//!
//! These tests bind the system boundary (the `CHOREOGRAPHR_DB_PATH`
//! environment variable, the filesystem, and a real redb database), so per
//! AGENTS.md they belong in the `#[ignore]` suite:
//! `cargo nextest run -p choreo-daemon --run-ignored only`.

use choreographr::db;
use redb::ReadableDatabase;

/// Mirrors the daemon's production `meta` table definition so the test can
/// read the stamped schema version directly.
const META: redb::TableDefinition<&str, u64> = redb::TableDefinition::new("meta");

#[test]
#[ignore]
fn open_db_creates_migrates_and_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    let db_file = dir.path().join("state.redb");

    // Point db_path() resolution at the temp file so open_db creates and
    // migrates there instead of the real data directory. set_var is unsafe
    // in edition 2024; nextest runs each test in its own process, so the
    // mutation cannot leak across tests.
    unsafe {
        std::env::set_var("CHOREOGRAPHR_DB_PATH", &db_file);
    }

    // Create path: open_db creates the database file AND stamps the initial
    // schema version (INITIAL_SCHEMA_VERSION) at creation, so a fresh
    // database is versioned from the moment it exists. run_migrations then
    // brings it up to SCHEMA_VERSION — a no-op fast path today, but the
    // migrate-from-`current` path once the chain grows past 1. This mirrors
    // main.rs's startup sequence.
    let db = db::open_db().unwrap();

    // open_db itself must have stamped the initial version — a fresh file
    // must never be left "unversioned", or the first real migration
    // (target > 1) would refuse it as pre-release leftovers.
    {
        let read_txn = db.begin_read().unwrap();
        let table = read_txn.open_table(META).unwrap();
        let version = table
            .get("schema_version")
            .unwrap()
            .expect("open_db must stamp the initial schema version at creation")
            .value();
        assert_eq!(
            version,
            db::INITIAL_SCHEMA_VERSION,
            "open_db must initialize a fresh database to the initial schema version"
        );
    }

    // The startup sequence then brings the database to the current version.
    db::run_migrations(&db).unwrap();

    // The schema version must be at the current version after the sequence.
    {
        let read_txn = db.begin_read().unwrap();
        let table = read_txn.open_table(META).unwrap();
        let version = table
            .get("schema_version")
            .unwrap()
            .expect("schema_version must be stamped by the open_db → run_migrations sequence")
            .value();
        assert_eq!(
            version,
            db::SCHEMA_VERSION,
            "fresh database must be at the current schema version after startup"
        );
    }

    // Idempotent: a second run is a no-op fast path.
    db::run_migrations(&db).unwrap();

    // Empty-database startup invariants.
    assert_eq!(db::purge_tombstoned_sessions(&db).unwrap(), 0);
    assert!(db::read_all_sessions(&db).unwrap().is_empty());

    // A real write/read round-trip through the MessagePack codec.
    let record = db::SessionRecord {
        title: Some("integration".into()),
        selected_model: None,
        reasoning_effort: None,
        parent_session_id: None,
        working_dir: None,
        turn_count: 0,
        created_at: 1,
        last_modified: 1,
        active_tool_groups: vec![],
        context_config: Default::default(),
        account_name: None,
        last_response_id: None,
        last_response_id_producer: None,
    };
    db::write_session(&db, 7, &record).unwrap();
    let read = db::read_session(&db, 7).unwrap().unwrap();
    assert_eq!(read.title.as_deref(), Some("integration"));

    // With an empty migration chain the 0 → 1 transition is pure stamping:
    // no backup artifact may appear next to the database file (db_path()
    // resolves to db_file because of the env override, so this check is
    // meaningful).
    assert!(
        !db_file.with_file_name("state.redb.bak-v1").exists(),
        "no backup may be written while the migration chain is empty"
    );

    // Tidy: clear the override so this process falls back to the real data
    // directory (nextest process isolation makes this belt-and-braces).
    unsafe {
        std::env::remove_var("CHOREOGRAPHR_DB_PATH");
    }
}

#[test]
#[ignore]
fn open_db_recreates_zero_byte_corpse_and_initializes() {
    // A 0-byte `state.redb` is the corpse of an interrupted create (crash
    // between file creation and the first write): it holds no recoverable
    // data, so open_db must recreate it — and stamp the initial schema
    // version, exactly like a brand-new file.
    let dir = tempfile::tempdir().unwrap();
    let db_file = dir.path().join("state.redb");
    std::fs::write(&db_file, b"").unwrap();

    unsafe {
        std::env::set_var("CHOREOGRAPHR_DB_PATH", &db_file);
    }

    let db = db::open_db().unwrap();
    {
        let read_txn = db.begin_read().unwrap();
        let table = read_txn.open_table(META).unwrap();
        let version = table
            .get("schema_version")
            .unwrap()
            .expect("recreated database must be stamped with the initial schema version")
            .value();
        assert_eq!(version, db::INITIAL_SCHEMA_VERSION);
    }

    // The recreated database is a normal, usable database.
    db::run_migrations(&db).unwrap();
    assert!(db::read_all_sessions(&db).unwrap().is_empty());

    unsafe {
        std::env::remove_var("CHOREOGRAPHR_DB_PATH");
    }
}
