//! Displayed-image state and on-demand fetch plumbing.
//!
//! Protocol v6 strips displayed-image bytes from turn snapshots (only metadata
//! survives), so the TUI fetches each image's bytes lazily: the per-frame render
//! path calls [`App::request_image_fetch`], the UI loop drains the queue with
//! [`App::flush_image_fetches`], and the daemon's reply is applied by
//! [`App::handle_image_reply`]. The encoded-bitmap jobs are handled separately by
//! [`App::apply_image_result`]/[`App::submit_image_job`]. All of these are
//! inherent `App` methods living in this sibling module; their fields stay on
//! `App` in `state/mod.rs`.

use super::App;
use crate::RenderedImage;
use crate::image_worker::{ImageId, ImageJob, ImageResult, next_job_id};
use choreo_proto::{ClientMessage, Turn};
use ratatui::layout::Size;
use std::sync::Arc;

impl App {
    pub(crate) fn sync_turn_images(&mut self, session_id: u64, turn_id: u32, turn: &Turn) {
        let images = self
            .rendered_images
            .entry(session_id)
            .or_default()
            .entry(turn_id)
            .or_default();
        for (idx, record) in turn.displayed_images.iter().enumerate() {
            images
                .entry(idx)
                .and_modify(|img| {
                    // Recovery signal: a turn is (re-)advertised here only when
                    // the daemon emits or re-broadcasts it. A transient failure
                    // earlier (the emit-time storage write failed, so the fetch
                    // came back `None` and latched `fetch_failed`) can be undone
                    // because a finalize-time re-broadcast means the daemon
                    // re-persisted the image. Only clear the latch when there is
                    // genuinely something to fetch (`byte_len > 0`) and no bytes
                    // were stored; `request_image_fetch` still skips zero-byte
                    // placeholders. Per-frame spinning stays prevented: between
                    // render passes `fetch_failed`/`fetching` gate re-requests,
                    // and a re-fetch is only attempted once the latch is cleared.
                    if img.fetch_failed && img.data.is_empty() && img.metadata.byte_len > 0 {
                        img.fetch_failed = false;
                    }
                })
                .or_insert_with(|| {
                    RenderedImage::new_placeholder(
                        record.metadata.clone(),
                        Arc::from(record.data.clone()),
                    )
                });
        }
        images.retain(|&idx, _| idx < turn.displayed_images.len());
    }

    pub(crate) fn apply_image_result(&mut self, result: ImageResult) {
        let Some((session_id, turn_id, img_idx)) = self.pending_job_idx.remove(&result.id) else {
            return;
        };
        if let Some(session_images) = self.rendered_images.get_mut(&session_id)
            && let Some(images) = session_images.get_mut(&turn_id)
            && let Some(img) = images.get_mut(&img_idx)
            && img.pending_job == Some(result.id)
        {
            tracing::trace!(
                "[choreo-tui] image job {} completed for session {} turn {} img {}",
                result.id,
                session_id,
                turn_id,
                img_idx,
            );
            img.apply_result(result);
        }
    }

    /// Queue an on-demand fetch for a displayed image whose bytes were not
    /// shipped in the turn snapshot (protocol v6 strips them). Deduped via the
    /// image's own `fetching`/`fetch_failed` flags so the per-frame render path
    /// cannot enqueue the same fetch repeatedly while a reply is in flight.
    /// Only images that HAVE bytes to fetch (`byte_len > 0`) are requested; a
    /// zero-byte image is a legitimately empty placeholder with nothing to
    /// load, and one already resolved (bytes present, in flight, or failed)
    /// is skipped.
    pub(crate) fn request_image_fetch(&mut self, session_id: u64, turn_id: u32, img_idx: usize) {
        let Some(img) = self
            .rendered_images
            .get_mut(&session_id)
            .and_then(|s| s.get_mut(&turn_id))
            .and_then(|imgs| imgs.get_mut(&img_idx))
        else {
            return;
        };
        if img.fetching || img.fetch_failed || !img.data.is_empty() || img.metadata.byte_len == 0 {
            return;
        }
        img.fetching = true;
        self.pending_image_fetch
            .push((session_id, turn_id, img_idx));
    }

    /// Send every queued `GetImage` request to the daemon. Called by the UI
    /// loop — the only place that owns the client sender — once per rendered
    /// frame, so a scroll that reveals new images issues their fetches on the
    /// next pass.
    pub(crate) fn flush_image_fetches(
        &mut self,
        client_tx: &std::sync::mpsc::Sender<ClientMessage>,
    ) {
        for (session_id, turn_id, img_idx) in self.pending_image_fetch.drain(..) {
            // Indices are small; use `try_from` so a hypothetical >u32 index
            // saturates rather than silently wrapping onto the wrong image.
            let image_index = u32::try_from(img_idx).unwrap_or(u32::MAX);
            let _ = client_tx.send(ClientMessage::GetImage {
                session_id,
                turn_id,
                image_index,
            });
        }
    }

