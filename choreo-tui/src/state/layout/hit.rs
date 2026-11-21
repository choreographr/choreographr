//! Full-page list click hit-testing (the AI-providers accounts list and the
//! session-manager list).
//!
//! Both pages render the same frame — a page chunk plus a status bar, a bordered
//! block, then a list column plus a scrollbar column — so both derive their
//! clickable rows from one rect ([`page_list_content_rect`]) and one bounds
//! check, which is what keeps a click from landing on a row the renderer never
//! drew.  Each list then differs only in its row→item mapping: the accounts list
//! divides the clicked row by the fixed item height and clamps to the blocks
//! actually drawn, while the session list skips the table header and offsets by
//! the rendered window start.
//!
//! Re-exported from `state` (`pub(crate) use layout::*`), so the page click
//! handlers and the crate's test suite keep reaching these through
//! `crate::state`.

use super::super::AI_PROVIDER_ITEM_LINES;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::widgets::{Block, Borders};

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
    // Layout::vertical([Min(1), Length(1)]) always yields exactly 2 chunks.
    let page = chunks.first().copied().unwrap_or_default();
    let inner = Block::default().borders(Borders::ALL).inner(page);
    let list = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(inner);
    // The first chunk always exists in a 2-constraint split.
    Some(list.first().copied().unwrap_or_default())
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
