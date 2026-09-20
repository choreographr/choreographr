//! Session-record and turn CRUD: everything that reads or writes a
//! [`SessionRecord`] or the turns and tombstones owned by a session.
//!
//! The `sessions` and `session_turns` tables, their MessagePack/zstd value
//! encoding, the deletion tombstones, and the retry wrappers are one cohesive
//! unit — but they are only one of several things `db` persists (credentials,
//! catalog state, the keystore binding, and the per-session KV store are the
//! others). Splitting them out keeps the storage plumbing in `super` (schema
//! constants, `open_db`, migrations, `db_err`) from carrying all of the
//! per-entity CRUD, matching the existing `db/attachments.rs` and `db/codec.rs`
//! split. The table definitions stay in `super` (shared with the migration
//! code) and are reached here via `super::`.
//!
//! The `1 → 2` turn-value migration is deliberately NOT here: it is part of the
//! schema-migration chain (it runs under the `Migration` framework and shares
//! `ZSTD_FRAME_MAGIC`/`zstd_encode` with the codec), so it stays beside the
//! migration machinery in `super`, and reaches the turn table through
//! `SESSION_TURNS` exactly as it did before this split.

use std::io;

use choreo_proto::{ContextConfig, ReasoningProducer, Turn};
use redb::{ReadableDatabase, ReadableTable};
use serde::{Deserialize, Serialize};
use tracing::{debug, error, info, warn};

use super::attachments;
use super::codec::{zstd_decode, zstd_encode};
use super::{DELETED_SESSIONS, SESSION_KV, SESSION_TURNS, SESSIONS, db_err};

/// A persisted session record: the durable form of a session's identity and
/// configuration. Stored MessagePack-encoded under its session id in
/// [`SESSIONS`]; read back by `read_session`/`read_all_sessions`.
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
    /// Whether this session is pinned. Daemon-owned: the session thread
    /// writes the full record on every mutation, but it has no knowledge of
    /// these flags, so [`write_session`] preserves whatever the daemon last
    /// set (see the preserve logic there). `#[serde(default)]` keeps old
    /// records decoding as `false`.
    #[serde(default)]
    pub pinned: bool,
    /// When this session was archived (Unix-epoch-milliseconds), or `None`.
    /// Daemon-owned, same preserve-across-full-record-writes contract as
    /// `pinned`. `#[serde(default)]` keeps old records decoding as `None`.
    #[serde(default)]
    pub archived_at: Option<i64>,
}

