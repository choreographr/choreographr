//! Session-record and turn CRUD: everything that reads or writes a
//! [`SessionRecord`] or the turns and tombstones owned by a session.
//!
//! The `sessions` and `session_turns` tables, their MessagePack/zstd value
//! encoding, the deletion tombstones, and the retry wrappers are one cohesive
//! unit — but they are only one of several things `db` persists (credentials,
//! catalog state, the keystore binding, and the per-session KV store are the
//! others). Splitting them out keeps the storage plumbing (`schema`, `open`,
//! `migrate`, `attachments`, `codec`, `store`) from carrying all of the
//! per-entity CRUD. The table definitions stay in `schema`, shared with the
//! migration code, and are reached here via `super::schema`.
//!
//! The `1 → 2` turn-value migration is deliberately NOT here: it is part of the
//! schema-migration chain (it runs under the `Migration` framework and shares
//! `ZSTD_FRAME_MAGIC`/`zstd_encode` with the codec), so it stays beside the
//! migration machinery in `migrate`, and reaches the turn table through
//! `SESSION_TURNS`.

use std::io;

use choreo_proto::{ContextConfig, ReasoningProducer, Turn};
use redb::{ReadableDatabase, ReadableTable};
use serde::{Deserialize, Serialize};
use tracing::{debug, error, info, warn};

use super::attachments;
use super::codec::{zstd_decode, zstd_encode};
use super::schema::{DELETED_SESSIONS, SESSION_KV, SESSION_TURNS, SESSIONS, session_range_end};
use super::store::Store;

