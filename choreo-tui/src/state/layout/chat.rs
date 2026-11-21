//! The Chat page's vertical layout maths.
//!
//! One `Layout::split` decides how the page's four chunks — history,
//! status/error bar, command input box, status bar — divide the terminal, and
//! every caller derives from it: `render_chat` draws into those chunks,
//! [`App::input_box_rect`] hit-tests clicks against the input box, and
//! `update_viewport_from_terminal_size` sizes the history viewport from the
//! history chunk.  Running the *identical* solver in all of them is what keeps
//! them from drifting — including on terminals too small for the fixed chrome to
//! fit, where the solver shrinks and relocates chunks rather than honouring
//! every `Length`.
//!
//! It sits with the overlay geometry rather than beside the rest of `App`
//! because it is geometry: the status/error bar's height and the input box's
//! content height come from the same measurement helpers the overlays use
//! (`super::popup`), and the input box's scroll window
//! ([`App::ensure_input_cursor_visible`]) is driven by the content height
//! computed here — which is why that scroll helper lives here too, beside the
//! height it reads, rather than with the accessors.
//!
//! Every method here is an inherent `App` method, so there is nothing to
//! re-export: the `mod` declaration in `layout/mod.rs` is the whole wiring (the
//! same shape as `pointer` in `state/mod.rs`).

use super::super::input::cached_visual_lines;
use super::super::{
    App, MAX_INPUT_CONTENT_LINES, MIN_INPUT_CONTENT_LINES, STATUS_BAR_HEIGHT, input_inner_width,
};
use super::popup::wrapped_text_rows;
use ratatui::layout::{Constraint, Direction, Layout, Rect};

impl App {
    /// Number of lines needed for the status/error bar, based on the current
    /// message content and the available terminal width.  Returns 0 when there
    /// is no message to display.
    pub(crate) fn status_error_height(&self, width: u16) -> u16 {
        // Read through the accessors, which return the stored (already
        // normalized) text.  Because normalization happens at ingest
        // (`App::set_error`/`App::set_status`), these are exactly the bytes the
        // renderer draws, so measuring them directly cannot reserve a height the
        // drawn wrapping never fills (or clip the tail) — a tab or control
        // character in a daemon-supplied message measured wider than it drew
        // when the raw text was measured here.
        let text = if let Some(err) = self.error() {
            err
        } else if let Some(status) = self.status() {
            status
        } else {
            return 0;
        };
        // The status/error Paragraph is drawn inset by one column on each side
        // (render.rs `notify_area`), so it wraps at `width - 2` columns. Measure
        // at that same inner width with the SAME wrapper the renderer uses
        // (`Paragraph::wrap`), so the reserved height equals the rows ratatui
        // actually draws — a hand-rolled wrapper can round differently and clip
        // the tail.
        let inner = width.saturating_sub(2);
        wrapped_text_rows(text, inner).max(1)
    }

    /// Number of visual content lines the input box currently occupies,
    /// computed from the text and terminal width.
    // Wrapped-line counts are bounded by the u16 terminal width, so the
    // usize→u16 cast cannot truncate in practice.
    #[expect(clippy::cast_possible_truncation)]
    pub(crate) fn input_bar_content_lines(&mut self, term_width: u16) -> u16 {
        // Must use the same inner width as the renderer (term_width minus the
        // INPUT_PAD padding on each side), or the box height can disagree with
        // the number of wrapped lines actually drawn — e.g. a wrap that the
        // renderer sees at the true inner width would not yet grow the box.
        let inner = input_inner_width(term_width);
        if inner < 1 {
            return 1;
        }
        let visual = cached_visual_lines(
            &self.input.text,
            inner,
            self.input.generation,
            &mut self.input.lines_cache,
        );
        (visual.len() as u16).clamp(MIN_INPUT_CONTENT_LINES, MAX_INPUT_CONTENT_LINES)
    }

    /// Total height of the input bar (content + borders).
    pub(crate) fn input_bar_height(&mut self, term_width: u16) -> u16 {
        self.input_bar_content_lines(term_width) + 2
    }

    /// The four vertical chunks of the Chat page: history, status/error,
    /// command input box, status bar.
    ///
    /// Single source of truth for the Chat page's vertical layout —
    /// `render_chat` draws into these chunks, `input_box_rect` hit-tests clicks
    /// against chunk 2, and `update_viewport_from_terminal_size` sizes the
    /// history viewport from chunk 0.  Every consumer runs the *identical*
    /// `Layout::split`, so they can never drift apart — including on terminals
    /// too small for the fixed chrome to fit, where the solver shrinks and
    /// relocates chunks rather than honouring every `Length`.
    pub(crate) fn chat_page_layout(&mut self, term_width: u16, term_height: u16) -> [Rect; 4] {
        let status_error_height = self.status_error_height(term_width);
        let input_height = self.input_bar_height(term_width);
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(1),
                Constraint::Length(status_error_height),
                Constraint::Length(input_height),
                Constraint::Length(STATUS_BAR_HEIGHT),
            ])
            .split(Rect {
                x: 0,
                y: 0,
                width: term_width,
                height: term_height,
            });
        // Layout::vertical([Min(1), Length(_), Length(_), Length(_)]) always
        // yields exactly 4 chunks; a zero-area fallback makes mouse hit-tests
        // no-ops (nothing to click on).
        [
            chunks.first().copied().unwrap_or_default(),
            chunks.get(1).copied().unwrap_or_default(),
            chunks.get(2).copied().unwrap_or_default(),
            chunks.get(3).copied().unwrap_or_default(),
        ]
    }

    /// Rectangle (terminal coordinates) occupied by the command input box on
    /// the Chat page, including its top/bottom borders.
    ///
    /// Delegates to [`Self::chat_page_layout`] so mouse hit-testing (clicking
    /// to position the cursor) always agrees with what `render_chat` draws —
    /// even on tiny terminals where the layout solver shrinks the box rather
    /// than placing it at a fixed distance above the status bar.
    pub(crate) fn input_box_rect(&mut self, term_width: u16, term_height: u16) -> Rect {
        self.chat_page_layout(term_width, term_height)[2]
    }

    pub(crate) fn ensure_input_cursor_visible(&mut self) {
        if let Some((term_w, _)) = self.last_terminal_size {
            // inner width must match the renderer's drawing width so the
            // scroll window matches what is actually displayed.
            let inner = input_inner_width(term_w);
            let visible_height = self.input_bar_content_lines(term_w) as usize;
            self.input.ensure_cursor_visible(inner, visible_height);
        }
    }
}
