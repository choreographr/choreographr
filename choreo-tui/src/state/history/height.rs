//! The history height model: the per-turn height prefix-sum, the scrollbar
//! markers projected from it, and the dirty flags that schedule a rebuild.
//!
//! The model answers two questions in logarithmic time: which turn covers a
//! given content row, and where a transcript marker (a turn's user prompt) sits
//! on the scrollbar.  It answers both by keeping three arrays in lockstep —
//! `visible_turn_ids`, `turn_heights` and their prefix sum `height_prefix` —
//! rebuilt only when something structural changed.  Keeping the model, the
//! marker projection and the image-block reserve in one file is what makes that
//! lockstep checkable: every writer of the three arrays is here.
//!
//! The reserve is the one turn-height input that depends on the viewport
//! ([`SessionDisplayState::image_block_height`]), which is why a height-only
//! viewport change lands on this module's invalidation flags and nothing else
//! (see [`super::ViewportChange::HeightsOnly`]).
//!
//! The `App` wrappers that forward to the active session's display sit here too,
//! beside the methods they drive, so the `App` entry point and the model behind
//! it are read together.  Re-exported from `state` (`pub(crate) use history::*`),
//! so `crate::state::Marker` and the methods keep resolving crate-wide.

use super::super::{App, SessionDisplayState, turn_image_count};
use super::cache::{RenderCacheKey, TurnLayout, cached_or_compute_lines};
use super::viewport::{HistoryViewport, semantic_line_row_range, semantic_line_row_start};
use crate::markdown_render::{reasoning_expanded_default, render_turn_lines};

/// A transcript marker projected onto the scrollbar: the content line it sits
/// on, and the virtual scrollbar slot that line is currently mapped to.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Marker {
    pub content_line: usize,
    /// The virtual slot this marker currently occupies in the cached
    /// [`SessionDisplayState::marker_slots`] bitmap — the slot it is drawn at.
    /// [`SessionDisplayState::ensure_marker_slots`] writes it in the same pass
    /// that sets the bitmap bit, so the stored slot and the bitmap agree by
    /// construction; the scrollbar click handler tests this field against the
    /// clicked half-slots, so a click hits exactly what was drawn.
    pub virtual_slot: usize,
}

// `HistoryViewport` is a 4-byte Copy struct; every call site already holds a
// reference (App-level wrappers, tests), so taking it by value would churn
// signatures across the crate for no measurable gain.
#[expect(clippy::trivially_copy_pass_by_ref)]
impl SessionDisplayState {
    pub(crate) fn total_history_height(&self) -> usize {
        self.height_prefix.last().copied().unwrap_or(0)
    }

