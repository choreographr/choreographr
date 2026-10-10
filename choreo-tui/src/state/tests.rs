//! Unit tests for `state` (moved out of `mod.rs`).

use super::*;
use crate::markdown_render::{LineChrome, LineJoin, render_turn_lines};
use crate::test_util::test_app;
use choreo_proto::ToolResultRecord;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Size;
use ratatui::text::Line;
use std::sync::Arc;

fn make_session(id: u64, title: &str) -> SessionSummary {
    SessionSummary {
        session_id: id,
        title: Some(title.into()),
        selected_model: None,
        reasoning_effort: None,
        parent_session_id: None,
        working_dir: None,
        created_at: 1000,
        // Decreasing with id so the session manager's sort keeps the
        // fixtures in ascending-id order (the order these tests assume);
        // the value stays small so an explicit `handle_session_status_changed`
        // timestamp still overrides it in the monotonicity test.
        last_modified: 1000 - id.cast_signed(),
        turn_count: 0,
        status: SessionStatus::Inactive,
        active_tool_groups: vec!["core".into()],
        account_name: None,
        token_usage: None,
        context_window: None,
        last_prompt_tokens: None,
        pinned: false,
        archived_at: None,
    }
}

fn make_detail_data(session_id: u64) -> SessionDetailData {
    SessionDetailData {
        session_id,
        title: String::new(),
        selected_model: String::new(),
        reasoning_effort: None,
        parent_session_id: None,
        working_dir: String::new(),
        created_at: 0,
        last_modified: 0,
        turn_count: 0,
        status: SessionStatus::Inactive,
        active_tool_groups: vec![],
        account_name: None,
        accumulated_usage: None,
        context_window: None,
        last_prompt_tokens: None,
        pinned: false,
        archived_at: None,
    }
}

// ── last_modified ordering ──

#[test]
fn input_buffer_drops_nul_and_keeps_newline() {
    // crossterm 0.29 parses kitty-protocol IME "text events"
    // (`CSI 0;;<codepoints>u`) as Char('\0') with the composed text
    // dropped.  The guard in handle_key must refuse to insert the NUL
    // while still accepting a literal newline (legacy Ctrl+J = 0x0A).
    let mut buf = InputBuffer::new();
    assert!(!buf.handle_key(KeyEvent::new(KeyCode::Char('\0'), KeyModifiers::NONE)));
    assert!(buf.handle_key(KeyEvent::new(KeyCode::Char('\n'), KeyModifiers::NONE)));
    assert_eq!(buf.text, "\n");
}

#[test]
fn input_buffer_ctrl_backspace_clears_whole_buffer_from_any_cursor() {
    // Ctrl+Backspace empties the draft prompt outright, independent of
    // the cursor position — unlike Ctrl+U, which keeps the tail after
    // the cursor, and unlike plain Backspace, which deletes one grapheme.
    let mut buf = InputBuffer::new();
    buf.text = "hello world".to_string();
    // Cursor parked mid-text: clearing must not leave the trailing "world".
    buf.cursor = 6;
    buf.generation = 7;
    buf.scroll_offset = 3;

    let consumed = buf.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::CONTROL));

    assert!(consumed, "ctrl+backspace must be consumed");
    assert!(buf.text.is_empty(), "the whole draft must be cleared");
    assert_eq!(buf.cursor, 0);
    assert_eq!(buf.scroll_offset, 0, "clear resets the visible window");
    assert_ne!(buf.generation, 7, "clear must invalidate the lines cache");
}

#[test]
fn set_sessions_orders_by_last_modified_desc() {
    let mut mgr = SessionManagerState::new();
    let mut old = make_session(1, "old");
    old.last_modified = 1000;
    let mut newest = make_session(2, "newest");
    newest.last_modified = 9000;
    let mut middle = make_session(3, "middle");
    middle.last_modified = 5000;
    // Deliberately unsorted input: the list must come back newest-first.
    mgr.set_sessions(vec![old, middle, newest]);
    let titles: Vec<&str> = mgr
        .sessions
        .iter()
        .map(|s| s.title.as_deref().unwrap())
        .collect();
    assert_eq!(titles, vec!["newest", "middle", "old"]);
}

#[test]
fn set_sessions_equal_timestamps_break_ties_by_id_desc() {
    // Equal last_modified values are tiebroken by session_id DESCENDING
    // (matching the daemon's own ordering), independent of input order.
    let mut mgr = SessionManagerState::new();
    let mut a = make_session(1, "a");
    let mut b = make_session(2, "b");
    a.last_modified = 5000;
    b.last_modified = 5000;
    mgr.set_sessions(vec![a, b]);
    let ids: Vec<u64> = mgr.sessions.iter().map(|s| s.session_id).collect();
    assert_eq!(ids, vec![2, 1]);
}

#[test]
fn set_sessions_keeps_existing_selection_across_refresh() {
    // A plain refresh (no `select_session`) must keep the current
    // selection pinned to the same session even when it moves index.
    let mut mgr = SessionManagerState::new();
    mgr.set_sessions(vec![make_session(1, "a"), make_session(2, "b")]);
    mgr.select_down();
    assert_eq!(mgr.selection, Some(1));
    // Session 2 jumps to the top (newer last_modified); the cursor follows
    // it by id even though its index changed.
    let mut refreshed = make_session(2, "b");
    refreshed.last_modified = 9999;
    mgr.set_sessions(vec![refreshed, make_session(1, "a")]);
    assert_eq!(mgr.selection, Some(0), "session 2 moved to index 0");
    assert_eq!(mgr.sessions[mgr.selection.unwrap()].session_id, 2);
}

#[test]
fn select_session_highlights_existing_session_immediately() {
    let mut mgr = SessionManagerState::new();
    mgr.set_sessions(vec![make_session(1, "a"), make_session(2, "b")]);
    mgr.select_session(2);
    assert_eq!(mgr.selection, Some(1));
    assert_eq!(mgr.sessions[mgr.selection.unwrap()].session_id, 2);
    // The preference is remembered for the next refresh too.
    assert_eq!(mgr.pending_select, Some(2));
}

#[test]
fn select_session_lands_on_session_once_list_arrives() {
    // First visit: the list hasn't been loaded yet, so the selection
    // stays unset until the ListSessions reply populates the list.
    let mut mgr = SessionManagerState::new();
    mgr.select_session(2);
    assert_eq!(mgr.selection, None);
    mgr.set_sessions(vec![make_session(1, "a"), make_session(2, "b")]);
    assert_eq!(mgr.selection, Some(1));
    assert_eq!(mgr.sessions[mgr.selection.unwrap()].session_id, 2);
    // The one-shot preference is consumed after the first refresh.
    assert_eq!(mgr.pending_select, None);
}

#[test]
fn select_session_wins_over_previous_selection() {
    // The user viewed session 1, but the session they were just looking
    // at before Alt+S is session 2: the pending highlight must win.
    let mut mgr = SessionManagerState::new();
    mgr.set_sessions(vec![make_session(1, "a"), make_session(2, "b")]);
    assert_eq!(mgr.selection, Some(0)); // default: first row
    mgr.select_session(2);
    mgr.set_sessions(vec![make_session(1, "a"), make_session(2, "b")]);
    assert_eq!(mgr.selection, Some(1));
    assert_eq!(mgr.sessions[mgr.selection.unwrap()].session_id, 2);
}

#[test]
fn pending_select_does_not_stick_across_later_refreshes() {
    // Once consumed, later refreshes must preserve the user's navigation
    // instead of re-applying an old highlight.
    let mut mgr = SessionManagerState::new();
    mgr.select_session(2);
    mgr.set_sessions(vec![make_session(1, "a"), make_session(2, "b")]);
    assert_eq!(mgr.selection, Some(1));
    mgr.select_up(); // user navigates back up to session 1
    mgr.set_sessions(vec![make_session(1, "a"), make_session(2, "b")]);
    assert_eq!(mgr.selection, Some(0));
}

#[test]
fn select_session_falls_back_to_first_row_when_missing() {
    // The attached session is not in the (possibly stale) list: fall
    // back to the first row rather than leaving the selection unset.
    let mut mgr = SessionManagerState::new();
    mgr.select_session(99);
    mgr.set_sessions(vec![make_session(1, "a"), make_session(2, "b")]);
    assert_eq!(mgr.selection, Some(0));
}

#[test]
fn select_session_switches_to_archived_view_when_target_is_archived() {
    // Opening the manager while attached to an ARCHIVED session must land
    // the highlight on it — it lives in the archived partition — instead
    // of falling onto an unrelated first row of the default live list.
    let mut mgr = SessionManagerState::new();
    let mut archived = make_session(2, "archived");
    archived.archived_at = Some(1_705_314_000_500);
    mgr.select_session(2);
    mgr.set_sessions(vec![make_session(1, "live"), archived]);
    assert_eq!(mgr.view, SessionManagerView::Archived);
    assert_eq!(mgr.selection, Some(0));
    assert_eq!(mgr.sessions[mgr.selection.unwrap()].session_id, 2);
}

// ── pinned/archived view model ──

#[test]
fn toggle_view_partitions_list_and_archived() {
    let mut mgr = SessionManagerState::new();
    let mut archived = make_session(2, "archived");
    archived.archived_at = Some(1_705_314_000_500);
    mgr.set_sessions(vec![make_session(1, "live"), archived]);
    // Default view is the live list: only non-archived sessions.
    assert_eq!(mgr.view, SessionManagerView::List);
    assert_eq!(
        mgr.sessions
            .iter()
            .map(|s| s.session_id)
            .collect::<Vec<_>>(),
        vec![1]
    );
    // Tab switches to the archived list.
    mgr.toggle_view();
    assert_eq!(mgr.view, SessionManagerView::Archived);
    assert_eq!(
        mgr.sessions
            .iter()
            .map(|s| s.session_id)
            .collect::<Vec<_>>(),
        vec![2]
    );
    assert_eq!(mgr.selection, Some(0));
    assert_eq!(mgr.scroll, 0);
    // Tab back returns to the live list.
    mgr.toggle_view();
    assert_eq!(mgr.view, SessionManagerView::List);
    assert_eq!(
        mgr.sessions
            .iter()
            .map(|s| s.session_id)
            .collect::<Vec<_>>(),
        vec![1]
    );
}

#[test]
#[expect(clippy::assert_is_empty)] // clearer than assert_eq! against []
fn toggle_view_empty_clears_selection() {
    let mut mgr = SessionManagerState::new();
    mgr.set_sessions(vec![make_session(1, "live")]);
    mgr.toggle_view();
    assert_eq!(mgr.view, SessionManagerView::Archived);
    assert!(mgr.sessions.is_empty());
    assert_eq!(mgr.selection, None);
}

#[test]
fn pinned_sorts_first_in_both_views() {
    let mut mgr = SessionManagerState::new();
    // Fixtures sort ascending by id; pin the LAST row so it must float to
    // the top of the live view.
    let mut pinned = make_session(3, "pinned");
    pinned.pinned = true;
    mgr.set_sessions(vec![make_session(1, "a"), make_session(2, "b"), pinned]);
    assert_eq!(
        mgr.sessions
            .iter()
            .map(|s| s.session_id)
            .collect::<Vec<_>>(),
        vec![3, 1, 2]
    );
    // In the archived view, pinning still wins over recency.
    let mut archived_pinned = make_session(5, "archived-pinned");
    archived_pinned.pinned = true;
    archived_pinned.archived_at = Some(1_705_314_000_500);
    let mut archived = make_session(4, "archived");
    archived.archived_at = Some(1_705_314_000_600);
    mgr.set_sessions(vec![archived, archived_pinned]);
    mgr.toggle_view();
    assert_eq!(
        mgr.sessions
            .iter()
            .map(|s| s.session_id)
            .collect::<Vec<_>>(),
        vec![5, 4]
    );
}

#[test]
fn apply_session_flags_repartitions_and_preserves_selection() {
    let mut mgr = SessionManagerState::new();
    mgr.set_sessions(vec![
        make_session(1, "a"),
        make_session(2, "b"),
        make_session(3, "c"),
    ]);
    mgr.selection = Some(1);
    assert_eq!(mgr.sessions[1].session_id, 2);
    // Pin session 2: it stays in the live view and floats to the top, and
    // the cursor follows it by id.
    mgr.apply_session_flags(2, true, None);
    assert_eq!(mgr.sessions[0].session_id, 2);
    assert!(mgr.sessions[0].pinned);
    assert_eq!(mgr.selection, Some(0));
}

#[test]
fn apply_session_flags_updates_open_detail_view() {
    // The detail view renders its own `detail_data` snapshot, not
    // `sessions`, so a flag change must update it too or the Detail page
    // shows stale Pinned/Archived values.
    let mut mgr = SessionManagerState::new();
    mgr.set_sessions(vec![make_session(1, "a")]);
    mgr.enter_detail();
    assert_eq!(mgr.view, SessionManagerView::Detail);
    assert!(!mgr.detail_data.as_ref().unwrap().pinned);

    mgr.apply_session_flags(1, true, Some(1_705_314_000_500));
    let detail = mgr.detail_data.as_ref().unwrap();
    assert!(detail.pinned);
    assert_eq!(detail.archived_at, Some(1_705_314_000_500));
}

#[test]
fn archiving_highlighted_session_moves_selection_to_neighbour() {
    let mut mgr = SessionManagerState::new();
    mgr.set_sessions(vec![
        make_session(1, "a"),
        make_session(2, "b"),
        make_session(3, "c"),
    ]);
    mgr.selection = Some(1); // session 2
    // Archive the highlighted session: it leaves the live view, so the
    // cursor clamps to the neighbour now occupying its row (session 3).
    mgr.apply_session_flags(2, false, Some(1_705_314_000_500));
    assert_eq!(
        mgr.sessions
            .iter()
            .map(|s| s.session_id)
            .collect::<Vec<_>>(),
        vec![1, 3]
    );
    assert_eq!(mgr.selection, Some(1));
    assert_eq!(mgr.sessions[1].session_id, 3);
    // The archived session appears in the archived view.
    mgr.toggle_view();
    assert_eq!(
        mgr.sessions
            .iter()
            .map(|s| s.session_id)
            .collect::<Vec<_>>(),
        vec![2]
    );
}

