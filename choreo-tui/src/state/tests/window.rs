//! Selection-driven window scrolling and page-jump paging.

use super::make_session;
use crate::state::{PAGE_SCROLL_LINES, SessionManagerState};

// ── window (selection-driven scroll) ──

/// Assert the highlighted row always lies inside the returned window.
fn assert_selection_in_window(mgr: &SessionManagerState, height: usize) {
    let (start, count) = mgr.window(height);
    if mgr.sessions.is_empty() {
        assert_eq!((start, count), (0, 0));
        return;
    }
    assert!(count > 0, "non-empty list must yield rows");
    assert_eq!(start + count, mgr.sessions.len().min(start + height));
    if let Some(sel) = mgr.selection {
        assert!(
            (start..start + count).contains(&sel),
            "selection {sel} outside window {start}..{}",
            start + count
        );
    }
}

#[test]
fn window_empty_and_zero_height() {
    let mut mgr = SessionManagerState::new();
    assert_eq!(mgr.window(10), (0, 0), "empty list");
    mgr.set_sessions(vec![make_session(1, "a")]);
    assert_eq!(mgr.window(0), (0, 0), "zero height");
}

#[test]
fn window_does_not_scroll_down_until_selection_reaches_bottom_edge() {
    let mut mgr = SessionManagerState::new();
    let sessions: Vec<_> = (1..=30).map(|id| make_session(id, "s")).collect();
    mgr.set_sessions(sessions);
    let height = 10;
    mgr.viewport_height = height;

    // The first `height - 1` presses move the selection through the
    // visible window without scrolling it: the window stays at 0 with
    // the selection pinned to the bottom edge.
    for _ in 0..height - 1 {
        mgr.select_down();
    }
    assert_eq!(mgr.selection, Some(9));
    assert_eq!(mgr.window(height), (0, 10), "window must not move yet");

    // One more press pushes the selection past the bottom edge, so the
    // window scrolls down by exactly one row to keep it visible.
    mgr.select_down();
    assert_eq!(mgr.selection, Some(10));
    assert_eq!(mgr.window(height), (1, 10));
    assert_selection_in_window(&mgr, height);
}

#[test]
fn window_does_not_scroll_up_immediately_after_scrolling_down() {
    // Regression: after scrolling to the bottom, pressing up must move
    // the highlight through the visible rows — the window may only
    // scroll back up once the selection reaches the top edge.
    let mut mgr = SessionManagerState::new();
    let sessions: Vec<_> = (1..=30).map(|id| make_session(id, "s")).collect();
    mgr.set_sessions(sessions);
    let height = 10;
    mgr.viewport_height = height;

    // Scroll to the bottom: selection 29, window rows 20..29.
    for _ in 0..29 {
        mgr.select_down();
    }
    assert_eq!(mgr.selection, Some(29));
    assert_eq!(mgr.window(height), (20, 10));

    // The first nine presses up climb the selection from 29 to 20 (the
    // top edge of the window) without moving the window.
    for _ in 0..9 {
        mgr.select_up();
    }
    assert_eq!(mgr.selection, Some(20));
    assert_eq!(
        mgr.window(height),
        (20, 10),
        "window must stay fixed while the selection climbs"
    );
    assert_selection_in_window(&mgr, height);

    // One more press up leaves the top edge, so the window scrolls up.
    mgr.select_up();
    assert_eq!(mgr.selection, Some(19));
    assert_eq!(mgr.window(height), (19, 10));
    assert_selection_in_window(&mgr, height);
}

#[test]
fn window_scrolls_down_again_from_top_of_scrolled_window() {
    // After scrolling up so the selection sits at the top edge of the
    // window, pressing down must move the highlight down within the
    // window — not scroll it back down immediately.
    let mut mgr = SessionManagerState::new();
    let sessions: Vec<_> = (1..=30).map(|id| make_session(id, "s")).collect();
    mgr.set_sessions(sessions);
    let height = 10;
    mgr.viewport_height = height;

    // Scroll to the bottom then back up one: selection 28, window
    // rows 20..29.
    for _ in 0..29 {
        mgr.select_down();
    }
    mgr.select_up();
    assert_eq!(mgr.selection, Some(28));
    assert_eq!(mgr.window(height), (20, 10));

    // Pressing down moves the selection within the window without
    // scrolling it.
    mgr.select_down();
    assert_eq!(mgr.selection, Some(29));
    assert_eq!(mgr.window(height), (20, 10));
}

#[test]
fn window_reanchors_after_stale_scroll() {
    // A reorder/removal can leave `scroll` pointing below the new
    // selection.  The window must clamp back up so the selection stays
    // visible, and the next navigation step re-anchors from that.
    let mut mgr = SessionManagerState::new();
    mgr.set_sessions(vec![make_session(1, "a"), make_session(2, "b")]);
    mgr.viewport_height = 2;
    mgr.selection = Some(1);
    mgr.scroll = 5; // stale anchor below the selection
    assert_eq!(mgr.window(2), (0, 2), "window clamps up to keep selection");

    // Navigation re-anchors before shifting, so the next move up works
    // from the displayed window instead of the stale anchor.
    mgr.select_up();
    assert_eq!(mgr.selection, Some(0));
    assert_eq!(mgr.scroll, 0);
    assert_eq!(mgr.window(2), (0, 2));
}

#[test]
fn window_follows_selection_on_reorder_and_removal() {
    // After a removal the selection clamps and the window re-derives
    // from the (new) selection instead of showing stale rows.
    let mut mgr = SessionManagerState::new();
    mgr.set_sessions(vec![make_session(1, "a"), make_session(2, "b")]);
    mgr.viewport_height = 1;
    mgr.select_down();
    mgr.remove_session(1);
    assert_selection_in_window(&mgr, 1);
    assert_eq!(mgr.selection, Some(0));
    assert_eq!(mgr.window(1), (0, 1));
}

// ── paging the selection ──

#[test]
fn page_up_down_moves_selection_and_keeps_it_in_window() {
    let mut mgr = SessionManagerState::new();
    let sessions: Vec<_> = (1..=20).map(|id| make_session(id, "s")).collect();
    mgr.set_sessions(sessions);
    mgr.viewport_height = 10;

    mgr.scroll_down_page();
    assert_eq!(mgr.selection, Some(PAGE_SCROLL_LINES));
    assert_selection_in_window(&mgr, 10);

    mgr.scroll_down_page();
    assert_eq!(mgr.selection, Some(PAGE_SCROLL_LINES * 2));
    assert_selection_in_window(&mgr, 10);

    // Paging past the end clamps to the last row.
    for _ in 0..10 {
        mgr.scroll_down_page();
    }
    assert_eq!(mgr.selection, Some(19));
    assert_selection_in_window(&mgr, 10);

    mgr.scroll_up_page();
    assert_eq!(mgr.selection, Some(16));
    assert_selection_in_window(&mgr, 10);

    // Paging up past the top clamps to row 0.
    for _ in 0..10 {
        mgr.scroll_up_page();
    }
    assert_eq!(mgr.selection, Some(0));
    assert_selection_in_window(&mgr, 10);
}

#[test]
fn paging_with_no_selection_is_a_noop() {
    let mut mgr = SessionManagerState::new();
    mgr.scroll_up_page();
    mgr.scroll_down_page();
    assert_eq!(mgr.selection, None);
}
