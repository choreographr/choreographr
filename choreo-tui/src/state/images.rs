//! Turn-image state and on-demand fetch plumbing, covering BOTH kinds of turn
//! image: DISPLAYED images (from `display_image`/`generate_image`/a
//! `retrieve_webpage` screenshot) and tool-result VISION images (the normalized
//! bytes a tool such as `read_image` fed to a vision model).
//!
//! Turn snapshots strip image bytes (only metadata survives), so the TUI fetches
//! each image's bytes lazily: the per-frame render path calls
//! [`App::request_image_fetch`], the UI loop drains the queue with
//! [`App::flush_image_fetches`], and the daemon's reply is applied by
//! [`App::handle_image_reply`]. Both kinds are keyed by the same [`ImageSlot`]
//! and served by one wire pair ([`ClientMessageType::GetImage`] carrying an
//! [`ImageKey`]). The encoded-bitmap jobs are handled separately by
//! [`App::apply_image_result`]/[`App::submit_image_job`]. All of these are
//! inherent `App` methods living in this sibling module; their fields stay on
//! `App` in `state/mod.rs`.

use super::App;
use crate::RenderedImage;
use crate::image_worker::{ImageId, ImageJob, ImageResult, next_job_id};
use choreo_client_core::PendingContext;
use choreo_proto::{
    ClientMessage, ClientMessageType, ImageKey, ImageMetadata, ImageReference, Turn,
};
use ratatui::layout::Size;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// Which image within a turn a [`RenderedImage`] slot addresses.
///
/// A turn carries two kinds of images, both fetched on demand under one wire
/// protocol ([`ClientMessageType::GetImage`] with an [`ImageKey`]): DISPLAYED
/// images (produced by `display_image`/`generate_image`/a `retrieve_webpage`
/// screenshot) addressed positionally within `turn.displayed_images`, and
/// tool-result VISION images (the normalized bytes a tool such as `read_image`
/// fed to the model) addressed by the producing tool call's id. This mirrors the
/// wire [`ImageKey`] so the TUI's per-image plumbing is keyed by one value for
/// both kinds; [`displayed`](ImageSlot::Displayed) keeps the positional index
/// the render/height accounting already used.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum ImageSlot {
    /// The `index`-th entry of the turn's `displayed_images`.
    Displayed(usize),
    /// The vision image attached to the tool result with this `call_id`.
    ToolResult(String),
}

impl ImageSlot {
    /// The wire key this slot fetches under ([`ClientMessageType::GetImage`]).
    fn to_key(&self) -> ImageKey {
        match self {
            // usize→u32: an image index is bounded far below u32 in practice;
            // saturate rather than wrap onto the wrong image.
            ImageSlot::Displayed(index) => ImageKey::Displayed {
                index: u32::try_from(*index).unwrap_or(u32::MAX),
            },
            ImageSlot::ToolResult(call_id) => ImageKey::ToolResult {
                call_id: call_id.clone(),
            },
        }
    }

    /// The slot a wire [`ImageKey`] addresses (the inverse of [`Self::to_key`]).
    fn from_key(key: ImageKey) -> Self {
        match key {
            ImageKey::Displayed { index } => {
                ImageSlot::Displayed(usize::try_from(index).unwrap_or(usize::MAX))
            }
            ImageKey::ToolResult { call_id } => ImageSlot::ToolResult(call_id),
        }
    }
}

