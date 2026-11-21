//! Popup sizing: how large an overlay box is and where it sits.
//!
//! Every modal in the TUI comes in one of two shapes — a large list popup sized
//! as a fraction of the screen ([`PopupSize`] with [`centered_popup`]), or a
//! content-sized dialog whose width is the only dimension a spec can give
//! ([`DialogWidth`], because its height comes from the rows it has to show).
//! Both funnel through [`clamped_extent`], so every overlay shares one
//! panic-free clamping rule instead of each re-deriving the arithmetic.
//!
//! The text-measurement helpers live here too, because their only callers size
//! boxes: a reserved height must equal the rows ratatui actually draws, so the
//! measurement goes through the same `Paragraph::wrap` the renderer uses.
//!
//! Re-exported from `state` (`pub(crate) use layout::*`), so the renderers and
//! the connection-layer mouse handlers keep importing these through
//! `crate::state`.

use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Wrap};

/// Sizing parameters for [`centered_popup`]: width/height as fractions of the
/// terminal size (numerators over denominators), floored at the minimums,
/// capped at the maximums, and clipped so the popup never touches the screen
/// edges.
pub(crate) struct PopupSize {
    pub(crate) w_num: u32,
    pub(crate) w_den: u32,
    pub(crate) h_num: u32,
    pub(crate) h_den: u32,
    pub(crate) min_w: u16,
    pub(crate) min_h: u16,
    pub(crate) max_w: u16,
    pub(crate) max_h: u16,
}

impl PopupSize {
    /// The large list-popup sizing shared by the model selector and the
    /// wizard's provider picker: ~60% of the width, ~2/3 of the height.
    pub(crate) const LIST: PopupSize = PopupSize {
        w_num: 3,
        w_den: 5,
        h_num: 2,
        h_den: 3,
        min_w: 24,
        min_h: 8,
        max_w: 100,
        max_h: 40,
    };
}

/// The width of a small no-filter dialog (the staged flow's reasoning dialog):
/// a fraction of the terminal width, floored at `min` and capped at `max`.
///
/// Deliberately width-only. A dialog's height comes from its content (see
/// [`dialog_layout`](super::overlay::dialog_layout)) — a three-row ladder drawn
/// as a fixed fraction of the screen would be mostly empty box — so a height
/// field here would be a value no caller reads.
pub(crate) struct DialogWidth {
    pub(crate) num: u16,
    pub(crate) den: u16,
    pub(crate) min: u16,
    pub(crate) max: u16,
}

impl DialogWidth {
    /// A narrow box: ~40% of the width, never below 28 columns and never wider
    /// than 56, so the ladder stays a dialog rather than a list popup.
    pub(crate) const DIALOG: DialogWidth = DialogWidth {
        num: 2,
        den: 5,
        min: 28,
        max: 56,
    };

    /// This dialog's width on a terminal `area_width` columns wide, clipped off
    /// the screen edges by the same 4-column slack [`clamped_extent`] applies to
    /// every popup.
    pub(super) fn width(&self, area_width: u16) -> u16 {
        clamped_extent(
            area_width,
            u32::from(self.num),
            u32::from(self.den),
            self.min,
            self.max,
            4,
        )
    }
}

/// One popup dimension from a terminal `extent`: the fraction `num/den` of it,
/// floored at `min`, capped at `max`, then narrowed by `slack` columns/rows so
/// the popup never touches the screen edges, and never below one cell.
///
/// `extent.saturating_sub(slack)` is panic-free on a tiny terminal, and the
/// `.max(1)` keeps the clamp's bounds from inverting (clamp panics when its min
/// exceeds its max) — the arithmetic that lets every overlay share one sizing
/// rule without a panic path.
fn clamped_extent(extent: u16, num: u32, den: u32, min: u16, max: u16, slack: u16) -> u16 {
    // u16 → u32 widening is lossless and the divided fraction always fits back
    // in u16 here because it is subsequently clamped to `max` (a u16) before any
    // other use.
    #[expect(clippy::cast_possible_truncation)] // clamped to a u16 max below
    {
        ((u32::from(extent) * num / den) as u16)
            .clamp(min, max)
            .min(extent.saturating_sub(slack))
            .max(1)
    }
}