/// Serialize and upsert a session record under `session_id`.
///
/// # Errors
///
/// Returns Err if msgpack encoding, the write transaction, table open,
/// insert, or commit fails.
pub fn write_session(
    db: &redb::Database,
    session_id: u64,
    record: &SessionRecord,
) -> io::Result<()> {
    let write_txn = db
        .begin_write()
        .map_err(|e| db_err(format!("redb write txn: {e}")))?;
    {
        let mut table = write_txn
            .open_table(SESSIONS)
            .map_err(|e| db_err(format!("redb open sessions: {e}")))?;
        // Preserve the daemon-owned flags. The session thread writes the FULL
        // record on every mutation, but the incoming `record` is built from
        // `SessionConfig`, which does NOT carry `pinned`/`archived_at` (the
        // daemon is their sole authority — see `update_session_flags`). Read
        // the row we are about to overwrite and copy its two flag fields onto
        // a mutable clone of the incoming record, so a full-record write can
        // never clobber what the daemon last set. An absent row (first write)
        // or an undecodable one means there is nothing to preserve — use the
        // incoming values as-is.
        let mut record = record.clone();
        if let Some(guard) = table
            .get(session_id)
            .map_err(|e| db_err(format!("redb get session: {e}")))?
        {
            match rmp_serde::from_slice::<SessionRecord>(guard.value()) {
                Ok(existing) => {
                    record.pinned = existing.pinned;
                    record.archived_at = existing.archived_at;
                }
                Err(e) => {
                    warn!(
                        session_id,
                        error = %e,
                        "undecodable existing session record; keeping incoming flags"
                    );
                }
            }
        }
        let payload = rmp_serde::to_vec_named(&record)
            .map_err(|e| db_err(format!("codec encode session: {e}")))?;
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

/// Read-modify-write ONLY the daemon-owned `pinned` and `archived_at` fields
/// of a session record, inside a single write transaction. This is how the
/// daemon mutates its flags without touching (or racing) the rest of the
/// record the session thread owns.
///
/// If the row is absent (the session does not exist, or has already been
/// deleted), this is a successful no-op: there is nothing to flag, and the
/// caller's index update is the only effect. Undecodable rows are treated the
/// same way (logged and left untouched), matching `read_session`'s
/// tolerant-read policy.
///
/// # Errors
///
/// Returns Err if the write transaction, table open, row lookup, decode of
/// the CURRENT record, encode, insert, or commit fails.
pub fn update_session_flags(
    db: &redb::Database,
    session_id: u64,
    pinned: bool,
    archived_at: Option<i64>,
) -> io::Result<()> {
    debug!("update_session_flags: id={session_id} pinned={pinned}");
    let write_txn = db
        .begin_write()
        .map_err(|e| db_err(format!("redb write txn: {e}")))?;
    {
        let mut table = write_txn
            .open_table(SESSIONS)
            .map_err(|e| db_err(format!("redb open sessions: {e}")))?;
        // Read the current row and decode it, so the OTHER fields survive the
        // flag update untouched. An absent row is a no-op (a deleted session
        // has no record to flag).
        let Some(guard) = table
            .get(session_id)
            .map_err(|e| db_err(format!("redb get session: {e}")))?
        else {
            debug!(session_id, "update_session_flags: no record, nothing to do");
            return Ok(());
        };
        let mut record = match rmp_serde::from_slice::<SessionRecord>(guard.value()) {
            Ok(record) => record,
            Err(e) => {
                warn!(
                    session_id,
                    error = %e,
                    "update_session_flags: undecodable record, leaving it untouched"
                );
                return Ok(());
            }
        };
        drop(guard);
        // Mutate ONLY the two daemon-owned fields, then re-encode the whole
        // record so every other field is preserved byte-for-byte.
        record.pinned = pinned;
        record.archived_at = archived_at;
        let payload = rmp_serde::to_vec_named(&record)
            .map_err(|e| db_err(format!("codec encode session: {e}")))?;
        table
            .insert(session_id, payload.as_slice())
            .map_err(|e| db_err(format!("redb insert session: {e}")))?;
    }
    write_txn
        .commit()
        .map_err(|e| db_err(format!("redb commit session: {e}")))?;
    Ok(())
}

/// Read a single session record. Returns `Ok(None)` both when the session
/// does not exist and when the stored record cannot be decoded — an
/// undecodable record is skipped with a warning and treated as absent, the
/// same policy as `read_all_sessions`/`read_turns`. A corrupt record is
/// unrecoverable, so it must never fail the caller (or the daemon); the
/// warning keeps the loss loud-but-non-fatal.
///
/// # Errors
///
/// Returns Err if the read transaction, table open, or row lookup fails.
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

/// Read every session record from the database. Returns an empty vector when
/// the sessions table does not exist yet (first run); undecodable records
/// are skipped with a warning, not an error.
///
/// # Errors
///
/// Returns Err if the read transaction, table iteration, or row decode of
/// a well-formed entry fails.
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
///
/// `pub(super)` so `super`'s KV range helpers and the `attachments` module can
/// keep sharing this one range-bound rule; production callers outside `db`
/// never see it.
pub(super) fn session_range_end(session_id: u64) -> u64 {
    session_id.saturating_add(1)
}

/// Delete a session record, its turns, and its attachments.
///
/// # Errors
///
/// Returns Err if the write transaction, table open, row removal, or
/// commit fails.
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
            .filter_map(std::result::Result::ok)
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
            .filter_map(std::result::Result::ok)
            .map(|(k, _)| k.value())
            .collect();
        for key in kv_keys {
            kv_table
                .remove(key)
                .map_err(|e| db_err(format!("redb remove session_kv: {e}")))?;
        }
    }
    // Range-remove this session's attachment rows too — the same
    // (session_id, …)..(session_range_end(session_id), …) bound as the
    // turns/KV deletes above, so no orphaned image bytes survive a delete.
    attachments::delete_session_attachments(&write_txn, session_id)?;
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
///
/// # Errors
///
/// Returns Err if the write transaction, table open, insert, or commit
/// fails.
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
///
/// # Errors
///
/// Returns Err if the write transaction, table open, removal, or commit
/// fails.
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
///
/// # Errors
///
/// Returns Err if the read or write transactions, table opens/iterations,
/// or commits fail.
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
        .filter_map(std::result::Result::ok)
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

/// Compress and persist a turn (payload bytes split into the attachments
/// table) under `(session_id, turn_id)`.
///
/// # Errors
///
/// Returns Err if msgpack/zstd encoding, the write transaction, table
/// opens, inserts, or commit fails.
pub fn write_turn(
    db: &redb::Database,
    session_id: u64,
    turn_id: u32,
    turn: &Turn,
) -> io::Result<()> {
    // Build a "storage view" of the turn: clone it and empty every image byte
    // field (display + vision). The bytes themselves are persisted separately
    // into SESSION_ATTACHMENTS (raw, incompressible), so the zstd-compressed
    // session_turns blob carries only the text/tool metadata. Keep all the
    // metadata (mime, width, height, path, tool_call_id, call_id) — only the
    // byte payloads move out. Slots are derivable from the byte-less turn on
    // read, so re-attachment needs no extra metadata.
    let mut storage = turn.clone();
    for img in &mut storage.displayed_images {
        img.data.clear();
    }
    for tr in &mut storage.tool_results {
        if let Some(image) = &mut tr.image {
            image.data.clear();
        }
    }
    let payload =
        rmp_serde::to_vec_named(&storage).map_err(|e| db_err(format!("codec encode turn: {e}")))?;
    // Serialize first, then compress the whole blob (see [`zstd_encode`]).
    let compressed = zstd_encode(&payload);
    let write_txn = db
        .begin_write()
        .map_err(|e| db_err(format!("redb write txn: {e}")))?;
    {
        // Persist the image/attachment bytes and the byte-less turn blob in the
        // SAME write transaction so they can never diverge: a crash leaves
        // either both written or neither (the blob and its attachments are
        // atomic as a set). The attachment step — clearing any stale slots
        // from a previous write of this turn, then inserting exactly the
        // current image set — lives in [`attachments::write_turn_attachments`]
        // so the codec/blob logic here stays focused; the ordering (clear
        // before insert) is what keeps write_turn idempotent under a shifted
        // image layout.
        attachments::write_turn_attachments(&write_txn, session_id, turn_id, turn)?;
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

/// Read all turns of a session, re-attaching payload bytes from the
/// attachments table. Returns turns sorted by `turn_id`; a missing turns
/// table reads as empty.
///
/// # Errors
///
/// Returns Err if the read transaction, table opens, iteration, or decode
/// of a well-formed entry fails.
pub fn read_turns(db: &redb::Database, session_id: u64) -> io::Result<Vec<(u32, Turn)>> {
    let read_txn = db
        .begin_read()
        .map_err(|e| db_err(format!("redb read txn: {e}")))?;
    let table = read_txn
        .open_table(SESSION_TURNS)
        .map_err(|e| db_err(format!("redb open turns: {e}")))?;
    // The attachments table is opened in the SAME read transaction as the turns
    // table so both see a consistent snapshot — a turn and its attachment rows
    // were committed atomically, so re-attachment can never observe a half-
    // written turn. A fresh database has no table yet (created lazily on first
    // write), so a missing table reads as empty.
    let attachments = match read_txn.open_table(attachments::SESSION_ATTACHMENTS) {
        Ok(t) => Some(t),
        Err(redb::TableError::TableDoesNotExist(_)) => None,
        Err(e) => return Err(db_err(format!("redb open session_attachments: {e}"))),
    };
    let mut turns: Vec<(u32, Turn)> = Vec::new();
    // Bounded range scan over exactly this session's turn ids rather than
    // iterating (and, since schema 2, decompressing) every turn in every
    // session — the old full-table scan made each read O(total turns) and
    // wasted zstd decode cycles on unrelated sessions' blobs.
    let iter = table
        .range::<(u64, u32)>((session_id, 0u32)..(session_range_end(session_id), 0u32))
        .map_err(|e| db_err(format!("redb range turns: {e}")))?;
    for result in iter {
        let (key, value) = result.map_err(|e| db_err(format!("redb iter item: {e}")))?;
        let (_, idx) = key.value();
        match zstd_decode(value.value())
            .and_then(|buf| rmp_serde::from_slice::<Turn>(&buf).map_err(io::Error::other))
        {
            Ok(mut turn) => {
                // Re-attach the split-out image bytes back into the byte-less
                // decoded turn. The helper owns the slot lookup and the
                // "missing table / missing row ⇒ leave empty" semantics (see
                // [`attachments::reattach_turn_attachments`]).
                attachments::reattach_turn_attachments(
                    attachments.as_ref(),
                    session_id,
                    idx,
                    &mut turn,
                )?;
                turns.push((idx, turn));
            }
            Err(e) => {
                tracing::warn!(session_id, turn_id = idx, error = %e, "undecodable turn, skipping");
            }
        }
    }
    // The range scan yields rows already in (session_id, turn_id) key order,
    // and every row here shares the same session, so turns come out sorted by
    // turn_id — no explicit sort needed.
    Ok(turns)
}

/// Retry a `write_turn` on transient storage errors (e.g. I/O contention)
/// with up to 3 retries and a 1ms backoff.
///
/// # Errors
///
/// Returns Err if [`write_turn`] fails on every attempt (the last error is
/// propagated).
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
            }
            Err(e) => return Err(e),
        }
    }
}

