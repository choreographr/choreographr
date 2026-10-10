//! Session-manager list model: recency ordering, selection retention, the
//! pinned/archived partitions, flag application, status-change reordering, and
//! `remove_session` cleanup.

use super::make_session;
use crate::state::{SessionDetailData, SessionManagerState, SessionManagerView};
use crate::test_util::test_app;
use choreo_proto::SessionStatus;

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