    /// Rebuild `height_prefix`, `markers`, `visible_turn_ids`, and populate
    /// `render_cache`.
    pub(crate) fn rebuild_height_prefix(&mut self, viewport: &HistoryViewport) {
        self.height_prefix.clear();
        self.visible_turn_ids.clear();
        self.markers.clear();
        self.turn_layouts.clear();
        self.turn_heights.clear();
        self.any_image_turns = false;
        let mut total = 0usize;
        let fallback_img_height = Self::image_block_height(viewport) as usize;
        let turn_count = self.view.turns.len();
        tracing::trace!(turn_count, "rebuild_height_prefix");

        let visible_count = self.view.turns.iter().filter(|(_, t)| !t.undone).count();
        self.render_cache.resize(visible_count, None);

        let mut visible_idx = 0usize;
        for (&turn_id, turn) in &self.view.turns {
            if turn.undone {
                continue;
            }
            // Must stay in lockstep with render_history (render.rs) so the
            // render-cache key never drifts from what the renderer draws.
            let content_width = viewport.width.saturating_sub(9);
            let tool_content_width = viewport.width.saturating_sub(1);

            // Effective reasoning visibility for this turn: the per-turn
            // user override (from clicking the header), falling back to the
            // streaming-derived default.  The derived default is also stored
            // in the turn layout so the per-frame render path can reuse it
            // in O(1) without re-scanning turn strings.
            let reasoning_default_expanded = reasoning_expanded_default(turn);
            let reasoning_expanded =
                self.effective_reasoning_expanded(turn_id, reasoning_default_expanded);

            // Effective per-result collapse state, aligned with
            // `turn.tool_results`; part of the render-cache key so a stale
            // entry (rendered with a different visibility) is a miss.
            let tool_results_collapsed: Vec<bool> = turn
                .tool_results
                .iter()
                .map(|r| self.effective_tool_result_collapsed(turn_id, r))
                .collect();
            let key = RenderCacheKey {
                turn_id,
                width: content_width,
                viewport_width: viewport.width,
                reasoning_expanded,
                tool_results_collapsed,
                content_version: self.turn_content_version(turn_id),
            };
            let rendered_turn =
                cached_or_compute_lines(&mut self.render_cache, visible_idx, &key, || {
                    render_turn_lines(
                        turn,
                        content_width,
                        tool_content_width,
                        key.reasoning_expanded,
                        &key.tool_results_collapsed,
                    )
                });
            let text_height = rendered_turn.height;
            let text_offsets = rendered_turn.visual_offsets;
            let reasoning_header_idx = rendered_turn.reasoning_header_idx;
            let tool_result_header_idxs = rendered_turn.tool_result_header_idxs;

            // The reasoning header's visual-row range for click hit-testing.
            // The renderer reports the header's semantic-line index directly
            // (no output scanning); the cached offsets convert it to a
            // visual-row range — O(1) in the click handler, same approach
            // as image ranges.
            let reasoning_header_range =
                reasoning_header_idx.map(|idx| semantic_line_row_range(&text_offsets, idx));

            // Same conversion for every tool result header, so clicking a
            // triangle toggles exactly that result.  One range per result,
            // aligned with `turn.tool_results`.
            let tool_result_header_ranges = tool_result_header_idxs
                .iter()
                .map(|&idx| semantic_line_row_range(&text_offsets, idx))
                .collect();

            let mut image_ranges: Vec<(usize, usize)> = Vec::new();
            let mut total_img_height: usize = 0;
            // Every image slot the turn exposes (displayed + tool-result vision)
            // reserves one block, in the SAME order the render loop draws them
            // (the height path uses `turn_image_count`, the render loop
            // `turn_image_slots`, and both walk the same source order), so a
            // click's `image_ranges` index maps onto that slot list exactly.
            let img_count = turn_image_count(turn);
            if img_count > 0 {
                self.any_image_turns = true;
            }
            for _ in 0..img_count {
                let start = text_height + total_img_height;
                image_ranges.push((start, start + fallback_img_height));
                total_img_height += fallback_img_height;
            }
            // Visual-row offset of the response block's top row, for the
            // streaming auto-pin.  Converted from the response's semantic-line
            // index through `text_offsets` — the same mapping the reasoning
            // header range uses above.
            let response_start_row = rendered_turn
                .response_start_idx
                .map(|idx| semantic_line_row_start(&text_offsets, idx));
            self.turn_layouts.push(TurnLayout {
                reasoning_header_range,
                reasoning_default_expanded,
                image_ranges,
                tool_result_header_ranges,
                response_start_row,
            });
            let turn_height = text_height + total_img_height;
            self.turn_heights.push(turn_height);
            total += turn_height;
            self.height_prefix.push(total);
            self.visible_turn_ids.push(turn_id);
            visible_idx += 1;
        }
        // Derive the markers from the freshly filled `visible_turn_ids` /
        // `turn_heights` (the one source of the marker set) now that the total
        // height is known.
        self.rebuild_markers(viewport);
        let final_total = self.total_history_height().max(1);
        tracing::trace!(
            marker_count = self.markers.len(),
            final_total,
            "computed markers"
        );
        self.markers_dirty = false;
    }

    /// Mark the streaming fast path as the owner of the next settle: only the
    /// in-flight turn needs re-rendering, so the height model takes the
    /// incremental path (`apply_streaming_update`) instead of the full rebuild.
    pub(crate) fn mark_streaming_changed(&mut self) {
        self.streaming_dirty = true;
        self.content_dirty = true;
    }

    /// Mark a structural content change (a turn appended, replaced, undone, or a
    /// snapshot merged): the whole height model is stale and the streaming fast
    /// path's cached turn index no longer identifies the streaming turn.
    pub(crate) fn mark_content_changed(&mut self) {
        self.markers_dirty = true;
        self.content_dirty = true;
        self.streaming_turn_index = None;
        self.streaming_dirty = false;
    }

    pub(crate) fn resolve_streaming_turn_index(&mut self, stream_id: u64) {
        if self.streaming_turn_index.is_none()
            && let Some(turn_id) = self.view.request_turn_id(stream_id)
        {
            self.streaming_turn_index = self.visible_turn_ids.iter().position(|id| *id == turn_id);
        }
    }