#[test]
fn status_change_reorders_only_when_timestamp_advances() {
    let mut app = test_app();
    app.session_mgr
        .set_sessions(vec![make_session(1, "a"), make_session(2, "b")]);
    // The fixture timestamps decrease with id, so the list order is
    // ascending by id (session 1 first); move the cursor onto session 2
    // (index 1).
    app.session_mgr.select_down();
    assert_eq!(app.session_mgr.selection, Some(1));

    // A pure status transition (Inference at request start) carries the
    // session's *current* last_modified — the daemon no longer bumps the
    // timestamp for internal pipeline churn, so the list must NOT re-sort.
    let ts = app.session_mgr.sessions[1].last_modified;
    app.handle_session_status_changed(2, &SessionStatus::Inference, ts);
    assert_eq!(
        app.session_mgr.sessions[0].session_id, 1,
        "no reorder on status-only change"
    );
    assert_eq!(
        app.session_mgr.sessions[1].status,
        SessionStatus::Inference,
        "status still updates without reordering"
    );
    assert_eq!(app.session_mgr.selection, Some(1));

    // Only when the timestamp actually advances (a request completed) does
    // the session jump to the top, with the cursor following it.
    app.handle_session_status_changed(2, &SessionStatus::Inactive, ts + 1000);
    assert_eq!(
        app.session_mgr.sessions[0].session_id, 2,
        "completed session re-sorted to top"
    );
    assert_eq!(app.session_mgr.sessions[0].status, SessionStatus::Inactive);
    assert_eq!(
        app.session_mgr.selection,
        Some(0),
        "cursor follows the session"
    );
}

#[test]
fn status_change_timestamp_is_monotonic() {
    // Duplicate/out-of-order deliveries must never regress last_modified.
    let mut app = test_app();
    app.session_mgr.set_sessions(vec![make_session(1, "a")]);
    app.handle_session_status_changed(1, &SessionStatus::Inference, 9000);
    app.handle_session_status_changed(1, &SessionStatus::Inference, 5000);
    assert_eq!(app.session_mgr.sessions[0].last_modified, 9000);
}

// ── remove_session ──

#[test]
fn remove_session_removes_from_list() {
    let mut mgr = SessionManagerState::new();
    mgr.set_sessions(vec![make_session(1, "a"), make_session(2, "b")]);
    mgr.selection = Some(0);
    mgr.remove_session(1);
    assert_eq!(mgr.sessions.len(), 1);
    assert_eq!(mgr.sessions[0].session_id, 2);
}

#[test]
fn remove_session_nonexistent_is_noop() {
    let mut mgr = SessionManagerState::new();
    mgr.set_sessions(vec![make_session(1, "a")]);
    mgr.selection = Some(0);
    mgr.remove_session(999);
    assert_eq!(mgr.sessions.len(), 1);
    assert_eq!(mgr.selection, Some(0));
}

#[test]
#[expect(clippy::assert_is_empty)] // clearer than assert_eq! against []
fn remove_session_last_item_clears_selection() {
    let mut mgr = SessionManagerState::new();
    mgr.set_sessions(vec![make_session(1, "a")]);
    mgr.selection = Some(0);
    mgr.remove_session(1);
    assert!(mgr.sessions.is_empty());
    assert_eq!(mgr.selection, None);
}

#[test]
fn remove_session_clamps_selection_to_new_len() {
    let mut mgr = SessionManagerState::new();
    mgr.set_sessions(vec![make_session(1, "a"), make_session(2, "b")]);
    mgr.selection = Some(1);
    mgr.remove_session(2);
    assert_eq!(mgr.sessions.len(), 1);
    assert_eq!(mgr.selection, Some(0));
}

#[test]
fn remove_session_clears_detail_view_for_deleted_session() {
    let mut mgr = SessionManagerState::new();
    mgr.set_sessions(vec![make_session(1, "a"), make_session(2, "b")]);
    mgr.view = SessionManagerView::Detail;
    mgr.detail_data = Some(make_detail_data(1));
    mgr.remove_session(1);
    assert_eq!(mgr.view, SessionManagerView::List);
    assert!(mgr.detail_data.is_none());
}

#[test]
fn remove_session_clears_confirmation_for_deleted_session() {
    let mut mgr = SessionManagerState::new();
    mgr.set_sessions(vec![make_session(1, "a")]);
    mgr.confirm_delete = Some((1, "a".into()));
    mgr.remove_session(1);
    assert!(mgr.confirm_delete.is_none());
}

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

// ── scroll_to_content_line ──

#[test]
fn scroll_to_content_line_scrolls_to_content_line() {
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 5;

    for i in 0..5u32 {
        let turn = Turn {
            created_at: choreo_proto::TimestampMs::now(),
            undone: false,
            error: None,
            user_text: Some(format!("user text {i}")),
            assistant_text: Some(format!("assistant text {i}")),
            assistant_reasoning: None,
            tool_calls: vec![],
            token_usage: None,
            tool_results: vec![],
            displayed_images: vec![],
            reasoning_artifact: None,
            reasoning_producer: None,
        };
        let display = app.active_display().unwrap();
        display.view.insert_or_replace(i, turn);
    }
    app.rebuild_height_prefix();

    app.scroll_to_content_line(0);
    assert_eq!(app.effective_scroll(), app.max_scroll_offset());
}

// ── find_turn_at_row ──

#[test]
fn find_turn_at_row_returns_none_out_of_bounds() {
    let app = test_app();
    assert!(find_turn_at_row(&app, 999).is_none());
}

#[test]
fn find_turn_at_row_returns_turn_idx_and_offset() {
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 200;
    let turn = Turn {
        created_at: choreo_proto::TimestampMs::now(),
        undone: false,
        error: None,
        user_text: Some("hello".into()),
        assistant_text: Some("world".into()),
        assistant_reasoning: None,
        tool_calls: vec![],
        token_usage: None,
        tool_results: vec![],
        displayed_images: vec![],
        reasoning_artifact: None,
        reasoning_producer: None,
    };
    let display = app.active_display().unwrap();
    display.view.insert_or_replace(1, turn);
    app.rebuild_height_prefix();

    // The history is shorter than the viewport, so content is anchored to
    // the bottom: content line 0 sits at screen row `vh - total`.
    let total = app.active_display().unwrap().total_history_height();
    #[expect(clippy::cast_possible_truncation)] // height-derived values fit u16
    let first_row = (app.history_viewport.height as usize - total) as u16;
    let (turn_idx, offset) = find_turn_at_row(&app, first_row).unwrap();
    assert_eq!(turn_idx, 0);
    assert_eq!(offset, 0);

    // Rows above the content are blank and must not map to a turn.
    assert!(find_turn_at_row(&app, first_row.saturating_sub(1)).is_none());
}

#[test]
fn find_turn_at_row_scrolled_history_maps_rows_correctly() {
    // Tall session with a scrollbar: scroll away from the bottom and
    // verify the mapping agrees with `render_history`'s bottom-up draw
    // order (content line `c` sits at screen row `vh - total + scroll + c`).
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 10;
    for i in 0..8 {
        let turn = Turn {
            created_at: choreo_proto::TimestampMs::now(),
            undone: false,
            error: None,
            user_text: Some(format!("user {i}")),
            assistant_text: Some(format!("assistant {i}")),
            assistant_reasoning: None,
            tool_calls: vec![],
            token_usage: None,
            tool_results: vec![],
            displayed_images: vec![],
            reasoning_artifact: None,
            reasoning_producer: None,
        };
        app.active_display()
            .unwrap()
            .view
            .insert_or_replace(i, turn);
    }
    app.rebuild_height_prefix();

    let total = app.active_display().unwrap().total_history_height();
    let vh = app.history_viewport.height as usize;
    assert!(
        total > vh,
        "test requires content taller than the viewport (scrollbar present)"
    );

    // Scroll partway up: max_scroll = total - vh.
    let scroll = (total - vh) / 2;
    app.scroll_to(scroll);
    assert_eq!(app.effective_scroll(), scroll);

    // The topmost visible content line is `total - scroll - vh`; the
    // bottom row of the viewport shows content line `total - scroll - 1`.
    let top_line = total - scroll - vh;
    let (idx, offset) = find_turn_at_row(&app, 0).expect("top row must map to a turn");
    assert_eq!(offset, top_line - turn_start(&app, idx));

    #[expect(clippy::cast_possible_truncation)] // viewport row fits u16
    let bottom_row = (vh - 1) as u16;
    let (idx_b, offset_b) = find_turn_at_row(&app, bottom_row).expect("bottom row must map");
    assert_eq!(
        offset_b,
        total - scroll - 1 - turn_start(&app, idx_b),
        "bottom row must map to the last visible content line"
    );
}

/// Content line where the turn at `turn_idx` starts (`height_prefix`
/// prefix-sum entry, 0 for the first turn).
fn turn_start(app: &App, turn_idx: usize) -> usize {
    app.active_display_ref()
        .and_then(|d| {
            turn_idx
                .checked_sub(1)
                .and_then(|prev| d.height_prefix.get(prev))
        })
        .copied()
        .unwrap_or(0)
}

#[test]
fn find_turn_at_row_short_history_anchors_content_at_bottom() {
    // Regression: when the history is shorter than the viewport (no
    // scrollbar shown), the renderer anchors the content at the bottom of
    // the viewport, but the click mapping assumed content always starts at
    // screen row 0.  The reasoning header (and image clicks) therefore
    // couldn't be hit on short sessions.
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 20;
    let turn = Turn {
        created_at: choreo_proto::TimestampMs::now(),
        undone: false,
        error: None,
        user_text: None,
        assistant_text: Some("Response text.".into()),
        assistant_reasoning: Some("Hidden thinking.".into()),
        tool_calls: vec![],
        token_usage: None,
        tool_results: vec![],
        displayed_images: vec![],
        reasoning_artifact: None,
        reasoning_producer: None,
    };
    app.active_display()
        .unwrap()
        .view
        .insert_or_replace(1, turn);
    app.rebuild_height_prefix();

    let (start, total) = {
        let display = app.active_display().unwrap();
        let (start, _end) = display.turn_layouts[0]
            .reasoning_header_range
            .expect("reasoning header range should exist");
        (start, display.total_history_height())
    };
    assert!(
        total < app.history_viewport.height as usize,
        "test requires a session too short to need the scrollbar"
    );

    // The header is drawn at screen row `vh - total + start` (bottom
    // anchored); clicking that row must resolve to the header's content
    // line `start`.
    #[expect(clippy::cast_possible_truncation)] // viewport row fits u16
    let screen_row = (app.history_viewport.height as usize - total + start) as u16;
    let (turn_idx, offset) = find_turn_at_row(&app, screen_row).expect("row must map to a turn");
    assert_eq!(turn_idx, 0);
    assert_eq!(offset, start);

    // The blank band above the content must not map to any turn.
    #[expect(clippy::cast_possible_truncation)] // viewport row fits u16
    let blank_row = (app.history_viewport.height as usize - total - 1) as u16;
    assert!(
        find_turn_at_row(&app, blank_row).is_none(),
        "empty rows above the content must not hit a turn"
    );
}

// ── text selection lifecycle ──

#[test]
fn reset_for_session_switch_clears_text_selection() {
    // A selection is stored in screen coordinates keyed to the previous
    // session's rendered content; switching sessions must clear it so a
    // stale rectangle can never highlight another session's history.
    let mut app = test_app();
    app.text_selection = Some(crate::selection::TextSelection {
        anchor: (0, 0),
        head: (2, 3),
        cursor: (0, 0),
        active: true,
        head_sync: None,
    });
    app.reset_for_session_switch(1);
    assert!(
        app.text_selection.is_none(),
        "session switch must clear the in-progress selection"
    );
}

#[test]
fn reset_for_session_switch_ends_history_browsing() {
    // A history-recalled entry belongs to the session being left; switch
    // must end browsing so the field's "reset on session switch" contract
    // holds even when the input hand-off is skipped.
    let mut app = test_app();
    app.history_index = Some(0);
    app.reset_for_session_switch(1);
    assert!(
        app.history_index.is_none(),
        "session switch must end history browsing"
    );
}

#[test]
fn set_page_clears_text_selection() {
    // Leaving the Chat page invalidates the selection's screen-coordinate
    // context (it is keyed to the history it was drawn over); a stale
    // gesture must not survive a page switch and swallow the first click
    // on return.
    let mut app = test_app();
    app.text_selection = Some(crate::selection::TextSelection {
        anchor: (0, 0),
        head: (2, 3),
        cursor: (0, 0),
        active: true,
        head_sync: None,
    });
    app.set_page(Page::SessionManager);
    assert!(
        app.text_selection.is_none(),
        "page switch must clear the in-progress selection"
    );
}

#[test]
fn terminal_resize_clears_text_selection() {
    // A resize re-wraps every rendered line, so a stored (content line,
    // viewport column) anchor would point at different text afterwards.
    // The gesture must be dropped exactly like a suspend/page switch
    // drops it (the anchor is deliberately never re-resolved).
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 30;
    app.text_selection = Some(crate::selection::TextSelection {
        anchor: (0, 0),
        head: (2, 3),
        cursor: (0, 0),
        active: true,
        head_sync: None,
    });
    // A different cached terminal size drives a viewport change without
    // touching a real terminal (crossterm::size() is only queried when
    // `terminal_resized` is set).
    app.last_terminal_size = Some((60, 20));
    app.terminal_resized = false;
    app.update_viewport_from_terminal_size();
    assert!(
        app.text_selection.is_none(),
        "terminal resize must clear the in-progress selection"
    );
}

// ── scrollbar_notch ──

#[test]
fn scrollbar_notch_no_content() {
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 20;
    assert_eq!(app.scrollbar_notch(), 1);
}

#[test]
fn scrollbar_notch_track_one() {
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 1;
    let display = app.active_display().unwrap();
    display.height_prefix.push(50);
    // max_scroll = 50 - 1 = 49, virtual_track = 2, notch = ceil(49 / 2) = 25
    assert_eq!(app.scrollbar_notch(), 25);
}

#[test]
fn scrollbar_notch_ceiling_division() {
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 50;
    let display = app.active_display().unwrap();
    display.height_prefix.push(150);
    // max_scroll = 150 - 50 = 100, virtual_track = 100, notch = ceil(100 / 100) = 1
    assert_eq!(app.scrollbar_notch(), 1);
}

#[test]
fn scrollbar_notch_rounds_up() {
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 30;
    let display = app.active_display().unwrap();
    display.height_prefix.push(105);
    // max_scroll = 105 - 30 = 75, virtual_track = 60, notch = ceil(75 / 60) = 2
    assert_eq!(app.scrollbar_notch(), 2);
}

// ── scrollbar_scroll_up / scrollbar_scroll_down ──

#[test]
fn scrollbar_scroll_up_increases_scroll_by_notch() {
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 10;
    let display = app.active_display().unwrap();
    display.height_prefix.push(110);
    // max_scroll = 100, virtual_track = 20, notch = 5
    display.history_scroll.scroll = 0;
    let before = app.effective_scroll();

    app.scrollbar_scroll_up();

    assert_eq!(app.effective_scroll(), before + 5);
}

