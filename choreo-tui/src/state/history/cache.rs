//! The per-turn render cache: what a turn's rendered lines are keyed by, and the
//! per-turn layout ranges the click handlers and the auto-pin read.
//!
//! Rendering a turn — markdown parse, wrapping, image and tool-result layout —
//! is the expensive half of a frame, so the result is cached per visible turn
//! and reused until a key field changes.  The cache lives in its own file
//! because its correctness argument is self-contained: an entry is valid iff
//! [`RenderCacheKey`] matches, and the key carries the turn's *content version*
//! (`SessionDisplayState::turn_versions`) so a mutation that left every other
//! key field intact still forces a recompute.
//!
//! [`TurnLayout`] rides along because it is produced by the same render pass, from
//! the same cumulative visual offsets the cached lines carry, and is stored in
//! lockstep with the cache entry it describes — splitting the two would mean
//! computing each twice or trusting them to stay aligned across files.
//!
//! Re-exported from `state` (`pub(crate) use history::*`), so
//! `crate::state::RenderCacheKey` and friends keep resolving for the render path
//! and the tests.

use super::super::{App, SessionDisplayState};
use crate::markdown_render::{
    LineChrome, LineJoin, RenderedTurnLines, compute_visual_offsets, lines_height,
};
use ratatui::text::Line;
use std::sync::Arc;

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
    /// Visual-row offset of the assistant response block's top row (its top
    /// padding row), relative to the turn's start.  `None` when the turn has no
    /// response text (a reasoning-only, tool-only, or error turn).  The
    /// streaming auto-pin reads this to compute the response's absolute content
    /// line and pin it at the viewport top.  Kept in sync by the streaming fast
    /// path.
    pub response_start_row: Option<usize>,
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
    /// Semantic-line index of the assistant response block's top row within
    /// `lines` (see [`RenderedTurnLines::response_start_idx`]), used by the
    /// streaming auto-pin to anchor the response's top at the viewport top.
    /// `None` for a reasoning-only, tool-only, or error turn.
    pub response_start_idx: Option<usize>,
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
        response_start_idx: rendered.response_start_idx,
    };
    if let Some(slot) = cache.get_mut(index) {
        *slot = Some(RenderedCache {
            key: key.clone(),
            rendered: turn.clone(),
        });
    }
    turn
}

// The cache and its content versions are owned by `SessionDisplayState`, so the
// three methods that read and maintain them sit in this block rather than in the
// height model's: a cache entry's validity is a property of these versions, not
// of a turn's height.
impl SessionDisplayState {
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
}

impl App {
    pub(crate) fn ensure_cache_synced(&mut self) {
        if let Some(d) = self.active_display() {
            d.ensure_cache_synced();
        }
    }
}
