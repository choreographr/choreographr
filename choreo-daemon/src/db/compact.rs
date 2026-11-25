//! Startup space reclamation for the `redb` store.
//!
//! `redb` never returns freed space to the filesystem. Deleting a session
//! removes its rows, and the pages they occupied become free for LATER writes,
//! but the file keeps its high-water length until something compacts it. An
//! install that deletes old sessions therefore keeps paying for them on disk:
//! measured here, a store whose every session had been deleted still occupied
//! its full multi-megabyte length, and one `compact()` call returned it to
//! tens of kilobytes.
//!
//! Compaction runs at startup, on the owned `Store` handle, after
//! [`super::run_migrations_at`]. Two orderings are load-bearing:
//!
//! - **After the pre-migration backup.** That snapshot exists to roll a failed
//!   migration back, so it must image the pre-migration store; compacting
//!   first would put a rewritten file where the rollback point should be.
//! - **Before the handle is shared.** `redb::Database::compact()` takes
//!   `&mut self`, which the `Arc<Store>` the daemon threads everywhere cannot
//!   give it. Startup is also the only point where the store is provably free
//!   of the daemon's own read and write transactions.
//!
//! [`CompactionMode::Balanced`] is the mode used. `Maximum` also repacks
//! partially filled pages, which measured no extra win on this schema — the
//! store's rows are large and deleted whole, so the pages free up completely
//! rather than half — while costing a slower pass and leaving pages packed
//! full, which slows the inserts that follow until they split.
//!
//! [`CompactionMode::Balanced`]: redb::CompactionMode::Balanced

use std::io;
use std::path::Path;

use tracing::{debug, info};

use super::store::Store;

/// A store's on-disk length and the bytes its live records actually occupy.
///
/// `live_bytes` comes from `redb`'s own accounting (`stored_bytes` +
/// `metadata_bytes`), so `file_bytes - live_bytes` bounds what a compaction
/// could return. It is an *upper* bound: `Balanced` compaction relocates whole
/// pages and truncates the free space that leaves at the end, but leaves the
/// free space inside partially filled pages for later inserts to fill.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Footprint {
    file_bytes: u64,
    live_bytes: u64,
}

impl Footprint {
    /// Bytes the file holds beyond its live records, e.g. pages freed by
    /// deleted sessions.
    ///
    /// Saturating: a store that reports more live bytes than the file is long
    /// (in-flight caps, or a file truncated underneath us) has nothing to
    /// reclaim, and must not underflow a `u64` subtraction.
    fn reclaimable(&self) -> u64 {
        self.file_bytes.saturating_sub(self.live_bytes)
    }
}

/// When a compaction is worth its cost, which is a full pass over the file.
///
/// Both conditions must hold, so a small store never pays for a pass and a
/// mostly-live store never pays for one either — the second is what keeps
/// startup fast for the common case, because an install that never deletes
/// anything keeps a large file with little free space in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Policy {
    /// Never compact for less than this many reclaimable bytes.
    min_reclaim_bytes: u64,
    /// ...nor when the reclaimable bytes are less than this share of the file.
    min_reclaim_percent: u64,
}

/// The thresholds production uses: a quarter of the file, and at least 32 MiB.
///
/// Sized from measurement rather than taste. A store with nothing deleted
/// reported 22% reclaimable but was entirely in-page slack that `Balanced`
/// cannot take (redb's own `compact()` declined it), so a 25% floor declines
/// that pass rather than paying for it; deleting half a 100 MiB store left 61%
/// reclaimable and returned 50 MiB, which clears both thresholds.
const PRODUCTION_POLICY: Policy = Policy {
    min_reclaim_bytes: 32 * 1024 * 1024,
    min_reclaim_percent: 25,
};

impl Policy {
    /// Whether a compaction of `footprint` is worth running.
    ///
    /// Pure so the decision is testable without a database, a file, or a
    /// clock: it is a total function of the two measured numbers.
    fn worth_compacting(&self, footprint: Footprint) -> bool {
        let reclaimable = footprint.reclaimable();
        reclaimable >= self.min_reclaim_bytes
            && reclaimable.saturating_mul(100)
                >= footprint
                    .file_bytes
                    .saturating_mul(self.min_reclaim_percent)
    }
}

/// Reclaim a `redb` store's free space before it is shared with the daemon's
/// threads.
///
/// Compaction is an optimisation, never a correctness requirement, so the
/// caller is expected to warn and continue if this fails: losing a daemon over
/// a failed space reclaim would be worse than keeping a large file. Errors are
/// still returned rather than swallowed so the caller controls that policy and
/// the log line names the stage.
///
/// # Errors
///
/// Returns Err if the file's length cannot be read, the accounting-only write
/// transaction cannot be opened, or `redb` fails to compact the file.
pub(crate) fn maybe_compact(db: &mut Store, path: &Path) -> io::Result<()> {
    maybe_compact_with(db, path, PRODUCTION_POLICY)
}