#[test]
fn scrollbar_scroll_up_clamps_at_max_scroll() {
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 10;
    let display = app.active_display().unwrap();
    display.height_prefix.push(110);
    // max_scroll = 100, virtual_track = 20, notch = 5
    display.history_scroll.scroll = 100;

    app.scrollbar_scroll_up();

    assert_eq!(app.effective_scroll(), 100);
}

#[test]
fn scrollbar_scroll_down_decreases_scroll_by_notch() {
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 10;
    let display = app.active_display().unwrap();
    display.height_prefix.push(110);
    // max_scroll = 100, virtual_track = 20, notch = 5
    display.history_scroll.scroll = 100;
    let before = app.effective_scroll();

    app.scrollbar_scroll_down();

    assert_eq!(app.effective_scroll(), before - 5);
}

#[test]
fn scrollbar_scroll_down_clamps_at_zero() {
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 10;
    let display = app.active_display().unwrap();
    display.height_prefix.push(110);
    // max_scroll = 100, virtual_track = 20, notch = 5
    display.history_scroll.scroll = 5;

    app.scrollbar_scroll_down();

    assert_eq!(app.effective_scroll(), 0);
}

// ── scroll_to_track_row ──

#[test]
fn scroll_to_track_row_at_bottom() {
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 20;
    let display = app.active_display().unwrap();
    display.height_prefix.push(110);
    // max_scroll = 90, denom = 19
    display.history_scroll.scroll = 90;

    app.scroll_to_track_row(0, 20);

    assert_eq!(app.effective_scroll(), 90);
}

#[test]
fn scroll_to_track_row_at_top() {
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 20;
    let display = app.active_display().unwrap();
    display.height_prefix.push(110);
    // max_scroll = 90, denom = 19
    display.history_scroll.scroll = 0;

    app.scroll_to_track_row(19, 20);

    assert_eq!(app.effective_scroll(), 0);
}

#[test]
fn scroll_to_track_row_midpoint() {
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 10;
    let display = app.active_display().unwrap();
    display.height_prefix.push(110);
    // max_scroll = 100, denom = 9

    app.scroll_to_track_row(4, 10);

    assert_eq!(app.effective_scroll(), 56);
}

#[test]
fn scroll_to_track_row_zero_viewport() {
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 0;
    let display = app.active_display().unwrap();
    display.height_prefix.push(110);
    display.history_scroll.scroll = 42;

    app.scroll_to_track_row(0, 0);

    assert_eq!(app.effective_scroll(), 42);
}

#[test]
fn scroll_to_track_row_track_one() {
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 10;
    let display = app.active_display().unwrap();
    display.height_prefix.push(110);
    display.history_scroll.scroll = 42;

    app.scroll_to_track_row(0, 1);

    assert_eq!(app.effective_scroll(), 42);
}

#[test]
fn scroll_to_track_row_mouse_row_clamped() {
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 20;
    let display = app.active_display().unwrap();
    display.height_prefix.push(110);
    // max_scroll = 90, denom = 19
    display.history_scroll.scroll = 0;

    app.scroll_to_track_row(30, 20);

    assert_eq!(app.effective_scroll(), 0);
}

// ── scroll_to_content_line ──

#[test]
fn scroll_to_content_line_idempotent_when_already_visible() {
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 200;

    let turn = Turn {
        created_at: choreo_proto::TimestampMs::now(),
        undone: false,
        error: None,
        user_text: Some("hello".into()),
        assistant_text: Some("world".into()),
        assistant_reasoning: None,
        tool_calls: vec![],
        token_usage: None,
        tool_results: vec![],
        displayed_images: vec![],
        reasoning_artifact: None,
        reasoning_producer: None,
    };
    let display = app.active_display().unwrap();
    display.view.insert_or_replace(0, turn);
    app.rebuild_height_prefix();

    let before = app.effective_scroll();
    app.scroll_to_content_line(0);
    assert_eq!(app.effective_scroll(), before);
}

#[test]
fn scroll_to_content_line_large_content_line_saturates() {
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 5;

    for i in 0..5u32 {
        let turn = Turn {
            created_at: choreo_proto::TimestampMs::now(),
            undone: false,
            error: None,
            user_text: Some(format!("user text {i}")),
            assistant_text: Some(format!("assistant text {i}")),
            assistant_reasoning: None,
            tool_calls: vec![],
            token_usage: None,
            tool_results: vec![],
            displayed_images: vec![],
            reasoning_artifact: None,
            reasoning_producer: None,
        };
        let display = app.active_display().unwrap();
        display.view.insert_or_replace(i, turn);
    }
    app.rebuild_height_prefix();

    app.scroll_to_content_line(9999);
    assert_eq!(app.effective_scroll(), 0);
}

// ── status_error_height ──

#[test]
fn status_error_height_neither_set_returns_zero() {
    let app = test_app();
    assert_eq!(app.status_error_height(80), 0);
}

#[test]
fn status_error_height_short_error_returns_one() {
    let mut app = test_app();
    app.error = Some("oops".into());
    assert_eq!(app.status_error_height(80), 1);
}

#[test]
fn status_error_height_short_status_returns_one() {
    let mut app = test_app();
    app.status = Some("all good".into());
    assert_eq!(app.status_error_height(80), 1);
}

#[test]
fn status_error_height_error_preferred_over_status() {
    let mut app = test_app();
    app.error = Some("error".into());
    app.status = Some("status".into());
    // Should use error text, not status text
    assert_eq!(app.status_error_height(80), 1);
}

#[test]
fn status_error_height_wrapping() {
    let mut app = test_app();
    // The status Paragraph wraps at width-2 (the inset `notify_area`), so
    // at width 5 the inner width is 3: "12345 7890" hard-splits to
    // ["123", "45 ", "789", "0"] → 4 rows (matches what ratatui draws).
    app.error = Some("12345 7890".into());
    assert_eq!(app.status_error_height(5), 4);
}

#[test]
fn status_error_height_multi_line() {
    let mut app = test_app();
    // Three explicit lines via \n
    app.status = Some("line a\nline b\nline c".into());
    // Each line fits in width 80, so total = 3
    assert_eq!(app.status_error_height(80), 3);
}

#[test]
fn status_error_height_multi_line_with_wrapping() {
    let mut app = test_app();
    // Two lines; at width 5 the inner wrap width is 3: "hello" hard-splits
    // to ["hel", "lo"] (2 rows) and "12345 7890" to 4 rows — 6 total,
    // matching the rows the inset status Paragraph actually draws.
    app.error = Some("hello\n12345 7890".into());
    assert_eq!(app.status_error_height(5), 6);
}

#[test]
fn status_error_height_empty_after_clearing() {
    let mut app = test_app();
    app.error = Some("error".into());
    app.error = None;
    assert_eq!(app.status_error_height(80), 0);
}

#[test]
fn status_error_height_status_takes_over_when_error_cleared() {
    let mut app = test_app();
    app.status = Some("status".into());
    assert_eq!(app.status_error_height(80), 1);
}

#[test]
fn sync_turn_images_populates_rendered_images() {
    let mut app = test_app();
    let metadata = choreo_proto::ImageMetadata {
        mime_type: "image/svg+xml".to_string(),
        width: 100,
        height: 200,
        byte_len: 50,
        alt: None,
    };
    let turn = Turn {
        created_at: choreo_proto::TimestampMs::now(),
        undone: false,
        error: None,
        user_text: None,
        assistant_text: None,
        assistant_reasoning: None,
        tool_calls: vec![],
        token_usage: None,
        tool_results: vec![],
        displayed_images: vec![
            choreo_proto::DisplayedImageRecord {
                metadata: metadata.clone(),
                data: b"svg-data".to_vec(),
                tool_call_id: Some("call-1".into()),
            },
            choreo_proto::DisplayedImageRecord {
                metadata: metadata.clone(),
                data: b"more-svg".to_vec(),
                tool_call_id: None,
            },
        ],
        reasoning_artifact: None,
        reasoning_producer: None,
    };
    app.sync_turn_images(0, 42, &turn);

    let images = app.rendered_images.get(&0).unwrap().get(&42).unwrap();
    assert_eq!(images.len(), 2);
    assert_eq!(images[&ImageSlot::Displayed(0)].data.as_ref(), b"svg-data");
    assert_eq!(images[&ImageSlot::Displayed(1)].data.as_ref(), b"more-svg");
    // Second call is idempotent — preserves existing entries
    app.sync_turn_images(0, 42, &turn);
    assert_eq!(
        app.rendered_images.get(&0).unwrap().get(&42).unwrap().len(),
        2
    );
}

// ── TurnImageLayout image_ranges ──

#[test]
#[expect(clippy::assert_is_empty)] // clearer than assert_eq! against []
fn turn_layout_empty_when_no_images() {
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 20;
    let turn = Turn {
        created_at: choreo_proto::TimestampMs::now(),
        undone: false,
        error: None,
        user_text: Some("hello".into()),
        assistant_text: Some("world".into()),
        assistant_reasoning: None,
        tool_calls: vec![],
        token_usage: None,
        tool_results: vec![],
        displayed_images: vec![],
        reasoning_artifact: None,
        reasoning_producer: None,
    };
    app.active_display()
        .unwrap()
        .view
        .insert_or_replace(1, turn);
    app.rebuild_height_prefix();

    assert_eq!(app.active_display().unwrap().turn_layouts.len(), 1);
    assert!(
        app.active_display().unwrap().turn_layouts[0]
            .image_ranges
            .is_empty()
    );
}

#[test]
fn turn_layout_populates_image_ranges_with_fallback_height() {
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 20;
    let metadata = choreo_proto::ImageMetadata {
        mime_type: "image/png".to_string(),
        width: 100,
        height: 100,
        byte_len: 500,
        alt: None,
    };
    let turn = Turn {
        created_at: choreo_proto::TimestampMs::now(),
        undone: false,
        error: None,
        user_text: None,
        assistant_text: Some("short".into()),
        assistant_reasoning: None,
        tool_calls: vec![],
        token_usage: None,
        tool_results: vec![],
        displayed_images: vec![
            choreo_proto::DisplayedImageRecord {
                metadata: metadata.clone(),
                data: vec![0u8; 10],
                tool_call_id: None,
            },
            choreo_proto::DisplayedImageRecord {
                metadata: metadata.clone(),
                data: vec![1u8; 10],
                tool_call_id: None,
            },
        ],
        reasoning_artifact: None,
        reasoning_producer: None,
    };
    let turn_clone = turn.clone();
    app.active_display()
        .unwrap()
        .view
        .insert_or_replace(2, turn);
    app.sync_turn_images(0, 2, &turn_clone);
    app.rebuild_height_prefix();

    assert_eq!(app.active_display().unwrap().turn_layouts.len(), 1);
    // Mutable borrow dropped.

    // Capture needed values before taking another mutable borrow for layout.
    let fallback_h = app.image_block_height() as usize;
    let vp_width = app.history_viewport.width;
    let text_h = {
        let display = app.active_display().unwrap();
        let turn = &display.view.turns[&2];
        lines_height(
            &render_turn_lines(turn, 71, vp_width, false, &[]).lines,
            vp_width,
        )
        .max(1)
    };

    let layout = &app.active_display().unwrap().turn_layouts[0];
    assert_eq!(layout.image_ranges.len(), 2);

    let (s0, e0) = layout.image_ranges[0];
    assert_eq!(s0, text_h);
    assert_eq!(e0, text_h + fallback_h);

    let (s1, e1) = layout.image_ranges[1];
    assert_eq!(s1, text_h + fallback_h);
    assert_eq!(e1, text_h + 2 * fallback_h);
}

// ── TurnLayout reasoning_header_range ──

#[test]
fn turn_layout_reasoning_header_range_present() {
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 20;
    let turn = Turn {
        created_at: choreo_proto::TimestampMs::now(),
        undone: false,
        error: None,
        user_text: Some("hello".into()),
        assistant_text: Some("world".into()),
        assistant_reasoning: Some("think".into()),
        tool_calls: vec![],
        token_usage: None,
        tool_results: vec![],
        displayed_images: vec![],
        reasoning_artifact: None,
        reasoning_producer: None,
    };
    app.active_display()
        .unwrap()
        .view
        .insert_or_replace(1, turn);
    app.rebuild_height_prefix();

    let layout = &app.active_display().unwrap().turn_layouts[0];
    let Some((start, end)) = layout.reasoning_header_range else {
        panic!("reasoning header range should be present");
    };
    assert!(
        start < end,
        "header range must be non-empty ({start}..{end})"
    );
    // No images on this turn, so the full turn height is its text block;
    // the header must lie inside it.
    let turn_h = app.active_display().unwrap().turn_heights[0];
    assert!(end <= turn_h, "header must lie within the turn text");
}

#[test]
fn turn_layout_reasoning_default_expanded_reflects_turn_content() {
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 20;

    // Response present → default collapsed.
    let responded = Turn {
        created_at: choreo_proto::TimestampMs::now(),
        undone: false,
        error: None,
        user_text: None,
        assistant_text: Some("world".into()),
        assistant_reasoning: Some("think".into()),
        tool_calls: vec![],
        token_usage: None,
        tool_results: vec![],
        displayed_images: vec![],
        reasoning_artifact: None,
        reasoning_producer: None,
    };
    app.active_display()
        .unwrap()
        .view
        .insert_or_replace(1, responded);

    // Streaming (no response yet) → default expanded.
    let streaming = Turn {
        created_at: choreo_proto::TimestampMs::now(),
        undone: false,
        error: None,
        user_text: None,
        assistant_text: None,
        assistant_reasoning: Some("thinking".into()),
        tool_calls: vec![],
        token_usage: None,
        tool_results: vec![],
        displayed_images: vec![],
        reasoning_artifact: None,
        reasoning_producer: None,
    };
    app.active_display()
        .unwrap()
        .view
        .insert_or_replace(2, streaming);

    app.rebuild_height_prefix();

    let display = app.active_display().unwrap();
    assert!(
        !display.turn_layouts[0].reasoning_default_expanded,
        "response present → collapsed default"
    );
    assert!(
        display.turn_layouts[1].reasoning_default_expanded,
        "no response yet → expanded default"
    );
}

#[test]
fn turn_layout_reasoning_header_range_none_without_reasoning() {
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 20;
    let turn = Turn {
        created_at: choreo_proto::TimestampMs::now(),
        undone: false,
        error: None,
        user_text: Some("hello".into()),
        assistant_text: Some("world".into()),
        assistant_reasoning: None,
        tool_calls: vec![],
        token_usage: None,
        tool_results: vec![],
        displayed_images: vec![],
        reasoning_artifact: None,
        reasoning_producer: None,
    };
    app.active_display()
        .unwrap()
        .view
        .insert_or_replace(1, turn);
    app.rebuild_height_prefix();

    let layout = &app.active_display().unwrap().turn_layouts[0];
    assert!(
        layout.reasoning_header_range.is_none(),
        "no reasoning → no header range"
    );
}

