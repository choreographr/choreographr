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

use super::schema::{KvRangeIter, SESSION_KV, session_range_end};
use super::store::Store;

/// Insert or overwrite a key-value pair for the given session.
///
/// # Errors
///
/// Returns Err if the write transaction, table open, insert, or commit
/// fails.
pub fn kv_set(db: &Store, session_id: u64, key: &str, value: &[u8]) -> io::Result<()> {
    let write_txn = db
        .begin_write()
        .map_err(|e| db.err(format!("redb write txn: {e}")))?;
    {
        let mut table = write_txn
            .open_table(SESSION_KV)
            .map_err(|e| db.err(format!("redb open session_kv: {e}")))?;
        table
            .insert((session_id, key.to_string()), value.to_vec())
            .map_err(|e| db.err(format!("redb kv_set: {e}")))?;
    }
    write_txn
        .commit()
        .map_err(|e| db.err(format!("redb commit kv_set: {e}")))?;
    // The key name is the caller's data (a `db_set` argument), so the log line
    // carries its length only — never the argument itself.
    debug!(session_id, key_len = key.len(), "kv_set ok");
    Ok(())
}

/// Retrieve a value by session and key. Returns `None` if the key does not exist.
///
/// # Errors
///
/// Returns Err if the read transaction, table open, or row lookup fails.
pub fn kv_get(db: &Store, session_id: u64, key: &str) -> io::Result<Option<Vec<u8>>> {
    let read_txn = db
        .begin_read()
        .map_err(|e| db.err(format!("redb read txn: {e}")))?;
    let table = read_txn
        .open_table(SESSION_KV)
        .map_err(|e| db.err(format!("redb open session_kv: {e}")))?;
    match table
        .get((session_id, key.to_string()))
        .map_err(|e| db.err(format!("redb kv_get: {e}")))?
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
pub fn kv_delete(db: &Store, session_id: u64, key: &str) -> io::Result<bool> {
    let write_txn = db
        .begin_write()
        .map_err(|e| db.err(format!("redb write txn: {e}")))?;
    let removed = {
        let mut table = write_txn
            .open_table(SESSION_KV)
            .map_err(|e| db.err(format!("redb open session_kv: {e}")))?;
        table
            .remove((session_id, key.to_string()))
            .map_err(|e| db.err(format!("redb kv_delete: {e}")))?
            .is_some()
    };
    write_txn
        .commit()
        .map_err(|e| db.err(format!("redb commit kv_delete: {e}")))?;
    debug!(session_id, key_len = key.len(), removed, "kv_delete done");
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
    db: &Store,
    session_id: u64,
    start: &str,
    end: Option<&str>,
) -> io::Result<u64> {
    let write_txn = db
        .begin_write()
        .map_err(|e| db.err(format!("redb write txn: {e}")))?;
    let count = {
        let mut table = write_txn
            .open_table(SESSION_KV)
            .map_err(|e| db.err(format!("redb open session_kv: {e}")))?;
        // One `retain_in` pass removes the range and counts what it visited, so
        // no key list is buffered and a per-row iteration failure comes back as
        // an error (it is where a latched handle first shows up) instead of
        // being skipped.
        let mut removed: u64 = 0;
        let range_start = (session_id, start.to_string());
        if let Some(end) = end {
            let range_end = (session_id, end.to_string());
            table
                .retain_in(range_start..range_end, |_, _| {
                    removed += 1;
                    false
                })
                .map_err(|e| db.err(format!("redb kv_delete_range: {e}")))?;
        } else {
            let range_end = (session_range_end(session_id), String::new());
            table
                .retain_in(range_start..range_end, |_, _| {
                    removed += 1;
                    false
                })
                .map_err(|e| db.err(format!("redb kv_delete_range: {e}")))?;
        }
        removed
    };
    write_txn
        .commit()
        .map_err(|e| db.err(format!("redb commit kv_delete_range: {e}")))?;
    debug!(
        session_id,
        start_len = start.len(),
        bounded = end.is_some(),
        removed = count,
        "kv_delete_range done"
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
    db: &Store,
    session_id: u64,
    start: &str,
    end: Option<&str>,
) -> io::Result<Vec<(String, Vec<u8>)>> {
    let read_txn = db
        .begin_read()
        .map_err(|e| db.err(format!("redb read txn: {e}")))?;
    let table = read_txn
        .open_table(SESSION_KV)
        .map_err(|e| db.err(format!("redb open session_kv: {e}")))?;
    let range = if let Some(end) = end {
        let range_start = (session_id, start.to_string());
        let range_end = (session_id, end.to_string());
        table
            .range((range_start)..(range_end))
            .map_err(|e| db.err(format!("redb range kv_get_range: {e}")))?
    } else {
        let range_start = (session_id, start.to_string());
        let range_end = (session_range_end(session_id), String::new());
        table
            .range((range_start)..(range_end))
            .map_err(|e| db.err(format!("redb range kv_get_range: {e}")))?
    };
    let mut results = Vec::new();
    for result in range {
        let (key, value) = result.map_err(|e| db.err(format!("redb iter kv_get_range: {e}")))?;
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
    db: &Store,
    session_id: u64,
    start: Option<&str>,
    end: Option<&str>,
) -> io::Result<Vec<String>> {
    let read_txn = db
        .begin_read()
        .map_err(|e| db.err(format!("redb read txn: {e}")))?;
    let table = read_txn
        .open_table(SESSION_KV)
        .map_err(|e| db.err(format!("redb open session_kv: {e}")))?;
    let range: KvRangeIter<'_> = match (start, end) {
        (Some(start), Some(end)) => {
            let range_start = (session_id, start.to_string());
            let range_end = (session_id, end.to_string());
            Box::new(
                table
                    .range((range_start)..(range_end))
                    .map_err(|e| db.err(format!("redb range kv_list: {e}")))?,
            )
        }
        (Some(start), None) => {
            let range_start = (session_id, start.to_string());
            let range_end = (session_range_end(session_id), String::new());
            Box::new(
                table
                    .range((range_start)..(range_end))
                    .map_err(|e| db.err(format!("redb range kv_list: {e}")))?,
            )
        }
        (None, Some(end)) => {
            let range_start = (session_id, String::new());
            let range_end = (session_id, end.to_string());
            Box::new(
                table
                    .range((range_start)..(range_end))
                    .map_err(|e| db.err(format!("redb range kv_list: {e}")))?,
            )
        }
        (None, None) => {
            let range_start = (session_id, String::new());
            let range_end = (session_range_end(session_id), String::new());
            Box::new(
                table
                    .range((range_start)..(range_end))
                    .map_err(|e| db.err(format!("redb range kv_list: {e}")))?,
            )
        }
    };
    let mut keys = Vec::new();
    for result in range {
        let (key, _) = result.map_err(|e| db.err(format!("redb iter kv_list: {e}")))?;
        keys.push(key.value().1);
    }
    Ok(keys)
}

/// Count keys in the given session, optionally filtered by prefix.
///
/// When `prefix` is `Some(p)`, counts the keys that START WITH `p` (see the
/// private `prefix_end` helper for the bound, and why it cannot be a byte
/// sentinel). When `prefix` is `None`, counts every key of the session.
///
/// # Errors
///
/// Returns Err if the read transaction, table open, range iteration, or
/// row decode fails.
pub fn kv_count(db: &Store, session_id: u64, prefix: Option<&str>) -> io::Result<u64> {
    let read_txn = db
        .begin_read()
        .map_err(|e| db.err(format!("redb read txn: {e}")))?;
    let table = read_txn
        .open_table(SESSION_KV)
        .map_err(|e| db.err(format!("redb open session_kv: {e}")))?;
    // A prefix scan is the range [`prefix`]..[`prefix_end`], where the upper
    // bound is the smallest key that sorts after every key carrying the prefix.
    // The `starts_with` filter then makes the count exact even when the bound is
    // the session end (an empty prefix, or a prefix that cannot be advanced).
    let lower = (session_id, prefix.map_or_default(str::to_string));
    let upper = match prefix.and_then(prefix_end) {
        Some(end) => (session_id, end),
        None => (session_range_end(session_id), String::new()),
    };
    let range = table
        .range(lower..upper)
        .map_err(|e| db.err(format!("redb range kv_count: {e}")))?;
    let mut count: u64 = 0;
    for result in range {
        let (key, _) = result.map_err(|e| db.err(format!("redb iter kv_count: {e}")))?;
        if prefix.is_none_or(|prefix| key.value().1.starts_with(prefix)) {
            count += 1;
        }
    }
    Ok(count)
}

/// The exclusive upper bound that covers every key continuing `prefix`: the
/// prefix minus its last character, plus that character's successor.
///
/// Returning `None` means "no bound this can compute" — an empty prefix (which
/// covers every key, so the caller uses the session end) or a prefix ending in
/// `char::MAX` (which no character can advance past). The caller filters on
/// `starts_with` as well, so neither case can overcount.
///
/// The successor is taken over CHARACTERS, not bytes: a byte sentinel like a
/// trailing `0xFF` has no valid UTF-8 form, and rounding it to `U+FFFD` (as a
/// lossy conversion does) puts the bound *below* every key whose next character
/// encodes at or above `EF BF BD` — which silently drops every key continuing
/// with `U+FFFD`, `U+FFFE`/`U+FFFF`, or any astral-plane character (most emoji).
fn prefix_end(prefix: &str) -> Option<String> {
    let mut chars = prefix.chars();
    let last = chars.next_back()?;
    let head = chars.as_str();
    let next = char::from_u32(u32::from(last) + 1)?;
    Some(format!("{head}{next}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Store;

    /// An in-memory store: these tests exercise the key-space arithmetic, not
    /// redb's on-disk format, so they stay off the filesystem boundary.
    fn store() -> Store {
        Store::with_backend(redb::backends::InMemoryBackend::new())
            .expect("create the in-memory store")
    }

    #[test]
    fn kv_count_prefix_sees_every_key_that_shares_it() {
        // Regression guard for the byte-sentinel bound this replaced: appending
        // 0xFF and rounding it lossily to U+FFFD (EF BF BD) puts the upper bound
        // BELOW every key whose next character encodes at or above EF BF BD, so
        // an emoji — or U+FFFD itself — silently vanished from the count.
        let db = store();
        for key in [
            "user:alice",
            "user:bob",
            "user:\u{1F600}",  // astral plane (most emoji)
            "user:\u{FFFD}",   // the replacement character the old bound used
            "user:\u{10FFFF}", // the last scalar value
            "other",
        ] {
            kv_set(&db, 1, key, b"v").unwrap();
        }

        assert_eq!(
            kv_count(&db, 1, Some("user:")).unwrap(),
            5,
            "every key sharing the prefix must be counted, whatever its next character"
        );
    }

    #[test]
    fn kv_count_without_a_prefix_counts_the_session() {
        let db = store();
        for key in ["a", "b", ""] {
            kv_set(&db, 1, key, b"v").unwrap();
        }
        kv_set(&db, 2, "a", b"v").unwrap();

        assert_eq!(kv_count(&db, 1, None).unwrap(), 3);
        assert_eq!(kv_count(&db, 1, Some("")).unwrap(), 3);
    }

    #[test]
    fn kv_count_prefix_is_per_session() {
        // The bound is a `(session_id, …)` tuple, so a prefix scan must never
        // pick up a neighbour's rows.
        let db = store();
        kv_set(&db, 1, "p:a", b"v").unwrap();
        kv_set(&db, 2, "p:a", b"v").unwrap();
        kv_set(&db, 2, "p:b", b"v").unwrap();

        assert_eq!(kv_count(&db, 1, Some("p:")).unwrap(), 1);
        assert_eq!(kv_count(&db, 2, Some("p:")).unwrap(), 2);
    }

    #[test]
    fn kv_count_prefix_ending_in_the_max_char_is_still_exact() {
        // `U+10FFFF` has no successor, so `prefix_end` declines and the caller
        // falls back to the session end plus a `starts_with` filter.
        let db = store();
        let prefix = "x\u{10FFFF}";
        kv_set(&db, 1, &format!("{prefix}a"), b"v").unwrap();
        kv_set(&db, 1, &format!("{prefix}b"), b"v").unwrap();
        kv_set(&db, 1, "x", b"v").unwrap();

        assert_eq!(kv_count(&db, 1, Some(prefix)).unwrap(), 2);
    }

    #[test]
    fn kv_delete_range_removes_and_counts_exactly_its_range() {
        let db = store();
        for key in ["a", "b", "c", "d"] {
            kv_set(&db, 1, key, b"v").unwrap();
        }
        kv_set(&db, 2, "b", b"v").unwrap();

        let removed = kv_delete_range(&db, 1, "b", Some("d")).unwrap();
        assert_eq!(removed, 2, "the range is start-inclusive, end-exclusive");
        assert_eq!(kv_list(&db, 1, None, None).unwrap(), vec!["a", "d"]);
        assert_eq!(
            kv_get(&db, 2, "b").unwrap(),
            Some(b"v".to_vec()),
            "another session's row must survive"
        );
    }

    #[test]
    fn kv_delete_range_without_an_end_stops_at_the_session_bound() {
        let db = store();
        for key in ["m", "n", "z"] {
            kv_set(&db, 1, key, b"v").unwrap();
        }
        kv_set(&db, 2, "n", b"v").unwrap();

        assert_eq!(kv_delete_range(&db, 1, "n", None).unwrap(), 2);
        assert_eq!(kv_list(&db, 1, None, None).unwrap(), vec!["m"]);
    }

    #[test]
    fn kv_list_and_count_agree() {
        // The two read paths must agree on what a prefix covers; they used to
        // derive their bounds differently.
        let db = store();
        for key in ["k1", "k2", "k\u{1F600}", "other"] {
            kv_set(&db, 1, key, b"v").unwrap();
        }

        let listed = kv_list(&db, 1, Some("k"), Some("l")).unwrap();
        assert_eq!(listed.len() as u64, kv_count(&db, 1, Some("k")).unwrap());
    }
}
