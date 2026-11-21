//! The modal overlays' internal geometry and their left-click mapping.
//!
//! Two overlay shapes are covered.  A LIST picker (the new-account wizard's
//! provider picker and the model selector) has three bands — filter row, list
//! body, footer — computed once by [`selector_list_layout`] and consumed by the
//! renderer, the viewport cache and the mouse hit-test, so the three can never
//! drift apart.  A content-sized dialog (the staged flow's reasoning ladder) has
//! two bands and is deliberately *not* a [`SelectorLayout`]: it has nothing to
//! filter, so a third band would be an empty filter row with a cursor in it.
//!
//! The click mapping lives beside the bands it reads: [`selector_click_target`]
//! resolves a terminal click through the same layout the renderer drew, and
//! maps it onto the *rendered* window start rather than the stored scroll — a
//! narrowing filter or a PgUp/PgDn jump can leave the stored offset stale, and
//! the raw value would then select a different row than the one clicked.
//!
//! Re-exported from `state` (`pub(crate) use layout::*`), so
//! `crate::state::selector_list_layout` and the click helpers keep resolving for
//! the connection-layer handlers, the renderers and the tests.

use super::super::InputBuffer;
use super::super::input::grapheme_offset_at_column;
use super::popup::{DialogWidth, PopupSize, centered_popup};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::widgets::{Block, Borders};

/// The three vertical bands of the LIST-popup layout (shared by the wizard's
/// provider picker and the model selector): the filter row on top, the
/// scrollable list body, and the footer hint row.  Computed by
/// [`selector_list_layout`] — the single source of truth for rendering, mouse
/// hit-testing, and viewport caching, so the three can never drift apart
/// (mirroring the `chat_page_layout` pattern).
pub(crate) struct SelectorLayout {
    pub(crate) filter_row: Rect,
    pub(crate) body: Rect,
    pub(crate) footer: Rect,
}

/// Compute the LIST-popup layout over `area` (the terminal frame): the
/// centered popup, its bordered inner area, then the vertical 3-way split
/// (filter row / body / footer).  Replicates exactly what
/// `render_wizard_provider`/`render_model_selector` draw, so the mouse
/// handlers and the viewport cache reuse the same geometry.  The renderers
/// still call [`centered_popup`] themselves for the popup rect (used by
/// `Clear` and the bordered `Block`); both paths share that pure function, so
/// the derived bands are always the ones drawn.
pub(crate) fn selector_list_layout(area: Rect) -> SelectorLayout {
    let popup = centered_popup(area, PopupSize::LIST);
    // The bordered block shrinks the inner area by one cell on every side,
    // exactly like the renderers' `Block::borders(Borders::ALL).inner(popup)`.
    let inner = Block::default().borders(Borders::ALL).inner(popup);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(inner);
    SelectorLayout {
        // Layout::vertical([Length(1), Min(1), Length(1)]) always yields
        // exactly 3 chunks; a zero-area fallback is harmless (no-op clicks).
        filter_row: chunks.first().copied().unwrap_or_default(),
        body: chunks.get(1).copied().unwrap_or_default(),
        footer: chunks.get(2).copied().unwrap_or_default(),
    }
}

/// The two vertical bands of a small dialog: the content row band and the
/// footer hint row, inside a bordered, content-sized popup.
///
/// Deliberately not a [`SelectorLayout`]: those popups are list *pickers* with a
/// filter row, and a three-to-five row effort ladder has nothing to filter.
/// Forcing the third band on it would spend a row on an empty filter and put a
/// cursor where nothing is typed.
pub(crate) struct DialogLayout {
    pub(crate) popup: Rect,
    pub(crate) body: Rect,
    pub(crate) footer: Rect,
}

/// Compute a small dialog's geometry over `area` (the terminal frame): a
/// centered box as wide as [`DialogWidth::DIALOG`] asks for, but only
/// `body_rows` content rows plus a border and a footer tall — clamped to what
/// the terminal can actually show.  The renderer draws into exactly these
/// bands, so a content-sized dialog can never claim rows it does not draw.
pub(crate) fn dialog_layout(area: Rect, body_rows: u16) -> DialogLayout {
    let width = DialogWidth::DIALOG.width(area.width);
    // `body_rows + 2` borders + 1 footer row, never taller than the screen
    // allows (the `.max(3)` keeps room for the borders and the footer when the
    // terminal is tiny).
    let height = body_rows
        .saturating_add(3)
        .min(area.height.saturating_sub(2).max(3));
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width: width.min(area.width).max(1),
        height,
    };
    // The bordered block shrinks the inner area by one cell on every side,
    // exactly like the renderer's `Block::borders(Borders::ALL).inner(popup)`.
    let inner = Block::default().borders(Borders::ALL).inner(popup);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(inner);
    DialogLayout {
        popup,
        body: chunks.first().copied().unwrap_or_default(),
        footer: chunks.get(1).copied().unwrap_or_default(),
    }
}

