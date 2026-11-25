//! Deletion tombstones: the markers that keep a deleted session from
//! resurfacing.
//!
//! Deleting a session whose thread is still shutting down races that thread's
//! final record write: the thread may re-create the record after the delete, and
//! if the daemon then dies before the delete is finalized the session would come
//! back on the next start. A tombstone is written first (so the delete is
//! durable before the record can be re-created), the record is deleted, and
//! [`purge_tombstoned_sessions`] sweeps up anything that was resurrected in the
//! window.

use std::io;

use redb::{ReadableDatabase, ReadableTable};
use tracing::{debug, info, warn};

use crate::db::schema::DELETED_SESSIONS;
use crate::db::store::Store;

/// Write a deletion tombstone for `session_id`.
///
/// Called by the daemon when deleting a session whose thread is still alive.
/// If that thread re-creates the record (via `persist_and_exit`) and the
/// daemon crashes before `handle_session_exited` finalizes the delete, the
/// tombstone survives so [`purge_tombstoned_sessions`] removes the record at
/// the next startup instead of letting a deleted session reappear.
///
/// # Errors
///
/// Returns Err if the write transaction, table open, insert, or commit
/// fails.
pub fn mark_session_deleted(db: &Store, session_id: u64) -> io::Result<()> {
    debug!("mark_session_deleted: id={}", session_id);
    let write_txn = db
        .begin_write()
        .map_err(|e| db.err(format!("redb write txn: {e}")))?;
    {
        let mut table = write_txn
            .open_table(DELETED_SESSIONS)
            .map_err(|e| db.err(format!("redb open deleted_sessions: {e}")))?;
        table
            .insert(session_id, ())
            .map_err(|e| db.err(format!("redb insert tombstone: {e}")))?;
    }
    write_txn
        .commit()
        .map_err(|e| db.err(format!("redb commit tombstone: {e}")))?;
    Ok(())
}

/// Remove the deletion tombstone for `session_id`.
///
/// Called once the exit handler has deleted the record the still-shutting-down
/// thread re-created, so the tombstone does not accumulate.
///
/// # Errors
///
/// Returns Err if the write transaction, table open, removal, or commit
/// fails.
pub fn clear_session_tombstone(db: &Store, session_id: u64) -> io::Result<()> {
    debug!("clear_session_tombstone: id={}", session_id);
    let write_txn = db
        .begin_write()
        .map_err(|e| db.err(format!("redb write txn: {e}")))?;
    {
        let mut table = write_txn
            .open_table(DELETED_SESSIONS)
            .map_err(|e| db.err(format!("redb open deleted_sessions: {e}")))?;
        table
            .remove(session_id)
            .map_err(|e| db.err(format!("redb remove tombstone: {e}")))?;
    }
    write_txn
        .commit()
        .map_err(|e| db.err(format!("redb commit tombstone: {e}")))?;
    Ok(())
}

/// Delete every session that carries a deletion tombstone and clear the
/// tombstones. Returns the number of sessions purged.
///
/// Called once at daemon startup, before the session index is loaded: a
/// deleted session whose still-shutting-down thread re-created the record,
/// then died with a crashed daemon before the delete could be finalized,
/// must not resurface. Deleting a record that is already gone is a harmless
/// no-op.
///
/// # Errors
///
/// Returns Err if the read transaction, table open, iteration (an iteration
/// failure is a storage failure and must reach the store's latch hook), or any
/// per-session delete fails. A single session whose delete fails is logged and
/// skipped so one bad row cannot stop the sweep for the others.
pub fn purge_tombstoned_sessions(db: &Store) -> io::Result<usize> {
    let read_txn = db
        .begin_read()
        .map_err(|e| db.err(format!("redb read txn: {e}")))?;
    let table = match read_txn.open_table(DELETED_SESSIONS) {
        Ok(table) => table,
        // No tombstone table yet (e.g. a pre-upgrade database): nothing to purge.
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(0),
        Err(e) => return Err(db.err(format!("redb open deleted_sessions: {e}"))),
    };
    let ids: Vec<u64> = table
        .iter()
        .map_err(|e| db.err(format!("redb iter deleted_sessions: {e}")))?
        .map(|result| {
            result
                .map(|(key, _)| key.value())
                .map_err(|e| db.err(format!("redb iter item: {e}")))
        })
        .collect::<io::Result<_>>()?;
    drop(read_txn);

    let mut purged = 0usize;
    for id in ids {
        if let Err(e) = super::delete_session(db, id) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::sessions::test_support::{dummy_turn, sample_record, store_at};
    use crate::db::{read_session, read_turns, write_session, write_turn};

    #[test]
    fn purge_removes_tombstoned_resurrected_record() {
        // Simulates the crash window: a session is deleted (tombstone
        // written), its still-shutting-down thread re-creates the record, and
        // the daemon dies before the delete is finalized. The startup purge
        // must remove the record so the deleted session cannot resurface.
        let dir = tempfile::tempdir().unwrap();
        let db = store_at(&dir);
        let record = sample_record();

        write_session(&db, 5, &record).unwrap();
        mark_session_deleted(&db, 5).unwrap();
        // The still-shutting-down thread re-creates the record after the delete…
        write_session(&db, 5, &record).unwrap();
        write_turn(&db, 5, 0, &dummy_turn()).unwrap();

        let purged = purge_tombstoned_sessions(&db).unwrap();
        assert_eq!(purged, 1, "the resurrected record must be purged");
        assert!(
            read_session(&db, 5).unwrap().is_none(),
            "tombstoned session must not survive the purge"
        );
        assert!(
            read_turns(&db, 5).unwrap().is_empty(),
            "the purge must collect the resurrected session's turns too"
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
        let db = store_at(&dir);
        write_session(&db, 6, &sample_record()).unwrap();
        mark_session_deleted(&db, 6).unwrap();
        clear_session_tombstone(&db, 6).unwrap();

        let purged = purge_tombstoned_sessions(&db).unwrap();
        assert_eq!(purged, 0);
        assert!(read_session(&db, 6).unwrap().is_some());
    }

    #[test]
    fn purge_empty_database_is_zero() {
        let dir = tempfile::tempdir().unwrap();
        let db = store_at(&dir);
        assert_eq!(purge_tombstoned_sessions(&db).unwrap(), 0);
    }
}
