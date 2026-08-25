//! Shared LIST-popup geometry for the two picker overlays (the new-account
//! wizard's provider picker and the model selector): popup sizing/centering
//! (`PopupSize`/`centered_popup`), the 3-band filter-row/body/footer layout
//! (`SelectorLayout`/`selector_list_layout`), and the mouse hit-testing that
//! maps a left-click onto a list row or the filter cursor
//! (`selector_click_target`/`apply_selector_left_click`/
//! `selector_position_filter_cursor`).
//!
//! Everything here is pure geometry — the renderers, the connection-layer
//! mouse handlers, and the viewport cache all consume the *same* functions,
//! so what is drawn is exactly what is clickable and exactly the height
//! navigation caches (mirroring the `chat_page_layout` pattern in the parent
//! `App`).  Lives in `state/` rather than `render/` so the connection-layer
//! handlers can use it without an import cycle.

use super::input::grapheme_offset_at_column;
use super::{AI_PROVIDER_ITEM_LINES, InputBuffer};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::widgets::{Block, Borders};

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

/// Compute a centered popup rect for a modal overlay from a [`PopupSize`].
/// The `.min(area…)` guards keep the arithmetic panic-free on tiny terminals
/// (clamp panics if its bounds are inverted).  Shared by the model selector
/// and the account/credential modals so every overlay uses the same centering
/// and clamping rules.  Lives here (state/layout.rs) so both the renderers and
/// the connection-layer mouse handlers can use it without an import cycle.
pub(crate) fn centered_popup(area: Rect, size: PopupSize) -> Rect {
    let width = ((area.width as u32 * size.w_num / size.w_den) as u16)
        .clamp(size.min_w, size.max_w)
        .min(area.width.saturating_sub(4))
        .max(1);
    let height = ((area.height as u32 * size.h_num / size.h_den) as u16)
        .clamp(size.min_h, size.max_h)
        .min(area.height.saturating_sub(2))
        .max(1);
    Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    }
}

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
        filter_row: chunks[0],
        body: chunks[1],
        footer: chunks[2],
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
    /// a PgUp/PgDn jump leaves it past the new max_scroll), and the raw value
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

// ── Full-page list click hit-testing (accounts + sessions) ────────────

/// The left content rect of a full-page list view — the bordered inner area
/// minus the trailing scrollbar column — derived from the terminal size the
/// renderers draw with.  Shared by the AI-providers accounts list and the
/// session-manager list, which both render the same `[Min(1), Length(1)]`
/// vertical split (page + status bar), a bordered `Block`, and a
/// `[Min(1), Length(1)]` horizontal split (list + scrollbar column).  The
/// click handlers hit-test against exactly this rect, so a click can never
/// land on a row the renderer did not draw.
///
/// `None` before the first frame (terminal size unknown), mirroring the
/// selector popups' `selector_click_target` — every click is then a no-op.
pub(crate) fn page_list_content_rect(last_terminal_size: Option<(u16, u16)>) -> Option<Rect> {
    let (width, height) = last_terminal_size?;
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(Rect {
            x: 0,
            y: 0,
            width,
            height,
        });
    let inner = Block::default().borders(Borders::ALL).inner(chunks[0]);
    let list = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(inner);
    Some(list[0])
}

/// Bounds-check a full-page-list `(column, row)` left-click against the list's
/// content rect and return `(content, local_row)`, where `local_row` is the
/// clicked row relative to `content.y`.  `None` when the click lands outside
/// the content rows — the block border, the status bar, or the scrollbar
/// column — where it is a no-op for row selection.  Shared by the AI-providers
/// accounts list and the session-manager list so both hit-tests agree on
/// precisely what is clickable.
fn list_content_local_row(
    last_terminal_size: Option<(u16, u16)>,
    column: u16,
    row: u16,
) -> Option<(Rect, usize)> {
    let content = page_list_content_rect(last_terminal_size)?;
    if column < content.x || column >= content.x + content.width {
        return None;
    }
    if row < content.y || row >= content.y + content.height {
        return None;
    }
    Some((content, (row - content.y) as usize))
}