/// Map a terminal `(column, row)` click onto the visible rows of the LIST
/// popup's list body.  Returns the 0-based local row index within the body
/// (0 == the first rendered list row) when the click falls inside the body
/// area, or `None` when it lands outside the body — the filter row, the
/// footer, the popup's borders, or past the popup entirely — where a click is
/// a no-op for row selection.  The caller combines the local row with the
/// current *rendered* window start and clamps against the filtered-list
/// length (the body may extend past the visible tail when the list is shorter
/// than the window).
pub(crate) fn selector_local_row(layout: &SelectorLayout, column: u16, row: u16) -> Option<usize> {
    if column < layout.body.x || column >= layout.body.x + layout.body.width {
        return None;
    }
    if row < layout.body.y || row >= layout.body.y + layout.body.height {
        return None;
    }
    Some((row - layout.body.y) as usize)
}

/// Position `filter`'s cursor from a left-click in the popup's filter row:
/// the click column minus the `"> "` prefix and the popup's left border/
/// padding maps (grapheme-aware, clamped to the text) to a byte offset in the
/// filter text.  Shared by the wizard's provider picker and the model
/// selector so both modals position the cursor identically.  A click at or
/// before the prefix clamps to the start of the text; a click past the end
/// clamps to the end (`grapheme_offset_at_column` returns `s.len()`).
/// `filter_row_x` is the filter row's left edge inside the popup border
/// (see [`SelectorLayout::filter_row`]).
pub(crate) fn selector_position_filter_cursor(
    filter: &mut InputBuffer,
    filter_row_x: u16,
    column: u16,
) {
    let col = column.saturating_sub(filter_row_x + 2) as usize;
    filter.cursor = grapheme_offset_at_column(&filter.text, col);
}

/// Outcome of hit-testing a left-click against the LIST popup, shared by the
/// wizard's provider picker and the model selector.
pub(crate) enum SelectorClick {
    /// The click landed on filtered-list index `idx`.  The index is resolved
    /// through the *rendered* window start (see `window()`/`picker_window`),
    /// never the stored `scroll` — which can be stale (a filter narrowing or
    /// a PgUp/PgDn jump leaves it past the new `max_scroll`), and the raw value
    /// would then map the click onto a different row than the one drawn.
    Row(usize),
    /// The click landed on the filter row; `filter_row_x` is its left edge
    /// (inside the popup border) so the caller can position the cursor via
    /// [`selector_position_filter_cursor`].
    FilterRow { column: u16, filter_row_x: u16 },
    /// The click landed on the footer, a popup border, or outside the popup
    /// entirely (including below the visible tail of a list shorter than the
    /// window) — a no-op for both row selection and cursor placement.
    Noop,
}

/// Hit-test a left-click against the LIST popup geometry derived from
/// `last_terminal_size` (the same geometry the renderers draw and the viewport
/// cache measures), mapping it onto [`SelectorClick`].  `filtered_len` is the
/// current filtered-list length and `window_start` the first *drawn* row of
/// the visible window (`window()`'s start) — both supplied by the caller so
/// this function stays pure geometry plus a bounds check.  Before the first
/// frame the terminal size is unknown and every click is a [`SelectorClick::Noop`].
pub(crate) fn selector_click_target(
    last_terminal_size: Option<(u16, u16)>,
    column: u16,
    row: u16,
    filtered_len: usize,
    window_start: usize,
) -> SelectorClick {
    let Some((width, height)) = last_terminal_size else {
        return SelectorClick::Noop;
    };
    let layout = selector_list_layout(Rect {
        x: 0,
        y: 0,
        width,
        height,
    });
    if let Some(local) = selector_local_row(&layout, column, row) {
        let idx = window_start.saturating_add(local);
        if idx < filtered_len {
            SelectorClick::Row(idx)
        } else {
            // Inside the body band but past the visible tail (a list shorter
            // than the window): there is no row there to pick.
            SelectorClick::Noop
        }
    } else if row == layout.filter_row.y
        && column >= layout.filter_row.x
        && column < layout.filter_row.x + layout.filter_row.width
    {
        SelectorClick::FilterRow {
            column,
            filter_row_x: layout.filter_row.x,
        }
    } else {
        SelectorClick::Noop
    }
}

