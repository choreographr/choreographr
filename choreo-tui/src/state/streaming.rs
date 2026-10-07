//! The streaming fast path: incremental re-render of the in-flight turn.
//!
//! While a response streams, [`SessionDisplayState::compute_total_height_and_markers`]
//! routes each chunk through [the streaming update](SessionDisplayState::apply_streaming_update)
//! instead of the O(n) full rebuild, so the per-chunk cost stays proportional to
//! the appended bytes rather than the whole conversation.  The assistant
//! response's committed markdown is kept in [`StreamingResponseCache`] (see
//! [`IncrementalMarkdown`]) so only the appended tail is re-parsed each frame.
//!
//! This module owns the response cache and the fast-path method; the height
//! prefix and render-cache plumbing it drives live in [`super`].

use super::{HistoryViewport, RenderCacheKey, RenderedCache, RenderedTurn, SessionDisplayState};
use crate::markdown_render::{
    IncrementalMarkdown, compute_visual_offsets, lines_height, reasoning_expanded_default,
    render_turn_lines, render_turn_lines_streaming,
};
use std::sync::Arc;

/// Incremental response-render state for the streaming fast path.
///
/// The streaming fast path re-renders the in-flight turn on every chunk;
/// keeping the assistant response's committed markdown (see
/// [`IncrementalMarkdown`]) here lets it re-parse only the appended tail instead
/// of the whole, growing response each frame.  It is keyed to the streaming
/// turn so a new turn never reuses another turn's committed lines.
pub(crate) struct StreamingResponseCache {
    pub(crate) turn_id: u32,
    pub(crate) markdown: IncrementalMarkdown,
}

