//! The history pane's render model, assembled from four subjects — viewport,
//! render cache, height model and scroll plumbing — plus the policy that decides
//! when they are invalidated.
//!
//! The pane has to map transcript content to screen rows (to draw them, to
//! hit-test a click, to place a scrollbar marker) and keep that mapping in
//! lockstep with the per-turn rendered lines.  Each subject that mapping is built
//! from changes for its own reason, so each gets its own file:
//!
//! - `viewport` — the pure geometry and reading position: [`HistoryViewport`],
//!   [`HistoryScrollState`], [`ScrollRestore`], and the
//!   semantic-line→visual-row conversions ([`semantic_line_row_start`] /
//!   [`semantic_line_row_range`]) that every consumer of a renderer-reported
//!   line index goes through.
//! - `cache` — the per-turn render cache: [`TurnLayout`], [`RenderCacheKey`],
//!   [`RenderedTurn`], [`RenderedCache`], [`cached_or_compute_lines`], and the
//!   per-turn content versions that keep a cached entry content-correct.
//! - `height` — the height prefix-sum and the scrollbar markers projected from
//!   it: [`Marker`], the binary-search row→turn mapping, and the image-block
//!   reserve that makes a turn's height depend on the viewport.
//! - `scroll` — the from-bottom scroll offset: its clamp, the reading position
//!   captured when a session is left and restored on the next visit, and the
//!   scrollbar notch.
//!
//! What stays here is the decision that spans all four: a recomputed viewport is
//! classified by [`classify_viewport_change`] into a [`ViewportChange`], which
//! [`App::update_viewport_from_terminal_size`] applies — a width change re-wraps
//! every line (so the render caches and the selection's column anchors go), while
//! a height change re-derives only the image-block reserve (so a text-only
//! session keeps its whole height model).  Its [`App::mark_terminal_resized`]
//! companion arms that recomputation.  The two free functions at the end stay
//! beside the driver for the same reason: [`find_turn_at_row`] and the test-only
//! [`history_text_height`] are the pane's outside-facing row mapping, not the
//! model of any one child.
//!
//! The streaming auto-pin ([`ResponsePin`](super::pin::ResponsePin) /
//! `maybe_pin_response_top`) lives in [`super::pin`]; it consumes the
//! `TurnLayout::response_start_row` anchor the `height` child computes.
//!
//! Every item is re-exported from `state` (`pub(crate) use history::*` in
//! `state/mod.rs`), so `crate::state::X` keeps resolving everywhere else in the
//! crate.

use ratatui::layout::Rect;

use super::{App, selector_list_layout};

// The height helper is used only by the test-only `history_text_height` below, so
// the import is gated to keep a non-test build warning-free.
#[cfg(test)]
use crate::markdown_render::lines_height;

mod cache;
mod height;
mod scroll;
mod viewport;

// Compatibility layer: every item in the children is re-exported here, and
// `state/mod.rs` re-exports this module wholesale, so `crate::state::X` keeps
// resolving exactly as it did when the whole history model was one file.
pub(crate) use cache::*;
pub(crate) use height::*;
// `scroll` declares only inherent `App`/`SessionDisplayState` methods, so there
// is nothing to re-export: its `mod` declaration above is the whole wiring (the
// same shape as `pointer` in `state/mod.rs`).
pub(crate) use viewport::*;

/// What a recomputed history viewport implies for the per-session render caches
/// and the in-progress selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ViewportChange {
    /// Nothing that affects the rendered content changed.  The history viewport
    /// height can still shrink or grow (the status/error line appeared or
    /// disappeared, the input box grew) but no line re-wraps, so the render
    /// caches and the selection must be left alone.
    None,
    /// The rendered width changed, so every line re-wraps: the width-keyed
    /// render caches are stale and the selection's stored column anchors no
    /// longer point at the same text.
    Rewrap,
    /// Only the height changed — a real vertical resize, or the chrome (the
    /// status/error line, the input box) growing or shrinking.
    /// Nothing re-wraps, so the caches and the selection stay valid.  Turn
    /// heights read the viewport height only through the image-block reserve,
    /// so a session with no image-bearing turn keeps its whole height model and
    /// only the scrollbar's marker slots rescale lazily to the new track; a
    /// session with an image-bearing turn (`any_image_turns`) marks the model
    /// dirty so the next cache-hit rebuild recomputes the reserve.
    HeightsOnly,
}

/// Classify a history-viewport change into the invalidation it warrants.
///
/// Pure (no terminal access) so the policy — and its full case matrix — is
/// unit-testable.  Only the rendered WIDTH decides a re-wrap: nothing about
/// wrapping reads the height.  A height change re-wraps nothing and is not
/// itself a reason to rebuild the height model — turn heights depend on the
/// viewport height only through the image-block reserve.  So a text-only
/// session's height model stays valid and only the scrollbar's marker slots
/// rescale; a session with an image-bearing turn (`any_image_turns`) marks the
/// model dirty so the next cache-hit rebuild recomputes the reserve.  The
/// [`ViewportChange`] variants carry that split, and the caller (with the
/// display in hand) decides which case its session is in.
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
        let [history_area, _, _, _] = self.chat_page_layout(width, height);
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
        // resize, OR the chrome (the status/error line, the input box) growing
        // or shrinking — re-wraps nothing, so the caches and the selection
        // survive.  Turn heights read the viewport height only through the
        // image-block reserve (`image_block_height`), so a text-only session's
        // height model stays valid and only the scrollbar's marker slots rescale
        // to the new virtual track (lazily, in `ensure_marker_slots`); a session
        // with an image-bearing turn marks the model dirty so the next
        // cache-hit rebuild recomputes the reserve.
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
                // Nothing re-wraps.  The only turn-height input that reads the
                // viewport height is the image-block reserve
                // (`image_block_height`), so a turn's height changes only when
                // that reserve changes AND the turn actually has an image.
                // Otherwise the height prefix, turn heights and render caches
                // are all still valid — only the scrollbar's marker slots
                // rescale to the new virtual track, which `ensure_marker_slots`
                // does lazily at the next draw (O(markers), no height rebuild).
                // This keeps a chrome-height change — a status line appearing on
                // the first keystroke, the input box wrapping onto a new line —
                // off the O(turns) full-rebuild path for text-only sessions.
                let old_img = (old_height / 2).max(1);
                let new_img = (new_height / 2).max(1);
                let img_height_changed = old_img != new_img;
                for display in self.session_displays.values_mut() {
                    if img_height_changed && display.any_image_turns {
                        display.markers_dirty = true;
                    } else {
                        display.marker_slots_dirty = true;
                    }
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
}

/// Test-only: the height a plain-text string occupies at a given width.  Lives
/// here rather than beside the height model because it is a measurement helper
/// for tests that compare the pane's arithmetic against the markdown renderer,
/// not part of the model itself.
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
