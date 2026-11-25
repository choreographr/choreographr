//! The per-session key-value store behind the `db_*` tools.
//!
//! Rows are keyed `(session_id, key)`, so one session's entries form a
//! contiguous key range and every whole-session read, delete, list and count is
//! a bounded range scan over that range rather than a walk of the whole table.
//! The upper bound closing the range is
//! [`session_range_end`](super::schema::session_range_end).

use std::io;

use redb::ReadableDatabase;
use redb::ReadableTable;
use tracing::debug;

use super::db_err;
use super::schema::{KvRangeIter, SESSION_KV, session_range_end};

/// Insert or overwrite a key-value pair for the given session.
///
/// # Errors
///
/// Returns Err if the write transaction, table open, insert, or commit
/// fails.
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
///
/// # Errors
///
/// Returns Err if the read transaction, table open, or row lookup fails.
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
        Some(guard) => Ok(Some(guard.value().clone())),
        None => Ok(None),
    }
}

/// Remove a single key. Returns `true` if the key existed, `false` otherwise.
///
/// # Errors
///
/// Returns Err if the write transaction, table open, removal, or commit
/// fails.
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
///
/// # Errors
///
/// Returns Err if the write transaction, table open/iteration, removals,
/// or commit fails.
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
        let range = if let Some(end) = end {
            let range_start = (session_id, start.to_string());
            let range_end = (session_id, end.to_string());
            table
                .range((range_start)..(range_end))
                .map_err(|e| db_err(format!("redb range kv_delete_range: {e}")))?
        } else {
            let range_start = (session_id, start.to_string());
            let range_end = (session_range_end(session_id), String::new());
            table
                .range((range_start)..(range_end))
                .map_err(|e| db_err(format!("redb range kv_delete_range: {e}")))?
        };
        let keys: Vec<(u64, String)> = range
            .filter_map(std::result::Result::ok)
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
///
/// # Errors
///
/// Returns Err if the read transaction, table open, range iteration, or
/// row decode fails.
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
    let range = if let Some(end) = end {
        let range_start = (session_id, start.to_string());
        let range_end = (session_id, end.to_string());
        table
            .range((range_start)..(range_end))
            .map_err(|e| db_err(format!("redb range kv_get_range: {e}")))?
    } else {
        let range_start = (session_id, start.to_string());
        let range_end = (session_range_end(session_id), String::new());
        table
            .range((range_start)..(range_end))
            .map_err(|e| db_err(format!("redb range kv_get_range: {e}")))?
    };
    let mut results = Vec::new();
    for result in range {
        let (key, value) = result.map_err(|e| db_err(format!("redb iter kv_get_range: {e}")))?;
        results.push((key.value().1, value.value().clone()));
    }
    Ok(results)
}

/// List all keys in the range [`start`, `end`) for the given session.
///
/// Returns only key names (not values). If `start` is `None`, starts from
/// the beginning of the session's keys. If `end` is `None`, goes to the end.
///
/// # Errors
///
/// Returns Err if the read transaction, table open, range iteration, or
/// row decode fails.
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
                    .range((range_start)..(range_end))
                    .map_err(|e| db_err(format!("redb range kv_list: {e}")))?,
            )
        }
        (Some(start), None) => {
            let range_start = (session_id, start.to_string());
            let range_end = (session_range_end(session_id), String::new());
            Box::new(
                table
                    .range((range_start)..(range_end))
                    .map_err(|e| db_err(format!("redb range kv_list: {e}")))?,
            )
        }
        (None, Some(end)) => {
            let range_start = (session_id, String::new());
            let range_end = (session_id, end.to_string());
            Box::new(
                table
                    .range((range_start)..(range_end))
                    .map_err(|e| db_err(format!("redb range kv_list: {e}")))?,
            )
        }
        (None, None) => {
            let range_start = (session_id, String::new());
            let range_end = (session_range_end(session_id), String::new());
            Box::new(
                table
                    .range((range_start)..(range_end))
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
/// When `prefix` is `Some(p)`, counts keys in [`p`, `p` + `max_char`).
/// When `prefix` is `None`, counts all keys for the session.
///
/// # Errors
///
/// Returns Err if the read transaction, table open, range iteration, or
/// row decode fails.
pub fn kv_count(db: &redb::Database, session_id: u64, prefix: Option<&str>) -> io::Result<u64> {
    let read_txn = db
        .begin_read()
        .map_err(|e| db_err(format!("redb read txn: {e}")))?;
    let table = read_txn
        .open_table(SESSION_KV)
        .map_err(|e| db_err(format!("redb open session_kv: {e}")))?;
    let range = if let Some(prefix) = prefix {
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
        let range_end_str = String::from_utf8_lossy_owned(end_bytes);
        let range_end = (session_id, range_end_str);
        table
            .range((range_start)..(range_end))
            .map_err(|e| db_err(format!("redb range kv_count: {e}")))?
    } else {
        let range_start = (session_id, String::new());
        let range_end = (session_range_end(session_id), String::new());
        table
            .range((range_start)..(range_end))
            .map_err(|e| db_err(format!("redb range kv_count: {e}")))?
    };
    let mut count: u64 = 0;
    for result in range {
        result.map_err(|e| db_err(format!("redb iter kv_count: {e}")))?;
        count += 1;
    }
    Ok(count)
}
