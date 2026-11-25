//! The turn rows of a session: write, read and delete.
//!
//! A turn is persisted as one compressed blob plus (optionally) a handful of raw
//! attachment rows. [`write_turn`] splits the image bytes OUT of the blob and
//! writes both halves in one transaction, so a crash leaves either both or
//! neither; [`read_turns`] re-attaches them from the same snapshot; and
//! [`delete_session_turns`] removes both for a whole session.
//!
//! Turn rows are keyed `(session_id, turn_id)`, so every per-session operation
//! here is a bounded range pass over that session's keys rather than a walk of
//! the whole table.

use std::io;

use choreo_proto::{DisplayedImageRecord, ImageReference, ToolResultRecord, Turn};
use redb::{ReadableDatabase, ReadableTable};

use crate::db::attachments;
use crate::db::codec::{zstd_decode, zstd_encode};
use crate::db::schema::{SESSION_TURNS, session_range_end};
use crate::db::store::Store;

/// The `session_turns` storage view of a turn: the same record with every image
/// byte field emptied, so the compressed blob carries only text and tool
/// metadata (the bytes themselves live in `session_attachments`, raw).
///
/// Built field by field instead of `turn.clone()` plus clearing: a `Turn`
/// carries every attached image inline, so the clone would copy megabytes to
/// drop them a moment later. Listing the fields explicitly is also the drift
/// guard — a new field on [`Turn`] stops this from compiling, where a
/// `..turn.clone()` update would silently copy the new field's data twice.
pub(super) fn storage_view(turn: &Turn) -> Turn {
    Turn {
        created_at: turn.created_at,
        undone: turn.undone,
        error: turn.error.clone(),
        user_text: turn.user_text.clone(),
        assistant_text: turn.assistant_text.clone(),
        assistant_reasoning: turn.assistant_reasoning.clone(),
        tool_calls: turn.tool_calls.clone(),
        token_usage: turn.token_usage,
        tool_results: turn.tool_results.iter().map(storage_view_result).collect(),
        displayed_images: turn
            .displayed_images
            .iter()
            .map(storage_view_image)
            .collect(),
        reasoning_artifact: turn.reasoning_artifact.clone(),
        reasoning_producer: turn.reasoning_producer.clone(),
    }
}

/// One displayed image without its bytes: metadata and the producing tool call
/// survive, the payload does not.
fn storage_view_image(img: &DisplayedImageRecord) -> DisplayedImageRecord {
    DisplayedImageRecord {
        metadata: img.metadata.clone(),
        data: Vec::new(),
        tool_call_id: img.tool_call_id.clone(),
    }
}

/// One tool result without its vision image's bytes.
fn storage_view_result(result: &ToolResultRecord) -> ToolResultRecord {
    ToolResultRecord {
        call_id: result.call_id.clone(),
        name: result.name.clone(),
        content: result.content.clone(),
        is_error: result.is_error,
        invocation_description: result.invocation_description.clone(),
        image: result.image.as_ref().map(|image| ImageReference {
            path: image.path.clone(),
            mime_type: image.mime_type.clone(),
            width: image.width,
            height: image.height,
            data: Vec::new(),
        }),
    }
}

