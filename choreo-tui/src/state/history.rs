//! The history pane's render model: the height prefix-sum, markers, scroll
//! position, and viewport, plus the per-turn render cache they are built from.
//!
//! This module owns the machinery the history pane needs to map content to
//! screen rows in O(log n) (click hit-testing) and to keep those mappings in
//! lockstep with the per-turn rendered lines: [`HistoryViewport`] /
//! [`HistoryScrollState`] / [`ScrollRestore`], the [`TurnLayout`] /
//! [`RenderCacheKey`] / [`RenderedCache`] cache types and
//! [`cached_or_compute_lines`], the `App` and [`SessionDisplayState`] methods
//! that rebuild the height model, scroll it, and classify a viewport change
//! ([`classify_viewport_change`]), and the row→turn mapping (`find_turn_at_row`).
//! The `App` methods here are thin wrappers that forward to the active session's
//! [`SessionDisplayState`] with the shared [`HistoryViewport`].
//!
//! Moved out of `state/mod.rs` to keep that file focused on the `App` struct and
//! the display-state plumbing; every public item is re-exported from `state`
//! (`pub(crate) use history::*`) so `crate::state::X` keeps resolving.

use crate::markdown_render::{
    LineChrome, LineJoin, RenderedTurnLines, compute_visual_offsets, lines_height,
    reasoning_expanded_default, render_turn_lines, tool_result_default_collapsed,
};
use choreo_proto::ToolResultRecord;
use ratatui::layout::Rect;
use ratatui::text::Line;
use std::sync::Arc;

use super::{App, Marker, SessionDisplayState, selector_list_layout, turn_image_count};

/// What a recomputed history viewport implies for the per-session render caches
/// and the in-progress selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ViewportChange {
    /// Nothing that affects the rendered content changed.  The history viewport
    /// height can still shrink or grow (the status/error line appeared or
    /// disappeared, the help overlay toggled, the input box grew) but no line
    /// re-wraps, so the render caches and the selection must be left alone.
    None,
    /// The rendered width changed, so every line re-wraps: the width-keyed
    /// render caches are stale and the selection's stored column anchors no
    /// longer point at the same text.
    Rewrap,
    /// Only the height changed — a real vertical resize, or the chrome (the
    /// status/error line, the help overlay, the input box) growing or shrinking.
    /// Nothing re-wraps, so the caches and the selection stay valid, but the
    /// heights must be recomputed because the image-block height the height
    /// prefix reserves derives from the viewport height.  That recompute is a
    /// cache-hit rebuild, so it is cheap enough to run for every chrome change.
    HeightsOnly,
}

/// Classify a history-viewport change into the invalidation it warrants.
///
/// Pure (no terminal access) so the policy — and its full case matrix — is
/// unit-testable.  Only the rendered WIDTH decides a re-wrap: nothing about
/// wrapping reads the height.  Any height change recomputes the height model,
/// because the image-block height the prefix reserves derives from the viewport
/// height — and that recompute is a cache-hit rebuild (no re-render), so it is
/// cheap enough to run even when a status line merely appears or disappears.
/// See [`App::update_viewport_from_terminal_size`] for why wiping the render
/// cache on a chrome height change was the bug.
pub(crate) fn classify_viewport_change(
    old_width: u16,
    new_width: u16,
    old_height: u16,
    new_height: u16,
) -> ViewportChange {
    if old_width != new_width {
        ViewportChange::Rewrap
    } else if old_height != new_height {
        ViewportChange::HeightsOnly
    } else {
        ViewportChange::None
    }
}

/// Per-turn content-line ranges used for click hit-testing, computed
/// alongside `height_prefix`.  Maps a content-line offset within the turn to
/// the reasoning header or the correct image index — no text-height
/// recomputation needed in the click handler.
#[derive(Debug)]
pub(crate) struct TurnLayout {
    /// (start, end) content-line range of the reasoning header row(s),
    /// relative to the turn's start.  None when the turn has no reasoning.
    pub reasoning_header_range: Option<(usize, usize)>,
    /// Whether this turn's reasoning section is expanded by default, derived
    /// from turn content at layout time (an explicit header-click override in
    /// `reasoning_override` takes precedence at render time).  Stored here so
    /// the per-frame render path can compute the effective state in O(1)
    /// without re-scanning turn strings.
    pub reasoning_default_expanded: bool,
    /// (start, end) content-line ranges for each displayed image,
    /// relative to the turn's start.  Empty when the turn has no images.
    pub image_ranges: Vec<(usize, usize)>,
    /// (start, end) content-line ranges for each tool result's collapsible
    /// header row, relative to the turn's start, aligned with
    /// `turn.tool_results`.  Empty when the turn has no tool results (or
    /// short-circuits on the error block).  Populated in lockstep with
    /// `height_prefix` and kept in sync by the streaming fast path.
    pub tool_result_header_ranges: Vec<(usize, usize)>,
}

