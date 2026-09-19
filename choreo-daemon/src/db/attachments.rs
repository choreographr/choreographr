//! Turn attachment byte store: raw, uncompressed image bytes kept OUT of the
//! zstd-compressed `session_turns` blob.
//!
//! Images (display + vision) are already incompressible (PNG/JPEG), so storing
//! them raw here avoids wasted zstd CPU and keeps `MAX_TURN_DECODED_BYTES`
//! meaningful for the text/tool blob. Splitting the bytes out of the turn also
//! lets an on-demand `GetImage` fetch resolve with a single `get` (no turn
//! decode, no whole-session scan) and lets `emit_image` persist exactly one
//! slot per image as it is produced, rather than rewriting the whole turn.
//!
//! This module owns ALL attachment-store I/O — the table definition, the slot
//! naming, the write/read re-attachment steps, the delete paths, and the
//! single-slot emit-time write — so the storage plumbing in `super` (schema,
//! tables, migrations) does not carry the attachment details.

use std::io;

use choreo_proto::Turn;
use redb::{ReadOnlyTable, ReadableDatabase, ReadableTable, TableDefinition};
use tracing::debug;

use super::{db_err, session_range_end};

/// Raw, uncompressed image/attachment bytes for a turn, keyed by
/// (`session_id`, `turn_id`, slot). Images (display + vision) are kept OUT of the
/// zstd-compressed `session_turns` blob because they are already
/// incompressible (PNG/JPEG) — storing them raw here avoids wasted zstd CPU
/// and keeps `MAX_TURN_DECODED_BYTES` meaningful for the text/tool blob.
/// Created lazily on first write (additive; no schema bump needed).
///
/// This is the general on-demand byte store for a turn (per the D2 decision):
/// anything that is incompressible or sizeable and owned by a turn — today the
/// display + vision image bytes, future blobs as they arise — is split out of
/// the compressed text/tool blob into this raw table at the persistence
/// boundary and re-attached on read.
///
/// `pub(super)` so `super`'s tests can open the table directly; production
/// access goes through the helpers in this module.
pub(super) const SESSION_ATTACHMENTS: TableDefinition<(u64, u32, String), &[u8]> =
    TableDefinition::new("session_attachments");

// ── Slot naming ────────────────────────────────────────────────────────────────

/// Slot name for a displayed image at `index` in its turn: `d{index}`. This is
/// exactly the index the wire `ClientMessage::GetImage` carries, so the slot
/// name and the fetch key are the same by construction.
fn display_slot(index: u32) -> String {
    format!("d{index}")
}

/// Slot name for a tool-result vision image: `r{call_id}`. Keyed by the tool
/// call id (not a positional index) because a vision image belongs to a
/// specific call and the call id is the only stable handle across rewrites.
fn result_slot(call_id: &str) -> String {
    format!("r{call_id}")
}

/// The read-only attachment table type, aliased so the `Option<&…>` parameter
/// of [`reattach_turn_attachments`] does not trip `clippy::type_complexity`.
type ReadOnlyAttachments<'a> = ReadOnlyTable<(u64, u32, String), &'a [u8]>;

// ── Delete paths ────────────────────────────────────────────────────────────────

/// Remove every attachment row belonging to `session_id` (all of its turns).
///
/// Shared by the session-wide delete paths ([`super::delete_session`] and
/// [`super::delete_session_turns`]) so no orphaned image bytes survive a
/// session or turn purge. Range-removes rows keyed by `(session_id, turn_id,
/// slot)` using the same `(session_id, 0, "")..(session_range_end(session_id),
/// 0, "")` bound the turns/KV deletes use, so the whole session's attachments
/// go in one pass.
pub(super) fn delete_session_attachments(
    write_txn: &redb::WriteTransaction,
    session_id: u64,
) -> io::Result<()> {
    let mut att_table = write_txn
        .open_table(SESSION_ATTACHMENTS)
        .map_err(|e| db_err(format!("redb open session_attachments: {e}")))?;
    let att_keys: Vec<(u64, u32, String)> = att_table
        .range::<(u64, u32, String)>(
            (session_id, 0u32, String::new())..(session_range_end(session_id), 0u32, String::new()),
        )
        .map_err(|e| db_err(format!("redb range session_attachments: {e}")))?
        .filter_map(std::result::Result::ok)
        .map(|(k, _)| k.value())
        .collect();
    for key in att_keys {
        att_table
            .remove(key)
            .map_err(|e| db_err(format!("redb remove session_attachment: {e}")))?;
    }
    Ok(())
}