// ── toggle_reasoning ──

#[test]
fn toggle_reasoning_flips_override_and_invalidates_cache() {
    let mut app = test_app();
    let turn = Turn {
        created_at: choreo_proto::TimestampMs::now(),
        undone: false,
        error: None,
        user_text: None,
        assistant_text: Some("response".into()),
        assistant_reasoning: Some("thinking".into()),
        tool_calls: vec![],
        token_usage: None,
        tool_results: vec![],
        displayed_images: vec![],
        reasoning_artifact: None,
        reasoning_producer: None,
    };
    let display = app.active_display().unwrap();
    display.view.insert_or_replace(1, turn);
    display.visible_turn_ids.push(1);
    display.render_cache = vec![Some(RenderedCache {
        key: RenderCacheKey {
            turn_id: 1,
            width: 71,
            viewport_width: 80,
            reasoning_expanded: false, // response present → collapsed default
            tool_results_collapsed: vec![],
            content_version: 0,
        },
        rendered: RenderedTurn {
            lines: Arc::from(vec![Line::from("stale")]),
            height: 1,
            visual_offsets: Arc::from([1]),
            joins: Arc::from([LineJoin::Break]),
            content_ranges: Arc::from([Some((0, 5))]),
            chrome_ranges: Arc::from([LineChrome::default()]),
            reasoning_header_idx: None,
            tool_result_header_idxs: vec![],
        },
    })];

    // Default is collapsed (response present) → first click expands.
    display.toggle_reasoning(1);
    assert_eq!(
        display.reasoning_override.get(&1),
        Some(&true),
        "first click should expand"
    );
    assert!(
        display.render_cache[0].is_none(),
        "toggle must invalidate the render cache"
    );

    // Second click collapses again.
    display.toggle_reasoning(1);
    assert_eq!(
        display.reasoning_override.get(&1),
        Some(&false),
        "second click should collapse"
    );
}

#[test]
fn toggle_reasoning_missing_turn_is_noop() {
    let mut app = test_app();
    let display = app.active_display().unwrap();
    display.toggle_reasoning(999);
    assert!(
        display.reasoning_override.is_empty(),
        "unknown turn should not record an override"
    );
}

#[test]
fn toggle_reasoning_default_expanded_without_response() {
    let mut app = test_app();
    let turn = Turn {
        created_at: choreo_proto::TimestampMs::now(),
        undone: false,
        error: None,
        user_text: None,
        assistant_text: None,
        assistant_reasoning: Some("thinking".into()),
        tool_calls: vec![],
        token_usage: None,
        tool_results: vec![],
        displayed_images: vec![],
        reasoning_artifact: None,
        reasoning_producer: None,
    };
    let display = app.active_display().unwrap();
    display.view.insert_or_replace(1, turn);
    // No response yet → default expanded → first click collapses.
    display.toggle_reasoning(1);
    assert_eq!(
        display.reasoning_override.get(&1),
        Some(&false),
        "first click on streaming reasoning should collapse"
    );
}

// ── effective_reasoning_expanded ──

#[test]
fn effective_reasoning_expanded_prefers_override() {
    let mut app = test_app();
    let display = app.active_display().unwrap();
    // No override → the derived default wins.
    assert!(!display.effective_reasoning_expanded(1, false));
    assert!(display.effective_reasoning_expanded(1, true));
    // An explicit override wins over the derived default.
    display.reasoning_override.insert(1, true);
    assert!(
        display.effective_reasoning_expanded(1, false),
        "override should beat a collapsed default"
    );
    display.reasoning_override.insert(1, false);
    assert!(
        !display.effective_reasoning_expanded(1, true),
        "override should beat an expanded default"
    );
}

// ── reasoning_override pruning on undo ──

#[test]
fn turns_undone_prunes_reasoning_override() {
    let mut app = test_app();
    let turn = Turn {
        created_at: choreo_proto::TimestampMs::now(),
        undone: false,
        error: None,
        user_text: None,
        assistant_text: Some("response".into()),
        assistant_reasoning: Some("thinking".into()),
        tool_calls: vec![],
        token_usage: None,
        tool_results: vec![],
        displayed_images: vec![],
        reasoning_artifact: None,
        reasoning_producer: None,
    };
    {
        let display = app.active_display().unwrap();
        display.view.insert_or_replace(1, turn);
        // Simulate the user having expanded the reasoning section.
        display.reasoning_override.insert(1, true);
    }

    app.handle_turns_undone(0, &[1]);

    let display = app.active_display_ref().unwrap();
    assert!(
        !display.reasoning_override.contains_key(&1),
        "undo should prune the reasoning override"
    );
    assert!(
        display.view.turns[&1].undone,
        "the turn should be marked undone"
    );
}

#[test]
fn turns_undone_prunes_content_version() {
    // The content-version map must stay bounded by the live (non-undone)
    // turn set, mirroring the reasoning/collapse override pruning: a
    // redone turn re-invalidates its cache slot, so dropping the version
    // here can never serve a stale rendering.
    let mut app = test_app();
    let turn = Turn {
        created_at: choreo_proto::TimestampMs::now(),
        undone: false,
        error: None,
        user_text: None,
        assistant_text: Some("response".into()),
        assistant_reasoning: None,
        tool_calls: vec![],
        token_usage: None,
        tool_results: vec![],
        displayed_images: vec![],
        reasoning_artifact: None,
        reasoning_producer: None,
    };
    {
        let display = app.active_display().unwrap();
        display.view.insert_or_replace(1, turn);
        // A chunk-like mutation records a version for the turn.
        display.bump_turn_version(1);
        assert_eq!(display.turn_content_version(1), 1);
    }

    app.handle_turns_undone(0, &[1]);

    let display = app.active_display_ref().unwrap();
    assert!(
        !display.turn_versions.contains_key(&1),
        "undo should prune the turn's content version"
    );
    assert_eq!(
        display.turn_content_version(1),
        0,
        "an undone turn reports version 0 (no recorded mutations)"
    );
}

// ── tool result collapse ──

#[test]
fn effective_tool_result_collapsed_prefers_override() {
    let mut app = test_app();
    let display = app.active_display().unwrap();
    let quiet = ToolResultRecord {
        call_id: "c".into(),
        name: "read_file".into(),
        content: "x".into(),
        is_error: false,
        invocation_description: String::new(),
        image: None,
    };
    let loud = ToolResultRecord {
        call_id: "c2".into(),
        name: "find".into(),
        content: "y".into(),
        is_error: false,
        invocation_description: String::new(),
        image: None,
    };
    // No override → the derived default wins (quiet collapsed, others
    // expanded).
    assert!(display.effective_tool_result_collapsed(1, &quiet));
    assert!(!display.effective_tool_result_collapsed(1, &loud));
    // An explicit override wins over the derived default.
    display
        .tool_collapse_override
        .entry(1)
        .or_default()
        .insert("c".into(), false);
    assert!(!display.effective_tool_result_collapsed(1, &quiet));
    display
        .tool_collapse_override
        .entry(1)
        .or_default()
        .insert("c2".into(), true);
    assert!(display.effective_tool_result_collapsed(1, &loud));
}

#[test]
fn toggle_tool_result_flips_override_and_invalidates_cache() {
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 20;
    let turn = Turn {
        created_at: choreo_proto::TimestampMs::now(),
        undone: false,
        error: None,
        user_text: None,
        assistant_text: None,
        assistant_reasoning: None,
        tool_calls: vec![],
        token_usage: None,
        tool_results: vec![ToolResultRecord {
            call_id: "call-1".into(),
            name: "read_file".into(),
            content: "file contents".into(),
            is_error: false,
            invocation_description: "Reading file `src/main.rs`.".into(),
            image: None,
        }],
        displayed_images: vec![],
        reasoning_artifact: None,
        reasoning_producer: None,
    };
    let display = app.active_display().unwrap();
    display.view.insert_or_replace(1, turn);
    display.visible_turn_ids.push(1);
    display.render_cache = vec![Some(RenderedCache {
        key: RenderCacheKey {
            turn_id: 1,
            width: 71,
            viewport_width: 80,
            reasoning_expanded: false,
            tool_results_collapsed: vec![true], // quiet default → collapsed
            content_version: 0,
        },
        rendered: RenderedTurn {
            lines: Arc::from(vec![Line::from("stale")]),
            height: 1,
            visual_offsets: Arc::from([1]),
            joins: Arc::from([LineJoin::Break]),
            content_ranges: Arc::from([Some((0, 5))]),
            chrome_ranges: Arc::from([LineChrome::default()]),
            reasoning_header_idx: None,
            tool_result_header_idxs: vec![0],
        },
    })];

    // Quiet default is collapsed → the first click expands.
    display.toggle_tool_result(1, "call-1");
    assert_eq!(
        display
            .tool_collapse_override
            .get(&1)
            .and_then(|m| m.get("call-1")),
        Some(&false),
        "first click should expand a collapsed quiet result"
    );
    assert!(
        display.render_cache[0].is_none(),
        "toggle must invalidate the render cache"
    );

    // Second click collapses again.
    display.toggle_tool_result(1, "call-1");
    assert_eq!(
        display
            .tool_collapse_override
            .get(&1)
            .and_then(|m| m.get("call-1")),
        Some(&true),
        "second click should collapse the result again"
    );
}

#[test]
fn toggle_tool_result_missing_turn_or_call_is_noop() {
    let mut app = test_app();
    let display = app.active_display().unwrap();
    // No such turn → no-op.
    display.toggle_tool_result(99, "call-1");
    assert!(display.tool_collapse_override.is_empty());
    // Turn exists but no matching call_id → no-op.
    let turn = Turn {
        created_at: choreo_proto::TimestampMs::now(),
        undone: false,
        error: None,
        user_text: None,
        assistant_text: None,
        assistant_reasoning: None,
        tool_calls: vec![],
        token_usage: None,
        tool_results: vec![ToolResultRecord {
            call_id: "other".into(),
            name: "sh".into(),
            content: "y".into(),
            is_error: false,
            invocation_description: String::new(),
            image: None,
        }],
        displayed_images: vec![],
        reasoning_artifact: None,
        reasoning_producer: None,
    };
    display.view.insert_or_replace(1, turn);
    display.toggle_tool_result(1, "call-1");
    assert!(display.tool_collapse_override.is_empty());
}

#[test]
fn turns_undone_prunes_tool_collapse_override() {
    let mut app = test_app();
    let turn = Turn {
        created_at: choreo_proto::TimestampMs::now(),
        undone: false,
        error: None,
        user_text: None,
        assistant_text: None,
        assistant_reasoning: None,
        tool_calls: vec![],
        token_usage: None,
        tool_results: vec![ToolResultRecord {
            call_id: "call-1".into(),
            name: "read_file".into(),
            content: "x".into(),
            is_error: false,
            invocation_description: String::new(),
            image: None,
        }],
        displayed_images: vec![],
        reasoning_artifact: None,
        reasoning_producer: None,
    };
    {
        let display = app.active_display().unwrap();
        display.view.insert_or_replace(1, turn);
        // Simulate the user having expanded the quiet result.
        display
            .tool_collapse_override
            .entry(1)
            .or_default()
            .insert("call-1".into(), false);
    }

    app.handle_turns_undone(0, &[1]);

    let display = app.active_display_ref().unwrap();
    assert!(
        display.tool_collapse_override.is_empty(),
        "undo should prune the tool collapse override"
    );
    assert!(
        display.view.turns[&1].undone,
        "the turn should be marked undone"
    );
}

#[test]
fn turn_layout_populates_tool_result_header_ranges() {
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 20;
    let turn = Turn {
        created_at: choreo_proto::TimestampMs::now(),
        undone: false,
        error: None,
        user_text: None,
        assistant_text: None,
        assistant_reasoning: None,
        tool_calls: vec![],
        token_usage: None,
        tool_results: vec![
            ToolResultRecord {
                call_id: "c1".into(),
                name: "read_file".into(),
                content: "x".into(),
                is_error: false,
                invocation_description: "Reading `a`.".into(),
                image: None,
            },
            ToolResultRecord {
                call_id: "c2".into(),
                name: "sh".into(),
                content: "y".into(),
                is_error: false,
                invocation_description: "Running `b`.".into(),
                image: None,
            },
        ],
        displayed_images: vec![],
        reasoning_artifact: None,
        reasoning_producer: None,
    };
    app.active_display()
        .unwrap()
        .view
        .insert_or_replace(1, turn);
    app.rebuild_height_prefix();

    // Capture the ranges and turn height in one borrow scope to avoid
    // overlapping borrows of the display.
    let (ranges, turn_h) = {
        let display = app.active_display().unwrap();
        let layout = &display.turn_layouts[0];
        (
            layout.tool_result_header_ranges.clone(),
            display.turn_heights[0],
        )
    };
    assert_eq!(ranges.len(), 2, "one header range per tool result");
    // No other sections on this turn: both headers are the first two
    // lines (both quiet results are collapsed, so each is one header row).
    assert_eq!(ranges[0], (0, 1));
    assert_eq!(ranges[1], (1, 2));
    assert!(
        ranges[1].1 <= turn_h,
        "headers must lie within the turn text"
    );
}

// ── auto-collapse on first answer chunk ──

#[test]
fn first_answer_chunk_auto_collapses_reasoning() {
    let mut app = test_app();
    let turn = Turn {
        created_at: choreo_proto::TimestampMs::now(),
        undone: false,
        error: None,
        user_text: None,
        assistant_text: None,
        assistant_reasoning: Some("thinking".into()),
        tool_calls: vec![],
        token_usage: None,
        tool_results: vec![],
        displayed_images: vec![],
        reasoning_artifact: None,
        reasoning_producer: None,
    };
    let display = app.active_display().unwrap();
    display.view.insert_or_replace(1, turn);
    display.view.request_to_turn.insert(7, 1);
    // The user expanded reasoning during streaming.
    display.reasoning_override.insert(1, true);

    app.handle_request_stream(0, 7, OutputStream::Answer, Cow::Borrowed("Hi"));

    let display = app.active_display().unwrap();
    assert!(
        !display.reasoning_override.contains_key(&1),
        "first answer chunk should auto-collapse reasoning"
    );
    assert_eq!(display.view.turns[&1].assistant_text.as_deref(), Some("Hi"));
    assert!(
        display.view.turns[&1].assistant_reasoning.is_some(),
        "reasoning content must be retained after the response streams"
    );
}

