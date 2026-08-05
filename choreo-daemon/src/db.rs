use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::PathBuf;

use choreo_proto::{ContextConfig, Turn};
use redb::ReadableDatabase;
use redb::ReadableTable;
use redb::TableDefinition;
use serde::{Deserialize, Serialize};
use tracing::{debug, error, info, warn};

const SESSIONS: TableDefinition<u64, &[u8]> = TableDefinition::new("sessions");
const SESSION_TURNS: TableDefinition<(u64, u32), &[u8]> = TableDefinition::new("session_turns");
const CREDENTIALS: TableDefinition<&str, &[u8]> = TableDefinition::new("credentials");
#[cfg(test)]
const META: TableDefinition<&str, u64> = TableDefinition::new("meta");
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

pub fn open_db() -> io::Result<redb::Database> {
    let path = db_path()?;
    info!(path = %path.display(), "opening database");
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    // Try open first (fails if file doesn't exist), then fall back to create
    let result = redb::Database::open(&path);
    match result {
        Ok(db) => return Ok(db),
        Err(redb::DatabaseError::Storage(redb::StorageError::Io(io_err)))
            if io_err.kind() == std::io::ErrorKind::NotFound =>
        {
            info!("database file not found, creating new database");
        }
        Err(e) => {
            warn!("failed to open existing database, trying to recreate: {e}");
        }
    }
    redb::Database::create(path)
        .map_err(|e| io::Error::other(format!("failed to open database: {e}")))
}

pub fn write_session(
    db: &redb::Database,
    session_id: u64,
    record: &SessionRecord,
) -> io::Result<()> {
    let payload = postcard::to_allocvec(record)
        .map_err(|e| db_err(format!("postcard encode session: {e}")))?;
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
        Some(guard) => {
            let record: SessionRecord = postcard::from_bytes(guard.value())
                .map_err(|e| db_err(format!("postcard decode session: {e}")))?;
            Ok(Some(record))
        }
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
        match postcard::from_bytes::<SessionRecord>(value.value()) {
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

pub fn write_turn(
    db: &redb::Database,
    session_id: u64,
    turn_id: u32,
    turn: &Turn,
) -> io::Result<()> {
    let payload =
        postcard::to_allocvec(turn).map_err(|e| db_err(format!("postcard encode turn: {e}")))?;
    let write_txn = db
        .begin_write()
        .map_err(|e| db_err(format!("redb write txn: {e}")))?;
    {
        let mut table = write_txn
            .open_table(SESSION_TURNS)
            .map_err(|e| db_err(format!("redb open turns: {e}")))?;
        table
            .insert((session_id, turn_id), payload.as_slice())
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
            match postcard::from_bytes::<Turn>(value.value()) {
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
    fn read_turns_skips_corrupt_entries() {
        let dir = tempfile::tempdir().unwrap();
        let db = redb::Database::create(dir.path().join("test.redb")).unwrap();
        let id = 1u64;

        // Write a valid turn at index 0
        let valid_turn = dummy_turn();
        write_turn(&db, id, 0, &valid_turn).unwrap();

        // Manually insert a corrupt blob at index 1 (not valid postcard)
        {
            let write_txn = db.begin_write().unwrap();
            {
                let mut table = write_txn.open_table(SESSION_TURNS).unwrap();
                table
                    .insert((id, 1u32), b"not valid postcard data".as_slice())
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
}
