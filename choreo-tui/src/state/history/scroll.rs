//! The history pane's scroll position: the from-bottom offset, its clamp, the
//! reading position captured on leaving a session, and the scrollbar notch.
//!
//! The offset this module manipulates is a distance from the *bottom*, so it is
//! only meaningful against one particular pair of viewport height and total
//! content height — both of which can differ by the time the user returns to a
//! session.  Keeping the whole conversion chain in one file (clamp against the
//! freshly measured height, capture an absolute top line on the way out, convert
//! it back against the possibly changed height on the way in) is what keeps the
//! two ends of that conversion consistent.
//!
//! This is also where a *manual* scroll disarms the streaming auto-pin: every
//! scroll entry point compares the offset before and after and disarms only on
//! real movement, so a scroll already at its limit — or a jump onto the current
//! position — leaves a pending pin armed.  Keeping all the entry points here is
//! what makes that rule reviewable in one place.
//!
//! The `App` wrappers that forward to the active session's display sit beside the
//! methods they drive, so the UI loop's entry point and the arithmetic behind it
//! are read together.  Re-exported from `state` (`pub(crate) use history::*`),
//! so the methods and wrappers keep resolving crate-wide.

use super::super::{App, SessionDisplayState};
use super::viewport::{HistoryViewport, ScrollRestore};

// `HistoryViewport` is a 4-byte Copy struct; every call site already holds a
// reference (App-level wrappers, tests), so taking it by value would churn
// signatures across the crate for no measurable gain.
#[expect(clippy::trivially_copy_pass_by_ref)]
impl SessionDisplayState {
    pub(crate) fn max_scroll_offset(&self, viewport: &HistoryViewport) -> usize {
        let viewport_height = viewport.height as usize;
        let total_height = self.total_history_height();
        total_height.saturating_sub(viewport_height)
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
    ///
    /// `pub(crate)` rather than private to this module because its one caller is
    /// the height model's settle point (`compute_total_height_and_markers` in
    /// `history::height`): the restore must run at exactly the moment the prefix
    /// sum is fresh, which is that method's contract, not this module's.
    pub(crate) fn apply_scroll_restore(&mut self, viewport: &HistoryViewport) {
        let Some(restore) = self.scroll_restore.take() else {
            return;
        };
        if restore.at_bottom {
            // Was pinned to the bottom: follow any content that arrived while
            // the session was in the background, exactly as an in-session
            // content change does when the user is at the bottom.
            self.history_scroll.scroll = 0;
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
    }

    pub(crate) fn effective_scroll(&self, viewport: &HistoryViewport) -> usize {
        self.history_scroll
            .effective_scroll(self.max_scroll_offset(viewport))
    }

    /// How many content rows one scrollbar notch (one track row, less the
    /// rounding) scrolls: the whole scrollable range divided by the virtual
    /// track, rounded up and never below one row.
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
        let before = self.history_scroll.scroll;
        self.history_scroll
            .scroll_up(amount, self.max_scroll_offset(viewport));
        // A manual scroll that actually moved the viewport takes the user out of
        // the auto-follow, disarming a not-yet-pinned response so returning to
        // the bottom cannot later yank the view up to the response top.  A
        // no-op scroll (already at the limit) leaves the pin armed.
        if self.history_scroll.scroll != before {
            self.response_pin.disarm_on_manual_scroll();
        }
    }

    pub(crate) fn scroll_down(&mut self, amount: usize, viewport: &HistoryViewport) {
        let before = self.history_scroll.scroll;
        self.history_scroll
            .scroll_down(amount, self.max_scroll_offset(viewport));
        if self.history_scroll.scroll != before {
            self.response_pin.disarm_on_manual_scroll();
        }
    }

    pub(crate) fn scroll_to(&mut self, row: usize, viewport: &HistoryViewport) {
        let max_scroll = self.max_scroll_offset(viewport);
        let amount = row.min(max_scroll);
        // Same disarm-on-real-movement rule as `scroll_up`/`scroll_down`: a jump
        // that lands on the current position is not a user-initiated move away
        // from the followed bottom.
        if amount != self.history_scroll.scroll {
            self.response_pin.disarm_on_manual_scroll();
        }
        self.history_scroll.scroll = amount;
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

/// Integer ceiling division: `ceil(a / b)`.
/// Returns 0 when `b == 0`.
///
/// Private to this module: the scrollbar notch is its only user, and the notch
/// is the only place the scrollable range is divided by the virtual track.
fn ceil_div(a: usize, b: usize) -> usize {
    if b == 0 {
        return 0;
    }
    a.saturating_add(b).saturating_sub(1) / b
}

impl App {
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

    pub(crate) fn effective_scroll(&self) -> usize {
        self.active_display_ref()
            .map_or(0, |d| d.effective_scroll(&self.history_viewport))
    }

    /// Test-only: production reaches the notch through
    /// [`Self::scrollbar_scroll_up`] / [`Self::scrollbar_scroll_down`], which
    /// apply it; tests read it directly to pin the notch arithmetic.
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
