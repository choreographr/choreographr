//! Terminal-resize viewport classification and the cache/heights-dirty handling
//! it drives.

use crate::state::{
    RenderCacheKey, RenderedCache, RenderedTurn, ViewportChange, classify_viewport_change,
};
use crate::test_util::test_app;
use ratatui::text::Line;
use std::sync::Arc;

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