/// Remove every attachment row belonging to one turn of `session_id`.
///
/// Called at the start of [`write_turn_attachments`] so a re-persisted turn can
/// never leave stale attachment rows behind: if a turn is rewritten with a
/// shifted image layout (e.g. a display image dropped and another appended), a
/// stale `d{i}`/`r<call_id>` row from a previous write would otherwise be
/// re-attached to the wrong slot by [`reattach_turn_attachments`]. The per-turn
/// range bound mirrors [`delete_session_attachments`] but scoped to a single
/// `turn_id`.
fn delete_turn_attachments(
    write_txn: &redb::WriteTransaction,
    session_id: u64,
    turn_id: u32,
) -> io::Result<()> {
    let mut att_table = write_txn
        .open_table(SESSION_ATTACHMENTS)
        .map_err(|e| db_err(format!("redb open session_attachments: {e}")))?;
    // `saturating_add` mirrors `session_range_end`: at the theoretical
    // `turn_id == u32::MAX` the bound equals the start, so the range is empty
    // (removes nothing) instead of overflowing in debug / wrapping in release.
    let att_keys: Vec<(u64, u32, String)> = att_table
        .range::<(u64, u32, String)>(
            (session_id, turn_id, String::new())
                ..(session_id, turn_id.saturating_add(1), String::new()),
        )
        .map_err(|e| db_err(format!("redb range session_attachments: {e}")))?
        .filter_map(std::result::Result::ok)
        .map(|(k, _)| k.value())
        .collect();
    for key in att_keys {
        att_table
            .remove(key)
            .map_err(|e| db_err(format!("redb remove session_attachment: {e}")))?;
    }
    Ok(())
}

// ── write_turn / read_turns attachment steps ────────────────────────────────────

/// Persist a turn's image bytes into [`SESSION_ATTACHMENTS`], clearing the
/// turn's stale slots first. This is the attachment half of
/// [`super::write_turn`], factored out so the turn-blob writer stays focused
/// on the codec and the two can be committed in the SAME write transaction.
///
/// Ordering is load-bearing: the stale-slot clear runs first, so a re-persisted
/// turn with a shifted image layout (`d0` emptied, a new image appended at
/// `d1`) cannot leave the old `d0`/`r<call_id>` rows behind to be re-attached
/// to the wrong slot on read. Then exactly the current image set is inserted —
/// empty-data images are skipped (nothing to persist), matching on read where
/// an absent row simply leaves `data` empty.
pub(super) fn write_turn_attachments(
    write_txn: &redb::WriteTransaction,
    session_id: u64,
    turn_id: u32,
    turn: &Turn,
) -> io::Result<()> {
    // Drop any attachment rows left over from a previous write of this turn —
    // the inserts below then persist exactly the current image set, so
    // write_turn stays idempotent.
    delete_turn_attachments(write_txn, session_id, turn_id)?;
    let mut attachments = write_txn
        .open_table(SESSION_ATTACHMENTS)
        .map_err(|e| db_err(format!("redb open session_attachments: {e}")))?;
    for (i, img) in turn.displayed_images.iter().enumerate() {
        if img.data.is_empty() {
            continue; // nothing to persist
        }
        // usize→u32: an image index is bounded far below u32 in practice;
        // saturating (rather than truncating) keeps the slot name total even
        // at the theoretical cap, matching the display_slot contract.
        let slot = display_slot(u32::try_from(i).unwrap_or(u32::MAX));
        attachments
            .insert((session_id, turn_id, slot), img.data.as_slice())
            .map_err(|e| db_err(format!("redb insert display attachment: {e}")))?;
    }
    for tr in &turn.tool_results {
        if let Some(image) = &tr.image
            && !image.data.is_empty()
        {
            let slot = result_slot(&tr.call_id);
            attachments
                .insert((session_id, turn_id, slot), image.data.as_slice())
                .map_err(|e| db_err(format!("redb insert result attachment: {e}")))?;
        }
    }
    // No per-turn log here: this fires on every turn write (often with zero
    // attachments) and is pure noise at DEBUG. Storage anomalies surface as
    // errors from the inserts above.
    Ok(())
}

/// Re-attach a decoded turn's split-out image bytes from [`SESSION_ATTACHMENTS`].
/// This is the read half of the split done by [`write_turn_attachments`],
/// factored out so [`super::read_turns`] keeps only the decode/driver logic.
///
/// `attachments` is the table opened in the SAME read transaction as the turn
/// blob (or `None` when the table does not exist yet — a fresh database, or one
/// written before the table existed). Only slots whose in-memory `data` is
/// empty are looked up: an image that was never split out (empty at write time)
/// stays empty, and a missing row leaves `data` empty rather than erroring —
/// the request builder's placeholder path handles that case.
pub(super) fn reattach_turn_attachments(
    attachments: Option<&ReadOnlyAttachments<'_>>,
    session_id: u64,
    turn_id: u32,
    turn: &mut Turn,
) -> io::Result<()> {
    // No attachments table yet ⇒ nothing was ever split out; leave every
    // image byte field empty (the caller still returns the turn).
    let Some(attachments) = attachments else {
        return Ok(());
    };
    for (i, img) in turn.displayed_images.iter_mut().enumerate() {
        if img.data.is_empty() {
            let slot = display_slot(u32::try_from(i).unwrap_or(u32::MAX));
            if let Some(guard) = attachments
                .get((session_id, turn_id, slot))
                .map_err(|e| db_err(format!("redb get display attachment: {e}")))?
            {
                img.data = guard.value().to_vec();
            }
        }
    }
    for tr in &mut turn.tool_results {
        if let Some(image) = &mut tr.image
            && image.data.is_empty()
        {
            let slot = result_slot(&tr.call_id);
            if let Some(guard) = attachments
                .get((session_id, turn_id, slot))
                .map_err(|e| db_err(format!("redb get result attachment: {e}")))?
            {
                image.data = guard.value().to_vec();
            }
        }
    }
    debug!(session_id, turn_id, "re-attached turn image attachments");
    Ok(())
}

