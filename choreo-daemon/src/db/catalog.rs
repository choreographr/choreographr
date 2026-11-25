//! The runtime catalog-refresh state: the last models.dev fetch-attempt
//! timestamp and the stored etag, in one table so the pacing state is a single
//! coherent record.
//!
//! Both getters are deliberately tolerant — a missing table, a missing key and
//! a corrupt value all read as absent — because this state only paces and
//! revalidates fetches, so it must never fail a caller (or the daemon).

use std::io;

use redb::ReadableDatabase;
use redb::ReadableTable;
use tracing::warn;

use super::db_err;
use super::schema::{CATALOG_ETAG_KEY, CATALOG_LAST_ATTEMPT_KEY, CATALOG_STATE};

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
///
/// # Errors
///
/// Returns Err if the underlying `catalog_state` write fails.
pub fn set_catalog_last_attempt_ms(db: &redb::Database, ms: u64) -> io::Result<()> {
    catalog_state_write(db, CATALOG_LAST_ATTEMPT_KEY, Some(&ms.to_le_bytes()))
}

/// Read the recorded catalog fetch-attempt timestamp. `None` when no attempt
/// has ever been recorded (first run, or an upgrade from a build without the
/// key) — callers treat that as "stale, fetch now". A stored value with an
/// unexpected length is logged and treated as absent, the same policy as
/// undecodable session records: the timestamp is advisory pacing, so a corrupt
/// value must never fail the caller (or the daemon).
///
/// # Errors
///
/// Returns Err if the underlying `catalog_state` read fails.
pub fn get_catalog_last_attempt_ms(db: &redb::Database) -> io::Result<Option<u64>> {
    let Some(bytes) = catalog_state_get(db, CATALOG_LAST_ATTEMPT_KEY)? else {
        return Ok(None);
    };
    if let Ok(bytes) = <[u8; 8]>::try_from(bytes.as_slice()) {
        Ok(Some(u64::from_le_bytes(bytes)))
    } else {
        warn!("catalog last_attempt_ms has an invalid length; treating as absent");
        Ok(None)
    }
}

/// Store or clear the models.dev etag. `Some` inserts the raw entity-tag
/// (replacing any previous value); `None` removes the key — a fetch that came
/// back without an etag must not leave a stale one behind (it would be served
/// as `If-None-Match` forever).
///
/// # Errors
///
/// Returns Err if the underlying `catalog_state` write fails.
pub fn set_catalog_etag(db: &redb::Database, etag: Option<&str>) -> io::Result<()> {
    catalog_state_write(db, CATALOG_ETAG_KEY, etag.map(str::as_bytes))
}

/// Read the stored models.dev etag. `None` when absent or blank (an empty
/// stored value is treated as absent — it could never be a valid entity-tag).
///
/// # Errors
///
/// Returns Err if the underlying `catalog_state` read fails.
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