#[test]
fn reasoning_chunk_keeps_expansion_override() {
    let mut app = test_app();
    let turn = Turn {
        created_at: choreo_proto::TimestampMs::now(),
        undone: false,
        error: None,
        user_text: None,
        assistant_text: None,
        assistant_reasoning: Some("thinking".into()),
        tool_calls: vec![],
        token_usage: None,
        tool_results: vec![],
        displayed_images: vec![],
        reasoning_artifact: None,
        reasoning_producer: None,
    };
    let display = app.active_display().unwrap();
    display.view.insert_or_replace(1, turn);
    display.view.request_to_turn.insert(7, 1);
    display.reasoning_override.insert(1, true);

    app.handle_request_stream(0, 7, OutputStream::Reasoning, Cow::Borrowed(" more"));

    let display = app.active_display().unwrap();
    assert_eq!(
        display.reasoning_override.get(&1),
        Some(&true),
        "reasoning chunks must not collapse the section"
    );
    assert_eq!(
        display.view.turns[&1].assistant_reasoning.as_deref(),
        Some("thinking more"),
        "reasoning chunk should append to the reasoning text"
    );
}

// ── apply_image_result ──

#[test]
fn apply_image_result_clears_pending_job_and_records_failure() {
    use crate::image_worker::next_job_id;

    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 20;
    let metadata = choreo_proto::ImageMetadata {
        mime_type: "image/png".to_string(),
        width: 100,
        height: 100,
        byte_len: 500,
        alt: None,
    };
    let turn = Turn {
        created_at: choreo_proto::TimestampMs::now(),
        undone: false,
        error: None,
        user_text: None,
        assistant_text: None,
        assistant_reasoning: None,
        tool_calls: vec![],
        token_usage: None,
        tool_results: vec![],
        displayed_images: vec![choreo_proto::DisplayedImageRecord {
            metadata: metadata.clone(),
            data: vec![3u8; 30],
            tool_call_id: None,
        }],
        reasoning_artifact: None,
        reasoning_producer: None,
    };
    let turn_clone = turn.clone();
    app.active_display()
        .unwrap()
        .view
        .insert_or_replace(4, turn);
    app.sync_turn_images(0, 4, &turn_clone);

    let img_id = next_job_id();
    app.pending_job_idx
        .insert(img_id, (0, 4, ImageSlot::Displayed(0)));
    let img = app
        .rendered_images
        .get_mut(&0)
        .unwrap()
        .get_mut(&4)
        .unwrap()
        .get_mut(&ImageSlot::Displayed(0))
        .unwrap();
    img.pending_job = Some(img_id);

    let inline_size = Size::new(app.history_viewport.width, app.image_block_height());
    let result = crate::image_worker::ImageResult {
        id: img_id,
        protocol: None,
        cell_size: inline_size,
    };
    app.apply_image_result(result);

    let img = app
        .rendered_images
        .get(&0)
        .unwrap()
        .get(&4)
        .unwrap()
        .get(&ImageSlot::Displayed(0))
        .unwrap();
    assert!(img.failed_sizes.contains(&inline_size));
    assert!(img.pending_job.is_none());
}

#[test]
fn apply_image_result_records_failure_at_any_size() {
    use crate::image_worker::next_job_id;

    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 20;
    let metadata = choreo_proto::ImageMetadata {
        mime_type: "image/png".to_string(),
        width: 100,
        height: 100,
        byte_len: 500,
        alt: None,
    };
    let turn = Turn {
        created_at: choreo_proto::TimestampMs::now(),
        undone: false,
        error: None,
        user_text: None,
        assistant_text: None,
        assistant_reasoning: None,
        tool_calls: vec![],
        token_usage: None,
        tool_results: vec![],
        displayed_images: vec![choreo_proto::DisplayedImageRecord {
            metadata: metadata.clone(),
            data: vec![4u8; 40],
            tool_call_id: None,
        }],
        reasoning_artifact: None,
        reasoning_producer: None,
    };
    let turn_clone = turn.clone();
    app.active_display()
        .unwrap()
        .view
        .insert_or_replace(5, turn);
    app.sync_turn_images(0, 5, &turn_clone);

    let img_id = next_job_id();
    app.pending_job_idx
        .insert(img_id, (0, 5, ImageSlot::Displayed(0)));
    let img = app
        .rendered_images
        .get_mut(&0)
        .unwrap()
        .get_mut(&5)
        .unwrap()
        .get_mut(&ImageSlot::Displayed(0))
        .unwrap();
    img.pending_job = Some(img_id);

    // Use a cell_size that is NOT the inline size.
    let non_inline_size = Size::new(80, app.image_block_height() + 1);
    let result = crate::image_worker::ImageResult {
        id: img_id,
        protocol: None,
        cell_size: non_inline_size,
    };
    app.apply_image_result(result);

    let img = app
        .rendered_images
        .get(&0)
        .unwrap()
        .get(&5)
        .unwrap()
        .get(&ImageSlot::Displayed(0))
        .unwrap();
    assert!(img.failed_sizes.contains(&non_inline_size));
}

// ── compute_total_height_and_markers scroll preservation ──

/// Helper: insert a minimal turn into `app`.
fn insert_turn(app: &mut App, id: u32, user_text: &str, assistant_text: &str) {
    let turn = Turn {
        created_at: choreo_proto::TimestampMs::now(),
        undone: false,
        error: None,
        user_text: Some(user_text.into()),
        assistant_text: Some(assistant_text.into()),
        assistant_reasoning: None,
        tool_calls: vec![],
        token_usage: None,
        tool_results: vec![],
        displayed_images: vec![],
        reasoning_artifact: None,
        reasoning_producer: None,
    };
    app.display_for(0).view.insert_or_replace(id, turn);
}

#[test]
fn scroll_preserved_when_scrolled_up_and_content_changes() {
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 5;

    insert_turn(&mut app, 0, "a", "a");
    insert_turn(&mut app, 1, "b", "b");
    app.rebuild_height_prefix();

    // Capture viewport height before taking a mutable borrow.
    let viewport_height = app.history_viewport.height;
    {
        let display = app.active_display().unwrap();
        let initial_total = display.total_history_height();

        display.history_scroll.scroll = initial_total.saturating_sub(viewport_height as usize) / 2;
    }
    assert!(app.effective_scroll() > 0, "should be scrolled up");

    insert_turn(&mut app, 2, "new content", "new content");
    let old_total = app.total_history_height();
    let old_scroll;
    {
        let display = app.active_display().unwrap();
        old_scroll = display.history_scroll.scroll;

        display.mark_content_changed();
    }

    app.compute_total_height_and_markers();

    let display = app.active_display_ref().unwrap();
    let new_total = display.total_history_height();
    let delta = new_total.saturating_sub(old_total);
    assert!(
        delta > 0,
        "total height should increase after adding content"
    );
    assert_eq!(
        display.history_scroll.scroll,
        old_scroll + delta,
        "scroll should be adjusted by the content delta"
    );
    assert!(
        !display.content_dirty,
        "content_dirty should be cleared after computation"
    );
}

#[test]
fn scroll_not_preserved_when_at_bottom_and_content_changes() {
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 5;

    insert_turn(&mut app, 0, "a", "a");
    insert_turn(&mut app, 1, "b", "b");
    app.rebuild_height_prefix();

    {
        let display = app.active_display().unwrap();
        display.history_scroll.scroll = 0;
    }
    assert_eq!(app.effective_scroll(), 0, "should be at bottom");

    insert_turn(&mut app, 2, "more", "more");
    let old_scroll;
    {
        let display = app.active_display().unwrap();
        old_scroll = display.history_scroll.scroll;
        display.mark_content_changed();
    }

    app.compute_total_height_and_markers();

    let display = app.active_display_ref().unwrap();
    assert_eq!(
        display.history_scroll.scroll, old_scroll,
        "scroll should stay at 0 when user is at bottom"
    );
}

// ── marker computation ──

#[test]
fn markers_empty_when_no_user_text_turns() {
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 20;

    let turn = Turn {
        created_at: choreo_proto::TimestampMs::now(),
        undone: false,
        error: None,
        user_text: None,
        assistant_text: Some("hello".into()),
        assistant_reasoning: None,
        tool_calls: vec![],
        token_usage: None,
        tool_results: vec![],
        displayed_images: vec![],
        reasoning_artifact: None,
        reasoning_producer: None,
    };
    {
        let display = app.active_display().unwrap();
        display.view.insert_or_replace(0, turn);
    }
    app.rebuild_height_prefix();

    let display = app.active_display_ref().unwrap();
    assert!(
        display.markers.is_empty(),
        "no markers should be created when no turn has user_text"
    );
}

#[test]
fn markers_created_for_each_user_text_turn() {
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 20;

    insert_turn(&mut app, 0, "user a", "assistant a");
    let turn_no_user = Turn {
        created_at: choreo_proto::TimestampMs::now(),
        undone: false,
        error: None,
        user_text: None,
        assistant_text: Some("assistant only".into()),
        assistant_reasoning: None,
        tool_calls: vec![],
        token_usage: None,
        tool_results: vec![],
        displayed_images: vec![],
        reasoning_artifact: None,
        reasoning_producer: None,
    };
    {
        let display = app.active_display().unwrap();
        display.view.insert_or_replace(1, turn_no_user);
    }
    insert_turn(&mut app, 2, "user c", "assistant c");

    app.rebuild_height_prefix();

    let display = app.active_display_ref().unwrap();
    assert_eq!(
        display.markers.len(),
        2,
        "expected 2 markers for 2 user-text turns"
    );
    assert!(
        display.markers[0].content_line < display.markers[1].content_line,
        "first user-text turn should appear before the second"
    );

    let total = display.total_history_height();
    for marker in &display.markers {
        assert!(
            marker.content_line < total,
            "marker content_line {0} should be < total history {total}",
            marker.content_line
        );
    }
}

#[test]
fn marker_virtual_slot_uses_final_total_height() {
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 20;
    let virtual_track = 2 * app.history_viewport.height as usize;

    insert_turn(&mut app, 0, "x", "y");
    insert_turn(&mut app, 1, "x", "y");
    app.rebuild_height_prefix();

    let display = app.active_display_ref().unwrap();
    let total = display.total_history_height();
    assert!(total > 0, "total history should be positive");

    let mut prev_end = 0usize;
    for (i, marker) in display.markers.iter().enumerate() {
        assert_eq!(
            marker.content_line, prev_end,
            "marker {i} content_line should equal the start of the turn"
        );
        if let Some(&end) = display.height_prefix.get(i) {
            prev_end = end;
        }

        let expected_slot = marker.content_line * virtual_track / total;
        assert_eq!(
            marker.virtual_slot, expected_slot,
            "marker {i} virtual_slot should use final total={total} as denominator"
        );
    }
}

#[test]
fn marker_virtual_slot_proportional_to_position() {
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 20;
    let virtual_track = 2 * app.history_viewport.height as usize;

    insert_turn(&mut app, 0, "a", "a");
    insert_turn(&mut app, 1, "b", "b");
    insert_turn(&mut app, 2, "c", "c");
    app.rebuild_height_prefix();

    let display = app.active_display_ref().unwrap();
    assert!(
        display.markers[0].virtual_slot <= display.markers[1].virtual_slot,
        "second marker slot should be >= first marker slot"
    );
    assert!(
        display.markers[1].virtual_slot <= display.markers[2].virtual_slot,
        "third marker slot should be >= second marker slot"
    );
    assert!(
        display.markers[2].virtual_slot < virtual_track,
        "last marker slot should be less than virtual_track={virtual_track}"
    );
}

#[test]
fn scroll_not_preserved_when_content_dirty_is_false() {
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 5;

    insert_turn(&mut app, 0, "a", "a");
    app.rebuild_height_prefix();

    let old_scroll;
    {
        let display = app.active_display().unwrap();
        display.history_scroll.scroll = 10;
        old_scroll = display.history_scroll.scroll;

        display.markers_dirty = true;
        assert!(!display.content_dirty, "content should not be dirty");
    }
    app.compute_total_height_and_markers();

    let display = app.active_display_ref().unwrap();
    assert_eq!(
        display.history_scroll.scroll, old_scroll,
        "scroll should not change when content_dirty is false"
    );
}

// ── update_viewport_from_terminal_size ──

#[test]
fn help_overlay_reduces_viewport_height() {
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 26;

    app.last_terminal_size = Some((80, 30));
    app.terminal_resized = false;

    app.show_help_overlay = false;
    app.update_viewport_from_terminal_size();
    let height_without_help = app.history_viewport.height;

    app.last_terminal_size = Some((80, 30));
    app.terminal_resized = false;
    app.show_help_overlay = true;
    app.update_viewport_from_terminal_size();
    let height_with_help = app.history_viewport.height;

    assert_eq!(height_without_help - height_with_help, 2,);

    let total = app.total_history_height();
    let max_scroll = app.max_scroll_offset();
    if total > height_with_help as usize {
        assert_eq!(max_scroll + height_with_help as usize, total,);
    }
}

