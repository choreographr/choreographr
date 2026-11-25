//! Session records, the session-level delete, and the two child modules that
//! own the rest of a session's durable state: [`turns`] (the turn rows and their
//! attachment bytes) and [`tombstones`] (the deleted-session markers that stop a
//! shutting-down thread's re-created record from resurfacing).
//!
//! Session state is split this way because the three tables have different
//! owners and lifetimes: a record is rewritten whenever a request finishes, a
//! turn row is checkpointed per agent-loop iteration and carries a compressed
//! blob plus split-out image bytes, and a tombstone lives only across the window
//! between a delete and the session thread's exit. [`delete_session`] is the one
//! operation that spans all of them, and it does so in a single write
//! transaction so a partial delete is impossible.
//!
//! The `sessions` and `session_turns` tables themselves, and the `session_kv`
//! range bound the delete shares with the KV store, live in
//! [`schema`](super::schema). The `1 → 2` turn-value migration is deliberately
//! NOT here: it is part of the schema-migration chain (it runs under the
//! `Migration` framework and shares `ZSTD_FRAME_MAGIC`/`zstd_encode` with the
//! codec), so it stays beside the migration machinery in
//! [`migrate`](super::migrate).

use std::io;

use choreo_proto::{ContextConfig, ReasoningProducer};
use redb::{ReadableDatabase, ReadableTable};
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use super::attachments;
use super::schema::{SESSION_KV, SESSION_TURNS, SESSIONS, session_range_end};
use super::store::Store;

mod tombstones;
pub use tombstones::{clear_session_tombstone, mark_session_deleted, purge_tombstoned_sessions};