/// Compress and persist a turn (payload bytes split into the attachments
/// table) under `(session_id, turn_id)`.
///
/// # Errors
///
/// Returns Err if msgpack/zstd encoding fails (a data error, built with
/// `io::Error::other`), or if the write transaction, table opens, inserts, or
/// commit fails (a storage error, built by `Store::err`).
pub fn write_turn(db: &Store, session_id: u64, turn_id: u32, turn: &Turn) -> io::Result<()> {
    let storage = storage_view(turn);
    let payload = rmp_serde::to_vec_named(&storage)
        .map_err(|e| io::Error::other(format!("codec encode turn: {e}")))?;
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
/// A row that will not decode is skipped with a warning (a data error must not
/// hide the rest of the session); an iteration failure is a storage failure and
/// propagates, so the store's latch hook sees it.
///
/// # Errors
///
/// Returns Err if the read transaction, table opens, an iteration step, or an
/// attachment lookup fails.
pub fn read_turns(db: &Store, session_id: u64) -> io::Result<Vec<(u32, Turn)>> {
    let read_txn = db
        .begin_read()
        .map_err(|e| db.err(format!("redb read txn: {e}")))?;
    let table = match read_txn.open_table(SESSION_TURNS) {
        Ok(t) => t,
        // A fresh database that has never persisted a turn has no table yet:
        // that is an empty session history, not an error.
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
    // session.
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
/// One `retain_in` pass over the session's own key range: no whole-table walk
/// (which made this O(total turns) across every session), no key list buffered,
/// and a per-row failure comes back as an error instead of being skipped.
///
/// # Errors
///
/// Returns Err if the write transaction, table open, range pass, attachment
/// removal, or commit fails.
pub fn delete_session_turns(db: &Store, session_id: u64) -> io::Result<()> {
    let write_txn = db
        .begin_write()
        .map_err(|e| db.err(format!("redb write txn: {e}")))?;
    {
        let mut table = write_txn
            .open_table(SESSION_TURNS)
            .map_err(|e| db.err(format!("redb open turns: {e}")))?;
        table
            .retain_in(
                (session_id, 0u32)..(session_range_end(session_id), 0u32),
                |_, _| false,
            )
            .map_err(|e| db.err(format!("redb remove session turns: {e}")))?;
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
    use crate::db::sessions::test_support::{dummy_turn, store_at};
    use choreo_proto::{ImageMetadata, Turn};
    use redb::ReadableDatabase;

    /// A turn carrying both kinds of image bytes, so the split is exercised on
    /// the display path and the tool-result vision path at once.
    fn turn_with_images() -> Turn {
        let mut turn = dummy_turn();
        turn.displayed_images = vec![DisplayedImageRecord {
            metadata: ImageMetadata {
                mime_type: "image/png".into(),
                width: 32,
                height: 32,
                byte_len: 5,
                alt: Some("a display image".into()),
            },
            data: b"\x89PNG\r".to_vec(),
            tool_call_id: Some("call_disp".into()),
        }];
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
        turn
    }

    #[test]
    fn storage_view_is_the_clone_and_clear_encoding() {
        // The storage view exists to avoid copying image bytes. It must be
        // indistinguishable on disk from the shape it replaces — the same
        // record with every byte field emptied — so a reader (and the schema-2
        // contract) cannot tell which code wrote a row. Byte equality is the
        // strongest form of that claim: same fields, same order, same values.
        let turn = turn_with_images();

        let mut clone_and_clear = turn.clone();
        for img in &mut clone_and_clear.displayed_images {
            img.data.clear();
        }
        for result in &mut clone_and_clear.tool_results {
            if let Some(image) = &mut result.image {
                image.data.clear();
            }
        }

        assert_eq!(
            rmp_serde::to_vec_named(&storage_view(&turn)).unwrap(),
            rmp_serde::to_vec_named(&clone_and_clear).unwrap(),
            "the storage view must serialize exactly like a bytes-cleared clone"
        );
    }

    #[test]
    fn turn_image_bytes_split_into_attachments_and_reattached() {
        // Persistence boundary contract: a turn's image bytes (display + vision)
        // are split OUT of the zstd-compressed session_turns blob into the raw
        // session_attachments table on write, and re-attached on read so the
        // round-tripped turn matches the original byte-for-byte.
        let dir = tempfile::tempdir().unwrap();
        let db = store_at(&dir);
        let id = 1u64;
        let turn = turn_with_images();

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
        assert!(
            read_turns(&db, id).unwrap().is_empty(),
            "delete_session_turns must remove the turn rows"
        );

        // Rewrite, then delete the whole session — attachments must go too.
        write_turn(&db, id, 0, &turn).unwrap();
        crate::db::delete_session(&db, id).unwrap();
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
        // `read_turns` to the wrong slots. write_turn clears the turn's
        // attachment range first, so only the current image set survives.
        let dir = tempfile::tempdir().unwrap();
        let db = store_at(&dir);
        let id = 1u64;

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
    fn read_turns_skips_corrupt_entries() {
        let dir = tempfile::tempdir().unwrap();
        let db = store_at(&dir);
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
    fn delete_session_turns_leaves_other_sessions_intact() {
        // The per-session range bound is the whole point of the retain pass:
        // clearing one session's history must not touch a neighbour's rows
        // (session 1 vs 2 and 1 vs 10 cover the prefix-collision shapes).
        let dir = tempfile::tempdir().unwrap();
        let db = store_at(&dir);
        for id in [1u64, 2, 10] {
            write_turn(&db, id, 0, &dummy_turn()).unwrap();
        }

        delete_session_turns(&db, 1).unwrap();

        assert_eq!(
            read_turns(&db, 1).unwrap(),
            [] as [(u32, Turn); 0],
            "the deleted session's turns must be gone"
        );
        for survivor in [2u64, 10] {
            assert_eq!(
                read_turns(&db, survivor).unwrap().len(),
                1,
                "session {survivor}'s turn must survive the delete of session 1"
            );
        }
    }
}
