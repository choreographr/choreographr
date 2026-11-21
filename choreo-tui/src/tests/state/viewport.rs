//! Terminal-resize viewport classification and the cache/heights-dirty handling
//! it drives.

use crate::state::{
    RenderCacheKey, RenderedCache, RenderedTurn, ViewportChange, classify_viewport_change,
};
use crate::tests::support::test_app;
use ratatui::text::Line;
use std::sync::Arc;

// ── update_viewport_from_terminal_size ──

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
            response_start_idx: None,
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
    // status/input chrome growing or shrinking.
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
fn height_only_change_without_images_skips_the_height_rebuild() {
    // A height-only change — a real vertical resize, or the chrome
    // (status/error line, input box) growing or shrinking — re-wraps nothing, so
    // it must NOT wipe the render cache (doing that forced a full O(session)
    // re-render, the input delay a large session showed after a copy set the
    // status and on the next keystroke that cleared it).  Turn heights depend on
    // the viewport height ONLY through the image-block reserve, so a session
    // with no image-bearing turn needs no height rebuild at all: the height
    // model stays valid and only the scrollbar's marker slots rescale (deferred
    // to the next draw).  This keeps a chrome-height change off the O(turns)
    // rebuild path for every text-only session.
    let mut app = test_app();

    app.history_viewport.width = 79;
    app.history_viewport.height = 20;

    app.last_terminal_size = Some((80, 30));
    app.terminal_resized = false;

    {
        let display = app.active_display().unwrap();
        display.content_dirty = true;
        display.markers_dirty = false;
        display.any_image_turns = false;
        display.render_cache = vec![Some(dummy_cache_entry())];
    }

    app.update_viewport_from_terminal_size();

    let display = app.active_display_ref().unwrap();
    assert!(
        !display.markers_dirty,
        "no image turns: heights are unchanged, so no height rebuild is needed"
    );
    assert!(
        display.marker_slots_dirty,
        "the scrollbar's marker slots still rescale to the new virtual track"
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

#[test]
fn height_only_change_with_images_recomputes_heights() {
    // When a turn DOES reserve an image block, its height derives from the
    // viewport height, so a height change must recompute the height model (a
    // cache-hit rebuild — the render cache is still preserved).
    let mut app = test_app();

    app.history_viewport.width = 79;
    app.history_viewport.height = 20;

    app.last_terminal_size = Some((80, 30));
    app.terminal_resized = false;

    {
        let display = app.active_display().unwrap();
        display.content_dirty = true;
        display.markers_dirty = false;
        display.any_image_turns = true;
        display.render_cache = vec![Some(dummy_cache_entry())];
    }

    app.update_viewport_from_terminal_size();

    let display = app.active_display_ref().unwrap();
    assert!(
        display.markers_dirty,
        "an image-bearing turn's height depends on the viewport height, so rebuild"
    );
    assert!(
        display.render_cache.iter().all(Option::is_some),
        "the rebuild is a cache hit; the render cache is preserved"
    );
}