/// Apply a left-click to the LIST popup, shared by the wizard's provider
/// picker and the model selector: a click on a visible list row returns the
/// filtered-list index to select, a click on the filter row positions the
/// input cursor (grapheme-aware, via [`selector_position_filter_cursor`]),
/// and every other click (footer, borders, the dimmed area, below the
/// visible tail) is a no-op.  `filtered_len` is the current filtered-list
/// length and `window_start` the first *drawn* row (`window()`'s start — see
/// [`selector_click_target`]); the caller performs the row-selection action
/// itself, because the two pickers confirm differently (the model selector
/// sends `SetModel` and closes; the wizard advances to the slug step).
pub(crate) fn apply_selector_left_click(
    last_terminal_size: Option<(u16, u16)>,
    column: u16,
    row: u16,
    filtered_len: usize,
    window_start: usize,
    filter: &mut InputBuffer,
) -> Option<usize> {
    match selector_click_target(last_terminal_size, column, row, filtered_len, window_start) {
        SelectorClick::Row(idx) => Some(idx),
        SelectorClick::FilterRow {
            column,
            filter_row_x,
        } => {
            selector_position_filter_cursor(filter, filter_row_x, column);
            None
        }
        SelectorClick::Noop => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A hand-built layout for the row-mapping tests (geometry-only, so the
    /// numbers are picked for clarity rather than derived from a popup).
    fn test_layout() -> SelectorLayout {
        SelectorLayout {
            filter_row: Rect {
                x: 2,
                y: 2,
                width: 40,
                height: 1,
            },
            body: Rect {
                x: 2,
                y: 3,
                width: 40,
                height: 10,
            },
            footer: Rect {
                x: 2,
                y: 13,
                width: 40,
                height: 1,
            },
        }
    }

    #[test]
    fn selector_local_row_maps_body_clicks_to_local_index() {
        let layout = test_layout();
        assert_eq!(selector_local_row(&layout, 5, 3), Some(0), "first body row");
        assert_eq!(selector_local_row(&layout, 5, 7), Some(4));
        assert_eq!(
            selector_local_row(&layout, 41, 12),
            Some(9),
            "last body row"
        );
    }

    #[test]
    fn selector_local_row_below_tail_is_none() {
        let layout = test_layout();
        // Below the body's bottom edge: the footer row, the popup border, and
        // the dimmed area past the popup all map to None.
        assert_eq!(selector_local_row(&layout, 5, 13), None, "footer row");
        assert_eq!(selector_local_row(&layout, 5, 14), None, "past the popup");
        assert_eq!(
            selector_local_row(&layout, 5, 2),
            None,
            "filter row is not body"
        );
    }

    #[test]
    fn selector_local_row_outside_body_is_none() {
        let layout = test_layout();
        // Outside the body horizontally (popup border / dimmed area).
        assert_eq!(selector_local_row(&layout, 1, 5), None, "left of the body");
        assert_eq!(
            selector_local_row(&layout, 42, 5),
            None,
            "right of the body"
        );
    }

    // ── selector_click_target (click → row / filter-cursor / no-op) ──────

    #[test]
    fn selector_click_target_resolves_through_window_start() {
        // Real 100x40 LIST geometry: a click on body row 2 with the window
        // drawn from row 5 maps to filtered index 5 + 2 = 7 — the RENDERED
        // row, never the (possibly stale) stored scroll (the caller passes
        // `window().0`).
        let layout = selector_list_layout(Rect {
            x: 0,
            y: 0,
            width: 100,
            height: 40,
        });
        let click =
            selector_click_target(Some((100, 40)), layout.body.x + 3, layout.body.y + 2, 15, 5);
        assert!(matches!(click, SelectorClick::Row(7)));
    }

    #[test]
    fn selector_click_target_below_tail_is_noop() {
        // 3 items in a 22-row body, window at 0: a click on body row 5 is
        // inside the band but past the visible tail.
        let layout = selector_list_layout(Rect {
            x: 0,
            y: 0,
            width: 100,
            height: 40,
        });
        let click =
            selector_click_target(Some((100, 40)), layout.body.x + 3, layout.body.y + 5, 3, 0);
        assert!(matches!(click, SelectorClick::Noop));
    }

    #[test]
    fn selector_click_target_filter_row_reports_left_edge() {
        // A click inside the filter row band (any column) returns the row's
        // left edge so the caller can position the cursor.
        let layout = selector_list_layout(Rect {
            x: 0,
            y: 0,
            width: 100,
            height: 40,
        });
        let click = selector_click_target(
            Some((100, 40)),
            layout.filter_row.x + 10,
            layout.filter_row.y,
            10,
            0,
        );
        match click {
            SelectorClick::FilterRow {
                column,
                filter_row_x,
            } => {
                assert_eq!(column, layout.filter_row.x + 10);
                assert_eq!(filter_row_x, layout.filter_row.x);
            }
            _ => panic!("expected a filter-row click"),
        }
    }

    #[test]
    fn selector_click_target_outside_is_noop() {
        let layout = selector_list_layout(Rect {
            x: 0,
            y: 0,
            width: 100,
            height: 40,
        });
        // Footer, the popup border (one column left of the body), and the
        // dimmed area all map to Noop.
        assert!(matches!(
            selector_click_target(Some((100, 40)), layout.footer.x + 2, layout.footer.y, 10, 0),
            SelectorClick::Noop
        ));
        assert!(matches!(
            selector_click_target(Some((100, 40)), layout.body.x - 1, layout.body.y + 2, 10, 0),
            SelectorClick::Noop
        ));
        assert!(matches!(
            selector_click_target(Some((100, 40)), 0, 0, 10, 0),
            SelectorClick::Noop
        ));
    }

    #[test]
    fn selector_click_target_unknown_terminal_size_is_noop() {
        // Before the first frame there is no geometry to hit-test against.
        assert!(matches!(
            selector_click_target(None, 5, 5, 10, 0),
            SelectorClick::Noop
        ));
    }

    // ── apply_selector_left_click (shared click application) ─────────────

    #[test]
    fn apply_selector_left_click_reports_row_and_positions_cursor() {
        let layout = selector_list_layout(Rect {
            x: 0,
            y: 0,
            width: 100,
            height: 40,
        });
        let mut filter = InputBuffer::new();
        filter.text = "abc".to_string();
        filter.cursor = 0;

        // A body-row click returns the filtered index (window start + local
        // row) and must not disturb the filter cursor.
        let row_click = apply_selector_left_click(
            Some((100, 40)),
            layout.body.x + 3,
            layout.body.y + 2,
            15,
            5,
            &mut filter,
        );
        assert_eq!(row_click, Some(7), "window start 5 + local row 2");
        assert_eq!(filter.cursor, 0, "a row click must not move the cursor");

        // A filter-row click positions the cursor (the click column minus the
        // "> " prefix) and reports no row.
        let row_click = apply_selector_left_click(
            Some((100, 40)),
            layout.filter_row.x + 4,
            layout.filter_row.y,
            10,
            0,
            &mut filter,
        );
        assert_eq!(row_click, None, "a filter-row click selects nothing");
        assert_eq!(filter.cursor, 2, "the click column minus the prefix");

        // Below the visible tail, outside the popup, and before the first
        // frame: no row and no cursor move.
        let below = apply_selector_left_click(
            Some((100, 40)),
            layout.body.x + 3,
            layout.body.y + 5,
            3,
            0,
            &mut filter,
        );
        assert_eq!(below, None);
        let outside = apply_selector_left_click(Some((100, 40)), 0, 0, 10, 0, &mut filter);
        assert_eq!(outside, None);
        let unknown = apply_selector_left_click(None, 5, 5, 10, 0, &mut filter);
        assert_eq!(unknown, None);
        assert_eq!(filter.cursor, 2, "non-row clicks must not move the cursor");
    }

    // ── dialog_layout (small no-filter modal) ────────────────────────────

    #[test]
    fn dialog_layout_sizes_the_box_to_its_content() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 100,
            height: 40,
        };
        let layout = dialog_layout(area, 4);
        // Four content rows + two borders + the footer row.
        assert_eq!(layout.popup.height, 7);
        assert_eq!(
            layout.body.height, 4,
            "the body band is exactly the content rows"
        );
        assert_eq!(layout.footer.height, 1);
        // Both bands sit inside the bordered box and inside the screen.
        for band in [layout.body, layout.footer] {
            assert!(band.x > layout.popup.x);
            assert!(band.x + band.width < layout.popup.x + layout.popup.width);
            assert!(band.y > layout.popup.y);
            assert!(band.y + band.height < layout.popup.y + layout.popup.height);
        }
        // Centered vertically (integer division, so a 7-row box on 40 rows
        // sits at row 16 with one row more below than above).
        assert_eq!(layout.popup.y, (area.height - layout.popup.height) / 2);
    }

    #[test]
    fn dialog_layout_cannot_outgrow_a_tiny_terminal() {
        // A ladder longer than the screen, on a screen too small for it: the
        // box clamps to the area (never taller than `height - 2`) and the bands
        // stay inside it, so nothing is drawn off-screen.
        let area = Rect {
            x: 0,
            y: 0,
            width: 30,
            height: 8,
        };
        let layout = dialog_layout(area, 20);
        assert_eq!(layout.popup.height, 6);
        assert_eq!(layout.popup.width, 26, "never wider than the screen allows");
        assert!(layout.body.height >= 1);
        assert_eq!(layout.footer.height, 1);
    }
}