// ── On-demand reads / single-slot writes ────────────────────────────────────────

/// Read a single persisted displayed-image attachment (raw bytes) by turn and
/// index.
///
/// The attachment table is keyed `(session_id, turn_id, slot)`, where a
/// displayed image at index `i` in its turn is stored under slot `d{i}` — the
/// exact index the wire `ClientMessage::GetImage` carries. This is a single
/// `get`: no turn decode and no whole-session scan, so an on-demand image
/// fetch (a client scrolling an image into view) is O(log n) rather than
/// proportional to the session's history.
///
/// Returns `Ok(None)` when the table or the slot is absent — a fresh database
/// has no attachments table, a turn that persisted no image at that index has
/// no row, and a deleted/evicted image is simply gone. All such cases are
/// "not found", never an error.
///
/// # Errors
///
/// Returns Err only for a genuine redb failure (read transaction open, table
/// open other than "does not exist", or the `get`).
pub fn read_display_image(
    db: &redb::Database,
    session_id: u64,
    turn_id: u32,
    image_index: u32,
) -> io::Result<Option<Vec<u8>>> {
    let read_txn = db
        .begin_read()
        .map_err(|e| db_err(format!("redb read txn (display image): {e}")))?;
    let table = match read_txn.open_table(SESSION_ATTACHMENTS) {
        Ok(t) => t,
        // No attachments table yet (fresh database): nothing has ever been
        // split out, so the image is simply not found.
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
        Err(e) => {
            return Err(db_err(format!(
                "redb open session_attachments (display image): {e}"
            )));
        }
    };
    // Display images occupy slot `d{index}` (see `write_turn_attachments`); the
    // vision image slot `r{call_id}` is not served by this path.
    let slot = display_slot(image_index);
    match table
        .get((session_id, turn_id, slot))
        .map_err(|e| db_err(format!("redb get display attachment: {e}")))?
    {
        Some(guard) => Ok(Some(guard.value().to_vec())),
        None => Ok(None),
    }
}

/// Persist exactly ONE displayed-image attachment — the O(1) persist-at-emit
/// write.
///
/// `emit_image` calls this after appending an image to the in-memory turn so an
/// on-demand `GetImage` for that image resolves immediately. Unlike
/// [`super::write_turn`], it opens one write transaction and inserts exactly the
/// single `(session_id, turn_id, d{image_index})` row: it does NOT clear the
/// turn's other slots (they were written by earlier emits and must survive) and
/// it does NOT touch the turn blob (re-written in full, blob + all attachments,
/// atomically at `finalize_turn`). This keeps an N-image turn's emit-time disk
/// cost O(N) instead of the O(N²) a whole-turn rewrite per image would incur.
/// The table is created lazily on first insert, as redb does.
///
/// Empty `data` writes nothing (mirrors [`write_turn_attachments`], which skips
/// empty images): there is no byte payload to fetch, so no row is needed.
///
/// # Errors
///
/// Returns Err only for a genuine redb failure (write transaction open, table
/// open, insert, or commit).
pub fn write_display_image_attachment(
    db: &redb::Database,
    session_id: u64,
    turn_id: u32,
    image_index: u32,
    data: &[u8],
) -> io::Result<()> {
    if data.is_empty() {
        return Ok(()); // nothing to persist — mirrors the write_turn skip
    }
    let write_txn = db
        .begin_write()
        .map_err(|e| db_err(format!("redb write txn (display image): {e}")))?;
    {
        let mut table = write_txn.open_table(SESSION_ATTACHMENTS).map_err(|e| {
            db_err(format!(
                "redb open session_attachments (display image): {e}"
            ))
        })?;
        table
            .insert((session_id, turn_id, display_slot(image_index)), data)
            .map_err(|e| db_err(format!("redb insert display attachment (emit): {e}")))?;
    }
    write_txn
        .commit()
        .map_err(|e| db_err(format!("redb commit display attachment (emit): {e}")))?;
    Ok(())
}