/// Everything that identifies a render-cache entry.  Two keys are equal iff
/// the cached lines can be reused: same turn, same widths, the same
/// effective reasoning/tool-result collapse state, and — critically — the
/// same *content version* of the turn (see
/// [`SessionDisplayState::turn_versions`]).
///
/// The content version is what makes the key content-correct: streaming
/// growth (tool-result chunks, answer chunks), turn replacement
/// (`TurnAppended`), and snapshot merges (`SessionState`) all bump it, so a
/// full rebuild can never serve a stale cached rendering of a turn whose
/// text changed behind the key's other fields.  Without it, a rebuild
/// between a chunk and its fast-path refresh would reuse the pre-chunk
/// lines — the visible results froze while the scrollbar total kept
/// reflecting fresh content, and everything snapped back only when the
/// final `TurnAppended` invalidated the slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RenderCacheKey {
    /// Turn ID this entry belongs to, used to detect stale entries after
    /// turns are removed/reordered.
    pub turn_id: u32,
    /// Content width the lines were wrapped at.
    pub width: u16,
    /// Full viewport width when this entry was computed.  Stored alongside
    /// `width` (the content width) so the cache key guards against skew in
    /// `lines_height` and `compute_visual_offsets` computations, which
    /// depend on viewport width.
    pub viewport_width: u16,
    /// Reasoning visibility the cached lines were rendered with.
    pub reasoning_expanded: bool,
    /// Collapse state of each tool result (aligned with `turn.tool_results`)
    /// the cached lines were rendered with.
    pub tool_results_collapsed: Vec<bool>,
    /// Monotonic per-turn content version the lines were rendered with.
    /// Bumped by every event handler that mutates a turn's rendered content;
    /// a mismatch forces a recompute even when every other field matches.
    pub content_version: u64,
}