/// Number of account items the AI-providers list renders starting at `scroll`,
/// for a content column `max_rows` tall.  Mirrors `render_ai_providers_list`'s
/// item loop exactly — each item is a 3-line block plus a blank separator on
/// non-last items while a row remains, and the renderer breaks out (without
/// drawing a partial tail) once the next 3-line block would overflow.  The
/// click hit-test clamps against this drawn set so a click in the blank band
/// below the drawn rows can never select an account that is not on screen.
fn ai_providers_drawn_count(scroll: usize, total_items: usize, max_rows: usize) -> usize {
    let mut lines = 0usize;
    let mut drawn = 0usize;
    for i in scroll..total_items {
        // The renderer always draws the first item (scroll) even if it can
        // only partially fit; later items break when a full block won't.
        if lines + 3 > max_rows && i != scroll {
            break;
        }
        lines += 3;
        drawn += 1;
        // A blank separator is appended after each non-last drawn item while
        // a row remains below it.
        if lines < max_rows && i + 1 < total_items {
            lines += 1;
        }
    }
    drawn
}

/// Map an AI-providers accounts-list left-click onto the account index to
/// select.  Each account is rendered as a fixed `AI_PROVIDER_ITEM_LINES`-row
/// block (name / provider / credential, plus a blank separator on non-last
/// items), so the clicked row's item is
/// `scroll + (row − content.y) / AI_PROVIDER_ITEM_LINES`.  The click is
/// resolved against the *stored* `scroll` — which is the renderer's drawn
/// start for the accounts list (there is no separate `window()` for it) — and
/// clamped to the items actually drawn from it (see
/// [`ai_providers_drawn_count`]), so it can never select an account the
/// renderer did not render.  Returns `None` when the click lands outside the
/// list's content rows (the block border, the status bar, the scrollbar
/// column) or past the drawn tail.
pub(crate) fn ai_providers_list_click_index(
    last_terminal_size: Option<(u16, u16)>,
    column: u16,
    row: u16,
    total_items: usize,
    scroll: usize,
) -> Option<usize> {
    let (content, local) = list_content_local_row(last_terminal_size, column, row)?;
    let drawn = ai_providers_drawn_count(scroll, total_items, content.height as usize);
    let idx = scroll.saturating_add(local / AI_PROVIDER_ITEM_LINES);
    // Clamp against the drawn tail; `drawn` is already ≤ `total_items − scroll`.
    (idx < scroll + drawn).then_some(idx)
}