// `HistoryViewport` is a 4-byte Copy struct; every call site already holds a
// reference (App-level wrappers, tests), so taking it by value would churn
// signatures across the crate for no measurable gain.  Same rationale as the
// main `impl SessionDisplayState` block in `state/mod.rs`.
#[expect(clippy::trivially_copy_pass_by_ref)]
impl SessionDisplayState {
    /// Re-render the in-flight turn through the incremental response cache and
    /// refresh the render cache, the turn layout's header hit-ranges, and the
    /// height prefix in place.
    ///
    /// The streaming fast path: when a chunk lands on the turn identified by
    /// [`Self::streaming_turn_index`], only that turn is re-rendered, and only
    /// its appended response tail is re-parsed (see [`StreamingResponseCache`]).
    /// When the fast path cannot run — no streaming turn, a missing render-cache
    /// entry, or a *shrinking* turn — it falls back to the O(n) rebuild via
    /// [`Self::rebuild_height_prefix_preserving_scroll`], which recomputes every
    /// turn from scratch.
    pub(crate) fn apply_streaming_update(&mut self, viewport: &HistoryViewport) {
        let Some(turn_idx) = self.streaming_turn_index else {
            return self.rebuild_height_prefix_preserving_scroll(viewport);
        };
        if turn_idx >= self.visible_turn_ids.len() {
            return self.rebuild_height_prefix_preserving_scroll(viewport);
        }

        // `turn_idx < self.visible_turn_ids.len()` was checked above.
        let turn_id = self
            .visible_turn_ids
            .get(turn_idx)
            .copied()
            .unwrap_or_default();
        let Some(turn) = self.view.turns.get(&turn_id) else {
            return self.rebuild_height_prefix_preserving_scroll(viewport);
        };

        // Must stay in lockstep with render_history (render.rs) so the
        // render-cache key never drifts from what the renderer draws.
        let content_width = viewport.width.saturating_sub(9);
        let tool_content_width = viewport.width.saturating_sub(1);

        // Re-render with the effective reasoning visibility so the streaming
        // fast path stays consistent with the collapsed/expanded state.  The
        // derived default is stored back into the turn layout, keeping the
        // per-frame render path O(1).
        let reasoning_default_expanded = reasoning_expanded_default(turn);
        let reasoning_expanded =
            self.effective_reasoning_expanded(turn_id, reasoning_default_expanded);

        // Effective per-result collapse state (aligned with tool_results).
        // This is what makes "stream while toggled visible" work: chunks
        // re-render the streaming turn with the user's visibility choice,
        // so an expanded result grows live and a collapsed one stays flat.
        let tool_results_collapsed: Vec<bool> = turn
            .tool_results
            .iter()
            .map(|r| self.effective_tool_result_collapsed(turn_id, r))
            .collect();

        // Snapshot the turn's current content version before the mutable
        // `render_cache` borrow below — the lookup borrows all of `self`,
        // which would conflict with the `get_mut` held across the cache write.
        let content_version = self.turn_content_version(turn_id);

        // Keep the incremental response cache keyed to this turn: a new
        // streaming turn gets a fresh cache, so an unrelated response never
        // reuses another turn's committed markdown.
        if !matches!(&self.streaming_response, Some(cache) if cache.turn_id == turn_id) {
            self.streaming_response = Some(StreamingResponseCache {
                turn_id,
                markdown: IncrementalMarkdown::new(),
            });
        }

        if let Some(Some(cached)) = self.render_cache.get_mut(turn_idx)
            && cached.key.turn_id == turn_id
            && cached.key.width == content_width
            && cached.key.viewport_width == viewport.width
        {
            // `streaming_response` was set to a cache for `turn_id` just above,
            // so the `Some` arm is taken; the `None` arm keeps the path total if
            // that invariant is ever broken.
            let rendered = match self.streaming_response.as_mut() {
                Some(cache) => render_turn_lines_streaming(
                    turn,
                    content_width,
                    tool_content_width,
                    reasoning_expanded,
                    &tool_results_collapsed,
                    &mut cache.markdown,
                ),
                None => render_turn_lines(
                    turn,
                    content_width,
                    tool_content_width,
                    reasoning_expanded,
                    &tool_results_collapsed,
                ),
            };
            // Pin the same parallel-array invariant the rebuild path asserts
            // in `cached_or_compute_lines`: the streaming fast path replaces
            // the cache entry wholesale, so a join/content-range mismatch
            // here would silently slip into the cache and degrade a later
            // selection copy to newline-joined rows.  The asserts catch the
            // drift in debug builds before the Arc conversions hide it.
            debug_assert_eq!(
                rendered.lines.len(),
                rendered.joins.len(),
                "joins must align with the lines"
            );
            debug_assert_eq!(
                rendered.lines.len(),
                rendered.content_ranges.len(),
                "content ranges must align with the lines"
            );
            debug_assert_eq!(
                rendered.lines.len(),
                rendered.chrome_ranges.len(),
                "chrome ranges must align with the lines"
            );
            let text_lines = rendered.lines;
            let text_height = lines_height(&text_lines, viewport.width).max(1);
            let visual_offsets = compute_visual_offsets(&text_lines, viewport.width);
            let content_ranges = Arc::from(rendered.content_ranges);
            let joins = Arc::from(rendered.joins);
            let chrome_ranges = Arc::from(rendered.chrome_ranges);

            // Keep the reasoning header's click-hit range and the precomputed
            // default in sync as the response streams — the header sits below
            // the growing response, so its position shifts on every chunk.
            // Rebuilds (via `rebuild_height_prefix`) recompute from scratch.
            if let Some(layout) = self.turn_layouts.get_mut(turn_idx) {
                layout.reasoning_header_range = rendered.reasoning_header_idx.map(|idx| {
                    // In-bounds by construction (see the rebuild path).
                    let start = if idx == 0 {
                        0
                    } else {
                        visual_offsets.get(idx - 1).copied().unwrap_or(0)
                    };
                    let end = visual_offsets.get(idx).copied().unwrap_or(0);
                    (start, end)
                });
                layout.reasoning_default_expanded = reasoning_default_expanded;
                // Same sync for tool result headers: they sit below the
                // growing response too, and their own bodies grow when
                // expanded, so their click ranges shift on every chunk.
                layout.tool_result_header_ranges = rendered
                    .tool_result_header_idxs
                    .iter()
                    .map(|&idx| {
                        // In-bounds by construction (see the rebuild path).
                        let start = if idx == 0 {
                            0
                        } else {
                            visual_offsets.get(idx - 1).copied().unwrap_or(0)
                        };
                        let end = visual_offsets.get(idx).copied().unwrap_or(0);
                        (start, end)
                    })
                    .collect();
            }

            // Replace the cache entry wholesale with the freshly rendered
            // state so the next frame's lookup is a valid hit.  The key
            // records the turn's current content version, so a later rebuild
            // (which may run while this turn's chunks are still streaming)
            // recomputes instead of reusing these lines once more content
            // arrives.
            *cached = RenderedCache {
                key: RenderCacheKey {
                    turn_id,
                    width: content_width,
                    viewport_width: viewport.width,
                    reasoning_expanded,
                    tool_results_collapsed,
                    content_version,
                },
                rendered: RenderedTurn {
                    lines: Arc::from(text_lines),
                    height: text_height,
                    visual_offsets,
                    joins,
                    content_ranges,
                    chrome_ranges,
                    reasoning_header_idx: rendered.reasoning_header_idx,
                    tool_result_header_idxs: rendered.tool_result_header_idxs,
                },
            };

            let full_img_height = Self::image_block_height(viewport) as usize;
            let img_count = turn.displayed_images.len();
            let turn_height = text_height + img_count * full_img_height;

            // `turn_idx` was bounds-checked against `visible_turn_ids` above,
            // and `turn_heights` is kept in lockstep with it (one entry per
            // visible turn); the fallback height of 0 routes a drift into the
            // rebuild branch below (`old_height > turn_height` is false, then
            // `turn_height > old_height` triggers a full rebuild).
            let old_height = self.turn_heights.get(turn_idx).copied().unwrap_or(0);

            if turn_height > old_height {
                let delta = turn_height - old_height;
                if let Some(h) = self.turn_heights.get_mut(turn_idx) {
                    *h = turn_height;
                }
                for i in turn_idx..self.height_prefix.len() {
                    // `i < self.height_prefix.len()` by the loop range.
                    if let Some(prefix) = self.height_prefix.get_mut(i) {
                        *prefix = prefix.saturating_add(delta);
                    }
                }
                let at_bottom = self.effective_scroll(viewport) == 0;
                if !at_bottom {
                    self.history_scroll.scroll = self.history_scroll.scroll.saturating_add(delta);
                }
                self.rebuild_markers(viewport);
            } else if old_height > turn_height {
                return self.rebuild_height_prefix_preserving_scroll(viewport);
            }
        } else {
            return self.rebuild_height_prefix_preserving_scroll(viewport);
        }

        self.streaming_dirty = false;
        // A structural rebuild may follow (markers_dirty was already set when
        // this streaming update ran): leave content_dirty set so that
        // rebuild's preserve-scroll logic can still anchor the viewport
        // against any *structural* height change layered on top of the
        // streaming delta (e.g. a turn appended mid-stream).  When no rebuild
        // follows, the fast path is the only consumer and clears it here.
        if !self.markers_dirty {
            self.content_dirty = false;
        }
    }
}