    /// Apply a `DaemonMessage::Image` reply: store the fetched bytes (clearing
    /// any decode-failure state so the next render submits an encoding job), or
    /// mark the image failed when the daemon had none (not found), so it is not
    /// re-requested every frame. A `fetch_failed` latch is not permanent: a
    /// later re-advertisement of the turn (finalize re-broadcast) clears it —
    /// see [`App::sync_turn_images`].
    pub(crate) fn handle_image_reply(
        &mut self,
        session_id: u64,
        turn_id: u32,
        image_index: u32,
        data: Option<Vec<u8>>,
    ) {
        let Ok(img_idx) = usize::try_from(image_index) else {
            return;
        };
        let Some(img) = self
            .rendered_images
            .get_mut(&session_id)
            .and_then(|s| s.get_mut(&turn_id))
            .and_then(|imgs| imgs.get_mut(&img_idx))
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
            }
            // Not found (deleted/evicted/stale index): stop re-requesting so a
            // missing image cannot spin a fetch every frame.
            None => img.fetch_failed = true,
        }
    }

    // All eight parameters are already owned by the caller (an image-ready
    // event handler); grouping them would only add a wrapper struct without
    // reducing the information flow.
    #[expect(clippy::too_many_arguments)]
    pub(crate) fn submit_image_job(
        &mut self,
        session_id: u64,
        turn_id: u32,
        img_idx: usize,
        data: std::sync::Arc<[u8]>,
        metadata: choreo_proto::ImageMetadata,
        cell_size: Size,
        resize: ratatui_image::Resize,
    ) -> Option<ImageId> {
        let tx = self.image_job_tx.as_ref()?;
        let id = next_job_id();

        tracing::trace!(
            "[choreo-tui] submitting image job {} for session {} turn {} img {} ({} {}x{} @ {}x{})",
            id,
            session_id,
            turn_id,
            img_idx,
            metadata.mime_type,
            metadata.width,
            metadata.height,
            cell_size.width,
            cell_size.height,
        );

        self.pending_job_idx
            .insert(id, (session_id, turn_id, img_idx));

        if let Some(session_images) = self.rendered_images.get_mut(&session_id)
            && let Some(images) = session_images.get_mut(&turn_id)
            && let Some(img) = images.get_mut(&img_idx)
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
        let (tx, rx) = std::sync::mpsc::channel::<ClientMessage>();
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
        app.request_image_fetch(3, 9, 0);
        app.request_image_fetch(3, 9, 1);
        app.request_image_fetch(3, 9, 2);
        // A repeat call for the queued image must not double-queue (deduped via
        // the `fetching` flag until the reply arrives).
        app.request_image_fetch(3, 9, 1);

        app.flush_image_fetches(&tx);
        let sent: Vec<ClientMessage> = rx.try_iter().collect();
        assert_eq!(
            sent,
            vec![ClientMessage::GetImage {
                session_id: 3,
                turn_id: 9,
                image_index: 1,
            }]
        );
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

        app.handle_image_reply(1, 2, 0, Some(b"PNG!".to_vec()));
        let img0 = &app.rendered_images[&1][&2][&0];
        assert_eq!(img0.data.as_ref(), b"PNG!");
        assert!(!img0.fetching);
        assert!(!img0.fetch_failed);

        app.handle_image_reply(1, 2, 1, None);
        let img1 = &app.rendered_images[&1][&2][&1];
        assert!(img1.fetch_failed);
        assert!(!img1.fetching);
        assert!(img1.data.is_empty());

        // A failed image is never re-queued by a later render pass.
        app.request_image_fetch(1, 2, 1);
        assert_eq!(app.pending_image_fetch.len(), 0);
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
        app.handle_image_reply(7, 3, 0, None);
        assert!(app.rendered_images[&7][&3][&0].fetch_failed);
        // While latched, a render pass leaves the image alone.
        app.request_image_fetch(7, 3, 0);
        assert_eq!(app.pending_image_fetch.len(), 0);

        // Finalize re-broadcasts the turn (image still advertised with
        // byte_len > 0 and no bytes inline): the latch must clear...
        app.sync_turn_images(7, 3, &turn);
        assert!(!app.rendered_images[&7][&3][&0].fetch_failed);

        // ...so the next render pass re-queues the fetch.
        app.request_image_fetch(7, 3, 0);
        assert_eq!(app.pending_image_fetch, vec![(7, 3, 0)]);
    }
}