/// A persisted session record: the durable form of a session's identity and
/// configuration. Stored MessagePack-encoded under its session id in
/// `SESSIONS`; read back by `read_session`/`read_all_sessions`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRecord {
    /// User-editable session title; `None` until set.
    pub title: Option<String>,
    /// The model slug currently selected for the session; `None` until one is
    /// chosen.
    pub selected_model: Option<String>,
    /// The parent's session id for a sub-session, `None` for a root session.
    pub parent_session_id: Option<u64>,
    /// The session's working directory; `None` to inherit the parent's, else the
    /// daemon's cwd.
    pub working_dir: Option<String>,
    /// Number of turns in the session, taken from the in-memory turn map when
    /// the record is written.
    ///
    /// Advisory, not a bound: `read_turns` finds a session's turns with a range
    /// scan over the turns table, so this count never limits what is read. It
    /// can therefore lag the turn rows — the turns are checkpointed per
    /// agent-loop iteration, while this record is only rewritten when a request
    /// finishes, so a crash mid-request leaves a stale (lower) count beside the
    /// turn rows it did not account for. The next record write corrects it.
    pub turn_count: u32,
    /// Creation time, Unix-epoch-milliseconds.
    pub created_at: i64,
    /// Most recent modification time, Unix-epoch-milliseconds (status changes,
    /// turn completion, title/model edits).  Persisted so the sessions list
    /// keeps its "newest first" ordering across daemon restarts.
    pub last_modified: i64,
    /// Names of the tool groups loaded for this session, persisted so a restart
    /// restores the same tool set.
    pub active_tool_groups: Vec<String>,
    /// Context-file discovery settings for this session; `#[serde(default)]`
    /// keeps older records decoding.
    #[serde(default)]
    pub context_config: ContextConfig,
    /// The inference account this session is bound to, `None` when unbound;
    /// `#[serde(default)]` keeps older records decoding.
    #[serde(default)]
    pub account_name: Option<String>,
    /// The selected reasoning-effort slug, `None` for the model's default;
    /// `#[serde(default)]` keeps older records decoding.
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
pub fn write_session(db: &Store, session_id: u64, record: &SessionRecord) -> io::Result<()> {
    let write_txn = db
        .begin_write()
        .map_err(|e| db.err(format!("redb write txn: {e}")))?;
    {
        let mut table = write_txn
            .open_table(SESSIONS)
            .map_err(|e| db.err(format!("redb open sessions: {e}")))?;
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
            .map_err(|e| db.err(format!("redb get session: {e}")))?
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
            .map_err(|e| db.err(format!("codec encode session: {e}")))?;
        table
            .insert(session_id, payload.as_slice())
            .map_err(|e| db.err(format!("redb insert session: {e}")))?;
    }
    write_txn
        .commit()
        .map_err(|e| db.err(format!("redb commit session: {e}")))?;
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
    db: &Store,
    session_id: u64,
    pinned: bool,
    archived_at: Option<i64>,
) -> io::Result<()> {
    debug!("update_session_flags: id={session_id} pinned={pinned}");
    let write_txn = db
        .begin_write()
        .map_err(|e| db.err(format!("redb write txn: {e}")))?;
    {
        let mut table = write_txn
            .open_table(SESSIONS)
            .map_err(|e| db.err(format!("redb open sessions: {e}")))?;
        // Read the current row and decode it, so the OTHER fields survive the
        // flag update untouched. An absent row is a no-op (a deleted session
        // has no record to flag).
        let Some(guard) = table
            .get(session_id)
            .map_err(|e| db.err(format!("redb get session: {e}")))?
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
            .map_err(|e| db.err(format!("codec encode session: {e}")))?;
        table
            .insert(session_id, payload.as_slice())
            .map_err(|e| db.err(format!("redb insert session: {e}")))?;
    }
    write_txn
        .commit()
        .map_err(|e| db.err(format!("redb commit session: {e}")))?;
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
pub fn read_session(db: &Store, session_id: u64) -> io::Result<Option<SessionRecord>> {
    debug!("read_session: id={}", session_id);
    let read_txn = db
        .begin_read()
        .map_err(|e| db.err(format!("redb read txn: {e}")))?;
    let table = read_txn
        .open_table(SESSIONS)
        .map_err(|e| db.err(format!("redb open sessions: {e}")))?;
    match table
        .get(session_id)
        .map_err(|e| db.err(format!("redb get session: {e}")))?
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
pub fn read_all_sessions(db: &Store) -> io::Result<Vec<(u64, SessionRecord)>> {
    debug!("read_all_sessions");
    let read_txn = db.begin_read().map_err(|e| {
        let msg = format!("redb read txn: {e}");
        error!("read_all_sessions: {msg}");
        db.err(msg)
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
            return Err(db.err(msg));
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

/// Delete a session record, its turns, and its attachments.
///
/// # Errors
///
/// Returns Err if the write transaction, table open, row removal, or
/// commit fails.
pub fn delete_session(db: &Store, session_id: u64) -> io::Result<()> {
    debug!("delete_session: id={}", session_id);
    let write_txn = db
        .begin_write()
        .map_err(|e| db.err(format!("redb write txn: {e}")))?;
    {
        let mut sessions = write_txn
            .open_table(SESSIONS)
            .map_err(|e| db.err(format!("redb open sessions: {e}")))?;
        sessions
            .remove(session_id)
            .map_err(|e| db.err(format!("redb remove session: {e}")))?;
    }
    {
        let mut turns = write_txn
            .open_table(SESSION_TURNS)
            .map_err(|e| db.err(format!("redb open turns: {e}")))?;
        // Bounded range scan over just this session's turn ids instead of
        // iterating the whole table (the old full-table scan made each delete
        // O(total turns) — costly for the largest sessions).
        let keys_to_remove: Vec<(u64, u32)> = turns
            .range((session_id, 0u32)..(session_range_end(session_id), 0u32))
            .map_err(|e| db.err(format!("redb range turns: {e}")))?
            .filter_map(std::result::Result::ok)
            .map(|(key, _)| key.value())
            .collect();
        for key in keys_to_remove {
            turns
                .remove(key)
                .map_err(|e| db.err(format!("redb remove turn: {e}")))?;
        }
    }
    {
        let mut kv_table = write_txn
            .open_table(SESSION_KV)
            .map_err(|e| db.err(format!("redb open session_kv: {e}")))?;
        let kv_keys: Vec<(u64, String)> = kv_table
            .range((session_id, String::new())..(session_range_end(session_id), String::new()))
            .map_err(|e| db.err(format!("redb range session_kv: {e}")))?
            .filter_map(std::result::Result::ok)
            .map(|(k, _)| k.value())
            .collect();
        for key in kv_keys {
            kv_table
                .remove(key)
                .map_err(|e| db.err(format!("redb remove session_kv: {e}")))?;
        }
    }
    // Range-remove this session's attachment rows too — the same
    // (session_id, …)..(session_range_end(session_id), …) bound as the
    // turns/KV deletes above, so no orphaned image bytes survive a delete.
    attachments::delete_session_attachments(db, &write_txn, session_id)?;
    write_txn
        .commit()
        .map_err(|e| db.err(format!("redb commit delete: {e}")))?;
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
/// Called once `handle_session_exited` has deleted the record the
/// still-shutting-down thread re-created, so the tombstone does not
/// accumulate.
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
pub fn write_turn(db: &Store, session_id: u64, turn_id: u32, turn: &Turn) -> io::Result<()> {
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
        rmp_serde::to_vec_named(&storage).map_err(|e| db.err(format!("codec encode turn: {e}")))?;
    // Serialize first, then compress the whole blob (see [`zstd_encode`]).
    let compressed = zstd_encode(&payload);
    let write_txn = db
        .begin_write()
        .map_err(|e| db.err(format!("redb write txn: {e}")))?;
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
        attachments::write_turn_attachments(db, &write_txn, session_id, turn_id, turn)?;
        let mut table = write_txn
            .open_table(SESSION_TURNS)
            .map_err(|e| db.err(format!("redb open turns: {e}")))?;
        table
            .insert((session_id, turn_id), compressed.as_slice())
            .map_err(|e| db.err(format!("redb insert turn: {e}")))?;
    }
    write_txn
        .commit()
        .map_err(|e| db.err(format!("redb commit turn: {e}")))?;
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
pub fn read_turns(db: &Store, session_id: u64) -> io::Result<Vec<(u32, Turn)>> {
    let read_txn = db
        .begin_read()
        .map_err(|e| db.err(format!("redb read txn: {e}")))?;
    let table = match read_txn.open_table(SESSION_TURNS) {
        Ok(t) => t,
        // A fresh database that has never persisted a turn has no table yet:
        // that is an empty session history, not an error (mirrors
        // `read_all_sessions`'s missing-`sessions` handling).
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
        Err(e) => return Err(db.err(format!("redb open turns: {e}"))),
    };
    // The attachments table is opened in the SAME read transaction as the turns
    // table so both see a consistent snapshot — a turn and its attachment rows
    // were committed atomically, so re-attachment can never observe a half-
    // written turn. A fresh database has no table yet (created lazily on first
    // write), so a missing table reads as empty.
    let attachments = match read_txn.open_table(attachments::SESSION_ATTACHMENTS) {
        Ok(t) => Some(t),
        Err(redb::TableError::TableDoesNotExist(_)) => None,
        Err(e) => return Err(db.err(format!("redb open session_attachments: {e}"))),
    };
    let mut turns: Vec<(u32, Turn)> = Vec::new();
    // Bounded range scan over exactly this session's turn ids rather than
    // iterating (and, since schema 2, decompressing) every turn in every
    // session — the old full-table scan made each read O(total turns) and
    // wasted zstd decode cycles on unrelated sessions' blobs.
    let iter = table
        .range((session_id, 0u32)..(session_range_end(session_id), 0u32))
        .map_err(|e| db.err(format!("redb range turns: {e}")))?;
    for result in iter {
        let (key, value) = result.map_err(|e| db.err(format!("redb iter item: {e}")))?;
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
                    db,
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

/// Delete all turns of a session and its attachment rows.
///
/// # Errors
///
/// Returns Err if the write transaction, table open/iteration, row
/// removal, or commit fails.
pub fn delete_session_turns(db: &Store, session_id: u64) -> io::Result<()> {
    let write_txn = db
        .begin_write()
        .map_err(|e| db.err(format!("redb write txn: {e}")))?;
    {
        let mut table = write_txn
            .open_table(SESSION_TURNS)
            .map_err(|e| db.err(format!("redb open turns: {e}")))?;
        let keys_to_remove: Vec<(u64, u32)> = table
            .iter()
            .map_err(|e| db.err(format!("redb iter turns: {e}")))?
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
                .map_err(|e| db.err(format!("redb remove turn: {e}")))?;
        }
    }
    // Deleting all of a session's turns must also remove that session's
    // attachment rows, so no orphaned image bytes accumulate when turns are
    // cleared without deleting the whole session.
    attachments::delete_session_attachments(db, &write_txn, session_id)?;
    write_txn
        .commit()
        .map_err(|e| db.err(format!("redb commit delete turns: {e}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::attachments::SESSION_ATTACHMENTS;
    use crate::db::codec::ZSTD_FRAME_MAGIC;
    use crate::db::schema::META;
    use choreo_proto::{DisplayedImageRecord, ImageMetadata, ImageReference, ToolResultRecord};

    /// Read the current `next_session_id` from the DB and atomically
    /// increment it.  Only used by tests — production code derives the
    /// next ID from max(existing keys) + 1 at startup.
    fn next_session_id(db: &Store) -> io::Result<u64> {
        let write_txn = db
            .begin_write()
            .map_err(|e| db.err(format!("redb write txn: {e}")))?;
        let current = {
            let mut table = write_txn
                .open_table(META)
                .map_err(|e| db.err(format!("redb open meta: {e}")))?;
            let current = table
                .get("next_session_id")
                .map_err(|e| db.err(format!("redb get meta: {e}")))?
                .map_or(1, |g| g.value());
            table
                .insert("next_session_id", current.wrapping_add(1))
                .map_err(|e| db.err(format!("redb set meta: {e}")))?;
            current
        };
        write_txn
            .commit()
            .map_err(|e| db.err(format!("redb commit meta: {e}")))?;
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
            reasoning_artifact: None,
            reasoning_producer: None,
        }
    }

    #[test]
    fn round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let db = Store::from_db(redb::Database::create(dir.path().join("test.redb")).unwrap());

        let id = next_session_id(&db).unwrap();
        assert_eq!(id, 1);

        let record = SessionRecord {
            title: Some("test session".into()),
            selected_model: Some("gpt-4".into()),
            reasoning_effort: None,
            parent_session_id: None,
            working_dir: Some("/tmp".into()),
            turn_count: 1,
            created_at: 1_234_567_890_000,
            last_modified: 1_234_567_890_000,
            active_tool_groups: vec!["core".into(), "git".into()],
            context_config: ContextConfig::default(),
            account_name: None,
            last_response_id: None,
            last_response_id_producer: None,
            pinned: false,
            archived_at: None,
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

        // The v2 codec stores the turn as a zstd-compressed MessagePack frame,
        // not raw MessagePack — the whole point of the schema-2 change.
        {
            let read_txn = db.begin_read().unwrap();
            let table = read_txn.open_table(SESSION_TURNS).unwrap();
            let guard = table.get((id, 0u32)).unwrap().unwrap();
            let v = guard.value();
            assert!(
                v.starts_with(&ZSTD_FRAME_MAGIC),
                "turns must be stored as zstd frames in schema 2"
            );
        }

        let turns = read_turns(&db, id).unwrap();
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].1, turn);

        let id2 = next_session_id(&db).unwrap();
        assert_eq!(id2, 2);

        delete_session(&db, id).unwrap();
        assert!(read_session(&db, id).unwrap().is_none());
        assert_eq!(
            read_turns(&db, id).unwrap(),
            [] as [(u32, choreo_proto::Turn); 0]
        );

        drop(db);
    }

    #[test]
    fn turn_image_bytes_split_into_attachments_and_reattached() {
        // Persistence boundary contract: a turn's image bytes (display + vision)
        // are split OUT of the zstd-compressed session_turns blob into the raw
        // session_attachments table on write, and re-attached on read so the
        // round-tripped turn matches the original byte-for-byte.
        let dir = tempfile::tempdir().unwrap();
        let db = Store::from_db(redb::Database::create(dir.path().join("test.redb")).unwrap());
        let id = 1u64;

        let mut turn = dummy_turn();
        // A display image with non-empty bytes at index 0 → slot `d0`.
        turn.displayed_images = vec![DisplayedImageRecord {
            metadata: ImageMetadata {
                mime_type: "image/png".into(),
                width: 32,
                height: 32,
                byte_len: 5,
                alt: None,
            },
            data: b"\x89PNG\r".to_vec(),
            tool_call_id: Some("call_disp".into()),
        }];
        // A tool result carrying a vision image with non-empty bytes → slot
        // `r<call_id>`.
        turn.tool_results = vec![ToolResultRecord {
            call_id: "call_v".into(),
            name: "read_image".into(),
            content: "image".into(),
            is_error: false,
            invocation_description: "read_image".into(),
            image: Some(ImageReference {
                path: "/tmp/foo.png".into(),
                mime_type: "image/png".into(),
                width: 16,
                height: 16,
                data: b"\x89PNG-vision".to_vec(),
            }),
        }];

        write_turn(&db, id, 0, &turn).unwrap();

        // (a) The stored session_turns blob decodes to a byte-less turn: the
        // zstd frame must NOT carry the image bytes.
        {
            let read_txn = db.begin_read().unwrap();
            let table = read_txn.open_table(SESSION_TURNS).unwrap();
            let guard = table.get((id, 0u32)).unwrap().unwrap();
            let blob = guard.value();
            assert!(blob.starts_with(&ZSTD_FRAME_MAGIC));
            let decoded: Turn = rmp_serde::from_slice(&zstd_decode(blob).unwrap()).unwrap();
            assert_eq!(decoded.displayed_images[0].data, Vec::<u8>::new());
            assert_eq!(
                decoded.tool_results[0].image.as_ref().unwrap().data,
                Vec::<u8>::new()
            );
        }
        // (b) The bytes ARE present in the session_attachments table at the
        // expected slots.
        {
            let read_txn = db.begin_read().unwrap();
            let table = read_txn.open_table(SESSION_ATTACHMENTS).unwrap();
            let d0 = table.get((id, 0u32, "d0".to_string())).unwrap().unwrap();
            assert_eq!(d0.value(), b"\x89PNG\r");
            let rv = table
                .get((id, 0u32, "rcall_v".to_string()))
                .unwrap()
                .unwrap();
            assert_eq!(rv.value(), b"\x89PNG-vision");
        }
        // (c) read_turns re-attaches both byte fields, matching the original.
        let turns = read_turns(&db, id).unwrap();
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].1, turn);

        // Both delete paths must clean up the attachment rows so no orphaned
        // image bytes are left behind.
        delete_session_turns(&db, id).unwrap();
        {
            let read_txn = db.begin_read().unwrap();
            let table = read_txn.open_table(SESSION_ATTACHMENTS).unwrap();
            assert!(
                table.get((id, 0u32, "d0".to_string())).unwrap().is_none(),
                "delete_session_turns must remove the display attachment"
            );
            assert!(
                table
                    .get((id, 0u32, "rcall_v".to_string()))
                    .unwrap()
                    .is_none(),
                "delete_session_turns must remove the result attachment"
            );
        }

        // Rewrite, then delete the whole session — attachments must go too.
        write_turn(&db, id, 0, &turn).unwrap();
        delete_session(&db, id).unwrap();
        {
            let read_txn = db.begin_read().unwrap();
            let table = read_txn.open_table(SESSION_ATTACHMENTS).unwrap();
            assert!(
                table.get((id, 0u32, "d0".to_string())).unwrap().is_none(),
                "delete_session must remove the display attachment"
            );
            assert!(
                table
                    .get((id, 0u32, "rcall_v".to_string()))
                    .unwrap()
                    .is_none(),
                "delete_session must remove the result attachment"
            );
        }
        drop(db);
    }

    #[test]
    fn turn_rewrite_clears_stale_attachment_slots() {
        // A re-persisted turn must not leave stale attachment rows behind: if
        // the image layout shifts between writes (a display image at index 0
        // emptied and a new one appended at index 1), the stale `d0`/`r<call_id>`
        // rows from the first write would otherwise be re-attached by
        // `read_turns` to the wrong slots. write_turn now clears the turn's
        // attachment range first, so only the current image set survives.
        let dir = tempfile::tempdir().unwrap();
        let db = Store::from_db(redb::Database::create(dir.path().join("test.redb")).unwrap());
        let id = 1u64;

        // First write: image at display index 0 (`d0`) and a tool-result vision
        // image (`rcall_v`) both carry bytes.
        let mut turn = dummy_turn();
        turn.displayed_images = vec![DisplayedImageRecord {
            metadata: ImageMetadata {
                mime_type: "image/png".into(),
                width: 1,
                height: 1,
                byte_len: 4,
                alt: None,
            },
            data: b"AAAA".to_vec(),
            tool_call_id: Some("c0".into()),
        }];
        turn.tool_results = vec![ToolResultRecord {
            call_id: "call_v".into(),
            name: "read_image".into(),
            content: "image".into(),
            is_error: false,
            invocation_description: "read_image".into(),
            image: Some(ImageReference {
                path: "/tmp/a.png".into(),
                mime_type: "image/png".into(),
                width: 1,
                height: 1,
                data: b"BBBB".to_vec(),
            }),
        }];
        write_turn(&db, id, 0, &turn).unwrap();

        // Rewrite with a shifted layout: display index 0 is now empty and a new
        // display image with bytes is appended at index 1; the tool-result
        // vision image has no bytes. The stale `d0`/`rcall_v` rows must be
        // removed, leaving only `d1`.
        let mut shifted = turn.clone();
        shifted.displayed_images[0].data.clear();
        shifted.displayed_images.push(DisplayedImageRecord {
            metadata: ImageMetadata {
                mime_type: "image/png".into(),
                width: 2,
                height: 2,
                byte_len: 4,
                alt: None,
            },
            data: b"CCCC".to_vec(),
            tool_call_id: Some("c1".into()),
        });
        shifted.tool_results[0].image.as_mut().unwrap().data.clear();
        write_turn(&db, id, 0, &shifted).unwrap();

        // Only the current image (`d1`) may persist — the stale `d0` and
        // `rcall_v` rows must be gone.
        {
            let read_txn = db.begin_read().unwrap();
            let table = read_txn.open_table(SESSION_ATTACHMENTS).unwrap();
            assert!(
                table.get((id, 0u32, "d0".to_string())).unwrap().is_none(),
                "stale d0 from the first write must be cleared on rewrite"
            );
            assert!(
                table
                    .get((id, 0u32, "rcall_v".to_string()))
                    .unwrap()
                    .is_none(),
                "stale rcall_v from the first write must be cleared on rewrite"
            );
            let d1 = table.get((id, 0u32, "d1".to_string())).unwrap().unwrap();
            assert_eq!(d1.value(), b"CCCC");
        }

        // read_turns re-attaches exactly the current set: display index 0 stays
        // empty (no stale bytes leak in), index 1 gets its bytes, and the
        // tool-result image stays byte-less.
        let turns = read_turns(&db, id).unwrap();
        assert_eq!(turns.len(), 1);
        let read = &turns[0].1;
        assert_eq!(read.displayed_images[0].data, Vec::<u8>::new());
        assert_eq!(read.displayed_images[1].data, b"CCCC");
        assert_eq!(
            read.tool_results[0].image.as_ref().unwrap().data,
            Vec::<u8>::new()
        );
        drop(db);
    }

    #[test]
    fn session_record_last_response_id_round_trips() {
        // Phase 4c persistence: the response id written to the record must
        // survive a write/read cycle so ResponseId-policy models chain across
        // user turns even after a daemon restart.
        let dir = tempfile::tempdir().unwrap();
        let db = Store::from_db(redb::Database::create(dir.path().join("test.redb")).unwrap());
        let id = 1u64;
        let record = SessionRecord {
            title: Some("t".into()),
            selected_model: None,
            reasoning_effort: None,
            parent_session_id: None,
            working_dir: None,
            turn_count: 0,
            created_at: 1,
            last_modified: 1,
            active_tool_groups: vec![],
            context_config: ContextConfig::default(),
            account_name: None,
            last_response_id: Some("resp_1".into()),
            last_response_id_producer: Some(ReasoningProducer {
                provider_slug: "openai".into(),
                model: "gpt-5.4".into(),
            }),
            pinned: false,
            archived_at: None,
        };
        write_session(&db, id, &record).unwrap();

        let read = read_session(&db, id).unwrap().unwrap();
        assert_eq!(read.last_response_id.as_deref(), Some("resp_1"));
        assert_eq!(
            read.last_response_id_producer
                .as_ref()
                .map(|p| p.model.as_str()),
            Some("gpt-5.4"),
            "response id provenance must survive the write/read cycle",
        );
        assert_eq!(read.title.as_deref(), Some("t"));
    }

    /// Build a minimal valid session record for the flag tests.
    fn flag_record() -> SessionRecord {
        SessionRecord {
            title: Some("flags".into()),
            selected_model: None,
            reasoning_effort: None,
            parent_session_id: None,
            working_dir: None,
            turn_count: 3,
            created_at: 1000,
            last_modified: 2000,
            active_tool_groups: vec!["core".into()],
            context_config: ContextConfig::default(),
            account_name: Some("acct".into()),
            last_response_id: None,
            last_response_id_producer: None,
            pinned: false,
            archived_at: None,
        }
    }

    #[test]
    fn write_session_preserves_daemon_flags() {
        // The session thread writes the FULL record (with pinned=false,
        // archived_at=None — it never knows the daemon's flags); a full-record
        // write must NOT clobber flags the daemon set via update_session_flags.
        let dir = tempfile::tempdir().unwrap();
        let db = Store::from_db(redb::Database::create(dir.path().join("test.redb")).unwrap());
        write_session(&db, 1, &flag_record()).unwrap();

        update_session_flags(&db, 1, true, Some(4242)).unwrap();
        // Confirm the daemon's update landed.
        let after_update = read_session(&db, 1).unwrap().unwrap();
        assert!(after_update.pinned);
        assert_eq!(after_update.archived_at, Some(4242));

        // Now the session thread writes a full record that tries to reset the
        // flags (the default `flag_record` has pinned=false/archived_at=None).
        let mut reset_attempt = flag_record();
        reset_attempt.turn_count = 9; // a genuine field change must persist
        reset_attempt.pinned = false;
        reset_attempt.archived_at = None;
        write_session(&db, 1, &reset_attempt).unwrap();

        let after_write = read_session(&db, 1).unwrap().unwrap();
        assert!(
            after_write.pinned,
            "a full-record write must preserve the daemon-set pinned flag"
        );
        assert_eq!(
            after_write.archived_at,
            Some(4242),
            "a full-record write must preserve the daemon-set archived_at"
        );
        assert_eq!(
            after_write.turn_count, 9,
            "the write's own fields must still take effect"
        );
    }

    #[test]
    fn update_session_flags_round_trips_and_preserves_other_fields() {
        let dir = tempfile::tempdir().unwrap();
        let db = Store::from_db(redb::Database::create(dir.path().join("test.redb")).unwrap());
        let record = flag_record();
        write_session(&db, 7, &record).unwrap();

        update_session_flags(&db, 7, true, Some(999)).unwrap();
        let read = read_session(&db, 7).unwrap().unwrap();
        assert!(read.pinned);
        assert_eq!(read.archived_at, Some(999));
        // Every other field is untouched.
        assert_eq!(read.title, record.title);
        assert_eq!(read.turn_count, record.turn_count);
        assert_eq!(read.created_at, record.created_at);
        assert_eq!(read.last_modified, record.last_modified);
        assert_eq!(read.active_tool_groups, record.active_tool_groups);
        assert_eq!(read.account_name, record.account_name);

        // Clearing works too (unpin + unarchive).
        update_session_flags(&db, 7, false, None).unwrap();
        let cleared = read_session(&db, 7).unwrap().unwrap();
        assert!(!cleared.pinned);
        assert_eq!(cleared.archived_at, None);
    }

    #[test]
    fn update_session_flags_missing_id_is_ok() {
        // A missing row (deleted session, or a race) is a harmless no-op, not
        // an error — the caller's index update still runs and other rows are
        // untouched.
        let dir = tempfile::tempdir().unwrap();
        let db = Store::from_db(redb::Database::create(dir.path().join("test.redb")).unwrap());
        // Create the SESSIONS table with a real row, so the reads below can
        // open it (a fresh DB has no table until the first committed write).
        write_session(&db, 1, &flag_record()).unwrap();

        update_session_flags(&db, 123, true, Some(1)).unwrap();
        assert!(read_session(&db, 123).unwrap().is_none());
        // The unrelated existing row is untouched by the no-op update.
        let one = read_session(&db, 1).unwrap().unwrap();
        assert!(!one.pinned);
        assert_eq!(one.archived_at, None);
    }

    #[test]
    fn read_turns_skips_corrupt_entries() {
        let dir = tempfile::tempdir().unwrap();
        let db = Store::from_db(redb::Database::create(dir.path().join("test.redb")).unwrap());
        let id = 1u64;

        // Write a valid turn at index 0
        let valid_turn = dummy_turn();
        write_turn(&db, id, 0, &valid_turn).unwrap();

        // Manually insert a corrupt blob at index 1 (neither a valid zstd
        // frame nor valid MessagePack) to exercise the skip-and-warn path.
        {
            let write_txn = db.begin_write().unwrap();
            {
                let mut table = write_txn.open_table(SESSION_TURNS).unwrap();
                table
                    .insert((id, 1u32), b"not a zstd frame".as_slice())
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
    fn read_session_skips_corrupt_record_with_warning() {
        // A corrupt/legacy session record must not fail the read (or the
        // daemon): read_session treats undecodable data as absent — warn and
        // return None — the same policy as the batch reads.
        let dir = tempfile::tempdir().unwrap();
        let db = Store::from_db(redb::Database::create(dir.path().join("test.redb")).unwrap());
        {
            let write_txn = db.begin_write().unwrap();
            {
                let mut table = write_txn.open_table(SESSIONS).unwrap();
                table
                    .insert(42u64, b"not a session record".as_slice())
                    .unwrap();
            }
            write_txn.commit().unwrap();
        }
        assert!(
            read_session(&db, 42).unwrap().is_none(),
            "undecodable record must read as absent, not error"
        );
        // A genuinely missing session is indistinguishable (also None).
        assert!(read_session(&db, 99).unwrap().is_none());
    }

    #[test]
    fn purge_removes_tombstoned_resurrected_record() {
        // Simulates the crash window: a session is deleted (tombstone
        // written), its still-shutting-down thread re-creates the record, and
        // the daemon dies before the delete is finalized.  The startup purge
        // must remove the record so the deleted session cannot resurface.
        let dir = tempfile::tempdir().unwrap();
        let db = Store::from_db(redb::Database::create(dir.path().join("test.redb")).unwrap());
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
            last_response_id: None,
            last_response_id_producer: None,
            pinned: false,
            archived_at: None,
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
        let db = Store::from_db(redb::Database::create(dir.path().join("test.redb")).unwrap());
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
            last_response_id: None,
            last_response_id_producer: None,
            pinned: false,
            archived_at: None,
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
        let db = Store::from_db(redb::Database::create(dir.path().join("test.redb")).unwrap());
        assert_eq!(purge_tombstoned_sessions(&db).unwrap(), 0);
    }
}
