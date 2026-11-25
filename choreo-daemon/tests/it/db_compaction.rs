//! The startup compaction pass, end to end against a real store on disk.
//!
//! `redb` never returns freed space to the filesystem: deleting a row frees the
//! space for later writes but leaves the file at its high-water length. On a
//! small-row store most of that space sits INSIDE pages that are still
//! allocated, which `Balanced` compaction deliberately keeps for later inserts;
//! `Maximum` packs those pages. These tests pin the difference on the shape that
//! shows it, and the no-op property that makes an ungated pass affordable.
//!
//! They open real files in a tempdir, so per AGENTS.md they live here and are
//! `#[ignore = "integration"]`: `cargo nextest run -p choreo-daemon --run-ignored
//! only`.
//!
//! The pass is driven through `db::compact_at_startup_for_test`, the crate's
//! `test-utils` seam around the REAL startup function, rather than re-issuing
//! redb's `compact()` call here: a test that carried its own mode constant would
//! keep passing if the daemon switched back to `Balanced`.

use std::path::Path;

use choreo_daemon::db::{self, Store};
use redb::{ReadableDatabase, ReadableTable, ReadableTableMetadata};

/// Rows the fixture writes. 200-byte values put ~250 KiB of logical rows on the
/// disk once redb's per-row overhead is included: small enough that the fixture
/// and both passes finish well inside a second, large enough that halving the
/// rows frees whole pages worth reclaiming.
const ROWS: u64 = 1200;

/// Rows per fixture transaction.
///
/// Batching is load-bearing for what this suite measures: every commit is
/// copy-on-write, so one transaction covering all the work leaves the file at
/// several times its live size from allocator slack alone — and then even a
/// truncation pass (`Balanced`) reclaims as much as a packing pass, hiding the
/// difference under test. Small batches reuse the pages earlier commits freed,
/// which is the shape a long-running store has: after the deletes the file is
/// still the length the inserts gave it, so the only space left to reclaim is
/// real slack.
const BATCH: usize = 100;

/// The per-row value size of the shape this store is mostly made of — the
/// `sessions`, `session_kv` and `meta` rows (see the mode measurements in
/// `db::compact`).
const VALUE_LEN: usize = 200;

/// Mirrors the production `session_kv` table (see `db::schema`) so the fixture
/// fills the same pages, with the same key and value types, that a real
/// half-deleted store fills.
const SESSION_KV: redb::TableDefinition<(u64, String), Vec<u8>> =
    redb::TableDefinition::new("session_kv");

/// A store whose small-row delete has been done but not yet compacted must at
/// least halve, and keep every surviving row.
#[test]
#[ignore = "integration"]
fn maximum_reclaims_what_balanced_leaves_behind() {
    let dir = tempfile::tempdir().expect("tempdir for the store");
    let path = dir.path().join("state.redb");
    let mut store = half_deleted_small_row_store(&path).expect("build the half-deleted fixture");

    let before = file_len(&path).expect("read the store length before the pass");
    db::compact_at_startup_for_test(&mut store, &path).expect("the startup pass must succeed");
    let after = file_len(&path).expect("read the store length after the pass");

    println!(
        "half-deleted {VALUE_LEN}-byte-row store: {before} -> {after} bytes ({} bytes reclaimed)",
        before.saturating_sub(after)
    );
    assert!(
        after * 2 <= before,
        "the pass must at least halve a half-deleted small-row store: {before} -> {after} bytes"
    );

    // The space came back without losing a row: half the keys survive, one
    // deleted key stays gone, and one survivor still reads its value.
    let read_txn = store.begin_read().expect("begin a read transaction");
    let table = read_txn
        .open_table(SESSION_KV)
        .expect("open the fixture table after the pass");
    assert_eq!(
        table.len().expect("count the surviving rows"),
        ROWS / 2,
        "the pass must not add or drop rows"
    );
    assert!(
        table
            .get((0, row_key(0)))
            .expect("look up a deleted row")
            .is_none(),
        "a row deleted before the pass must stay deleted"
    );
    assert_eq!(
        table
            .get((0, row_key(1)))
            .expect("look up a surviving row")
            .expect("row 1 must survive the pass")
            .value(),
        vec![0x5a_u8; VALUE_LEN],
        "a surviving row must keep its value byte for byte"
    );
}

/// A pass on an already-settled store finds nothing to do: the second run leaves
/// the file's length untouched. That is what makes running the pass at every
/// start affordable.
#[test]
#[ignore = "integration"]
fn a_settled_store_makes_the_pass_a_no_op() {
    let dir = tempfile::tempdir().expect("tempdir for the store");
    let path = dir.path().join("state.redb");
    let mut store = half_deleted_small_row_store(&path).expect("build the half-deleted fixture");

    db::compact_at_startup_for_test(&mut store, &path).expect("the first pass must succeed");
    let settled = file_len(&path).expect("read the store length after the first pass");

    db::compact_at_startup_for_test(&mut store, &path).expect("the second pass must succeed");
    let after = file_len(&path).expect("read the store length after the second pass");

    println!("settled store: {settled} bytes; the second pass left {after}");
    assert_eq!(
        after, settled,
        "a settled store's pass must report no work and leave the file's length unchanged"
    );
}

/// The fixture both tests measure: a store at `path` holding [`ROWS`] small rows
/// with every other one deleted — the shape a user leaves behind when they clear
/// half their history.
///
/// Returns a `String` error rather than panicking: a plain helper fn in an
/// integration-test module is not covered by clippy's `allow-*-in-tests` config
/// (that covers `#[test]` bodies and `#[cfg(test)]` modules), so the panic-family
/// calls stay in the test bodies, next to the data that produced them.
fn half_deleted_small_row_store(path: &Path) -> Result<Store, String> {
    let store = Store::open_at(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    let value = vec![0x5a_u8; VALUE_LEN];

    for chunk in (0..ROWS).collect::<Vec<u64>>().chunks(BATCH) {
        let insert = store
            .begin_write()
            .map_err(|e| format!("begin the insert transaction: {e}"))?;
        {
            let mut table = insert
                .open_table(SESSION_KV)
                .map_err(|e| format!("open session_kv to insert: {e}"))?;
            for &row in chunk {
                table
                    .insert((0, row_key(row)), value.clone())
                    .map_err(|e| format!("insert row {row}: {e}"))?;
            }
        }
        insert
            .commit()
            .map_err(|e| format!("commit the inserts: {e}"))?;
    }

    // Every other row, so the freed space is spread across every page the
    // deleted rows occupied — the interior-slack shape the mode difference
    // shows up on.
    let every_other: Vec<u64> = (0..ROWS).step_by(2).collect();
    for chunk in every_other.chunks(BATCH) {
        let delete = store
            .begin_write()
            .map_err(|e| format!("begin the delete transaction: {e}"))?;
        {
            let mut table = delete
                .open_table(SESSION_KV)
                .map_err(|e| format!("open session_kv to delete: {e}"))?;
            for &row in chunk {
                table
                    .remove((0, row_key(row)))
                    .map_err(|e| format!("remove row {row}: {e}"))?;
            }
        }
        delete
            .commit()
            .map_err(|e| format!("commit the deletes: {e}"))?;
    }

    Ok(store)
}

/// The fixture's key for `row`, shared by the insert and the delete so the two
/// cannot drift into deleting keys that were never written.
fn row_key(row: u64) -> String {
    format!("key-{row:06}")
}

/// The store file's length in bytes, or `None` when it cannot be read.
fn file_len(path: &Path) -> Option<u64> {
    std::fs::metadata(path).ok().map(|m| m.len())
}