/// Cached render output for a turn: the lines plus the precomputed height,
/// cumulative visual offsets, and section-header semantic indexes.  Returned
/// from [`cached_or_compute_lines`] so callers can render and hit-test
/// without re-walking the lines.
#[derive(Debug, Clone)]
pub(crate) struct RenderedTurn {
    pub lines: Arc<[Line<'static>]>,
    pub height: usize,
    /// Cumulative visual-row offset for each semantic line.
    /// `visual_offsets[i]` = total visual rows covered by lines[0..=i].
    /// Used with `partition_point` to map a visual row → semantic line index
    /// in O(log n).
    pub visual_offsets: Arc<[usize]>,
    /// Per-line [`LineJoin`] copy metadata aligned with `lines`: how each
    /// rendered row glues to the row before it when a selection is copied
    /// (see the enum docs in `markdown_render`).  The selection extraction
    /// uses this to rejoin wrapped continuations into the original text
    /// instead of reproducing the renderer's line breaks.
    pub joins: Arc<[LineJoin]>,
    /// Display-column range `(start, end)` of each line's meaningful content,
    /// aligned with `lines` — see [`RenderedTurnLines::content_ranges`].
    /// Mouse selection clamps its highlight and its copy to these ranges so
    /// a drag never captures UI chrome (the `┃` gutter, indents, fill).
    pub content_ranges: Arc<[Option<(usize, usize)>]>,
    /// Per-line [`LineChrome`] copy metadata aligned with `lines` — see
    /// [`RenderedTurnLines::chrome_ranges`].  The selection/copy machinery
    /// subtracts these non-selectable-chrome intervals from each row's
    /// `content_ranges` interval so a drag over a block quote never copies the
    /// `│ ` bar.  Empty for the overwhelming majority of rows.
    pub chrome_ranges: Arc<[LineChrome]>,
    /// Semantic-line index of the reasoning header within `lines` (see
    /// [`RenderedTurnLines`]), so click hit-testing never re-scans the
    /// rendered output.
    pub reasoning_header_idx: Option<usize>,
    /// Semantic-line index of each tool result header within `lines` (see
    /// [`RenderedTurnLines`]), so click hit-testing never re-scans the
    /// rendered output.
    pub tool_result_header_idxs: Vec<usize>,
}

/// One slot of the render cache: the key the entry was rendered with plus the
/// rendered output.  The key is compared on lookup so a stale entry (state
/// changed without invalidation) is treated as a miss instead of being served.
#[derive(Debug, Clone)]
pub(crate) struct RenderedCache {
    pub key: RenderCacheKey,
    pub rendered: RenderedTurn,
}

/// Check `render_cache[index]` for a valid entry matching `key`.  On hit,
/// return the cached [`RenderedTurn`].  On miss, call `compute`, store the
/// result in `render_cache[index]`, and return it.
///
/// When `index` is out of bounds (in-band or because the cache is shorter than
/// expected), the result is still returned but not cached.
pub(crate) fn cached_or_compute_lines(
    cache: &mut [Option<RenderedCache>],
    index: usize,
    key: &RenderCacheKey,
    compute: impl FnOnce() -> RenderedTurnLines,
) -> RenderedTurn {
    if let Some(Some(cached)) = cache.get(index)
        && cached.key == *key
    {
        return cached.rendered.clone();
    }

    let rendered = compute();
    let lines: Arc<[Line<'static>]> = Arc::from(rendered.lines);
    let joins: Arc<[LineJoin]> = Arc::from(rendered.joins);
    let content_ranges: Arc<[Option<(usize, usize)>]> = Arc::from(rendered.content_ranges);
    let chrome_ranges: Arc<[LineChrome]> = Arc::from(rendered.chrome_ranges);
    let visual_offsets = compute_visual_offsets(&lines, key.viewport_width);
    // Pin the parallel-array invariant the selection machinery relies on:
    // every rendered line must carry a content range (`None` marks
    // pure-chrome rows), a cumulative visual-row offset, and a copy-join
    // record.  A mismatch here is a programming error in the renderer, not
    // a runtime condition.
    debug_assert_eq!(
        content_ranges.len(),
        lines.len(),
        "every rendered line must carry a content range"
    );
    debug_assert_eq!(
        visual_offsets.len(),
        lines.len(),
        "visual offsets must stay aligned with the rendered lines"
    );
    debug_assert_eq!(joins.len(), lines.len(), "joins must align with the lines");
    debug_assert_eq!(
        chrome_ranges.len(),
        lines.len(),
        "chrome ranges must align with the lines"
    );
    let turn = RenderedTurn {
        height: lines_height(&lines, key.viewport_width).max(1),
        visual_offsets,
        lines,
        joins,
        content_ranges,
        chrome_ranges,
        reasoning_header_idx: rendered.reasoning_header_idx,
        tool_result_header_idxs: rendered.tool_result_header_idxs,
    };
    if let Some(slot) = cache.get_mut(index) {
        *slot = Some(RenderedCache {
            key: key.clone(),
            rendered: turn.clone(),
        });
    }
    turn
}

#[derive(Clone, Copy)]
pub(crate) struct HistoryViewport {
    pub(crate) width: u16,
    pub(crate) height: u16,
}

#[derive(Clone, Copy)]
pub(crate) struct HistoryScrollState {
    pub(crate) scroll: usize,
    pub(crate) scroll_compensation: usize,
}

/// The reading position captured when a session is left, so the next visit can
/// restore the same *content* even though the raw scroll offset (a distance
/// from the bottom) is only meaningful for the viewport height it was taken at:
/// the help/status bands reflow the history viewport on attach, and background
/// sessions keep streaming, so both the viewport height and the total content
/// height can differ by the time the user returns.
#[derive(Clone, Copy)]
pub(crate) struct ScrollRestore {
    /// Absolute content line (from the top of the history) drawn at the top of
    /// the viewport when the session was left.  Content appended below it — or
    /// a taller/shorter viewport — does not shift it, so restoring to this line
    /// re-shows exactly what the user was looking at.
    pub(crate) top_line: usize,
    /// Whether the viewport was pinned to the bottom (scroll 0) when the session
    /// was left.  A bottom-pinned session follows new content on return instead
    /// of staying anchored to what it was showing, matching the in-session
    /// behavior when content arrives while the user sits at the bottom.
    pub(crate) at_bottom: bool,
}

impl HistoryViewport {
    pub(crate) fn new() -> Self {
        Self {
            width: 80,
            height: 24,
        }
    }

    pub(crate) fn update(&mut self, area: Rect) {
        self.width = area.width.max(1);
        self.height = area.height;
    }
}

impl HistoryScrollState {
    pub(crate) fn new() -> Self {
        Self {
            scroll: 0,
            scroll_compensation: 0,
        }
    }

    fn unclamped_effective_scroll(&self) -> usize {
        self.scroll.saturating_add(self.scroll_compensation)
    }

    pub(crate) fn clamp(&mut self, max_scroll: usize) {
        let effective = self.unclamped_effective_scroll();
        if effective <= max_scroll {
            return;
        }
        let overflow = effective - max_scroll;
        let compensation_reduction = self.scroll_compensation.min(overflow);
        self.scroll_compensation -= compensation_reduction;
        let remaining = overflow - compensation_reduction;
        self.scroll = self.scroll.saturating_sub(remaining);
    }

    pub(crate) fn effective_scroll(&self, max_scroll: usize) -> usize {
        self.unclamped_effective_scroll().min(max_scroll)
    }

    pub(crate) fn scroll_up(&mut self, amount: usize, max_scroll: usize) {
        self.scroll = self.scroll.saturating_add(amount);
        self.clamp(max_scroll);
    }

    pub(crate) fn scroll_down(&mut self, amount: usize, max_scroll: usize) {
        let compensation_reduction = self.scroll_compensation.min(amount);
        self.scroll_compensation -= compensation_reduction;
        let remaining = amount.saturating_sub(compensation_reduction);
        self.scroll = self.scroll.saturating_sub(remaining);
        self.clamp(max_scroll);
    }
}

/// Integer ceiling division: `ceil(a / b)`.
/// Returns 0 when `b == 0`.
fn ceil_div(a: usize, b: usize) -> usize {
    if b == 0 {
        return 0;
    }
    a.saturating_add(b).saturating_sub(1) / b
}

impl App {
    pub(crate) fn update_viewport_from_terminal_size(&mut self) {
        let size = if self.terminal_resized || self.last_terminal_size.is_none() {
            if let Ok(size) = crossterm::terminal::size() {
                self.last_terminal_size = Some(size);
                self.terminal_resized = false;
                size
            } else {
                return;
            }
        } else {
            match self.last_terminal_size {
                Some(s) => s,
                None => return,
            }
        };
        let (width, height) = size;
        // The history viewport must match what render_chat actually draws:
        // chunk 0 of the shared Chat-page layout, minus the reserved scrollbar
        // column.  Deriving it from the solver output (rather than
        // `height - bottom_height`) keeps the viewport faithful even on
        // terminals too small for the fixed chrome to fit, where the solver
        // shrinks chunks — so the history-box mouse arm can never swallow
        // clicks that the renderer drew as part of the input box.
        let [history_area, _, _, _, _] = self.chat_page_layout(width, height);
        let old_width = self.history_viewport.width;
        let old_height = self.history_viewport.height;
        let new_width = history_area.width.saturating_sub(1);
        let new_height = history_area.height;
        self.history_viewport.update(Rect {
            x: 0,
            y: 0,
            width: new_width,
            height: new_height,
        });
        // Classify the viewport change and invalidate only what it warrants.  A
        // width change re-wraps every line, so the render caches and the
        // selection's column anchors go.  A height change — a real vertical
        // resize, OR the chrome (the status/error line, the help overlay, the
        // input box) growing or shrinking — re-wraps nothing, so the caches and
        // the selection survive; only the HEIGHTS are recomputed, because the
        // image-block height the prefix reserves derives from the viewport
        // height.  The old code cleared every session's render cache on any
        // height change, which forced a full O(session) re-render the instant
        // any status line was set or cleared (the input delay a large session
        // showed right after a copy and on the next keystroke); the height
        // recompute is now a cache-hit rebuild, so it is cheap enough to run for
        // every chrome change.
        match classify_viewport_change(old_width, new_width, old_height, new_height) {
            ViewportChange::Rewrap => {
                // The width changed, so every line re-wraps: the width-keyed
                // render caches are stale and the selection's stored column
                // anchors no longer point at the same text (the anchor is
                // deliberately never re-resolved — only the head follows the
                // cursor).  Drop the gesture like suspend/page-switch do, and
                // clear every session's cache so the next frame re-renders at
                // the new width.
                tracing::debug!(
                    old_width,
                    new_width,
                    "history width changed; invalidating render caches"
                );
                self.text_selection = None;
                for display in self.session_displays.values_mut() {
                    display.render_cache.fill(None);
                    display.markers_dirty = true;
                    display.content_dirty = false;
                }
            }
            ViewportChange::HeightsOnly => {
                // A real vertical resize: nothing re-wraps, so the caches and
                // the selection stay valid, but the image-block height the
                // prefix reserves derives from the viewport height, so recompute
                // the heights (a cache-hit rebuild — no re-render).
                for display in self.session_displays.values_mut() {
                    display.markers_dirty = true;
                }
            }
            ViewportChange::None => {}
        }
        // Session-manager list rows: full height minus the status bar (1),
        // the bordered list block (2), and the table header (1).  Must stay
        // in sync with render_session_list_view's layout; navigation uses
        // this cached height to decide when to shift the list window.
        // Computed here (outside the draw closure) because the renderer
        // never mutates app state.
        self.session_mgr.viewport_height = height.saturating_sub(4) as usize;
        // The picker popups (the wizard's provider picker and the model
        // selector) both render their lists in the LIST-popup body; cache
        // that body height so arrow/wheel navigation can pin the highlight at
        // the middle row and scroll the list under it.  Same layout math as
        // the renderers and the mouse hit-testers (`selector_list_layout`),
        // so the cache can never drift from what is drawn.  Only computed
        // while a picker is actually open: the value is consumed solely by
        // open-picker navigation/click handling, and building the layout (a
        // `Block` + `Layout::split`) every frame when no picker is up is
        // pure waste.  Both stay 0 until the first frame (viewport unknown),
        // mirroring `session_mgr.viewport_height` — navigation falls back to
        // focus-only moves then and `picker_window` clamps at render time.
        if self.model_selector.is_open() || self.ai_providers.wizard.is_open() {
            let selector_layout = selector_list_layout(Rect {
                x: 0,
                y: 0,
                width,
                height,
            });
            self.ai_providers.wizard.viewport_height = selector_layout.body.height as usize;
            self.model_selector.viewport_height = selector_layout.body.height as usize;
        }
    }

    pub(crate) fn mark_terminal_resized(&mut self) {
        self.terminal_resized = true;
    }

    pub(crate) fn total_history_height(&self) -> usize {
        self.active_display_ref()
            // Method reference: plain forwarding of the display's own method.
            .map_or(0, SessionDisplayState::total_history_height)
    }

    /// Whether the vertical scrollbar is currently rendered.  Must stay in
    /// lockstep with the click handling so a hidden scrollbar never swallows
    /// clicks in its (still-reserved) column on sessions whose history fits
    /// the viewport.
    pub(crate) fn scrollbar_visible(&self) -> bool {
        self.total_history_height() > self.history_viewport.height as usize
    }

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

    #[cfg(test)]
    pub(crate) fn mark_streaming_changed(&mut self) {
        if let Some(d) = self.active_display() {
            d.mark_streaming_changed();
        }
    }

    pub(crate) fn max_scroll_offset(&self) -> usize {
        self.active_display_ref()
            .map_or(0, |d| d.max_scroll_offset(&self.history_viewport))
    }

    pub(crate) fn clamp_scroll_state(&mut self) {
        let vp = self.history_viewport;
        if let Some(d) = self.active_display() {
            d.clamp_scroll_state(&vp);
        }
    }

    pub(crate) fn image_block_height(&self) -> u16 {
        // Pure function of the viewport; no display state needed.
        SessionDisplayState::image_block_height(&self.history_viewport)
    }

    pub(crate) fn ensure_cache_synced(&mut self) {
        if let Some(d) = self.active_display() {
            d.ensure_cache_synced();
        }
    }
    pub(crate) fn effective_scroll(&self) -> usize {
        self.active_display_ref()
            .map_or(0, |d| d.effective_scroll(&self.history_viewport))
    }

    #[cfg(test)]
    pub(crate) fn scrollbar_notch(&self) -> usize {
        self.active_display_ref()
            .map_or(1, |d| d.scrollbar_notch(&self.history_viewport))
    }

    pub(crate) fn scroll_up(&mut self, amount: usize) {
        let vp = self.history_viewport;
        if let Some(d) = self.active_display() {
            d.scroll_up(amount, &vp);
        }
    }

    pub(crate) fn scroll_down(&mut self, amount: usize) {
        let vp = self.history_viewport;
        if let Some(d) = self.active_display() {
            d.scroll_down(amount, &vp);
        }
    }

    pub(crate) fn scroll_to(&mut self, row: usize) {
        let vp = self.history_viewport;
        if let Some(d) = self.active_display() {
            d.scroll_to(row, &vp);
        }
    }

    pub(crate) fn scroll_to_track_row(&mut self, mouse_row: u16, track_height: u16) {
        let vp = self.history_viewport;
        if let Some(d) = self.active_display() {
            d.scroll_to_track_row(mouse_row, track_height, &vp);
        }
    }

    pub(crate) fn scroll_to_content_line(&mut self, content_line: usize) {
        let vp = self.history_viewport;
        if let Some(d) = self.active_display() {
            d.scroll_to_content_line(content_line, &vp);
        }
    }

    pub(crate) fn scrollbar_scroll_up(&mut self) {
        let vp = self.history_viewport;
        if let Some(d) = self.active_display() {
            d.scrollbar_scroll_up(&vp);
        }
    }

    pub(crate) fn scrollbar_scroll_down(&mut self) {
        let vp = self.history_viewport;
        if let Some(d) = self.active_display() {
            d.scrollbar_scroll_down(&vp);
        }
    }

    // The branches below guarantee a non-negative delta before each cast.
    #[expect(clippy::cast_sign_loss)]
    pub(crate) fn apply_scroll_delta(&mut self) {
        let delta = self.scroll_accumulator;
        self.scroll_accumulator = 0;
        if delta > 0 {
            self.scroll_up(delta as usize);
        } else if delta < 0 {
            self.scroll_down((-delta) as usize);
        }
    }
}

// ── SessionDisplayState methods ─────────────────────────────────────

// `HistoryViewport` is a 4-byte Copy struct; every call site already holds a
// reference (App-level wrappers, tests), so taking it by value would churn
// signatures across the crate for no measurable gain.
#[expect(clippy::trivially_copy_pass_by_ref)]
impl SessionDisplayState {
    pub(crate) fn total_history_height(&self) -> usize {
        self.height_prefix.last().copied().unwrap_or(0)
    }

    /// Current content version for `turn_id` (0 when no mutation has ever
    /// been recorded for it).  Part of the render-cache key so a rebuild
    /// recomputes a turn whose content changed since it was cached.
    pub(crate) fn turn_content_version(&self, turn_id: u32) -> u64 {
        self.turn_versions.get(&turn_id).copied().unwrap_or(0)
    }

    /// Bump the content version for `turn_id` and return the new value.
    ///
    /// Must be called by every event handler that mutates a turn's rendered
    /// content — after `stream_chunk`/`tool_result_chunk`/`tool_call_started`
    /// on the streaming turn, and after `insert_or_replace`/snapshot merges.
    /// Wrapping is deliberate: a version collision after 2^64 mutations is
    /// astronomically improbable and only risks one stale cache hit.
    pub(crate) fn bump_turn_version(&mut self, turn_id: u32) -> u64 {
        // `entry().or_insert(0)` yields `&mut u64`; increment through the
        // reference (auto-deref for the RHS, write-back on the LHS) so the
        // new version is persisted in the map, then return it.
        let version = self.turn_versions.entry(turn_id).or_insert(0);
        *version = version.wrapping_add(1);
        *version
    }

    /// Rebuild `height_prefix`, `markers`, `visible_turn_ids`, and populate
    /// `render_cache`.
    pub(crate) fn rebuild_height_prefix(&mut self, viewport: &HistoryViewport) {
        self.height_prefix.clear();
        self.visible_turn_ids.clear();
        self.markers.clear();
        self.turn_layouts.clear();
        self.turn_heights.clear();
        let mut total = 0usize;
        let virtual_track = Self::virtual_track_slots(viewport);
        let fallback_img_height = Self::image_block_height(viewport) as usize;
        let turn_count = self.view.turns.len();
        tracing::trace!(turn_count, "rebuild_height_prefix");

        let visible_count = self.view.turns.iter().filter(|(_, t)| !t.undone).count();
        self.render_cache.resize(visible_count, None);

        let mut user_text_start_lines: Vec<usize> = Vec::with_capacity(turn_count);
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
            let reasoning_header_range = reasoning_header_idx.map(|idx| {
                // `idx` indexes a semantic line of the same render that
                // produced `text_offsets`, so `idx < offsets.len()` and the
                // lookups are in bounds; fall back to a zero-width range.
                let start = if idx == 0 {
                    0
                } else {
                    text_offsets.get(idx - 1).copied().unwrap_or(0)
                };
                let end = text_offsets.get(idx).copied().unwrap_or(0);
                (start, end)
            });

            // Same conversion for every tool result header, so clicking a
            // triangle toggles exactly that result.  One range per result,
            // aligned with `turn.tool_results`.
            let tool_result_header_ranges = tool_result_header_idxs
                .iter()
                .map(|&idx| {
                    // In-bounds by construction, as above.
                    let start = if idx == 0 {
                        0
                    } else {
                        text_offsets.get(idx - 1).copied().unwrap_or(0)
                    };
                    let end = text_offsets.get(idx).copied().unwrap_or(0);
                    (start, end)
                })
                .collect();

            let mut image_ranges: Vec<(usize, usize)> = Vec::new();
            let mut total_img_height: usize = 0;
            // Every image slot the turn exposes (displayed + tool-result vision)
            // reserves one block, in the SAME order the render loop draws them
            // (the height path uses `turn_image_count`, the render loop
            // `turn_image_slots`, and both walk the same source order), so a
            // click's `image_ranges` index maps onto that slot list exactly.
            for _ in 0..turn_image_count(turn) {
                let start = text_height + total_img_height;
                image_ranges.push((start, start + fallback_img_height));
                total_img_height += fallback_img_height;
            }
            self.turn_layouts.push(TurnLayout {
                reasoning_header_range,
                reasoning_default_expanded,
                image_ranges,
                tool_result_header_ranges,
            });
            let turn_height = text_height + total_img_height;
            self.turn_heights.push(turn_height);
            if turn.user_text.is_some() {
                user_text_start_lines.push(total);
            }
            total += turn_height;
            self.height_prefix.push(total);
            self.visible_turn_ids.push(turn_id);
            visible_idx += 1;
        }
        let final_total = total.max(1);
        tracing::trace!(
            marker_count = user_text_start_lines.len(),
            final_total,
            "computed markers"
        );
        self.markers.reserve(user_text_start_lines.len());
        for &start_line in &user_text_start_lines {
            let slot = start_line * virtual_track / final_total;
            self.markers.push(Marker {
                content_line: start_line,
                virtual_slot: slot,
            });
        }
        self.markers_dirty = false;
    }

    pub(crate) fn mark_streaming_changed(&mut self) {
        self.streaming_dirty = true;
        self.content_dirty = true;
    }

    pub(crate) fn mark_content_changed(&mut self) {
        self.markers_dirty = true;
        self.content_dirty = true;
        self.streaming_turn_index = None;
        self.streaming_dirty = false;
    }

    /// Effective reasoning visibility for a turn: an explicit override from
    /// clicking the header wins; otherwise the caller-provided derived
    /// default is used.  Callers compute the default either from the turn
    /// content (`reasoning_expanded_default`) or from the precomputed
    /// `TurnLayout` when one is available (per-frame render path).
    pub(crate) fn effective_reasoning_expanded(&self, turn_id: u32, default: bool) -> bool {
        self.reasoning_override
            .get(&turn_id)
            .copied()
            .unwrap_or(default)
    }

    /// Toggle the reasoning section's visibility for a turn (clicking the
    /// header).  Records the explicit user preference in `reasoning_override`
    /// and invalidates the turn's render cache so the change takes effect on
    /// the next frame.
    pub(crate) fn toggle_reasoning(&mut self, turn_id: u32) {
        let Some(turn) = self.view.turns.get(&turn_id) else {
            return;
        };
        let current = self.effective_reasoning_expanded(turn_id, reasoning_expanded_default(turn));
        self.reasoning_override.insert(turn_id, !current);
        if let Some(idx) = self.visible_turn_ids.iter().position(|id| *id == turn_id)
            && let Some(slot) = self.render_cache.get_mut(idx)
        {
            *slot = None;
        }
        self.mark_content_changed();
    }

    /// Effective collapse state for a tool result: an explicit override from
    /// clicking the header wins; otherwise the derived default (quiet tools
    /// collapsed, everything else expanded — see
    /// [`tool_result_default_collapsed`]) is used.
    pub(crate) fn effective_tool_result_collapsed(
        &self,
        turn_id: u32,
        record: &ToolResultRecord,
    ) -> bool {
        // Nested (turn → call_id → state) lookup borrows the record's
        // call_id — this runs per result per frame, so avoiding a clone
        // here keeps the render path allocation-free for the common case.
        self.tool_collapse_override
            .get(&turn_id)
            .and_then(|by_call| by_call.get(&record.call_id))
            .copied()
            .unwrap_or_else(|| tool_result_default_collapsed(record))
    }

    /// Toggle a tool result's collapse state (clicking its header row).
    /// Records the explicit user preference in `tool_collapse_override` and
    /// invalidates the turn's render cache so the change takes effect on
    /// the next frame.
    pub(crate) fn toggle_tool_result(&mut self, turn_id: u32, call_id: &str) {
        let Some(turn) = self.view.turns.get(&turn_id) else {
            return;
        };
        let Some(record) = turn.tool_results.iter().find(|r| r.call_id == call_id) else {
            return;
        };
        let current = self.effective_tool_result_collapsed(turn_id, record);
        self.tool_collapse_override
            .entry(turn_id)
            .or_default()
            .insert(call_id.to_string(), !current);
        if let Some(idx) = self.visible_turn_ids.iter().position(|id| *id == turn_id)
            && let Some(slot) = self.render_cache.get_mut(idx)
        {
            *slot = None;
        }
        self.mark_content_changed();
    }

    pub(crate) fn resolve_streaming_turn_index(&mut self, stream_id: u64) {
        if self.streaming_turn_index.is_none()
            && let Some(&turn_id) = self.view.request_to_turn.get(&stream_id)
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
                let slot = accum * virtual_track / total;
                self.markers.push(Marker {
                    content_line: accum,
                    virtual_slot: slot,
                });
            }
            accum += turn_height;
        }
    }

    pub(crate) fn max_scroll_offset(&self, viewport: &HistoryViewport) -> usize {
        let viewport_height = viewport.height as usize;
        let total_height = self.total_history_height();
        total_height.saturating_sub(viewport_height)
    }

    // Pure function of the viewport — no display state involved.
    pub(crate) fn virtual_track_slots(viewport: &HistoryViewport) -> usize {
        2 * viewport.height as usize
    }

    pub(crate) fn clamp_scroll_state(&mut self, viewport: &HistoryViewport) {
        // Never clamp against a height model that is about to be rebuilt.
        // `markers_dirty` means the per-turn heights (and thus
        // `max_scroll_offset`) no longer reflect the content — a session
        // switch just cleared the height cache, or a resize re-wrapped every
        // line.  `max_scroll_offset` would read 0 there, so clamping would
        // collapse a legitimately preserved scroll offset to the bottom
        // *before* the rebuild in `compute_total_height_and_markers` runs
        // (the pre-render clamp in the UI loop runs ahead of the draw-time
        // rebuild).  Rendering clamps `effective_scroll` against the freshly
        // rebuilt height for the one frame this skips, and the next frame's
        // clamp settles any genuine overflow, so skipping here is safe.
        if self.markers_dirty {
            return;
        }
        self.history_scroll.clamp(self.max_scroll_offset(viewport));
    }

    /// Capture the current reading position so the next visit can restore the
    /// same content regardless of the viewport height then (see
    /// [`ScrollRestore`]).  Called on the outgoing session just before the
    /// active session is rebound.
    pub(crate) fn capture_scroll_restore(&mut self, viewport: &HistoryViewport) {
        let total = self.total_history_height();
        let eff = self.effective_scroll(viewport);
        let vh = viewport.height as usize;
        self.scroll_restore = Some(ScrollRestore {
            // The top content line of the bottom-anchored window; saturates to
            // 0 for content shorter than the viewport (where `eff` is 0 too).
            top_line: total.saturating_sub(eff.saturating_add(vh)),
            at_bottom: eff == 0,
        });
    }

    /// Re-establish a captured reading position against the freshly rebuilt
    /// height model.  Consumes the anchor, so it applies exactly once per
    /// restore (the first rebuild after the session switch).
    fn apply_scroll_restore(&mut self, viewport: &HistoryViewport) {
        let Some(restore) = self.scroll_restore.take() else {
            return;
        };
        if restore.at_bottom {
            // Was pinned to the bottom: follow any content that arrived while
            // the session was in the background, exactly as an in-session
            // content change does when the user is at the bottom.
            self.history_scroll.scroll = 0;
            self.history_scroll.scroll_compensation = 0;
            return;
        }
        let vh = viewport.height as usize;
        let total = self.total_history_height();
        // Convert the absolute top line back to a from-bottom offset against the
        // (possibly changed) total height, so the same content sits at the top
        // of the viewport again.  Clamp to the valid range; a top line past the
        // end (content shrank while away) falls back to the bottom.
        let scroll = total.saturating_sub(restore.top_line.saturating_add(vh));
        self.history_scroll.scroll = scroll.min(self.max_scroll_offset(viewport));
        self.history_scroll.scroll_compensation = 0;
    }

    pub(crate) fn effective_scroll(&self, viewport: &HistoryViewport) -> usize {
        self.history_scroll
            .effective_scroll(self.max_scroll_offset(viewport))
    }

    // Pure function of the viewport — no display state involved.
    pub(crate) fn image_block_height(viewport: &HistoryViewport) -> u16 {
        (viewport.height / 2).max(1)
    }

    pub(crate) fn ensure_cache_synced(&mut self) {
        let turns_len = self.visible_turn_ids.len();
        let cache_len = self.render_cache.len();
        if cache_len == turns_len {
            return;
        }
        if cache_len > turns_len {
            self.render_cache.truncate(turns_len);
            return;
        }
        self.render_cache.resize(turns_len, None);
    }

    pub(crate) fn scrollbar_notch(&self, viewport: &HistoryViewport) -> usize {
        let max_scroll = self.max_scroll_offset(viewport);
        let virtual_track = Self::virtual_track_slots(viewport);
        if virtual_track > 0 {
            ceil_div(max_scroll, virtual_track)
        } else {
            max_scroll
        }
        .max(1)
    }

    pub(crate) fn scroll_up(&mut self, amount: usize, viewport: &HistoryViewport) {
        self.history_scroll
            .scroll_up(amount, self.max_scroll_offset(viewport));
    }

    pub(crate) fn scroll_down(&mut self, amount: usize, viewport: &HistoryViewport) {
        self.history_scroll
            .scroll_down(amount, self.max_scroll_offset(viewport));
    }

    pub(crate) fn scroll_to(&mut self, row: usize, viewport: &HistoryViewport) {
        let max_scroll = self.max_scroll_offset(viewport);
        let amount = row.min(max_scroll);
        self.history_scroll.scroll = amount;
        self.history_scroll.scroll_compensation = 0;
    }

    pub(crate) fn scroll_to_track_row(
        &mut self,
        mouse_row: u16,
        track_height: u16,
        viewport: &HistoryViewport,
    ) {
        let track_height = track_height as usize;
        if track_height > 1 {
            let row = (mouse_row as usize).min(track_height.saturating_sub(1));
            let max_scroll = self.max_scroll_offset(viewport);
            let denom = track_height.saturating_sub(1);
            let target = row.saturating_mul(max_scroll).saturating_add(denom / 2) / denom;
            self.scroll_to(max_scroll.saturating_sub(target.min(max_scroll)), viewport);
        }
    }

    pub(crate) fn scroll_to_content_line(
        &mut self,
        content_line: usize,
        viewport: &HistoryViewport,
    ) {
        let total = self.total_history_height();
        let vh = viewport.height as usize;
        let target = total.saturating_sub(content_line + vh);
        self.scroll_to(target.min(self.max_scroll_offset(viewport)), viewport);
    }

    pub(crate) fn scrollbar_scroll_up(&mut self, viewport: &HistoryViewport) {
        let notch = self.scrollbar_notch(viewport);
        self.scroll_up(notch, viewport);
    }

    pub(crate) fn scrollbar_scroll_down(&mut self, viewport: &HistoryViewport) {
        let notch = self.scrollbar_notch(viewport);
        self.scroll_down(notch, viewport);
    }
}

