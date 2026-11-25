//! The daemon's stored secrets: the encrypted credential blobs and the keystore
//! binding that anchors them.
//!
//! Both tables are opaque byte stores — the daemon seals and opens the blobs
//! itself, so this module persists values without interpreting them. The
//! binding is a trust-on-first-use record: absent means the keystore is still
//! unbound and the next presented key is adopted, and a stored value of the
//! wrong length is treated as absent so a corrupt write cannot make the
//! keystore permanently unusable.

use std::collections::HashMap;
use std::io;

use redb::ReadableDatabase;
use redb::ReadableTable;
use tracing::{info, warn};

use super::db_err;
use super::schema::{CREDENTIALS, KEYSTORE, KEYSTORE_BINDING_KEY};

// ── Credential table ────────────────────────────────────────────────────────

/// Store (or overwrite) the encrypted credential blob for a service.
///
/// # Errors
///
/// Returns Err if the write transaction, table open, insert, or commit
/// fails.
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

/// Read all stored credential blobs, keyed by service name. Returns an
/// empty map when the credentials table does not exist yet.
///
/// # Errors
///
/// Returns Err if the read transaction, table open, or iteration fails.
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

/// Remove the stored credential blob for a service (no-op if absent).
///
/// # Errors
///
/// Returns Err if the write transaction, table open, removal, or commit
/// fails.
pub fn remove_credential_blob(db: &redb::Database, service: &str) -> Result<(), redb::Error> {
    let write_txn = db.begin_write()?;
    {
        let mut table = write_txn.open_table(CREDENTIALS)?;
        table.remove(service)?;
    }
    write_txn.commit()?;
    Ok(())
}

// ── Keystore binding table ──────────────────────────────────────────────────

/// Read the stored keystore binding: `None` when the daemon's keystore is
/// still UNBOUND (first contact — the presented key is adopted). A stored
/// value with the wrong length is logged and treated as absent (the same
/// tolerant policy as the catalog getters): the binding would be re-adopted
/// from the next presented key, which is the only sane recovery for a
/// corrupt/truncated write — the keystore is unusable otherwise.
///
/// # Errors
///
/// Returns Err if the read transaction, table open, or row lookup fails.
pub fn get_keystore_binding(db: &redb::Database) -> io::Result<Option<[u8; 32]>> {
    let read_txn = db
        .begin_read()
        .map_err(|e| db_err(format!("redb read txn: {e}")))?;
    let table = match read_txn.open_table(KEYSTORE) {
        Ok(table) => table,
        // No table yet ⇒ the keystore has never been bound (fresh DB, or a
        // database upgraded from a build before the keystore table existed).
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
        Err(e) => return Err(db_err(format!("redb open keystore: {e}"))),
    };
    let Some(guard) = table
        .get(KEYSTORE_BINDING_KEY)
        .map_err(|e| db_err(format!("redb get keystore binding: {e}")))?
    else {
        return Ok(None);
    };
    if let Ok(key) = <[u8; 32]>::try_from(guard.value()) {
        Ok(Some(key))
    } else {
        warn!(
            stored_len = guard.value().len(),
            "keystore binding has an invalid length; treating as unbound"
        );
        Ok(None)
    }
}

/// Persist the keystore binding (32-byte X25519 public key). Called exactly
/// once per daemon lifetime — on the first TOFU adoption — and never again
/// afterwards, so callers race nowhere in practice; the write is atomic in
/// a single redb transaction.
///
/// # Errors
///
/// Returns Err if the write transaction, table open, insert, or commit
/// fails.
pub fn set_keystore_binding(db: &redb::Database, public_key: &[u8; 32]) -> io::Result<()> {
    let write_txn = db
        .begin_write()
        .map_err(|e| db_err(format!("redb write txn: {e}")))?;
    {
        let mut table = write_txn
            .open_table(KEYSTORE)
            .map_err(|e| db_err(format!("redb open keystore: {e}")))?;
        table
            .insert(KEYSTORE_BINDING_KEY, public_key.as_slice())
            .map_err(|e| db_err(format!("redb set keystore binding: {e}")))?;
    }
    write_txn
        .commit()
        .map_err(|e| db_err(format!("redb commit keystore binding: {e}")))?;
    info!("persisted keystore binding (TOFU adoption)");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keystore_binding_adopts_and_round_trips() {
        // TOFU contract: a fresh database reads as UNBOUND (None); the first
        // set_keystore_binding persists the binding; a re-read returns the
        // exact 32-byte public key. The table is created lazily on first
        // write, so no schema bump or migration is needed.
        let dir = tempfile::tempdir().unwrap();
        let db = redb::Database::create(dir.path().join("test.redb")).unwrap();

        assert_eq!(get_keystore_binding(&db).unwrap(), None);

        let pubkey = [7u8; 32];
        set_keystore_binding(&db, &pubkey).unwrap();
        assert_eq!(get_keystore_binding(&db).unwrap(), Some(pubkey));
    }

    #[test]
    fn keystore_binding_corrupt_length_treated_as_absent() {
        // A stored binding with the wrong length (interrupted/foreign write)
        // must be treated as UNBOUND with a warning, never an error — the
        // same tolerant policy as the catalog getters. The next presented
        // key re-adopts the binding, the only usable recovery.
        let dir = tempfile::tempdir().unwrap();
        let db = redb::Database::create(dir.path().join("test.redb")).unwrap();
        {
            let write_txn = db.begin_write().unwrap();
            {
                let mut table = write_txn.open_table(KEYSTORE).unwrap();
                table
                    .insert(KEYSTORE_BINDING_KEY, b"short".as_slice())
                    .unwrap();
            }
            write_txn.commit().unwrap();
        }
        assert_eq!(get_keystore_binding(&db).unwrap(), None);
    }
}
