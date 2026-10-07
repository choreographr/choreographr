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
use choreo_proto::{ClientMessageType, ImageKey, ImageMetadata, ImageReference, Turn};
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
/// `call_id`) on every call, and both the render loop and the height accounting
/// call it once per turn per frame. That is accepted rather than cached —
/// image-bearing turns are rare, so the cost is negligible beside the per-frame
/// markdown render it accompanies. If a future workload ever makes it hot, the
/// height path only needs the COUNT (it discards the identities), so it could
/// use a plain `displayed_images.len() + <vision-slot count>` instead of
/// materializing the list.
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
            return;
        };
        if let Some(session_images) = self.rendered_images.get_mut(&session_id)
            && let Some(images) = session_images.get_mut(&turn_id)
            && let Some(img) = images.get_mut(&slot)
            && img.pending_job == Some(result.id)
        {
            tracing::trace!(
                "[choreo-tui] image job {} completed for session {} turn {} slot {:?}",
                result.id,
                session_id,
                turn_id,
                slot,
            );
            img.apply_result(result);
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
        let Some(img) = self
            .rendered_images
            .get_mut(&session_id)
            .and_then(|s| s.get_mut(&turn_id))
            .and_then(|imgs| imgs.get_mut(&slot))
        else {
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
        client_tx: &crossbeam_channel::Sender<ClientMessageType>,
    ) {
        for (session_id, turn_id, slot) in self.pending_image_fetch.drain(..) {
            let _ = client_tx.send(ClientMessageType::GetImage {
                session_id,
                turn_id,
                key: slot.to_key(),
            });
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
        let Some(img) = self
            .rendered_images
            .get_mut(&session_id)
            .and_then(|s| s.get_mut(&turn_id))
            .and_then(|imgs| imgs.get_mut(&slot))
        else {
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
    /// `None` when the worker is absent). The request carries the target slot
    /// and the already-owned encode inputs; see [`ImageJobRequest`].
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

        tracing::trace!(
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

        self.pending_job_idx
            .insert(id, (session_id, turn_id, slot.clone()));

        if let Some(session_images) = self.rendered_images.get_mut(&session_id)
            && let Some(images) = session_images.get_mut(&turn_id)
            && let Some(img) = images.get_mut(&slot)
        {
            img.pending_job = Some(id);
        }

        let _ = tx.send(ImageJob {
            id,
            data,
            metadata,
            cell_size,
            resize,
        });
        Some(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::test_app;

    #[test]
    fn request_image_fetch_queues_only_stripped_nonempty_images() {
        // After protocol v6 the client receives displayed images WITHOUT their
        // bytes (only metadata). The render path queues a fetch for each image
        // that actually has bytes (`byte_len > 0`) — never for one whose bytes
        // are already present, nor for a genuinely zero-byte image.
        let mut app = test_app();
        let (tx, rx) = crossbeam_channel::unbounded::<ClientMessageType>();
        let meta = |byte_len| choreo_proto::ImageMetadata {
            mime_type: "image/png".to_string(),
            width: 4,
            height: 4,
            byte_len,
            alt: None,
        };
        let turn = Turn {
            created_at: choreo_proto::TimestampMs::now(),
            undone: false,
            error: None,
            user_text: None,
            assistant_text: None,
            assistant_reasoning: None,
            tool_calls: vec![],
            token_usage: None,
            tool_results: vec![],
            displayed_images: vec![
                // 0: already has bytes — nothing to fetch.
                choreo_proto::DisplayedImageRecord {
                    metadata: meta(4),
                    data: b"AAAA".to_vec(),
                    tool_call_id: None,
                },
                // 1: stripped (kept metadata, byte_len>0) — must be fetched.
                choreo_proto::DisplayedImageRecord {
                    metadata: meta(8),
                    data: Vec::new(),
                    tool_call_id: None,
                },
                // 2: stripped AND empty (byte_len==0) — nothing to fetch.
                choreo_proto::DisplayedImageRecord {
                    metadata: meta(0),
                    data: Vec::new(),
                    tool_call_id: None,
                },
            ],
            reasoning_artifact: None,
            reasoning_producer: None,
        };
        app.sync_turn_images(3, 9, &turn);
        app.request_image_fetch(3, 9, ImageSlot::Displayed(0));
        app.request_image_fetch(3, 9, ImageSlot::Displayed(1));
        app.request_image_fetch(3, 9, ImageSlot::Displayed(2));
        // A repeat call for the queued image must not double-queue (deduped via
        // the `fetching` flag until the reply arrives).
        app.request_image_fetch(3, 9, ImageSlot::Displayed(1));

        app.flush_image_fetches(&tx);
        let sent: Vec<ClientMessageType> = rx.try_iter().collect();
        assert_eq!(
            sent,
            vec![ClientMessageType::GetImage {
                session_id: 3,
                turn_id: 9,
                key: ImageKey::Displayed { index: 1 },
            }]
        );
    }

    #[test]
    fn tool_result_vision_image_is_fetched_by_call_id() {
        // A `read_image` vision image rides a byte-less `ImageReference` on the
        // client view; the render path must fetch it under
        // `ImageKey::ToolResult { call_id }` and store the reply in the same
        // `RenderedImage` map, keyed by the call id.
        let mut app = test_app();
        let (tx, rx) = crossbeam_channel::unbounded::<ClientMessageType>();
        let turn = Turn {
            created_at: choreo_proto::TimestampMs::now(),
            undone: false,
            error: None,
            user_text: None,
            assistant_text: None,
            assistant_reasoning: None,
            tool_calls: vec![],
            token_usage: None,
            tool_results: vec![choreo_proto::ToolResultRecord {
                call_id: "call_v".into(),
                name: "read_image".into(),
                content: "image".into(),
                is_error: false,
                invocation_description: "read_image".into(),
                // Client view: metadata present, bytes stripped.
                image: Some(choreo_proto::ImageReference {
                    path: "/tmp/a.png".into(),
                    mime_type: "image/jpeg".into(),
                    width: 8,
                    height: 6,
                    data: Vec::new(),
                }),
            }],
            displayed_images: vec![],
            reasoning_artifact: None,
            reasoning_producer: None,
        };
        app.sync_turn_images(2, 5, &turn);
        let slot = ImageSlot::ToolResult("call_v".into());
        // A placeholder exists with the reference's metadata (path as alt).
        let img = &app.rendered_images[&2][&5][&slot];
        assert_eq!(img.metadata.mime_type, "image/jpeg");
        assert_eq!(img.metadata.alt.as_deref(), Some("/tmp/a.png"));
        assert!(img.data.is_empty());

        app.request_image_fetch(2, 5, slot.clone());
        app.flush_image_fetches(&tx);
        let sent: Vec<ClientMessageType> = rx.try_iter().collect();
        assert_eq!(
            sent,
            vec![ClientMessageType::GetImage {
                session_id: 2,
                turn_id: 5,
                key: ImageKey::ToolResult {
                    call_id: "call_v".into()
                },
            }]
        );

        // The reply routes back to the call-id slot.
        app.handle_image_reply(
            2,
            5,
            ImageKey::ToolResult {
                call_id: "call_v".into(),
            },
            Some(b"JPEG!".to_vec()),
        );
        assert_eq!(app.rendered_images[&2][&5][&slot].data.as_ref(), b"JPEG!");
        assert!(!app.rendered_images[&2][&5][&slot].fetching);
    }

    #[test]
    fn handle_image_reply_fills_bytes_and_marks_missing() {
        // A `Some` reply stores the bytes (clearing the in-flight flag and any
        // stale decode-failure state); a `None` reply marks the image failed so
        // it is not re-requested every frame.
        let mut app = test_app();
        let meta = choreo_proto::ImageMetadata {
            mime_type: "image/png".to_string(),
            width: 4,
            height: 4,
            byte_len: 4,
            alt: None,
        };
        let turn = Turn {
            created_at: choreo_proto::TimestampMs::now(),
            undone: false,
            error: None,
            user_text: None,
            assistant_text: None,
            assistant_reasoning: None,
            tool_calls: vec![],
            token_usage: None,
            tool_results: vec![],
            displayed_images: vec![
                choreo_proto::DisplayedImageRecord {
                    metadata: meta.clone(),
                    data: Vec::new(),
                    tool_call_id: None,
                },
                choreo_proto::DisplayedImageRecord {
                    metadata: meta.clone(),
                    data: Vec::new(),
                    tool_call_id: None,
                },
            ],
            reasoning_artifact: None,
            reasoning_producer: None,
        };
        app.sync_turn_images(1, 2, &turn);

        app.handle_image_reply(
            1,
            2,
            ImageKey::Displayed { index: 0 },
            Some(b"PNG!".to_vec()),
        );
        let img0 = &app.rendered_images[&1][&2][&ImageSlot::Displayed(0)];
        assert_eq!(img0.data.as_ref(), b"PNG!");
        assert!(!img0.fetching);
        assert!(!img0.fetch_failed);

        app.handle_image_reply(1, 2, ImageKey::Displayed { index: 1 }, None);
        let img1 = &app.rendered_images[&1][&2][&ImageSlot::Displayed(1)];
        assert!(img1.fetch_failed);
        assert!(!img1.fetching);
        assert!(img1.data.is_empty());

        // A failed image is never re-queued by a later render pass.
        app.request_image_fetch(1, 2, ImageSlot::Displayed(1));
        assert_eq!(app.pending_image_fetch.len(), 0);
    }

    #[test]
    fn handle_image_reply_empty_for_nonempty_image_latches_failure() {
        // A malformed reply — `Some(vec![])` for an image advertised with
        // `byte_len > 0` — must NOT spin the fetch loop: the empty payload is
        // latched as a (recoverable) failure so the render path stops, exactly
        // like a `None` reply. `sync_turn_images` still clears the latch on a
        // later re-advertisement, so recovery is preserved.
        let mut app = test_app();
        let meta = choreo_proto::ImageMetadata {
            mime_type: "image/png".to_string(),
            width: 4,
            height: 4,
            byte_len: 8,
            alt: None,
        };
        let turn = Turn {
            created_at: choreo_proto::TimestampMs::now(),
            undone: false,
            error: None,
            user_text: None,
            assistant_text: None,
            assistant_reasoning: None,
            tool_calls: vec![],
            token_usage: None,
            tool_results: vec![],
            displayed_images: vec![choreo_proto::DisplayedImageRecord {
                metadata: meta,
                data: Vec::new(),
                tool_call_id: None,
            }],
            reasoning_artifact: None,
            reasoning_producer: None,
        };
        app.sync_turn_images(4, 1, &turn);

        // The daemon returns an empty payload despite `byte_len > 0`.
        app.handle_image_reply(4, 1, ImageKey::Displayed { index: 0 }, Some(Vec::new()));
        let img = &app.rendered_images[&4][&1][&ImageSlot::Displayed(0)];
        assert!(img.fetch_failed);
        assert!(img.data.is_empty());
        assert!(!img.fetching);

        // No re-request on the next render pass (the loop is broken).
        app.request_image_fetch(4, 1, ImageSlot::Displayed(0));
        assert_eq!(app.pending_image_fetch.len(), 0);

        // A later re-advertisement clears the latch (recovery still works).
        app.sync_turn_images(4, 1, &turn);
        assert!(!app.rendered_images[&4][&1][&ImageSlot::Displayed(0)].fetch_failed);
        app.request_image_fetch(4, 1, ImageSlot::Displayed(0));
        assert_eq!(
            app.pending_image_fetch,
            vec![(4, 1, ImageSlot::Displayed(0))]
        );
    }

    #[test]
    fn sync_turn_images_recovers_transient_fetch_failure() {
        // A transient `None` (e.g. the emit-time storage write failed) latches
        // `fetch_failed`, which normally stops re-requesting. When the daemon
        // later finalizes and re-persists the image it re-broadcasts the turn,
        // which lands here; that re-advertisement must clear the latch so the
        // next render pass re-requests the image instead of hiding it forever.
        let mut app = test_app();
        let meta = choreo_proto::ImageMetadata {
            mime_type: "image/png".to_string(),
            width: 4,
            height: 4,
            byte_len: 8,
            alt: None,
        };
        let turn = Turn {
            created_at: choreo_proto::TimestampMs::now(),
            undone: false,
            error: None,
            user_text: None,
            assistant_text: None,
            assistant_reasoning: None,
            tool_calls: vec![],
            token_usage: None,
            tool_results: vec![],
            displayed_images: vec![choreo_proto::DisplayedImageRecord {
                metadata: meta.clone(),
                data: Vec::new(),
                tool_call_id: None,
            }],
            reasoning_artifact: None,
            reasoning_producer: None,
        };
        app.sync_turn_images(7, 3, &turn);

        // The daemon had no bytes yet: the fetch comes back `None`.
        app.handle_image_reply(7, 3, ImageKey::Displayed { index: 0 }, None);
        assert!(app.rendered_images[&7][&3][&ImageSlot::Displayed(0)].fetch_failed);
        // While latched, a render pass leaves the image alone.
        app.request_image_fetch(7, 3, ImageSlot::Displayed(0));
        assert_eq!(app.pending_image_fetch.len(), 0);

        // Finalize re-broadcasts the turn (image still advertised with
        // byte_len > 0 and no bytes inline): the latch must clear...
        app.sync_turn_images(7, 3, &turn);
        assert!(!app.rendered_images[&7][&3][&ImageSlot::Displayed(0)].fetch_failed);

        // ...so the next render pass re-queues the fetch.
        app.request_image_fetch(7, 3, ImageSlot::Displayed(0));
        assert_eq!(
            app.pending_image_fetch,
            vec![(7, 3, ImageSlot::Displayed(0))]
        );
    }
}