mod turns;
pub use turns::{delete_session_turns, read_turns, write_turn};

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
/// Returns Err if msgpack encoding fails (a data error, built with
/// `io::Error::other`), or if the write transaction, table open, insert, or
/// commit fails (a storage error, built by `Store::err`).
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
            .map_err(|e| io::Error::other(format!("codec encode session: {e}")))?;
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
/// Returns Err if the write transaction, table open, row lookup, encode, insert,
/// or commit fails.
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
            .map_err(|e| io::Error::other(format!("codec encode session: {e}")))?;
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
/// Returns Err if the read transaction, table iteration, or an iteration step
/// fails. An iteration failure is a storage failure, not a bad row: it is
/// propagated so the store's latch hook sees it, never skipped.
pub fn read_all_sessions(db: &Store) -> io::Result<Vec<(u64, SessionRecord)>> {
    debug!("read_all_sessions");
    let read_txn = db
        .begin_read()
        .map_err(|e| db.err(format!("redb read txn: {e}")))?;
    let table = match read_txn.open_table(SESSIONS) {
        Ok(t) => t,
        Err(e) => {
            warn!("read_all_sessions: table 'sessions' not found (first run?): {e}");
            return Ok(Vec::new());
        }
    };
    let mut sessions: Vec<(u64, SessionRecord)> = Vec::new();
    let iter = table
        .iter()
        .map_err(|e| db.err(format!("redb iter sessions: {e}")))?;
    for result in iter {
        let (key, value) = result.map_err(|e| db.err(format!("redb iter item: {e}")))?;
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

/// Delete a session record, its turns, its per-session KV rows, and its
/// attachments — one write transaction, so the session is never half-collected.
///
/// Each table is emptied with a single `retain_in` pass over exactly this
/// session's key range: no key list is collected (the old shape buffered keys
/// and then dropped the per-row iteration errors), and the range bound keeps the
/// cost proportional to the session rather than to the store.
///
/// # Errors
///
/// Returns Err if the write transaction, any table open, any range pass, or the
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
        // Bounded range pass over just this session's turn ids instead of a
        // walk of the whole table (which made each delete O(total turns)).
        let mut turns = write_txn
            .open_table(SESSION_TURNS)
            .map_err(|e| db.err(format!("redb open turns: {e}")))?;
        turns
            .retain_in(
                (session_id, 0u32)..(session_range_end(session_id), 0u32),
                |_, _| false,
            )
            .map_err(|e| db.err(format!("redb remove session turns: {e}")))?;
    }
    {
        let mut kv_table = write_txn
            .open_table(SESSION_KV)
            .map_err(|e| db.err(format!("redb open session_kv: {e}")))?;
        kv_table
            .retain_in(
                (session_id, String::new())..(session_range_end(session_id), String::new()),
                |_, _| false,
            )
            .map_err(|e| db.err(format!("redb remove session_kv: {e}")))?;
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

#[cfg(test)]
pub(super) mod test_support {
    use super::*;

    /// A minimal valid turn, so a test that only needs *a* turn does not repeat
    /// the field list.
    pub(in crate::db) fn dummy_turn() -> choreo_proto::Turn {
        choreo_proto::Turn {
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

    /// A minimal valid session record, for the flag and purge tests.
    pub(in crate::db) fn sample_record() -> SessionRecord {
        SessionRecord {
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
        }
    }

    /// A fresh file-backed store in `dir`, which the caller owns.
    pub(in crate::db) fn store_at(dir: &tempfile::TempDir) -> Store {
        Store::from_db(redb::Database::create(dir.path().join("test.redb")).unwrap())
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{dummy_turn, sample_record, store_at};
    use super::*;
    use crate::db::codec::ZSTD_FRAME_MAGIC;
    use crate::db::schema::META;

    /// Read the current `next_session_id` from the DB and atomically
    /// increment it. Only used by tests — production code derives the
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

    #[test]
    fn round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let db = store_at(&dir);

        let id = next_session_id(&db).unwrap();
        assert_eq!(id, 1);

        let record = sample_record();
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

        // `delete_session` collects every table this session touched in one
        // transaction: the record, the turn rows, the KV rows, the attachments.
        crate::db::kv_set(&db, id, "k", b"v").unwrap();
        delete_session(&db, id).unwrap();
        assert!(read_session(&db, id).unwrap().is_none());
        assert_eq!(
            read_turns(&db, id).unwrap(),
            [] as [(u32, choreo_proto::Turn); 0]
        );
        assert_eq!(
            crate::db::kv_get(&db, id, "k").unwrap(),
            None,
            "delete_session must remove the session's KV rows"
        );

        drop(db);
    }

    #[test]
    fn delete_session_leaves_other_sessions_intact() {
        // The range bounds are what keep one session's delete from touching a
        // neighbouring session's rows; a session id that shares its lower bound
        // with another (1 vs 2, 1 vs 10) is the case a prefix mistake breaks.
        let dir = tempfile::tempdir().unwrap();
        let db = store_at(&dir);
        for id in [1u64, 2, 10] {
            write_session(&db, id, &sample_record()).unwrap();
            write_turn(&db, id, 0, &dummy_turn()).unwrap();
            crate::db::kv_set(&db, id, "k", b"v").unwrap();
        }

        delete_session(&db, 1).unwrap();

        assert!(read_session(&db, 1).unwrap().is_none());
        for survivor in [2u64, 10] {
            assert!(
                read_session(&db, survivor).unwrap().is_some(),
                "session {survivor} must survive the delete of session 1"
            );
            assert_eq!(read_turns(&db, survivor).unwrap().len(), 1);
            assert_eq!(
                crate::db::kv_get(&db, survivor, "k").unwrap(),
                Some(b"v".to_vec()),
                "session {survivor}'s KV row must survive the delete of session 1"
            );
        }
    }

    #[test]
    fn session_record_last_response_id_round_trips() {
        // Phase 4c persistence: the response id written to the record must
        // survive a write/read cycle so ResponseId-policy models chain across
        // user turns even after a daemon restart.
        let dir = tempfile::tempdir().unwrap();
        let db = store_at(&dir);
        let id = 1u64;
        let mut record = sample_record();
        record.last_response_id = Some("resp_1".into());
        record.last_response_id_producer = Some(ReasoningProducer {
            provider_slug: "openai".into(),
            model: "gpt-5.4".into(),
        });
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
        assert_eq!(read.title.as_deref(), Some("test session"));
    }

    #[test]
    fn write_session_preserves_daemon_flags() {
        // The session thread writes the FULL record (with pinned=false,
        // archived_at=None — it never knows the daemon's flags); a full-record
        // write must NOT clobber flags the daemon set via update_session_flags.
        let dir = tempfile::tempdir().unwrap();
        let db = store_at(&dir);
        write_session(&db, 1, &sample_record()).unwrap();

        update_session_flags(&db, 1, true, Some(4242)).unwrap();
        // Confirm the daemon's update landed.
        let after_update = read_session(&db, 1).unwrap().unwrap();
        assert!(after_update.pinned);
        assert_eq!(after_update.archived_at, Some(4242));

        // Now the session thread writes a full record that tries to reset the
        // flags (the default record has pinned=false/archived_at=None).
        let mut reset_attempt = sample_record();
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
        let db = store_at(&dir);
        let record = sample_record();
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
        let db = store_at(&dir);
        // Create the SESSIONS table with a real row, so the reads below can
        // open it (a fresh DB has no table until the first committed write).
        write_session(&db, 1, &sample_record()).unwrap();

        update_session_flags(&db, 123, true, Some(1)).unwrap();
        assert!(read_session(&db, 123).unwrap().is_none());
        // The unrelated existing row is untouched by the no-op update.
        let one = read_session(&db, 1).unwrap().unwrap();
        assert!(!one.pinned);
        assert_eq!(one.archived_at, None);
    }

    #[test]
    fn read_session_skips_corrupt_record_with_warning() {
        // A corrupt/legacy session record must not fail the read (or the
        // daemon): read_session treats undecodable data as absent — warn and
        // return None — the same policy as the batch reads. (A decode failure
        // is a DATA error; a storage error on the same path still propagates.)
        let dir = tempfile::tempdir().unwrap();
        let db = store_at(&dir);
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
    fn read_all_sessions_skips_a_corrupt_record_but_reads_the_rest() {
        // The batch read is equally tolerant of a bad ROW: one undecodable
        // record must not hide the others (the daemon loads this index at
        // startup, so failing the whole read would lose every session).
        let dir = tempfile::tempdir().unwrap();
        let db = store_at(&dir);
        write_session(&db, 1, &sample_record()).unwrap();
        write_session(&db, 3, &sample_record()).unwrap();
        {
            let write_txn = db.begin_write().unwrap();
            {
                let mut table = write_txn.open_table(SESSIONS).unwrap();
                table.insert(2u64, b"garbage".as_slice()).unwrap();
            }
            write_txn.commit().unwrap();
        }

        let all = read_all_sessions(&db).unwrap();
        assert_eq!(
            all.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            vec![1, 3],
            "the undecodable row is skipped; the rest are returned in id order"
        );
    }
}