#[cfg(test)]
pub(crate) fn history_text_height(text: &str, width: u16) -> usize {
    lines_height(
        &crate::markdown_render::plain_text_lines(text, width),
        width,
    )
}

/// Find the visible turn index and the content-line offset within that
/// turn for a given screen row.  Binary search on `height_prefix`.
/// Returns `(turn_idx, offset_within_turn)`.
pub(crate) fn find_turn_at_row(app: &App, screen_row: u16) -> Option<(usize, usize)> {
    let display = app.active_display_ref()?;
    let vh = app.history_viewport.height;
    if screen_row >= vh {
        return None;
    }

    let effective_scroll = display.effective_scroll(&app.history_viewport);
    let total_height = display.total_history_height();

    // Map the screen row to a content line, mirroring `render_history`'s
    // bottom-up draw order: the viewport shows the bottom `vh` rows of the
    // unscrolled content window, i.e. content lines `[total - scroll - vh,
    // total - scroll)`, so screen row `r` maps to content line
    // `r + total - scroll - vh`.  The same formula covers both layouts:
    //  - Tall history (scrollbar present): `scroll + vh <= total`, so the
    //    result is always `>= 0` and every viewport row shows content.
    //  - Short history (no scrollbar, `scroll == 0`): `total < vh` leaves a
    //    blank band of `vh - total` rows at the top — rows whose computed
    //    content line is negative.  Those rows must map to no turn rather
    //    than being clamped into the content (which is what both a naive
    //    `saturating_sub` and the pre-fix code did, breaking header/image
    //    click hit-testing on short sessions).
    let scrolled = effective_scroll.saturating_add(vh as usize);
    let content_line = (screen_row as usize).saturating_add(total_height);
    if content_line < scrolled {
        // Click landed in the blank band above the content.
        return None;
    }
    let content_line = content_line - scrolled;

    if content_line >= total_height {
        return None;
    }

    let i = display
        .height_prefix
        .partition_point(|&p| p <= content_line);
    if i < display.height_prefix.len() {
        let turn_start = i
            .checked_sub(1)
            .and_then(|prev| display.height_prefix.get(prev))
            .copied()
            .unwrap_or(0);
        let offset = content_line.saturating_sub(turn_start);
        Some((i, offset))
    } else {
        None
    }
}
