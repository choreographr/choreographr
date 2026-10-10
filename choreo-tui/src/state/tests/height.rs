//! `compute_total_height_and_markers` scroll and anchor preservation across
//! content growth and removal.

use super::insert_turn;
use crate::test_util::test_app;

// ── compute_total_height_and_markers scroll preservation ──

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