/// The ordered list of image slots a turn exposes: every displayed image (by
/// index), then every tool-result vision image (by its tool call's id, in tool
/// result order). The render path and the height accounting BOTH derive their
/// image count and per-image identity from this one function, so the blocks they
/// draw and the rows they reserve can never disagree.
///
/// Per-frame allocation: this builds a fresh `Vec` (and clones each tool-result
/// `call_id`) on every call.  The RENDER loop needs the identities and calls it
/// once per visible turn per frame; the HEIGHT accounting only needs the count
/// and uses [`turn_image_count`] instead, so a height rebuild (which runs on
/// every viewport-height change — a real resize, or a status/input row
/// appearing) allocates nothing per turn.
pub(crate) fn turn_image_slots(turn: &Turn) -> Vec<ImageSlot> {
    let mut slots: Vec<ImageSlot> = (0..turn.displayed_images.len())
        .map(ImageSlot::Displayed)
        .collect();
    for tr in &turn.tool_results {
        if tr.image.is_some() {
            slots.push(ImageSlot::ToolResult(tr.call_id.clone()));
        }
    }
    slots
}

/// The number of image blocks a turn exposes — the count [`turn_image_slots`]
/// yields, without materializing the list or cloning any `call_id`.
///
/// The height accounting reserves one block per image and never needs the
/// identities, so it uses this.  It walks the same source order as
/// [`turn_image_slots`] (displayed images then tool-result vision images), so the
/// two can never disagree on the count.
pub(crate) fn turn_image_count(turn: &Turn) -> usize {
    turn.displayed_images.len()
        + turn
            .tool_results
            .iter()
            .filter(|tr| tr.image.is_some())
            .count()
}

/// The placeholder [`ImageMetadata`] for a tool-result vision image, derived from
/// the (byte-less, client-facing) [`ImageReference`]. `byte_len` is a POSITIVE
/// marker: the client never receives the true length, and the reference's mere
/// presence means "there is an image to fetch", so an otherwise empty
/// placeholder is kept fetchable. The source path becomes `alt` so a placeholder
/// can name the file. Shared by [`slot_source`] and [`App::sync_turn_images`] so
/// the two can never derive different metadata for the same reference.
fn vision_metadata(reference: &ImageReference) -> ImageMetadata {
    ImageMetadata {
        mime_type: reference.mime_type.clone(),
        width: reference.width,
        height: reference.height,
        byte_len: if reference.data.is_empty() {
            1
        } else {
            reference.data.len() as u64
        },
        alt: Some(reference.path.clone()),
    }
}

/// The placeholder metadata and any inline bytes for a slot from the turn the
/// client holds. Used to seed a [`RenderedImage`] lazily (e.g. the fullscreen
/// path opening before `sync_turn_images` ran) — it derives a vision slot's
/// metadata through the same [`vision_metadata`] helper [`App::sync_turn_images`]
/// uses, so the two can never disagree.
pub(crate) fn slot_source(turn: &Turn, slot: &ImageSlot) -> Option<(ImageMetadata, Vec<u8>)> {
    match slot {
        ImageSlot::Displayed(index) => turn
            .displayed_images
            .get(*index)
            .map(|record| (record.metadata.clone(), record.data.clone())),
        ImageSlot::ToolResult(call_id) => {
            let reference = turn
                .tool_results
                .iter()
                .find(|tr| &tr.call_id == call_id)
                .and_then(|tr| tr.image.as_ref())?;
            Some((vision_metadata(reference), reference.data.clone()))
        }
    }
}

/// Insert (or refresh) a slot's [`RenderedImage`], preserving the recovery
/// latch semantics: a transient fetch failure is undone when the turn is
/// re-advertised as long as there is genuinely something to fetch.
fn ensure_slot(
    images: &mut HashMap<ImageSlot, RenderedImage>,
    slot: ImageSlot,
    metadata: ImageMetadata,
    data: Vec<u8>,
) {
    images
        .entry(slot)
        .and_modify(|img| {
            // Recovery signal (see `App::sync_turn_images`): a re-advertisement
            // means the daemon re-persisted the image, so a latched failure is
            // cleared when there is something to fetch again.
            if img.fetch_failed && img.data.is_empty() && img.metadata.byte_len > 0 {
                img.fetch_failed = false;
            }
        })
        .or_insert_with(|| RenderedImage::new_placeholder(metadata, Arc::from(data)));
}