/// A minimal render-cache entry, so the resize tests below can assert
/// whether a viewport change preserved or wiped the cache.
fn dummy_cache_entry() -> RenderedCache {
    RenderedCache {
        key: RenderCacheKey {
            turn_id: 0,
            width: 0,
            viewport_width: 0,
            reasoning_expanded: false,
            tool_results_collapsed: vec![],
            content_version: 0,
        },
        rendered: RenderedTurn {
            lines: Arc::from(Vec::<Line<'static>>::new()),
            height: 0,
            visual_offsets: Arc::from([]),
            joins: Arc::from([]),
            content_ranges: Arc::from([]),
            chrome_ranges: Arc::from([]),
            reasoning_header_idx: None,
            tool_result_header_idxs: vec![],
        },
    }
}

#[test]
fn classify_viewport_change_matrix() {
    // A width change re-wraps every line: invalidate caches + selection.
    assert_eq!(
        classify_viewport_change(80, 79, 30, 30),
        ViewportChange::Rewrap
    );
    assert_eq!(
        classify_viewport_change(80, 79, 30, 25),
        ViewportChange::Rewrap
    );
    // Width unchanged + ANY height change → recompute the heights only
    // (nothing re-wraps).  This covers both a real vertical resize and the
    // status/help/input chrome growing or shrinking.
    assert_eq!(
        classify_viewport_change(79, 79, 30, 25),
        ViewportChange::HeightsOnly
    );
    assert_eq!(
        classify_viewport_change(79, 79, 25, 30),
        ViewportChange::HeightsOnly
    );
    // Nothing changed → nothing.
    assert_eq!(
        classify_viewport_change(79, 79, 30, 30),
        ViewportChange::None
    );
}

#[test]
fn width_change_clears_content_dirty() {
    let mut app = test_app();

    app.history_viewport.width = 80;
    app.history_viewport.height = 26;

    app.last_terminal_size = Some((100, 30));
    app.terminal_resized = false;

    {
        let display = app.active_display().unwrap();
        display.content_dirty = true;
        display.markers_dirty = true;
        display.render_cache = vec![Some(dummy_cache_entry())];
    }

    app.update_viewport_from_terminal_size();

    let display = app.active_display_ref().unwrap();
    assert!(
        !display.content_dirty,
        "content_dirty should be cleared on width change"
    );
    assert!(display.markers_dirty, "markers_dirty should remain true");
    assert!(
        display.render_cache.iter().all(Option::is_none),
        "render_cache should be cleared"
    );
    assert_eq!(app.history_viewport.width, 99);
}

#[test]
fn height_only_change_recomputes_heights_without_wiping_cache() {
    // A height-only change — a real vertical resize, or the chrome
    // (status/error line, help overlay, input box) growing or shrinking —
    // re-wraps nothing, so it must NOT wipe the render cache: doing that
    // forced a full O(session) re-render, the input delay a large session
    // showed right after a copy set the status and on the next keystroke
    // that cleared it.  It DOES mark the heights dirty, because the
    // image-block height the prefix reserves derives from the viewport
    // height; that rebuild is a cache hit.
    let mut app = test_app();

    app.history_viewport.width = 79;
    app.history_viewport.height = 20;

    app.last_terminal_size = Some((80, 30));
    app.terminal_resized = false;

    {
        let display = app.active_display().unwrap();
        display.content_dirty = true;
        display.markers_dirty = false;
        display.render_cache = vec![Some(dummy_cache_entry())];
    }

    app.update_viewport_from_terminal_size();

    let display = app.active_display_ref().unwrap();
    assert!(
        display.markers_dirty,
        "a height change must recompute the heights (image blocks depend on the viewport height)"
    );
    assert!(
        display.render_cache.iter().all(Option::is_some),
        "a height change must NOT wipe the render cache (nothing re-wraps)"
    );
    assert!(
        display.content_dirty,
        "content_dirty must be untouched by a height-only change"
    );
}

// ── compute_total_height_and_markers: anchor preservation on content removal ──

#[test]
fn content_removed_preserves_scroll_anchor() {
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 5;

    insert_turn(&mut app, 0, "user text", "assistant text");
    insert_turn(&mut app, 1, "more user", "more assistant");
    app.rebuild_height_prefix();

    let old_total = app.total_history_height();
    assert!(old_total > 0, "should have content");

    let viewport_height = app.history_viewport.height;
    let old_scroll;
    {
        let display = app.active_display().unwrap();
        // Scroll to the top of the history so the removed turn (the
        // last one) lies entirely below the viewport — the scenario
        // where anchor preservation keeps the viewport still.
        display.history_scroll.scroll = old_total.saturating_sub(viewport_height as usize);
        old_scroll = display.history_scroll.scroll;
    }
    assert!(app.effective_scroll() > 0, "should be scrolled up");

    {
        let display = app.active_display().unwrap();
        display.view.turns.remove(&1);
        assert_eq!(display.view.turns.len(), 1);

        display.mark_content_changed();
    }

    app.compute_total_height_and_markers();

    let display = app.active_display_ref().unwrap();
    let new_total = display.total_history_height();
    let new_scroll = display.history_scroll.scroll;
    assert!(
        new_total < old_total,
        "removing a turn should shrink the total height"
    );
    // The content row at the viewport's bottom edge stays anchored
    // instead of the viewport jumping to the bottom.
    assert_eq!(
        new_total.saturating_sub(new_scroll),
        old_total.saturating_sub(old_scroll),
        "the anchored content row should not move"
    );
}

#[test]
fn content_added_shifts_scroll_down() {
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 5;

    insert_turn(&mut app, 0, "a", "b");
    app.rebuild_height_prefix();

    let viewport_height = app.history_viewport.height;
    let old_total;
    let old_scroll;
    {
        let display = app.active_display().unwrap();
        old_total = display.total_history_height();
        display.history_scroll.scroll = old_total.saturating_sub(viewport_height as usize) / 2;
        old_scroll = display.history_scroll.scroll;
    }

    insert_turn(&mut app, 1, "c", "d");
    {
        let display = app.active_display().unwrap();
        display.mark_content_changed();
    }

    app.compute_total_height_and_markers();

    let display = app.active_display_ref().unwrap();
    let new_total = display.total_history_height();
    let delta = new_total.saturating_sub(old_total);
    assert!(delta > 0, "total height should increase");
    assert_eq!(
        display.history_scroll.scroll,
        old_scroll + delta,
        "scroll should be shifted down by the content delta"
    );
}

// ── streaming (incremental update) ──

#[test]
fn mark_streaming_changed_sets_flags() {
    let mut app = test_app();
    {
        let display = app.active_display_ref().unwrap();
        assert!(!display.streaming_dirty);
        assert!(!display.content_dirty);
    }

    app.mark_streaming_changed();

    let display = app.active_display_ref().unwrap();
    assert!(display.streaming_dirty, "streaming_dirty should be set");
    assert!(display.content_dirty, "content_dirty should be set");
}

#[test]
fn mark_content_changed_resets_streaming_turn_index() {
    let mut app = test_app();
    let display = app.active_display().unwrap();
    display.markers_dirty = false;
    display.streaming_turn_index = Some(0);

    display.mark_content_changed();

    assert!(display.markers_dirty, "markers_dirty should be set");
    assert!(display.content_dirty, "content_dirty should be set");
    assert!(
        display.streaming_turn_index.is_none(),
        "streaming_turn_index should be cleared"
    );
}

// ── turn_has_live_content (attach snapshot merge) ──

fn empty_placeholder() -> Turn {
    Turn {
        created_at: choreo_proto::TimestampMs::now(),
        undone: false,
        error: None,
        user_text: Some("q".into()),
        assistant_text: None,
        assistant_reasoning: None,
        tool_calls: vec![],
        token_usage: None,
        tool_results: vec![],
        displayed_images: vec![],
        reasoning_artifact: None,
        reasoning_producer: None,
    }
}

fn with_text(turn: &Turn, text: &str) -> Turn {
    let mut t = turn.clone();
    t.assistant_text = Some(text.into());
    t
}

#[test]
fn accumulated_live_content_beats_snapshot_placeholder() {
    let placeholder = empty_placeholder();
    let live = with_text(&placeholder, "streamed so far");
    // The accumulated turn has content the snapshot placeholder lacks.
    assert!(turn_has_live_content(&live, &placeholder));
    // But the placeholder never "wins" over a live turn.
    assert!(!turn_has_live_content(&placeholder, &live));
}

#[test]
fn snapshot_with_content_wins_over_accumulated() {
    let placeholder = empty_placeholder();
    let snapshot_final = with_text(&placeholder, "final answer from daemon");
    let accumulated = with_text(&placeholder, "earlier accumulated");
    // Both have content — the snapshot (daemon-canonical) wins.
    assert!(!turn_has_live_content(&accumulated, &snapshot_final));
    // Identical content: snapshot wins too (no clause triggers).
    let same = with_text(&placeholder, "same");
    assert!(!turn_has_live_content(&same, &same));
}

#[test]
fn reasoning_and_tool_content_also_count_as_live() {
    let placeholder = empty_placeholder();
    let mut reasoning = placeholder.clone();
    reasoning.assistant_reasoning = Some("thinking…".into());
    assert!(turn_has_live_content(&reasoning, &placeholder));

    let mut tool = placeholder.clone();
    tool.tool_calls.push(choreo_proto::AssistantToolCallRecord {
        call_id: "call_1".into(),
        name: "read_file".into(),
        arguments_json: "{}".into(),
    });
    assert!(turn_has_live_content(&tool, &placeholder));
}

#[test]
fn streaming_update_without_turn_index_falls_back() {
    let mut app = test_app();
    insert_turn(&mut app, 0, "hello", "world");
    app.rebuild_height_prefix();

    let display = app.active_display_ref().unwrap();
    let old_total = display.total_history_height();
    assert!(old_total > 0);

    // Simulate streaming without a streaming_turn_index.
    // Capture viewport before mutable borrow.
    let viewport = app.history_viewport;
    let display = app.active_display().unwrap();
    display.streaming_turn_index = None;
    display.streaming_dirty = true;
    display.content_dirty = true;

    let total = display.compute_total_height_and_markers(&viewport);

    assert!(!display.streaming_dirty, "streaming_dirty cleared");
    assert!(!display.content_dirty, "content_dirty cleared");
    assert_eq!(total, old_total, "full rebuild produces same total");
}

#[test]
fn streaming_update_recalculates_turn_height() {
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 200;

    insert_turn(&mut app, 0, "hello", "world");
    app.rebuild_height_prefix();

    let display = app.active_display_ref().unwrap();
    let before_height = display.turn_heights[0];
    let before_total = display.total_history_height();

    // Simulate streaming: append to assistant_text.
    let viewport = app.history_viewport;
    let display = app.active_display().unwrap();
    let turn = display.view.turns.get_mut(&0).unwrap();
    turn.assistant_text
        .as_mut()
        .unwrap()
        .push_str("\n\nnew streaming content");
    display.streaming_turn_index = Some(0);
    display.streaming_dirty = true;
    display.content_dirty = true;

    let total = display.compute_total_height_and_markers(&viewport);

    assert!(
        display.turn_heights[0] > before_height,
        "turn height should increase after content added"
    );
    assert!(
        total >= before_total,
        "total height should increase or stay same"
    );
    assert!(!display.streaming_dirty, "streaming_dirty cleared");
    assert!(!display.content_dirty, "content_dirty cleared");
}

#[test]
fn streaming_answer_moves_reasoning_header_range() {
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 200;

    // A turn with reasoning only (no response yet), actively streaming.
    let turn = Turn {
        created_at: choreo_proto::TimestampMs::now(),
        undone: false,
        error: None,
        user_text: None,
        assistant_text: None,
        assistant_reasoning: Some("thinking".into()),
        tool_calls: vec![],
        token_usage: None,
        tool_results: vec![],
        displayed_images: vec![],
        reasoning_artifact: None,
        reasoning_producer: None,
    };
    {
        let display = app.active_display().unwrap();
        display.view.insert_or_replace(1, turn);
        display.view.request_to_turn.insert(7, 1);
    }
    app.rebuild_height_prefix();

    // Before the answer: reasoning is the only content, so the header
    // sits at the top of the assistant block.
    let initial_start = app.active_display_ref().unwrap().turn_layouts[0]
        .reasoning_header_range
        .expect("header range should exist")
        .0;

    // First Answer chunk auto-collapses the reasoning and places the
    // response above the header.
    app.handle_request_stream(0, 7, OutputStream::Answer, Cow::Borrowed("Response text."));
    app.compute_total_height_and_markers();

    let (start, end) = app.active_display_ref().unwrap().turn_layouts[0]
        .reasoning_header_range
        .expect("header range should remain after auto-collapse");
    assert!(
        start > initial_start,
        "header should move below the streaming response ({initial_start} -> {start})"
    );
    assert!(start < end, "header range must be non-empty");
}

#[test]
fn streaming_tool_result_expanded_grows_collapsed_stays_flat() {
    // The streaming fast path re-renders the in-flight turn with the
    // effective per-result visibility every chunk: an expanded result's
    // body (and turn height) grows live, while a collapsed quiet result
    // stays a single header row no matter how much content streams in.
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 200;

    let turn = Turn {
        created_at: choreo_proto::TimestampMs::now(),
        undone: false,
        error: None,
        user_text: None,
        assistant_text: None,
        assistant_reasoning: None,
        tool_calls: vec![],
        token_usage: None,
        tool_results: vec![
            ToolResultRecord {
                call_id: "quiet".into(),
                name: "read_file".into(), // quiet → collapsed by default
                content: String::new(),
                is_error: false,
                invocation_description: "Reading `a`.".into(),
                image: None,
            },
            ToolResultRecord {
                call_id: "loud".into(),
                name: "find".into(), // not quiet → expanded by default
                content: String::new(),
                is_error: false,
                invocation_description: "Running `b`.".into(),
                image: None,
            },
        ],
        displayed_images: vec![],
        reasoning_artifact: None,
        reasoning_producer: None,
    };
    {
        let display = app.active_display().unwrap();
        display.view.insert_or_replace(1, turn);
        display.view.request_to_turn.insert(7, 1);
    }
    app.rebuild_height_prefix();

    // Capture the header ranges and turn height in one borrow scope.
    let snapshot = |app: &mut App| {
        let display = app.active_display_ref().unwrap();
        let layout = &display.turn_layouts[0];
        (
            layout.tool_result_header_ranges[0],
            layout.tool_result_header_ranges[1],
            display.turn_heights[0],
        )
    };
    let (quiet_range, _loud_range, height_before) = snapshot(&mut app);
    assert_eq!(quiet_range, (0, 1), "collapsed result: single header row");

    // Stream a chunk into the *expanded* result: the turn must grow and
    // the collapsed result must keep its single-row header range.
    app.handle_tool_result_chunk(
        0,
        7,
        "loud".into(),
        b"line one\nline two\nline three\n".to_vec(),
    );
    app.compute_total_height_and_markers();

    let (quiet_range, loud_range, height_after) = snapshot(&mut app);
    assert!(
        height_after > height_before,
        "expanded result grows as content streams ({height_before} -> {height_after})"
    );
    assert_eq!(quiet_range, (0, 1), "collapsed result stays a single row");
    assert_eq!(loud_range, (1, 2), "expanded header still on its own row");

    // Now stream an even bigger chunk into the *collapsed* quiet result:
    // nothing visible changes — the body is hidden behind the triangle.
    let height_before_collapsed = snapshot(&mut app).2;
    app.handle_tool_result_chunk(
        0,
        7,
        "quiet".into(),
        b"hidden\nhidden\nhidden\nhidden\n".to_vec(),
    );
    app.compute_total_height_and_markers();
    let height_after_collapsed = snapshot(&mut app).2;
    assert_eq!(
        height_after_collapsed, height_before_collapsed,
        "collapsed result stays flat while its content streams"
    );
}

#[test]
fn streaming_chunk_after_mark_content_changed_stays_fresh_and_incremental() {
    // Regression for "scrollbar moves but the results stay stuck": a
    // mid-stream `mark_content_changed` (here simulated with a `Done` for
    // an unrelated request — the same shape as a `TurnAppended` or
    // `SessionState` interleaving between chunks, which happens more
    // often when another session is active) disarms the streaming fast
    // path (`streaming_dirty=false`, `streaming_turn_index=None`).
    //
    // Before the fix the next chunk was processed by the O(n) full
    // rebuild, whose content-blind cache key served the *pre-chunk* lines
    // — the visible results froze until the final `TurnAppended`
    // invalidated the slot.  The content-version key forces the rebuild
    // to recompute, and running the fast path first keeps chunk
    // processing incremental.
    let mut app = test_app();
    app.attached_session_id = Some(0);
    app.history_viewport.width = 80;
    app.history_viewport.height = 200;

    let turn = Turn {
        created_at: choreo_proto::TimestampMs::now(),
        undone: false,
        error: None,
        user_text: None,
        assistant_text: None,
        assistant_reasoning: None,
        tool_calls: vec![],
        token_usage: None,
        tool_results: vec![ToolResultRecord {
            call_id: "call-1".into(),
            name: "find".into(), // not quiet → expanded by default
            content: String::new(),
            is_error: false,
            invocation_description: "Running `b`.".into(),
            image: None,
        }],
        displayed_images: vec![],
        reasoning_artifact: None,
        reasoning_producer: None,
    };
    {
        let display = app.active_display().unwrap();
        display.view.insert_or_replace(1, turn);
        display.view.request_to_turn.insert(7, 1);
    }
    app.rebuild_height_prefix();

    let version_before = app.active_display_ref().unwrap().turn_content_version(1);
    let height_before = app.active_display_ref().unwrap().turn_heights[0];

    // First chunk: fast path renders it live.
    app.handle_tool_result_chunk(0, 7, "call-1".into(), b"line one\n".to_vec());
    app.compute_total_height_and_markers();
    let version_after_chunk1 = app.active_display_ref().unwrap().turn_content_version(1);
    assert!(
        version_after_chunk1 > version_before,
        "chunk must bump the turn's content version"
    );

    // Mid-stream disarming event (unrelated Done): clears the streaming
    // flags and forces markers_dirty.
    app.handle_done(0, 99, None, None);
    assert!(
        app.active_display_ref().unwrap().markers_dirty,
        "Done must mark the display for a rebuild"
    );

    // Second chunk arrives while markers_dirty is still set.
    app.handle_tool_result_chunk(0, 7, "call-1".into(), b"line two\n".to_vec());
    app.compute_total_height_and_markers();

    let display = app.active_display_ref().unwrap();
    // The rebuild (or the fast path) must serve the *latest* content, not
    // the pre-second-chunk lines the content-blind key would have reused.
    let cached = display.render_cache[0].as_ref().expect("cache slot filled");
    let text: String = cached
        .rendered
        .lines
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        text.contains("line two"),
        "rebuild must not serve stale pre-chunk lines:\n{text}"
    );
    assert!(
        text.contains("line one"),
        "earlier chunk must still be present"
    );
    assert!(
        display.turn_heights[0] > height_before,
        "turn height must reflect the streamed content"
    );
    assert!(!display.streaming_dirty, "streaming flag consumed");
    assert!(!display.markers_dirty, "markers flag consumed");
    assert!(!display.content_dirty, "content flag consumed");
}