/// [`maybe_compact`] with an injectable [`Policy`], so a test can exercise the
/// real measure-and-compact path against a store too small to clear the
/// production thresholds.
fn maybe_compact_with(db: &mut Store, path: &Path, policy: Policy) -> io::Result<()> {
    let before = measure(db, path)?;
    if !policy.worth_compacting(before) {
        debug!(
            file_bytes = before.file_bytes,
            live_bytes = before.live_bytes,
            reclaimable = before.reclaimable(),
            "leaving the database uncompacted: too little free space to be worth a pass"
        );
        return Ok(());
    }

    info!(
        file_bytes = before.file_bytes,
        live_bytes = before.live_bytes,
        reclaimable = before.reclaimable(),
        "compacting database to reclaim the space freed by deleted sessions"
    );
    // One pass is complete: `Balanced` compaction moves live pages to the
    // front and truncates what that frees at the end, so a second call reports
    // no further compaction rather than finding more to do.
    let performed = match db.compact(redb::CompactionMode::Balanced) {
        Ok(performed) => performed,
        Err(e) => return Err(db.err(format!("redb compact: {e}"))),
    };
    // `performed == false` means redb found nothing it could reclaim, which is
    // possible with a footprint that cleared the thresholds: the estimate
    // counts in-page slack that `Balanced` deliberately leaves for later
    // inserts. Report the file's real length either way.
    let after = std::fs::metadata(path).map_err(|e| {
        io::Error::other(format!(
            "database compacted but {} could not be re-measured: {e}",
            path.display()
        ))
    })?;
    info!(
        performed,
        file_bytes_before = before.file_bytes,
        file_bytes_after = after.len(),
        reclaimed = before.file_bytes.saturating_sub(after.len()),
        "database compaction finished"
    );
    Ok(())
}