    pub(crate) fn compute_total_height_and_markers(&mut self, viewport: &HistoryViewport) -> usize {
        // Streaming content updates run FIRST — even when a separate event
        // also marked markers_dirty — so a mid-stream `Done`/`TurnAppended`/
        // `SessionState` (from this session or, via the all-activity
        // subscription, a busy background session) can never force the per-
        // chunk cost onto the O(n) full rebuild.  The fast path re-renders
        // only the streaming turn and applies its height delta incrementally;
        // the rebuild below (if markers_dirty is still set) then reuses the
        // fresh cache entry, so it only recomputes turns whose content
        // actually changed (content-version key) instead of re-rendering
        // everything.
        if self.streaming_dirty {
            self.apply_streaming_update(viewport);
        }
        if self.markers_dirty {
            let at_bottom = self.effective_scroll(viewport) == 0;
            let preserve_scroll = self.content_dirty && !at_bottom;
            let old_total = if preserve_scroll {
                self.total_history_height()
            } else {
                0
            };

            self.rebuild_height_prefix(viewport);

            if preserve_scroll {
                let new_total = self.total_history_height();
                if new_total > old_total {
                    self.history_scroll.scroll = self
                        .history_scroll
                        .scroll
                        .saturating_add(new_total - old_total);
                } else if old_total > new_total {
                    // Content shrank (e.g. collapsing a reasoning section or
                    // undoing turns).  Pull the scroll offset up by the
                    // removed height so the same content rows stay anchored
                    // in the viewport instead of jumping to the bottom.
                    self.history_scroll.scroll = self
                        .history_scroll
                        .scroll
                        .saturating_sub(old_total - new_total);
                }
            }

            // A session switch captured an absolute reading position on the
            // session we left; restore it now that the height model is fresh
            // (wins over the preserve adjustment above, which cannot be both
            // pending and meaningful on the same rebuild).
            self.apply_scroll_restore(viewport);

            self.content_dirty = false;
            self.streaming_dirty = false;
        }

        // After the height model and any restored position are settled, give
        // the streaming response its one chance to pin its top row at the
        // viewport top (see the `pin` module).  This is the single point every
        // frame's model passes through before rendering, so the pin is correct
        // on the very first frame after a session switch, a resize, a
        // cache-miss rebuild, or an interleaved `Done` — not only on the
        // streaming fast path.
        self.maybe_pin_response_top(viewport);

        // Settle the scrollbar's cached occupancy bitmap here, in the height
        // model's single settle point, so the scrollbar draw only borrows it.
        // A no-op unless the marker set or the virtual track changed, and it
        // keeps every marker's stored slot in lockstep with the bitmap it draws.
        self.ensure_marker_slots(viewport);

        self.total_history_height().max(1)
    }

    pub(crate) fn rebuild_height_prefix_preserving_scroll(&mut self, viewport: &HistoryViewport) {
        let at_bottom = self.effective_scroll(viewport) == 0;
        let preserve_scroll = self.content_dirty && !at_bottom;
        let old_total = if preserve_scroll {
            self.total_history_height()
        } else {
            0
        };

        self.rebuild_height_prefix(viewport);

        if preserve_scroll {
            let new_total = self.total_history_height();
            if new_total > old_total {
                self.history_scroll.scroll = self
                    .history_scroll
                    .scroll
                    .saturating_add(new_total - old_total);
            } else if old_total > new_total {
                // Mirror the anchor-preserving adjustment in
                // `compute_total_height_and_markers`: pull the scroll offset
                // up by the removed height rather than jumping to the bottom.
                self.history_scroll.scroll = self
                    .history_scroll
                    .scroll
                    .saturating_sub(old_total - new_total);
            }
        }

        self.streaming_dirty = false;
        self.content_dirty = false;
    }