#[test]
fn rebuild_after_disarmed_chunk_serves_fresh_content() {
    // Regression for the content-version cache key: a chunk arrives, then
    // a `mark_content_changed` event (a `Done`/`TurnAppended`/`SessionState`
    // interleaving between the chunk and its render) disarms the fast
    // path *before* it can re-render — so the rebuild runs against a cache
    // entry rendered from pre-chunk content.  Without the content version
    // in the key the rebuild would reuse those stale lines; with it, the
    // mismatch forces a recompute.
    let mut app = test_app();
    app.attached_session_id = Some(0);
    app.history_viewport.width = 80;
    app.history_viewport.height = 200;

    let turn = Turn {
        created_at: choreo_proto::TimestampMs::now(),
        undone: false,
        error: None,
        user_text: None,
        assistant_text: None,
        assistant_reasoning: None,
        tool_calls: vec![],
        token_usage: None,
        tool_results: vec![ToolResultRecord {
            call_id: "call-1".into(),
            name: "find".into(), // not quiet → expanded by default
            content: String::new(),
            is_error: false,
            invocation_description: "Running `b`.".into(),
            image: None,
        }],
        displayed_images: vec![],
        reasoning_artifact: None,
        reasoning_producer: None,
    };
    {
        let display = app.active_display().unwrap();
        display.view.insert_or_replace(1, turn);
        display.view.request_to_turn.insert(7, 1);
    }
    app.rebuild_height_prefix();

    // The cache now holds the pre-chunk rendering (empty result body).
    // A chunk appends content and bumps the version, but the fast path
    // has NOT run yet.
    app.handle_tool_result_chunk(0, 7, "call-1".into(), b"line one\n".to_vec());

    // The disarming event lands before the next render: streaming flags
    // are cleared, markers_dirty set — the rebuild path will run.
    app.handle_done(0, 99, None, None);

    app.compute_total_height_and_markers();

    let display = app.active_display_ref().unwrap();
    let cached = display.render_cache[0].as_ref().expect("cache slot filled");
    let text: String = cached
        .rendered
        .lines
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        text.contains("line one"),
        "rebuild must recompute the chunk's content (content-version key):\n{text}"
    );
    assert!(
        cached.key.content_version > 0,
        "cache entry must record the post-chunk version"
    );
}

#[test]
fn content_version_bumps_on_every_mutating_handler() {
    // The version is the cache key's content fingerprint: every handler
    // that changes a turn's rendered text must bump it, so a rebuild can
    // tell stale entries apart from current ones.
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 200;

    let turn = Turn {
        created_at: choreo_proto::TimestampMs::now(),
        undone: false,
        error: None,
        user_text: None,
        assistant_text: None,
        assistant_reasoning: None,
        tool_calls: vec![],
        token_usage: None,
        tool_results: vec![ToolResultRecord {
            call_id: "call-1".into(),
            name: "find".into(),
            content: String::new(),
            is_error: false,
            invocation_description: String::new(),
            image: None,
        }],
        displayed_images: vec![],
        reasoning_artifact: None,
        reasoning_producer: None,
    };
    {
        let display = app.active_display().unwrap();
        display.view.insert_or_replace(1, turn);
        display.view.request_to_turn.insert(7, 1);
    }
    app.rebuild_height_prefix();

    let v0 = app.active_display_ref().unwrap().turn_content_version(1);
    assert_eq!(v0, 0, "fresh turn starts at version 0");

    app.handle_tool_result_chunk(0, 7, "call-1".into(), b"one\n".to_vec());
    let v1 = app.active_display_ref().unwrap().turn_content_version(1);
    assert_eq!(v1, v0 + 1, "chunk bumps by one");

    app.handle_tool_result_chunk(0, 7, "call-1".into(), b"two\n".to_vec());
    let v2 = app.active_display_ref().unwrap().turn_content_version(1);
    assert_eq!(v2, v1 + 1, "every chunk bumps the version");

    // A replacement turn (the daemon's final TurnAppended) bumps too, so
    // a cached rendering of the accumulated version is never reused.
    let mut replacement = app.active_display_ref().unwrap().view.turns[&1].clone();
    replacement.tool_results[0].content.push_str("final\n");
    app.handle_turn_appended(0, 1, replacement);
    let v3 = app.active_display_ref().unwrap().turn_content_version(1);
    assert_eq!(v3, v2 + 1, "TurnAppended bumps the version");
}

#[test]
fn streaming_update_preserves_height_prefix_invariant() {
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 200;

    insert_turn(&mut app, 0, "a", "b");
    insert_turn(&mut app, 1, "c", "d");
    insert_turn(&mut app, 2, "e", "f");
    app.rebuild_height_prefix();

    let viewport = app.history_viewport;
    let display = app.active_display().unwrap();
    let old_prefix = display.height_prefix.clone();
    let old_heights = display.turn_heights.clone();

    // Stream content into turn 1.
    let turn = display.view.turns.get_mut(&1).unwrap();
    turn.assistant_text
        .as_mut()
        .unwrap()
        .push_str("\n\nlots of new content that should increase height");
    display.streaming_turn_index = Some(1);
    display.streaming_dirty = true;
    display.content_dirty = true;

    display.compute_total_height_and_markers(&viewport);

    // Verify invariant: height_prefix[i] == sum(turn_heights[0..=i]).
    let mut accum = 0usize;
    for i in 0..display.turn_heights.len() {
        accum += display.turn_heights[i];
        assert_eq!(
            display.height_prefix[i], accum,
            "invariant failed at index {i}: height_prefix[i] should equal cumulative turn_heights"
        );
    }

    // Turn 0 height unchanged.
    assert_eq!(
        display.turn_heights[0], old_heights[0],
        "turn 0 height should not change"
    );
    assert_eq!(
        display.height_prefix[0], old_prefix[0],
        "height_prefix[0] should not change"
    );
    // Markers must also be correct after the streaming update.
    assert_eq!(
        display.markers[0].content_line, 0,
        "marker[0] content_line should be 0"
    );
    assert_eq!(
        display.markers[1].content_line, display.turn_heights[0],
        "marker[1] content_line should equal turn 0 height"
    );
    assert_eq!(
        display.markers[2].content_line,
        display.turn_heights[0] + display.turn_heights[1],
        "marker[2] content_line should reflect updated turn 1 height"
    );
}

#[test]
fn streaming_answer_reuses_the_incremental_response_cache() {
    // Streaming a paragraph-heavy response one char at a time through the
    // fast path must keep the response's committed markdown (so the total
    // bytes re-parsed stays far below the naive sum of every prefix) and
    // still leave the rendered cache matching a whole-response render.
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 200;

    let turn = Turn {
        created_at: choreo_proto::TimestampMs::now(),
        undone: false,
        error: None,
        user_text: Some("ask".into()),
        assistant_text: None,
        assistant_reasoning: None,
        tool_calls: vec![],
        token_usage: None,
        tool_results: vec![],
        displayed_images: vec![],
        reasoning_artifact: None,
        reasoning_producer: None,
    };
    {
        let display = app.active_display().unwrap();
        display.view.insert_or_replace(1, turn);
        display.view.request_to_turn.insert(7, 1);
    }
    app.rebuild_height_prefix();

    let doc = "Hi.\n\n\
        One two three four five.\n\n\
        Six seven eight nine ten.\n\n\
        Eleven twelve thirteen.\n\n\
        Fourteen fifteen sixteen.\n";
    let mut naive_bytes = 0usize;
    let mut streamed = 0usize;
    for ch in doc.chars() {
        streamed += ch.len_utf8();
        naive_bytes += streamed;
        app.handle_request_stream(0, 7, OutputStream::Answer, Cow::Owned(ch.to_string()));
        app.compute_total_height_and_markers();
    }

    let display = app.active_display_ref().unwrap();
    let cache = display
        .streaming_response
        .as_ref()
        .expect("streaming fast path must populate the response cache");
    assert_eq!(cache.turn_id, 1, "cache is keyed to the streaming turn");
    assert!(
        cache.markdown.parsed_bytes * 2 < naive_bytes,
        "incremental parsed {} bytes vs {naive_bytes} naive — prefix not reused",
        cache.markdown.parsed_bytes
    );

    // The rendered cache entry must match a whole-response render exactly.
    let turn = &display.view.turns[&1];
    let cached = display.render_cache[0]
        .as_ref()
        .expect("streaming turn is cached");
    let full = render_turn_lines(turn, 71, 79, false, &[]);
    assert_eq!(&*cached.rendered.lines, full.lines.as_slice());
    assert_eq!(&*cached.rendered.joins, full.joins.as_slice());
}

#[test]
fn handle_started_sets_streaming_turn_index() {
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 200;

    // Pre-populate turns so visible_turn_ids exist.
    insert_turn(&mut app, 10, "user", "assistant");
    insert_turn(&mut app, 20, "another user", "another assistant");
    app.rebuild_height_prefix();

    {
        let display = app.active_display_ref().unwrap();
        assert_eq!(display.visible_turn_ids.len(), 2);
        assert_eq!(display.visible_turn_ids[0], 10);
        assert_eq!(display.visible_turn_ids[1], 20);
        assert!(display.streaming_turn_index.is_none());
    }

    // handle_started now requires session_id
    app.handle_started(0, 1, 10, 100);

    let display = app.active_display_ref().unwrap();
    assert_eq!(
        display.streaming_turn_index,
        Some(0),
        "should find turn 10 at index 0"
    );
    assert_eq!(display.view.request_to_turn.get(&1), Some(&10));
    assert!(display.active.contains(&1));

    // Idempotent: the requester sees the targeted `Started` reply AND the
    // broadcast `Started` for the same run, so applying it twice must not
    // change the display's live-stream state.
    app.handle_started(0, 1, 10, 100);
    let display = app.active_display_ref().unwrap();
    assert_eq!(display.streaming_turn_index, Some(0));
    assert_eq!(display.view.request_to_turn.get(&1), Some(&10));
    assert!(display.active.contains(&1));
}

#[test]
fn handle_done_fires_full_rebuild() {
    let mut app = test_app();
    // test_app's default display lives on session 0; treat it as the
    // attached session so `handle_done` routes to it.
    app.attached_session_id = Some(0);
    app.history_viewport.width = 80;
    app.history_viewport.height = 200;

    insert_turn(&mut app, 10, "user", "assistant");
    app.rebuild_height_prefix();
    {
        let display = app.active_display().unwrap();
        display.markers_dirty = false;
        display.streaming_turn_index = Some(0);
        display.streaming_dirty = false;
        display.content_dirty = false;
    }

    app.handle_done(0, 1, None, None);

    let display = app.active_display_ref().unwrap();
    assert!(
        display.streaming_turn_index.is_none(),
        "streaming_turn_index should be cleared"
    );
    assert!(
        display.markers_dirty,
        "markers_dirty should be set (full rebuild)"
    );
    assert!(display.content_dirty, "content_dirty should be set");
}

#[test]
fn handle_failed_clears_streaming() {
    let mut app = test_app();
    // test_app's default display lives on session 0; treat it as attached
    // so the connection-level (`None`) resolution keeps routing to it.
    app.attached_session_id = Some(0);
    {
        let display = app.active_display().unwrap();
        display.streaming_turn_index = Some(0);
        display.streaming_dirty = false;
        display.content_dirty = false;
        display.markers_dirty = false;
    }

    app.handle_failed(None, 1, "oops".into());

    let display = app.active_display_ref().unwrap();
    assert!(display.streaming_turn_index.is_none());
    assert!(display.error.is_some());
    assert!(display.markers_dirty, "markers_dirty should be set");
    assert!(display.content_dirty, "content_dirty should be set");
}