/// A request to encode one image into terminal-renderable form: the target slot
/// (session/turn/[`ImageSlot`]) plus the image bytes, metadata, and the cell
/// size / resize policy the encoder needs. Bundled so
/// [`App::submit_image_job`] takes one value instead of a positional list (and
/// clippy's `too_many_arguments` lint needs no suppression).
pub(crate) struct ImageJobRequest {
    pub session_id: u64,
    pub turn_id: u32,
    pub slot: ImageSlot,
    pub data: Arc<[u8]>,
    pub metadata: ImageMetadata,
    pub cell_size: Size,
    pub resize: ratatui_image::Resize,
}

impl App {
    /// Mirror a turn's images into the per-(session, turn, [`ImageSlot`])
    /// `RenderedImage` map: every displayed image (by index) and every
    /// tool-result vision image (by its tool call's id, when its `image`
    /// reference is present). Bytes stripped from the snapshot are filled in
    /// later by the on-demand fetch ([`App::handle_image_reply`]).
    pub(crate) fn sync_turn_images(&mut self, session_id: u64, turn_id: u32, turn: &Turn) {
        let images = self
            .rendered_images
            .entry(session_id)
            .or_default()
            .entry(turn_id)
            .or_default();
        // Displayed images carry their `ImageMetadata` (dimensions, mime,
        // `byte_len`, alt) directly.
        for (idx, record) in turn.displayed_images.iter().enumerate() {
            ensure_slot(
                images,
                ImageSlot::Displayed(idx),
                record.metadata.clone(),
                record.data.clone(),
            );
        }
        // Tool-result vision images ride a byte-less `ImageReference` on the
        // client view. Derive the placeholder metadata from it via the shared
        // helper; `byte_len` is a POSITIVE marker (the reference's presence means
        // "there is an image to fetch") because the client never receives the
        // true length — a fetch fills in the bytes and, on a genuinely absent
        // slot, latches `fetch_failed` rather than spinning.
        for tr in &turn.tool_results {
            let Some(reference) = &tr.image else {
                continue;
            };
            ensure_slot(
                images,
                ImageSlot::ToolResult(tr.call_id.clone()),
                vision_metadata(reference),
                reference.data.clone(),
            );
        }
        // Prune slots the turn no longer exposes (a re-advertised turn with a
        // shifted image set), so no stale entry survives to be drawn or fetched.
        let present: HashSet<ImageSlot> = turn_image_slots(turn).into_iter().collect();
        images.retain(|slot, _| present.contains(slot));
    }

    pub(crate) fn apply_image_result(&mut self, result: ImageResult) {
        let Some((session_id, turn_id, slot)) = self.pending_job_idx.remove(&result.id) else {
            tracing::debug!(
                "[choreo-tui] discarding image job {}: no slot is awaiting it",
                result.id
            );
            return;
        };
        let Some(img) = self.slot_image_mut(session_id, turn_id, &slot) else {
            // The turn was re-advertised without this slot (or the session was
            // closed) while the encode was in flight.
            tracing::debug!(
                "[choreo-tui] discarding image job {}: session {} turn {} slot {:?} is gone",
                result.id,
                session_id,
                turn_id,
                slot
            );
            return;
        };
        if img.pending_job != Some(result.id) {
            // A newer job for the same slot superseded this one; applying an
            // older protocol would resurrect a stale layout.
            tracing::debug!(
                "[choreo-tui] discarding image job {}: session {} turn {} slot {:?} has moved on",
                result.id,
                session_id,
                turn_id,
                slot
            );
            return;
        }
        let id = result.id;
        let encoded = result.protocol.is_some();
        let cell_size = result.cell_size;
        img.apply_result(result);
        // The two outcomes are logged at the default level so a missing image is
        // diagnosable from the log alone: an `Ok` protocol draws next frame,
        // while a failed one leaves the placeholder and latches the size.
        if encoded {
            tracing::info!(
                "[choreo-tui] image job {id} encoded session {session_id} turn {turn_id} slot {slot:?} at {}x{}",
                cell_size.width,
                cell_size.height,
            );
        } else {
            tracing::warn!(
                "[choreo-tui] image job {id} failed for session {session_id} turn {turn_id} slot {slot:?} at {}x{}; the placeholder stays drawn",
                cell_size.width,
                cell_size.height,
            );
        }
    }