    pub(crate) fn rebuild_markers(&mut self, viewport: &HistoryViewport) {
        self.markers.clear();
        let total = self.total_history_height().max(1);
        let virtual_track = Self::virtual_track_slots(viewport);
        let mut accum = 0usize;
        for (i, &turn_id) in self.visible_turn_ids.iter().enumerate() {
            // `turn_heights` is kept in lockstep with `visible_turn_ids` (one
            // entry per visible turn); a height of 0 skips the marker for a
            // drift-affected turn instead of panicking.
            let turn_height = self.turn_heights.get(i).copied().unwrap_or(0);
            if let Some(turn) = self.view.turns.get(&turn_id)
                && turn.user_text.is_some()
            {
                let slot = Self::marker_slot(accum, virtual_track, total);
                self.markers.push(Marker {
                    content_line: accum,
                    virtual_slot: slot,
                });
            }
            accum += turn_height;
        }
        // The marker set changed, so the renderer's slot bitmap is stale.
        self.marker_slots_dirty = true;
    }

    /// Map a content line to its virtual scrollbar slot: the line scaled into
    /// the 2-per-row virtual track against the current total height.  Every
    /// place that computes a marker slot goes through here, so the marker set,
    /// the cached occupancy bitmap and the click handler share one mapping.
    pub(crate) fn marker_slot(content_line: usize, virtual_track: usize, total: usize) -> usize {
        content_line * virtual_track / total.max(1)
    }

    /// Ensure the scrollbar's marker-slot bitmap matches the current markers and
    /// viewport: a viewport-sized `true`-per-occupied-slot array the renderer
    /// draws from in O(vh).
    ///
    /// Rebuilt only when the marker set changed ([`Self::marker_slots_dirty`])
    /// or the viewport height altered the track length; every other frame —
    /// including every scrolling frame and every streaming chunk — reuses it,
    /// which is what keeps the scrollbar draw O(vh) instead of O(markers).
    ///
    /// Each marker's slot is recomputed from its *content line* against the
    /// current total and track and written back into [`Marker::virtual_slot`] in
    /// the same pass that sets the bitmap bit, so the stored slot is always the
    /// slot the bitmap drew — even after a track change or a streaming response
    /// that drifted the total without rebuilding the marker set.
    pub(crate) fn ensure_marker_slots(&mut self, viewport: &HistoryViewport) {
        let virtual_track = Self::virtual_track_slots(viewport);
        if !self.marker_slots_dirty && self.marker_slots.len() == virtual_track {
            return;
        }
        let total = self.total_history_height().max(1);
        // Reuse the buffer: a rebuild happens once per structural change (or
        // track change), not per frame, so resizing in place avoids handing the
        // allocator a fresh viewport-sized `Vec` each time.
        self.marker_slots.resize(virtual_track, false);
        self.marker_slots.fill(false);
        for marker in &mut self.markers {
            let slot = Self::marker_slot(marker.content_line, virtual_track, total);
            marker.virtual_slot = slot;
            if let Some(bit) = self.marker_slots.get_mut(slot) {
                *bit = true;
            }
        }
        self.marker_slots_dirty = false;
    }

    // Pure function of the viewport — no display state involved.
    pub(crate) fn virtual_track_slots(viewport: &HistoryViewport) -> usize {
        2 * viewport.height as usize
    }

    // Pure function of the viewport — no display state involved.
    pub(crate) fn image_block_height(viewport: &HistoryViewport) -> u16 {
        (viewport.height / 2).max(1)
    }
}

impl App {
    /// Test-only: production paths reach the rebuild through
    /// [`Self::compute_total_height_and_markers`], which is the one place the
    /// model may be rebuilt mid-frame (it must also settle the scroll position
    /// and the marker slots).  Tests drive the rebuild directly to inspect the
    /// height model in isolation.
    #[cfg(test)]
    pub(crate) fn rebuild_height_prefix(&mut self) {
        let vp = self.history_viewport;
        if let Some(d) = self.active_display() {
            d.rebuild_height_prefix(&vp);
        }
    }

    pub(crate) fn compute_total_height_and_markers(&mut self) -> usize {
        let vp = self.history_viewport;
        self.active_display()
            .map_or(1, |d| d.compute_total_height_and_markers(&vp))
    }

    /// Test-only: production marks streaming changes on the display that owns
    /// the streaming turn as the chunk arrives; the `App`-level shortcut exists
    /// so a test can arm the fast path on the active session without reaching
    /// into its display.
    #[cfg(test)]
    pub(crate) fn mark_streaming_changed(&mut self) {
        if let Some(d) = self.active_display() {
            d.mark_streaming_changed();
        }
    }

    pub(crate) fn image_block_height(&self) -> u16 {
        // Pure function of the viewport; no display state needed.
        SessionDisplayState::image_block_height(&self.history_viewport)
    }
}