#[test]
fn handle_failed_connection_level_resolves_to_attached_session_without_phantom_display() {
    // A connection-level "no session attached" failure arrives with
    // `session_id: None`.  It must land in the attached session's display
    // and must NOT create a phantom display entry.
    let mut app = App::new();
    app.attached_session_id = Some(42);
    app.active_session_id = Some(42);

    app.handle_failed(None, 7, "no session attached".into());

    let display = app.display_for(42);
    assert_eq!(display.error.as_deref(), Some("no session attached"));
    assert!(
        !app.session_displays.contains_key(&0),
        "a connection-level failure must not create a phantom session-0 display"
    );
}

#[test]
fn handle_failed_for_request_failure_does_not_write_global_error() {
    // A request-level failure (a real session id) renders its full error
    // as the turn's red block in the transcript; the global status/error
    // bar must not print it a second time.  The per-session display still
    // records it.
    let mut app = test_app();
    app.attached_session_id = Some(42);
    app.active_session_id = Some(42);
    assert!(app.error.is_none());

    app.handle_failed(
        Some(42),
        1,
        "client error (402): Insufficient Balance".into(),
    );

    assert_eq!(
        app.error, None,
        "a request failure's transcript block must not be duplicated on the status bar"
    );
    assert_eq!(
        app.display_for(42).error.as_deref(),
        Some("client error (402): Insufficient Balance"),
        "the per-session display records the failure"
    );
}

#[test]
fn handle_failed_connection_level_writes_global_error_for_attached_session() {
    // A `session_id: None` envelope marks a connection-level failure (e.g.
    // "no session attached"), which has no turn to render an error block
    // in: the global status/error bar is its only surface.
    let mut app = App::new();
    app.attached_session_id = Some(42);
    app.active_session_id = Some(42);
    assert!(app.error.is_none());

    app.handle_failed(None, 7, "no session attached".into());

    assert_eq!(app.error.as_deref(), Some("no session attached"));
    assert_eq!(
        app.display_for(42).error.as_deref(),
        Some("no session attached")
    );
    assert!(
        !app.session_displays.contains_key(&0),
        "a connection-level failure must not create a phantom session-0 display"
    );
}

#[test]
fn handle_failed_connection_level_without_attached_session_still_writes_global_error() {
    // A connection-level rejection with no attached session to resolve to
    // has no display to update, but the user must still see it on the
    // status line — there is no transcript block for it.
    let mut app = test_app();
    app.attached_session_id = None;
    assert!(app.error.is_none());

    app.handle_failed(None, 9, "no session attached".into());

    assert_eq!(app.error.as_deref(), Some("no session attached"));
}

#[test]
fn handle_failed_for_background_session_does_not_write_global_error() {
    // The TUI subscribes to all activity, so a background session's
    // request failure arrives too.  It must be recorded on that session's
    // display but must not clobber the global status/error bar the user
    // is looking at (same gating as the ModelSelected / ReasoningEffortSet
    // arms).
    let mut app = test_app();
    app.attached_session_id = Some(42);
    app.active_session_id = Some(42);
    assert!(app.error.is_none());

    app.handle_failed(Some(99), 3, "background failure".into());

    assert_eq!(
        app.error, None,
        "background failure must not write the global error bar"
    );
    assert_eq!(
        app.display_for(99).error.as_deref(),
        Some("background failure")
    );
}

// ── ModelSelectorState ──

fn selector_with_models(models: &[&str]) -> ModelSelectorState {
    let mut sel = ModelSelectorState::new();
    sel.open();
    sel.apply_models(models.iter().map(|s| (*s).to_string()).collect(), None);
    sel
}

#[test]
#[expect(clippy::assert_is_empty)] // clearer than assert_eq! against ""
fn model_selector_open_resets_state_and_marks_loading() {
    let mut sel = ModelSelectorState::new();
    sel.all_models = vec!["a".into()];
    sel.selected = Some("a".into());
    sel.filter.text = "stale".to_string();
    sel.filter.cursor = 5;
    sel.focused = 3;
    sel.scroll = 2;
    sel.error = Some("old error".into());

    sel.open();

    assert!(sel.is_open());
    assert!(sel.loading);
    assert!(sel.filter.text.is_empty());
    assert_eq!(sel.focused, 0);
    assert_eq!(sel.scroll, 0);
    assert!(sel.error.is_none());
}

#[test]
fn model_selector_close_keeps_model_list() {
    let mut sel = selector_with_models(&["a", "b"]);
    sel.close();

    assert!(!sel.is_open());
    assert_eq!(sel.all_models.len(), 2, "cached list survives close");
}

#[test]
fn model_selector_apply_models_preselects_current() {
    let mut sel = ModelSelectorState::new();
    sel.open();
    sel.apply_models(
        vec![
            "gpt-4o".into(),
            "gpt-4o-mini".into(),
            "gpt-3.5-turbo".into(),
        ],
        Some("gpt-4o-mini".into()),
    );

    assert!(!sel.loading);
    assert_eq!(sel.focused, 1, "highlight lands on the active model");
    assert_eq!(sel.highlighted().as_deref(), Some("gpt-4o-mini"));
}

#[test]
fn model_selector_apply_models_falls_back_to_top_when_selected_missing() {
    let mut sel = ModelSelectorState::new();
    sel.open();
    sel.apply_models(vec!["a".into(), "b".into()], Some("missing".into()));

    assert_eq!(sel.focused, 0);
    assert_eq!(sel.highlighted().as_deref(), Some("a"));
}

#[test]
fn model_selector_filter_matches_case_insensitive_substring() {
    let mut sel = selector_with_models(&["gpt-4o", "GPT-4O-MINI", "claude-3"]);
    sel.filter.text = "gpt".to_string();
    sel.filter.cursor = 3;

    let filtered = sel.filtered();
    assert_eq!(filtered, vec!["gpt-4o", "GPT-4O-MINI"]);
}

#[test]
fn model_selector_empty_filter_returns_all() {
    let sel = selector_with_models(&["a", "b", "c"]);
    assert_eq!(sel.filtered(), vec!["a", "b", "c"]);
}

#[test]
#[expect(clippy::assert_is_empty)] // clearer than assert_eq! against []
fn model_selector_no_match_returns_empty() {
    let mut sel = selector_with_models(&["a", "b"]);
    sel.filter.text = "zzz".to_string();
    sel.filter.cursor = 3;
    assert!(sel.filtered().is_empty());
}

#[test]
fn model_selector_focus_clamps_when_filter_narrows_list() {
    let mut sel = selector_with_models(&["a", "b", "c"]);
    sel.focused = 2;
    // Narrow to a single row: the highlight must not point past the end.
    sel.filter.text = "a".to_string();
    sel.filter.cursor = 1;
    sel.clamp_focus();
    assert_eq!(sel.focused, 0);
}

#[test]
fn model_selector_move_up_down_clamped() {
    let mut sel = selector_with_models(&["a", "b", "c"]);
    sel.move_down();
    assert_eq!(sel.focused, 1);
    sel.move_down();
    sel.move_down();
    assert_eq!(sel.focused, 2, "move_down clamps at the last row");
    sel.move_up();
    assert_eq!(sel.focused, 1);
    sel.move_up();
    sel.move_up();
    assert_eq!(sel.focused, 0, "move_up clamps at the first row");
}

#[test]
fn model_selector_window_keeps_focus_visible() {
    let mut sel = selector_with_models(&["a", "b", "c", "d", "e"]);
    sel.focused = 4;
    let filtered = sel.filtered();
    let (start, count) = sel.window(&filtered, 3);
    assert_eq!((start, count), (2, 3), "window slides down to reveal focus");
    assert!(sel.focused >= start && sel.focused < start + count);
}

#[test]
fn model_selector_window_pulls_up_when_focus_above() {
    let mut sel = selector_with_models(&["a", "b", "c", "d", "e"]);
    sel.scroll = 4;
    sel.focused = 1;
    let filtered = sel.filtered();
    let (start, _) = sel.window(&filtered, 3);
    assert_eq!(start, 1, "window pulls up so focus is visible");
}

#[test]
fn model_selector_window_empty_list_returns_zero() {
    let sel = selector_with_models(&[]);
    assert_eq!(sel.window(&sel.filtered(), 5), (0, 0));
    assert!(sel.highlighted().is_none());
}

#[test]
fn model_selector_window_is_pure_and_idempotent() {
    // The renderer calls `window` during terminal.draw(), which must
    // never mutate scroll/focus state.  Verify repeated calls return
    // identical results and leave the fields untouched.
    let mut sel = selector_with_models(&["a", "b", "c", "d", "e"]);
    sel.scroll = 3;
    sel.focused = 4;
    let before_scroll = sel.scroll;
    let before_focused = sel.focused;

    let filtered = sel.filtered();
    let first = sel.window(&filtered, 3);
    let second = sel.window(&filtered, 3);

    assert_eq!(first, second, "window must be deterministic");
    assert_eq!(sel.scroll, before_scroll, "window must not mutate scroll");
    assert_eq!(sel.focused, before_focused, "window must not mutate focus");
}

#[test]
fn model_selector_submit_returns_highlighted_and_closes() {
    let mut sel = selector_with_models(&["a", "b"]);
    sel.move_down();
    let model = sel.submit();
    assert_eq!(model.as_deref(), Some("b"));
    assert!(!sel.is_open(), "submit closes the selector");
}

#[test]
fn model_selector_submit_empty_returns_none() {
    let mut sel = selector_with_models(&[]);
    assert!(sel.submit().is_none());
    assert!(!sel.is_open());
}

#[test]
#[expect(clippy::assert_is_empty)] // clearer than assert_eq! against ""
fn model_selector_filter_key_consumes_chars_and_backspace() {
    let mut sel = selector_with_models(&["gpt-4o", "claude-3"]);
    sel.filter_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE));
    assert_eq!(sel.filter.text, "g");
    assert_eq!(sel.filtered(), vec!["gpt-4o"]);

    sel.filter_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
    assert!(sel.filter.text.is_empty());
    assert_eq!(sel.filtered().len(), 2);
}

#[test]
fn model_selector_filter_key_ignores_enter() {
    let mut sel = selector_with_models(&["a"]);
    sel.filter_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(sel.is_open());
}

#[test]
fn model_selector_apply_error_records_and_clears_loading() {
    let mut sel = ModelSelectorState::new();
    sel.open();
    sel.apply_error("no credential".to_string());
    assert!(!sel.loading);
    assert_eq!(sel.error.as_deref(), Some("no credential"));
}

// ── OSC 7501 program status / OSC 2 window title ────────────────────────

#[test]
fn desired_records_include_attached_and_active_background_only() {
    let mut app = test_app();
    app.attached_session_id = Some(1);
    app.attached_status = Some(SessionStatus::Inference);
    app.session_mgr.all = vec![
        {
            let mut s = make_session(1, "attached");
            s.status = SessionStatus::Inference;
            s
        },
        {
            let mut s = make_session(2, "busy background");
            s.status = SessionStatus::ToolCall("shell".into());
            s
        },
        {
            let mut s = make_session(3, "idle background");
            s.status = SessionStatus::Inactive;
            s
        },
    ];

    let desired = app.desired_status_records();
    let ids: Vec<u64> = desired.iter().map(|(id, _)| *id).collect();
    assert!(ids.contains(&1), "the attached session is always present");
    assert!(ids.contains(&2), "an active background session is present");
    assert!(
        !ids.contains(&3),
        "an idle background session must be omitted so a stale record is cleared"
    );

    // The ToolCall msg carries the tool NAME only (base64 of "shell").
    let bg = desired
        .iter()
        .find(|(id, _)| *id == 2)
        .map(|(_, seq)| seq)
        .expect("background record");
    assert!(bg.contains("state=working"), "got {bg}");
    assert!(bg.contains(":msg=c2hlbGw="), "tool name as msg, got {bg}");
}

#[test]
fn done_override_wins_and_survives_the_trailing_idle() {
    let mut app = test_app();
    app.attached_session_id = Some(1);
    app.attached_status = Some(SessionStatus::Inference);
    app.session_mgr.all = vec![{
        let mut s = make_session(1, "t");
        s.status = SessionStatus::Inference;
        s
    }];

    app.handle_done(1, 7, None, None);
    // The daemon broadcasts an idle status right after the turn finishes; the
    // done outcome must survive it.
    app.handle_session_status_changed(1, &SessionStatus::Inactive, 1);

    let seq = app
        .desired_status_records()
        .into_iter()
        .find(|(id, _)| *id == 1)
        .map(|(_, seq)| seq)
        .expect("attached record");
    assert!(
        seq.contains("state=done"),
        "done survives the idle, got {seq}"
    );
    assert!(
        !seq.contains(":msg="),
        "a terminal outcome carries no tool msg"
    );
}

#[test]
fn fresh_active_status_clears_the_override() {
    let mut app = test_app();
    app.attached_session_id = Some(1);
    app.session_mgr.all = vec![make_session(1, "t")];

    app.handle_done(1, 7, None, None);
    assert!(app.term_status_override.contains_key(&1));

    // A new turn began (status went active): the old outcome is dropped.
    app.handle_session_status_changed(1, &SessionStatus::Inference, 2);
    assert!(!app.term_status_override.contains_key(&1));
}

#[test]
fn failed_and_cancelled_records() {
    let mut app = test_app();
    app.attached_session_id = Some(1);
    app.attached_status = Some(SessionStatus::Inference);
    app.session_mgr.all = vec![{
        let mut s = make_session(1, "t");
        s.status = SessionStatus::Inference;
        s
    }];

    app.handle_cancelled(Some(1), 8);
    let seq = app
        .desired_status_records()
        .into_iter()
        .find(|(id, _)| *id == 1)
        .map(|(_, seq)| seq)
        .expect("attached record");
    assert!(seq.contains("state=idle"), "got {seq}");
    assert!(
        app.display_for(1).error.is_none(),
        "a user cancel must not record an error"
    );
    assert!(
        app.error.is_none(),
        "a user cancel must not write the global error bar"
    );

    // A real failure still reports `error` and records its message.
    app.handle_failed(Some(1), 7, "boom".into());
    let seq = app
        .desired_status_records()
        .into_iter()
        .find(|(id, _)| *id == 1)
        .map(|(_, seq)| seq)
        .expect("attached record");
    assert!(seq.contains("state=error"), "got {seq}");
    assert_eq!(app.display_for(1).error.as_deref(), Some("boom"));
}

#[test]
fn window_title_reflects_the_attached_session() {
    let mut app = test_app();
    assert_eq!(app.window_title(), "choreo-tui");

    app.attached_session_id = Some(1);
    app.session_mgr.all = vec![make_session(1, "Fix the parser")];
    assert_eq!(app.window_title(), "choreo-tui — Fix the parser");
}