    /// Queue an on-demand fetch for an image whose bytes were not shipped in the
    /// turn snapshot (both displayed and tool-result vision bytes are stripped).
    /// Deduped via the image's own `fetching`/`fetch_failed` flags so the
    /// per-frame render path cannot enqueue the same fetch repeatedly while a
    /// reply is in flight. Only images that HAVE bytes to fetch (`byte_len > 0`)
    /// are requested; a zero-byte image is a legitimately empty placeholder with
    /// nothing to load, and one already resolved (bytes present, in flight, or
    /// failed) is skipped.
    pub(crate) fn request_image_fetch(&mut self, session_id: u64, turn_id: u32, slot: ImageSlot) {
        let Some(img) = self.slot_image_mut(session_id, turn_id, &slot) else {
            return;
        };
        if img.fetching || img.fetch_failed || !img.data.is_empty() || img.metadata.byte_len == 0 {
            return;
        }
        img.fetching = true;
        self.pending_image_fetch.push((session_id, turn_id, slot));
    }

    /// Send every queued `GetImage` request to the daemon. Called by the UI
    /// loop — the only place that owns the client sender — once per rendered
    /// frame, so a scroll that reveals new images issues their fetches on the
    /// next pass.
    pub(crate) fn flush_image_fetches(
        &mut self,
        client_tx: &crossbeam_channel::Sender<ClientMessage>,
    ) {
        // Collect the queue first so `self.pending.send` (which borrows `self`
        // mutably) does not alias the drain borrow of `self.pending_image_fetch`.
        let queued: Vec<(u64, u32, ImageSlot)> = std::mem::take(&mut self.pending_image_fetch);
        for (session_id, turn_id, slot) in queued {
            let key = slot.to_key();
            let id = self.pending.send(
                client_tx,
                ClientMessageType::GetImage {
                    session_id,
                    turn_id,
                    key: key.clone(),
                },
            );
            // The composite key rides the request context so the fetch can be
            // attributed by id; the wire `Image` reply still echoes it for the
            // byte-routing handler.
            self.pending.set_context(
                id,
                PendingContext::Image {
                    session_id,
                    turn_id,
                    key,
                },
            );
        }
    }

    /// Apply a `DaemonMessageType::Image` reply: store the fetched bytes (clearing
    /// any decode-failure state so the next render submits an encoding job), or
    /// mark the image failed when the daemon had none (not found), so it is not
    /// re-requested every frame. A `fetch_failed` latch is not permanent: a
    /// later re-advertisement of the turn (finalize re-broadcast) clears it —
    /// see [`App::sync_turn_images`]. The reply's [`ImageKey`] is mapped back to
    /// the slot it was requested under.
    pub(crate) fn handle_image_reply(
        &mut self,
        session_id: u64,
        turn_id: u32,
        key: ImageKey,
        data: Option<Vec<u8>>,
    ) {
        let slot = ImageSlot::from_key(key);
        let Some(img) = self.slot_image_mut(session_id, turn_id, &slot) else {
            return;
        };
        img.fetching = false;
        match data {
            Some(bytes) => {
                img.data = Arc::from(bytes);
                // Fresh bytes invalidate any prior encode outcome: an earlier
                // frame may have recorded a failure for an empty placeholder.
                img.protocols.clear();
                img.failed_sizes.clear();
                img.pending_job = None;
                // Defensive: an image advertised with `byte_len > 0` but
                // delivered empty would otherwise re-queue the fetch on EVERY
                // frame (`data` stays empty, `byte_len > 0`, `fetching` just
                // cleared, `fetch_failed` false) — an unbounded fetch loop.
                // Treat an empty payload for a non-empty image as terminal-
                // but-recoverable, exactly like a `None`: latch `fetch_failed`
                // so the render path stops, and let a later re-advertisement
                // of the turn (finalize re-broadcast) clear it via
                // `sync_turn_images`. The daemon never writes a zero-byte
                // slot, so this is a guard against a malformed/unexpected
                // reply, not an expected path.
                if img.data.is_empty() && img.metadata.byte_len > 0 {
                    img.fetch_failed = true;
                }
            }
            // Not found (deleted/evicted/stale key): stop re-requesting so a
            // missing image cannot spin a fetch every frame.
            None => img.fetch_failed = true,
        }
    }