/// Compute a centered popup rect for a modal overlay from a [`PopupSize`].
/// The `.min(area…)` guards keep the arithmetic panic-free on tiny terminals
/// (clamp panics if its bounds are inverted).  Shared by the model selector
/// and the account/credential modals so every overlay uses the same centering
/// and clamping rules.  Lives here (state/layout/popup.rs) so both the renderers
/// and the connection-layer mouse handlers can use it without an import cycle.
// `size` is not consumed, but taking a reference would change the signature
// used by the render-layer call sites; a small owned struct is intentional.
#[expect(clippy::needless_pass_by_value)] // keep the shared call-site signature
pub(crate) fn centered_popup(area: Rect, size: PopupSize) -> Rect {
    // The 4-column / 2-row slack is what keeps the box off the screen edges.
    let width = clamped_extent(
        area.width, size.w_num, size.w_den, size.min_w, size.max_w, 4,
    );
    let height = clamped_extent(
        area.height,
        size.h_num,
        size.h_den,
        size.min_h,
        size.max_h,
        2,
    );
    Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    }
}

/// Rows `text` occupies when drawn in a wrapping [`Paragraph`] at `width`
/// columns — exactly the rows the renderer's `Paragraph::wrap` draws, embedded
/// newlines and word-wrap included.
///
/// The status/error bar reserves its height with this so the rows it claims equal
/// the rows the renderer actually draws. Measuring with a hand-rolled wrapper (or
/// a `width.div_ceil(columns)` approximation) can round differently from
/// ratatui's word wrapper and either clip the tail or leave a dead row. `0` for a
/// zero width; empty text measures as the one empty line ratatui draws.
pub(crate) fn wrapped_text_rows(text: &str, width: u16) -> u16 {
    if width == 0 {
        return 0;
    }
    // `line_count` is bounded by the text length; saturate rather than truncate
    // if a pathological status message ever exceeded `u16::MAX` rows.
    u16::try_from(
        Paragraph::new(text)
            .wrap(Wrap { trim: false })
            .line_count(width),
    )
    .unwrap_or(u16::MAX)
}

/// Rows `lines` occupy when drawn in a wrapping [`Paragraph`] at `width` columns.
///
/// The same exactness contract as [`wrapped_text_rows`], for pre-built lines (the
/// welcome panel sizes its box from this so a wrapping advice line is budgeted for
/// every row it draws). Clones the lines to measure; the callers run only on an
/// empty session, so the copy is irrelevant.
pub(crate) fn wrapped_lines_rows(lines: &[Line<'_>], width: u16) -> u16 {
    if width == 0 {
        return 0;
    }
    u16::try_from(
        Paragraph::new(lines.to_vec())
            .wrap(Wrap { trim: false })
            .line_count(width),
    )
    .unwrap_or(u16::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrapped_text_rows_matches_the_renderer_wrap() {
        // Fits in one row.
        assert_eq!(wrapped_text_rows("short", 80), 1);
        // A single long unbroken word hard-splits: 25 columns at width 10.
        assert_eq!(wrapped_text_rows(&"x".repeat(25), 10), 3);
        // Embedded newlines are counted, one row each.
        assert_eq!(wrapped_text_rows("a\nb\nc", 80), 3);
        // Zero width draws nothing; empty text is the one empty line ratatui draws.
        assert_eq!(wrapped_text_rows("anything", 0), 0);
        assert_eq!(wrapped_text_rows("", 80), 1);
    }

    #[test]
    fn wrapped_lines_rows_matches_the_renderer_wrap() {
        let lines = vec![
            Line::from("short"),
            Line::from(""),
            Line::from("y".repeat(30)),
        ];
        // Two single rows (one empty) plus a 30-column word at width 10 → 3 rows.
        assert_eq!(wrapped_lines_rows(&lines, 10), 1 + 1 + 3);
        assert_eq!(wrapped_lines_rows(&lines, 0), 0);
    }
}