/// Delete all turns of a session and its attachment rows.
///
/// # Errors
///
/// Returns Err if the write transaction, table open/iteration, row
/// removal, or commit fails.
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
    // Deleting all of a session's turns must also remove that session's
    // attachment rows, so no orphaned image bytes accumulate when turns are
    // cleared without deleting the whole session.
    attachments::delete_session_attachments(&write_txn, session_id)?;
    write_txn
        .commit()
        .map_err(|e| db_err(format!("redb commit delete turns: {e}")))?;
    Ok(())
}

/// Retry `delete_session_turns` on transient storage errors with up to 3
/// retries and a 1ms backoff.
///
/// # Errors
///
/// Returns Err if [`delete_session_turns`] fails on every attempt (the
/// last error is propagated).
pub fn delete_session_turns_retry(db: &redb::Database, session_id: u64) -> io::Result<()> {
    let mut attempts = 0;
    loop {
        match delete_session_turns(db, session_id) {
            Ok(()) => return Ok(()),
            Err(_e) if attempts < 3 => {
                attempts += 1;
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            Err(e) => return Err(e),
        }
    }
}

/// Retry a `write_session` on transient storage errors with up to 3 retries.
///
/// # Errors
///
/// Returns Err if [`write_session`] fails on every attempt (the last error
/// is propagated).
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
            }
            Err(e) => return Err(e),
        }
    }
}