/// Measure a store's on-disk length and its live-record bytes.
///
/// `redb` exposes the live accounting only on a write transaction; the one
/// opened here is read-only in effect and is dropped without committing, so it
/// leaves the store untouched. Startup is safe for it because no other
/// transaction exists yet — `begin_write()` would otherwise block.
fn measure(db: &Store, path: &Path) -> io::Result<Footprint> {
    let file_bytes = std::fs::metadata(path)
        .map_err(|e| io::Error::other(format!("read {} length: {e}", path.display())))?
        .len();
    let stats = {
        let write_txn = db
            .begin_write()
            .map_err(|e| db.err(format!("redb write txn (stats): {e}")))?;
        write_txn
            .stats()
            .map_err(|e| db.err(format!("redb stats: {e}")))?
        // Dropped here without `commit()`: an empty transaction is aborted, so
        // measuring costs no write.
    };
    Ok(Footprint {
        file_bytes,
        live_bytes: stats.stored_bytes() + stats.metadata_bytes(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    // `Table::iter` is the `ReadableTable` trait method; there is no inherent
    // one on a write-transaction table.
    use redb::ReadableTable;

    const MIB: u64 = 1024 * 1024;

    /// The decision is a total function of two numbers, so tabulate it rather
    /// than reaching for a database. The 22%-reclaimable row is the measured
    /// "wrote a lot, deleted nothing" shape that `Balanced` cannot act on.
    #[test]
    fn policy_declines_small_shares_and_small_absolute_wins() {
        let p = PRODUCTION_POLICY;

        // Nothing deleted: 100 MiB file, 78 MiB live -> 22% is in-page slack.
        let healthy = Footprint {
            file_bytes: 100 * MIB,
            live_bytes: 78 * MIB,
        };
        assert_eq!(healthy.reclaimable(), 22 * MIB);
        assert!(
            !p.worth_compacting(healthy),
            "a mostly-live store must not pay for an O(file) pass"
        );

        // Half deleted: 61% reclaimable, which clears both thresholds.
        let half_deleted = Footprint {
            file_bytes: 100 * MIB,
            live_bytes: 39 * MIB,
        };
        assert!(p.worth_compacting(half_deleted));

        // Everything deleted.
        let emptied = Footprint {
            file_bytes: 100 * MIB,
            live_bytes: 20 * 1024,
        };
        assert!(p.worth_compacting(emptied));
    }

    #[test]
    fn policy_requires_both_thresholds() {
        let p = PRODUCTION_POLICY;

        // 90% of the file is reclaimable, but the file is small: no pass.
        let small = Footprint {
            file_bytes: 4 * MIB,
            live_bytes: 400 * 1024,
        };
        assert!(small.reclaimable() >= 3 * MIB);
        assert!(
            !p.worth_compacting(small),
            "an absolute floor keeps small stores off the compaction path"
        );

        // Ample reclaimable bytes, but only 20% of a huge file: no pass.
        let mostly_live = Footprint {
            file_bytes: 1000 * MIB,
            live_bytes: 800 * MIB,
        };
        assert_eq!(mostly_live.reclaimable(), 200 * MIB);
        assert!(
            !p.worth_compacting(mostly_live),
            "a large absolute win still must be a large share of the file"
        );
    }

    #[test]
    fn reclaimable_saturates_when_live_exceeds_the_file() {
        let impossible = Footprint {
            file_bytes: MIB,
            live_bytes: 2 * MIB,
        };
        assert_eq!(impossible.reclaimable(), 0);
    }

    /// End-to-end: a store whose sessions were all deleted must actually shrink
    /// on disk. Uses a permissive policy because the fixture is far below the
    /// production 32 MiB floor — the point is the real `redb` call and the real
    /// measurement, not the thresholds (covered above).
    #[test]
    fn compaction_reclaims_the_space_of_deleted_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.redb");
        write_then_delete_everything(&path);

        let before = std::fs::metadata(&path).unwrap().len();
        let mut db = Store::from_db(redb::Database::open(&path).unwrap());
        maybe_compact_with(
            &mut db,
            &path,
            Policy {
                min_reclaim_bytes: 0,
                min_reclaim_percent: 1,
            },
        )
        .unwrap();
        drop(db);

        let after = std::fs::metadata(&path).unwrap().len();
        assert!(
            after * 2 <= before,
            "compaction must at least halve a fully emptied store: {before} -> {after} bytes"
        );
    }

    /// The production thresholds decline a store this small, so the startup path
    /// leaves it byte-identical rather than rewriting it for a marginal win.
    #[test]
    fn production_policy_leaves_a_small_store_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.redb");
        write_then_delete_everything(&path);

        let before = std::fs::metadata(&path).unwrap().len();
        let mut db = Store::from_db(redb::Database::open(&path).unwrap());
        maybe_compact(&mut db, &path).unwrap();
        drop(db);

        assert_eq!(
            std::fs::metadata(&path).unwrap().len(),
            before,
            "a store under the absolute floor must not be rewritten at startup"
        );
    }

    /// Build the shape the daemon produces — a session record per id, one turn
    /// blob and one attachment row per turn — then delete every row, which is
    /// what `delete_session` does when a user clears their history.
    fn write_then_delete_everything(path: &Path) {
        const SESSIONS: redb::TableDefinition<u64, &[u8]> = redb::TableDefinition::new("sessions");
        const SESSION_TURNS: redb::TableDefinition<(u64, u32), &[u8]> =
            redb::TableDefinition::new("session_turns");
        const SESSION_ATTACHMENTS: redb::TableDefinition<(u64, u32, String), &[u8]> =
            redb::TableDefinition::new("session_attachments");

        let db = redb::Database::create(path).unwrap();
        let turn_blob = vec![0x5au8; 24 * 1024];
        let image_blob = vec![0x11u8; 24 * 1024];
        for session_id in 0..10u64 {
            let write_txn = db.begin_write().unwrap();
            {
                let mut sessions = write_txn.open_table(SESSIONS).unwrap();
                sessions.insert(session_id, b"record".as_slice()).unwrap();
                let mut turns = write_txn.open_table(SESSION_TURNS).unwrap();
                let mut attachments = write_txn.open_table(SESSION_ATTACHMENTS).unwrap();
                for turn_id in 0..10u32 {
                    turns
                        .insert((session_id, turn_id), turn_blob.as_slice())
                        .unwrap();
                    attachments
                        .insert(
                            (session_id, turn_id, "d0".to_string()),
                            image_blob.as_slice(),
                        )
                        .unwrap();
                }
            }
            write_txn.commit().unwrap();
        }

        // Collect first, then remove: a redb `Range` keeps the table borrowed
        // for as long as it is alive, so the removals cannot overlap it.
        let write_txn = db.begin_write().unwrap();
        let turn_keys: Vec<(u64, u32)> = {
            let turns = write_txn.open_table(SESSION_TURNS).unwrap();
            turns
                .iter()
                .unwrap()
                .filter_map(std::result::Result::ok)
                .map(|(k, _)| k.value())
                .collect()
        };
        let attachment_keys: Vec<(u64, u32, String)> = {
            let attachments = write_txn.open_table(SESSION_ATTACHMENTS).unwrap();
            attachments
                .iter()
                .unwrap()
                .filter_map(std::result::Result::ok)
                .map(|(k, _)| k.value())
                .collect()
        };
        let session_keys: Vec<u64> = {
            let sessions = write_txn.open_table(SESSIONS).unwrap();
            sessions
                .iter()
                .unwrap()
                .filter_map(std::result::Result::ok)
                .map(|(k, _)| k.value())
                .collect()
        };
        {
            let mut turns = write_txn.open_table(SESSION_TURNS).unwrap();
            for key in turn_keys {
                turns.remove(key).unwrap();
            }
        }
        {
            let mut attachments = write_txn.open_table(SESSION_ATTACHMENTS).unwrap();
            for key in attachment_keys {
                attachments.remove(key).unwrap();
            }
        }
        {
            let mut sessions = write_txn.open_table(SESSIONS).unwrap();
            for key in session_keys {
                sessions.remove(key).unwrap();
            }
        }
        write_txn.commit().unwrap();
    }
}