/// Map a session-manager list left-click onto the session index to select.
///
/// The session list is a `Table` whose header occupies the first content row;
/// each session follows one per row.  The drawn window start is supplied by
/// the caller (`window().0` — resolved through the rendered window, never the
/// stored `scroll`, mirroring the picker click handlers: a reorder/resize can
/// leave the stored anchor stale and the renderer clamps it).  A click on the
/// header, the trailing status row, the scrollbar column, or past the last
/// session is a no-op (`None`).
pub(crate) fn session_list_click_index(
    last_terminal_size: Option<(u16, u16)>,
    column: u16,
    row: u16,
    total_items: usize,
    window_start: usize,
) -> Option<usize> {
    // The header occupies content row 0; session rows start one row below.
    let (_content, local) = list_content_local_row(last_terminal_size, column, row)?;
    if local == 0 {
        return None;
    }
    let idx = window_start.saturating_add(local - 1);
    (idx < total_items).then_some(idx)
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

    // ── page_list_content_rect / list click hit-testing ────────────────

    #[test]
    fn page_list_content_rect_unknown_terminal_size_is_none() {
        assert_eq!(page_list_content_rect(None), None);
    }

    #[test]
    fn page_list_content_rect_derives_inner_left_column() {
        // 100x40 -> bordered inner area (x=1,y=1, w=98,h=38) minus the
        // trailing scrollbar column -> list content at x=1 w=97.
        let rect = page_list_content_rect(Some((100, 40))).expect("geometry");
        assert_eq!(rect.x, 1, "left block border");
        assert_eq!(rect.y, 1, "top block border");
        assert_eq!(rect.width, 97, "inner width minus scrollbar column");
        assert_eq!(rect.height, 37, "inner height (status bar excluded");
    }

    #[test]
    fn ai_providers_list_click_maps_row_to_item_block() {
        let content = page_list_content_rect(Some((100, 40))).expect("geometry");
        // Two accounts; each is an AI_PROVIDER_ITEM_LINES (4)-row block.
        // Click the first account (content row 0) and the second (row 4).
        assert_eq!(
            ai_providers_list_click_index(Some((100, 40)), content.x + 2, content.y, 2, 0),
            Some(0),
            "first account's name row"
        );
        assert_eq!(
            ai_providers_list_click_index(Some((100, 40)), content.x + 2, content.y + 4, 2, 0),
            Some(1),
            "second account's name row"
        );
        // A click just below the last block is past the tail -> no-op.
        assert_eq!(
            ai_providers_list_click_index(Some((100, 40)), content.x + 2, content.y + 8, 2, 0),
            None
        );
    }

    #[test]
    fn ai_providers_list_click_applies_scroll_offset() {
        let content = page_list_content_rect(Some((100, 40))).expect("geometry");
        // scroll=1: content row 0 is account 1, row 4 is account 2.
        assert_eq!(
            ai_providers_list_click_index(Some((100, 40)), content.x + 2, content.y, 4, 1),
            Some(1)
        );
        assert_eq!(
            ai_providers_list_click_index(Some((100, 40)), content.x + 2, content.y + 4, 4, 1),
            Some(2)
        );
    }

    #[test]
    fn ai_providers_list_click_outside_rows_is_noop() {
        let content = page_list_content_rect(Some((100, 40))).expect("geometry");
        // Top block border (row above content) and the scrollbar column.
        assert_eq!(
            ai_providers_list_click_index(Some((100, 40)), content.x + 2, content.y - 1, 2, 0),
            None,
            "top border"
        );
        assert_eq!(
            ai_providers_list_click_index(
                Some((100, 40)),
                content.x + content.width,
                content.y,
                2,
                0
            ),
            None,
            "scrollbar column"
        );
        assert_eq!(
            ai_providers_list_click_index(None, content.x + 2, content.y, 2, 0),
            None,
            "before the first frame"
        );
    }

    #[test]
    fn ai_providers_list_click_clamps_to_drawn_tail() {
        // A short page (content height 9 in a 20x12 terminal) draws only the
        // first two 4-row account blocks (rows 0-7); the remaining accounts are
        // off-screen.  A click in the blank band below the drawn tail must be a
        // no-op rather than selecting the second account's still-undrawn block.
        let content = page_list_content_rect(Some((20, 12))).expect("geometry");
        assert_eq!(content.height, 9, "content height for a 20x12 terminal");
        // Four accounts, only two drawn: rows 4 and 7 select index 1 (drawn),
        // row 8 is below the tail -> no-op, and row 9 is past the content.
        assert_eq!(
            ai_providers_list_click_index(Some((20, 12)), content.x + 2, content.y + 4, 4, 0),
            Some(1)
        );
        assert_eq!(
            ai_providers_list_click_index(Some((20, 12)), content.x + 2, content.y + 7, 4, 0),
            Some(1)
        );
        assert_eq!(
            ai_providers_list_click_index(Some((20, 12)), content.x + 2, content.y + 8, 4, 0),
            None,
            "a click below the drawn tail must not select an undrawn account"
        );
        assert_eq!(
            ai_providers_list_click_index(Some((20, 12)), content.x + 2, content.y + 9, 4, 0),
            None
        );
    }

    #[test]
    fn session_list_click_skips_header_and_applies_window_start() {
        let content = page_list_content_rect(Some((100, 40))).expect("geometry");
        // Header at content.y; session rows start below.  window_start=0:
        // content row 1 -> index 0, row 2 -> index 1.
        assert_eq!(
            session_list_click_index(Some((100, 40)), content.x + 2, content.y + 1, 4, 0),
            Some(0)
        );
        assert_eq!(
            session_list_click_index(Some((100, 40)), content.x + 2, content.y + 2, 4, 0),
            Some(1)
        );
        // A scrolled window start shifts the mapping.
        assert_eq!(
            session_list_click_index(Some((100, 40)), content.x + 2, content.y + 2, 6, 3),
            Some(4)
        );
    }

    #[test]
    fn session_list_click_header_and_past_tail_are_noop() {
        let content = page_list_content_rect(Some((100, 40))).expect("geometry");
        // Click on the header row -> no session.
        assert_eq!(
            session_list_click_index(Some((100, 40)), content.x + 2, content.y, 4, 0),
            None,
            "header row"
        );
        // Click below the visible tail (few sessions) -> no-op.
        assert_eq!(
            session_list_click_index(Some((100, 40)), content.x + 2, content.y + 5, 3, 0),
            None
        );
        // Top border and scrollbar column -> no-op.
        assert_eq!(
            session_list_click_index(Some((100, 40)), content.x + 2, content.y - 1, 4, 0),
            None
        );
        assert_eq!(
            session_list_click_index(
                Some((100, 40)),
                content.x + content.width,
                content.y + 1,
                4,
                0
            ),
            None
        );
    }
}
