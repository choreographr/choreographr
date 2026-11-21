//! The history pane's pure geometry: the viewport, the from-bottom scroll
//! offset, the captured reading position, and the semantic-line→visual-row
//! conversions.
//!
//! Everything here is a value type plus its arithmetic — no `App`, no
//! `SessionDisplayState`, no display state.  It has its own file because the
//! height model, the render cache and the scroll plumbing all take these types
//! as arguments: the vocabulary they share is easier to keep honest in one
//! small place than beside any one of its consumers.
//!
//! [`semantic_line_row_start`] / [`semantic_line_row_range`] belong here for the
//! same reason: they are the geometry of a render's cumulative visual offsets,
//! and the height model (click hit-ranges), the streaming fast path
//! (`state/streaming.rs`) and the auto-pin anchor all convert a
//! renderer-reported semantic line index through them, so one copy keeps the
//! conversions from drifting apart.
//!
//! Re-exported from `state` (`pub(crate) use history::*`), so
//! `crate::state::HistoryViewport` and the `semantic_line_row_*` helpers keep
//! resolving for `state/streaming.rs`, the render path and the tests.

use ratatui::layout::Rect;

/// The area the history pane draws into: the width every line is wrapped at and
/// the height every scroll/clamp decision is measured against.
///
/// Derived from the Chat-page layout's history chunk (see
/// `App::update_viewport_from_terminal_size`), so it always matches what
/// `render_chat` draws rather than an independent height estimate.
#[derive(Clone, Copy)]
pub(crate) struct HistoryViewport {
    pub(crate) width: u16,
    pub(crate) height: u16,
}

#[derive(Clone, Copy)]
pub(crate) struct HistoryScrollState {
    /// Distance in content rows from the bottom of the history to the top of the
    /// viewport (`0` = pinned to the bottom).  Clamped to the valid range by
    /// [`HistoryScrollState::clamp`].
    pub(crate) scroll: usize,
}

/// The reading position captured when a session is left, so the next visit can
/// restore the same *content* even though the raw scroll offset (a distance
/// from the bottom) is only meaningful for the viewport height it was taken at:
/// the status band reflows the history viewport on attach, and background
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
        Self { scroll: 0 }
    }

    pub(crate) fn clamp(&mut self, max_scroll: usize) {
        self.scroll = self.scroll.min(max_scroll);
    }

    pub(crate) fn effective_scroll(self, max_scroll: usize) -> usize {
        self.scroll.min(max_scroll)
    }

    pub(crate) fn scroll_up(&mut self, amount: usize, max_scroll: usize) {
        self.scroll = self.scroll.saturating_add(amount);
        self.clamp(max_scroll);
    }

    pub(crate) fn scroll_down(&mut self, amount: usize, max_scroll: usize) {
        self.scroll = self.scroll.saturating_sub(amount);
        self.clamp(max_scroll);
    }
}

/// Starting visual row of the semantic line at `idx` within the render whose
/// cumulative [`visual_offsets`](crate::markdown_render::compute_visual_offsets)
/// are `offsets`: the total visual rows covered by every earlier line.
///
/// Shared by the reasoning-header / tool-result click ranges and the streaming
/// auto-pin anchor, which all convert a semantic line index (as reported by the
/// renderer) into a visual-row offset the same way — keeping a single copy stops
/// the conversions from drifting apart.  `idx` indexes a line of the same render
/// that produced `offsets`, so it is in bounds by construction; the guards keep
/// the helper total on any drift.
pub(crate) fn semantic_line_row_start(offsets: &[usize], idx: usize) -> usize {
    if idx == 0 {
        0
    } else {
        offsets.get(idx - 1).copied().unwrap_or(0)
    }
}

/// Visual-row range `(start, end)` of the semantic line at `idx`, i.e.
/// `[semantic_line_row_start(idx), semantic_line_row_start(idx) + rows_of(idx))`.
/// The `end` is the start of the *next* line (`offsets[idx]`); a missing entry
/// (drift or the last line) collapses the range to `start`.
pub(crate) fn semantic_line_row_range(offsets: &[usize], idx: usize) -> (usize, usize) {
    let start = semantic_line_row_start(offsets, idx);
    let end = offsets.get(idx).copied().unwrap_or(start);
    (start, end)
}