    /// Queue a background encode job for one image, returning its job id (or
    /// `None` when the image could not be handed to the worker). The request
    /// carries the target slot and the already-owned encode inputs; see
    /// [`ImageJobRequest`].
    ///
    /// `None` has two causes, both of which leave the slot retryable rather
    /// than stuck: no worker exists (a headless app, where the caller keeps its
    /// placeholder), or the worker thread is gone — in which case the size is
    /// latched as failed so the render path stops asking, because a slot whose
    /// `pending_job` marker is set can never be re-submitted by itself.
    pub(crate) fn submit_image_job(&mut self, req: ImageJobRequest) -> Option<ImageId> {
        let ImageJobRequest {
            session_id,
            turn_id,
            slot,
            data,
            metadata,
            cell_size,
            resize,
        } = req;
        let tx = self.image_job_tx.as_ref()?;
        let id = next_job_id();

        tracing::info!(
            "[choreo-tui] submitting image job {} for session {} turn {} slot {:?} ({} {}x{} @ {}x{})",
            id,
            session_id,
            turn_id,
            slot,
            metadata.mime_type,
            metadata.width,
            metadata.height,
            cell_size.width,
            cell_size.height,
        );

        // The job is handed over BEFORE the pending markers are set: a send
        // into a dead worker is the one failure this function can observe, and
        // latching `pending_job` for a job that was never queued would leave the
        // slot permanently un-submittable. Results are only applied on the UI
        // thread (the thread calling this), so nothing can observe the slot
        // between the send and the marker.
        if tx
            .send(ImageJob {
                id,
                data,
                metadata,
                cell_size,
                resize,
            })
            .is_err()
        {
            tracing::warn!(
                "[choreo-tui] image worker is gone; failing image job {id} for session {session_id} turn {turn_id} slot {slot:?}"
            );
            self.fail_image_slot(session_id, turn_id, &slot, cell_size);
            return None;
        }

        self.pending_job_idx
            .insert(id, (session_id, turn_id, slot.clone()));

        if let Some(img) = self.slot_image_mut(session_id, turn_id, &slot) {
            img.pending_job = Some(id);
        }
        Some(id)
    }

    /// The rendered image for one slot, if the turn still exposes it.
    fn slot_image_mut(
        &mut self,
        session_id: u64,
        turn_id: u32,
        slot: &ImageSlot,
    ) -> Option<&mut RenderedImage> {
        self.rendered_images
            .get_mut(&session_id)
            .and_then(|images| images.get_mut(&turn_id))
            .and_then(|images| images.get_mut(slot))
    }

    /// Record an encode failure for one slot at `cell_size`: clear the pending
    /// marker and latch the size, so the render path stops re-submitting it
    /// (the placeholder stays drawn). The latch is per-size, exactly like a
    /// worker-reported failure, so a resize retries at the new size.
    fn fail_image_slot(
        &mut self,
        session_id: u64,
        turn_id: u32,
        slot: &ImageSlot,
        cell_size: Size,
    ) {
        if let Some(img) = self.slot_image_mut(session_id, turn_id, slot) {
            img.pending_job = None;
            img.failed_sizes.insert(cell_size);
        }
    }
}
