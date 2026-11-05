//! Shared chrome for the render layer's read-only reference popups.
//!
//! The Terminal page (`terminal_help`) and the Help modal (`help`) are
//! variable-length reference pages drawn over the Chat page. They share one
//! drawing routine so both get the same border, the same centering and — the
//! reason this module exists — the same **content-fitted, scrollable** sizing.
//!
//! A picker popup wants a fixed fraction of the screen (it is a scrolling list
//! either way), but a reference page must show every row it has whenever the
//! terminal can hold them: sizing it to a fraction clipped the last rows on
//! ordinary terminals even when the screen had room below the popup. So the
//! height is measured from the wrapped text here, and when the text is taller
//! than the screen the popup fills the screen and the caller scrolls the body.

use crate::state::{PopupSize, centered_popup};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Style},
    text::Line,
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};

/// Draw a titled, bordered, centered reference popup whose height fits its
/// wrapped content (clamped to the screen) and whose body is scrolled by
/// `scroll`.
///
/// `width_size` supplies the width fraction, the horizontal centering and the
/// width clamps (the height fraction is ignored — it is measured instead). The
/// body is measured with [`Paragraph::line_count`] so the computed height is
/// exactly the number of rows the same `Paragraph` draws, wrapping included.
///
/// `*scroll` is the caller's scroll offset in rows; it is clamped here to the
/// content (a page that fits, or an offset left over from a taller terminal, can
/// never draw blank rows), so the value is a valid offset for the next frame.
pub(super) fn render_reference_popup(
    frame: &mut Frame<'_>,
    title: &str,
    width_size: PopupSize,
    lines: Vec<Line<'static>>,
    scroll: &mut u16,
) {
    let area = frame.area();
    // Width and horizontal centering come from the shared popup geometry; the
    // height fraction is deliberately unused — it is measured below.
    let base = centered_popup(area, width_size);
    // The paragraph is drawn into the block's inner area, one cell in from each
    // side, so it wraps to that width and the measured height matches the draw.
    let inner_width = base.width.saturating_sub(2).max(1);
    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
    // Exact wrapped row count, using the same wrapping the renderer applies.
    // The count is a `usize`; it is bounded by the text length, which cannot
    // approach `u16::MAX` for these hand-built pages, but saturate rather than
    // wrap if it ever did.
    let content_rows = u16::try_from(paragraph.line_count(inner_width)).unwrap_or(u16::MAX);
    let (popup, max_scroll) = reference_popup_geometry(area, base, content_rows);
    // Clamp the caller's offset to what is actually scrollable, writing it back
    // so a later frame starts from the corrected value rather than re-clamping a
    // stale, oversized one.
    *scroll = (*scroll).min(max_scroll);
    // Erase the region beneath so the modal reads as a solid overlay rather than
    // text drawn on top of the chat history.
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .title(format!(" {title} "))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    frame.render_widget(paragraph.scroll((*scroll, 0)), inner);
}

/// Pure geometry for a reference popup: the popup rect and the maximum scroll
/// offset it admits.
///
/// `base` supplies the width and horizontal placement (from [`centered_popup`]);
/// only its height is overridden, fitted to `content_rows` wrapped rows plus the
/// two border rows, then clamped to the screen (one-cell margin each side) and
/// centred vertically. When the content is taller than the screen the popup
/// fills it, and the returned `max_scroll` is the number of rows that no longer
/// fit — the body then scrolls by up to that many rows.
///
/// Split out and pure so the sizing and scroll-clamp rules are unit-testable
/// without a terminal.
fn reference_popup_geometry(area: Rect, base: Rect, content_rows: u16) -> (Rect, u16) {
    // Grow to fit the content, but never past the screen (one-cell margin each
    // side) and never taller than the available height.
    let max_h = area.height.saturating_sub(2).max(3);
    let height = content_rows
        .saturating_add(2)
        .max(3)
        .min(max_h)
        .min(area.height.max(1));
    let inner_height = height.saturating_sub(2);
    let max_scroll = content_rows.saturating_sub(inner_height);
    // Keep the width and horizontal placement from the shared geometry but
    // centre the measured height vertically.
    let popup = Rect {
        x: base.x,
        y: area.y + area.height.saturating_sub(height) / 2,
        width: base.width,
        height,
    };
    (popup, max_scroll)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The width spec a reference page passes (`PopupSize::LIST`): the tests only
    /// care about the fitted height and scroll, not the exact width.
    fn base(area: Rect) -> Rect {
        centered_popup(area, PopupSize::LIST)
    }

    #[test]
    fn content_shorter_than_the_screen_fits_exactly_and_does_not_scroll() {
        // A four-row page in a tall terminal: the popup is borders + the four
        // rows (six), never the 2/3 fraction a picker would take, and there is
        // nothing to scroll.
        let area = Rect::new(0, 0, 80, 40);
        let (popup, max_scroll) = reference_popup_geometry(area, base(area), 4);
        assert_eq!(popup.height, 6, "borders + content");
        assert_eq!(max_scroll, 0, "a page that fits never scrolls");
        // The fixed LIST fraction would have been 26 rows (2/3 of 40): the
        // content-fitted popup is much shorter, so nothing is wasted.
        assert!(popup.height < 40 * 2 / 3);
    }

    #[test]
    fn content_taller_than_the_screen_fills_it_and_scrolls_the_overflow() {
        // Forty rows in a 24-row terminal: the popup fills to the 22-row screen
        // cap (24 minus a one-cell margin each side), and the 20 rows that no
        // longer fit are exactly the scrollable range.
        let area = Rect::new(0, 0, 80, 24);
        let (popup, max_scroll) = reference_popup_geometry(area, base(area), 40);
        assert_eq!(popup.height, 22, "fills the screen with a one-cell margin");
        assert_eq!(max_scroll, 40 - 20, "inner height is 20, so 20 rows scroll");
    }

    #[test]
    fn content_that_exactly_fills_the_inner_height_does_not_scroll() {
        // Content whose wrapped rows equal the inner height of a full-height
        // popup exactly: the whole page is visible and there is no dead scroll
        // range. On a 24-row terminal the popup fills to 22 (24 minus a
        // one-cell margin each side; that cap already includes the two border
        // rows), so 20 inner rows is the exact fit.
        let area = Rect::new(0, 0, 80, 24);
        let (popup, max_scroll) = reference_popup_geometry(area, base(area), 20);
        assert_eq!(popup.height, 22);
        assert_eq!(max_scroll, 0, "content fits exactly, nothing scrolls");
    }

    #[test]
    fn the_popup_stays_inside_the_area() {
        // Even on a degenerate tiny terminal the popup never exceeds the area.
        for height in 1..8u16 {
            let area = Rect::new(0, 0, 40, height);
            let (popup, _) = reference_popup_geometry(area, base(area), 100);
            assert!(
                popup.height <= area.height,
                "popup height {} exceeds area height {height}",
                popup.height
            );
        }
    }

    #[test]
    fn the_popup_is_vertically_centered() {
        let area = Rect::new(0, 0, 80, 40);
        let (popup, _) = reference_popup_geometry(area, base(area), 10);
        // 12-row popup in a 40-row area → 14 rows of margin, split evenly.
        assert_eq!(popup.height, 12);
        assert_eq!(popup.y, (40 - 12) / 2);
    }
}
