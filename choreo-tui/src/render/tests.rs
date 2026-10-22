//! Unit tests for the pieces that stayed in `render/mod.rs` (the mouse
//! hit-tests) plus the `history` clipping helper; the formatters and the image
//! slot label they also cover are reached through `super`'s re-exports.

use super::*;
use crate::markdown_render::{
    LineChrome, LineJoin, RenderedTurnLines, compute_visual_offsets, lines_height,
};
use crate::state::{RenderCacheKey, RenderedCache, RenderedTurn, cached_or_compute_lines};
use ratatui::text::Line;
use std::sync::Arc;

use super::history::clipped_area;

/// Wrap a line vector as a rendered turn with no headers.
fn rendered(lines: Vec<Line<'static>>) -> RenderedTurnLines {
    // Every line gets a full-width content range so the cache-alignment
    // invariant asserted in `cached_or_compute_lines` holds for test
    // fixtures too (each entry must align with `lines`).
    let joins = lines.iter().map(|_| LineJoin::Break).collect();
    let content_ranges = lines.iter().map(|l| Some((0, l.width()))).collect();
    // Empty chrome for every row keeps the third aligned buffer in
    // lockstep with `lines` (see the alignment asserts).
    let chrome_ranges = lines.iter().map(|_| LineChrome::default()).collect();
    RenderedTurnLines {
        lines,
        joins,
        content_ranges,
        chrome_ranges,
        reasoning_header_idx: None,
        tool_result_header_idxs: Vec::new(),
        response_start_idx: None,
    }
}

/// Build a cache entry with the given key/output pieces, defaulting the
/// rest, so tests can focus on the field under test.
fn cache_entry(
    key: RenderCacheKey,
    lines: Arc<[Line<'static>]>,
    height: usize,
    visual_offsets: Arc<[usize]>,
) -> RenderedCache {
    // Derive the copy-join and content-range metadata from the lines so
    // fixtures built through this helper keep the parallel-array
    // alignment invariant the selection machinery relies on (every
    // renderer-produced entry and the debug_asserts in
    // `cached_or_compute_lines` expect lines/joins/content_ranges to
    // have identical lengths).
    let joins = lines.iter().map(|_| LineJoin::Break).collect::<Vec<_>>();
    let content_ranges = lines
        .iter()
        .map(|l| Some((0, l.width())))
        .collect::<Vec<_>>();
    // Keep the chrome buffer aligned with the lines too (empty rows).
    let chrome_ranges = lines
        .iter()
        .map(|_| LineChrome::default())
        .collect::<Vec<_>>();
    RenderedCache {
        key,
        rendered: RenderedTurn {
            lines,
            height,
            visual_offsets,
            joins: Arc::from(joins),
            content_ranges: Arc::from(content_ranges),
            chrome_ranges: Arc::from(chrome_ranges),
            reasoning_header_idx: None,
            tool_result_header_idxs: Vec::new(),
            response_start_idx: None,
        },
    }
}

/// A key for a one-line entry rendered at content width 80 in a
/// viewport of width 100, reasoning collapsed, no tool results, at
/// content version 0 (no mutations recorded).
fn base_key() -> RenderCacheKey {
    RenderCacheKey {
        turn_id: 0,
        width: 80,
        viewport_width: 100,
        reasoning_expanded: false,
        tool_results_collapsed: Vec::new(),
        content_version: 0,
    }
}

// ── mouse_in_history_box ──

#[test]
fn mouse_in_history_box_inside() {
    assert!(mouse_in_history_box(5, 10, 80, 24));
}

#[test]
fn mouse_in_history_box_column_too_large() {
    assert!(!mouse_in_history_box(80, 10, 80, 24));
}

#[test]
fn mouse_in_history_box_row_too_large() {
    assert!(!mouse_in_history_box(5, 24, 80, 24));
}

#[test]
fn mouse_in_history_box_both_out_of_bounds() {
    assert!(!mouse_in_history_box(99, 99, 80, 24));
}

#[test]
fn mouse_in_history_box_zero_height_viewport() {
    assert!(!mouse_in_history_box(0, 0, 80, 0));
}

#[test]
fn mouse_in_history_box_zero_width_viewport() {
    assert!(!mouse_in_history_box(0, 0, 0, 24));
}

// ── mouse_in_scrollbar_column ──

#[test]
fn mouse_in_scrollbar_column_on_scrollbar() {
    assert!(mouse_in_scrollbar_column(80, 10, 80, 24));
}

#[test]
fn mouse_in_scrollbar_column_before_scrollbar() {
    assert!(!mouse_in_scrollbar_column(79, 10, 80, 24));
}

#[test]
fn mouse_in_scrollbar_column_after_scrollbar() {
    assert!(!mouse_in_scrollbar_column(81, 10, 80, 24));
}

#[test]
fn mouse_in_scrollbar_column_row_too_large() {
    assert!(!mouse_in_scrollbar_column(80, 24, 80, 24));
}

#[test]
fn mouse_in_scrollbar_column_zero_height() {
    assert!(!mouse_in_scrollbar_column(80, 0, 80, 0));
}

// ── clipped_area ──

#[test]
fn clipped_area_skip_when_rows_to_skip_equals_full_height() {
    let mut skip = 10usize;
    let mut remain = 20usize;
    let mut y = 30u16;
    let result = clipped_area(10, &mut skip, &mut remain, &mut y);
    assert!(
        result.is_none(),
        "should skip when rows_to_skip >= full_height"
    );
    assert_eq!(skip, 0, "rows_to_skip should be decremented by full_height");
    assert_eq!(remain, 20, "rows_remaining should be unchanged");
    assert_eq!(y, 30, "y should be unchanged");
}

#[test]
fn clipped_area_skip_when_rows_to_skip_exceeds_full_height() {
    let mut skip = 15usize;
    let mut remain = 20usize;
    let mut y = 30u16;
    let result = clipped_area(10, &mut skip, &mut remain, &mut y);
    assert!(
        result.is_none(),
        "should skip when rows_to_skip > full_height"
    );
    assert_eq!(skip, 5, "rows_to_skip should be decremented by full_height");
}

#[test]
fn clipped_area_partial_visibility_at_boundary() {
    let mut skip = 7usize;
    let mut remain = 20usize;
    let mut y = 30u16;
    let result = clipped_area(10, &mut skip, &mut remain, &mut y);
    let (top_line, visible_height) = result.expect("should be visible");
    // bottom_line = 10 - 7 = 3, top_line = 3 - 3 = 0, visible = 3
    assert_eq!(
        top_line, 0,
        "top_line should be at start of non-skipped region"
    );
    assert_eq!(visible_height, 3, "should show remaining 3 lines");
    assert_eq!(skip, 0, "rows_to_skip should be reset to 0");
    assert_eq!(
        remain, 17,
        "rows_remaining should decrease by visible_height"
    );
    assert_eq!(y, 27, "y should decrease by visible_height");
}

#[test]
fn clipped_area_partial_visibility_skip_some_rows_within_turn() {
    let mut skip = 3usize;
    let mut remain = 10usize;
    let mut y = 50u16;
    let result = clipped_area(10, &mut skip, &mut remain, &mut y);
    let (top_line, visible_height) = result.expect("should be visible");
    // bottom_line = 10 - 3 = 7, top_line = 7 - 7 = 0, visible = 7...
    // Wait: visible_height = min(10-3, 10) = 7, bottom_line = 10-3 = 7, top_line = 7-7 = 0
    assert_eq!(top_line, 0);
    assert_eq!(visible_height, 7);
    assert_eq!(skip, 0);
    assert_eq!(remain, 3);
    assert_eq!(y, 43);
}

#[test]
fn clipped_area_full_turn_within_viewport() {
    let mut skip = 0usize;
    let mut remain = 20usize;
    let mut y = 40u16;
    let result = clipped_area(10, &mut skip, &mut remain, &mut y);
    let (top_line, visible_height) = result.expect("should show all");
    assert_eq!(top_line, 0, "top_line should be 0 when nothing is skipped");
    assert_eq!(visible_height, 10, "should show full height");
    assert_eq!(y, 30, "y should decrease by full height");
}

#[test]
fn clipped_area_clamps_to_rows_remaining() {
    let mut skip = 0usize;
    let mut remain = 3usize;
    let mut y = 20u16;
    let result = clipped_area(10, &mut skip, &mut remain, &mut y);
    let (top_line, visible_height) = result.expect("should clip to remaining");
    // visible_height = min(10, 3) = 3
    // bottom_line = 10 - 0 = 10, top_line = 10 - 3 = 7
    assert_eq!(
        top_line, 7,
        "top_line should be offset from bottom by visible_height"
    );
    assert_eq!(visible_height, 3, "should be clamped by rows_remaining");
    assert_eq!(remain, 0);
    assert_eq!(y, 17);
}

#[test]
fn clipped_area_zero_rows_remaining_returns_none() {
    let mut skip = 0usize;
    let mut remain = 0usize;
    let mut y = 10u16;
    let result = clipped_area(10, &mut skip, &mut remain, &mut y);
    assert!(result.is_none(), "should return None when no rows remain");
    assert_eq!(y, 10, "y should be unchanged");
}

#[test]
fn clipped_area_skip_exactly_full_height_then_show_next() {
    let mut skip = 6usize;
    let mut remain = 10usize;
    let mut y = 30u16;
    // First turn: full height = 6, rows_to_skip = 6 → skip entirely
    let result1 = clipped_area(6, &mut skip, &mut remain, &mut y);
    assert!(result1.is_none());
    assert_eq!(skip, 0);
    // Second turn: full height = 4, rows_to_skip = 0 → show fully
    let result2 = clipped_area(4, &mut skip, &mut remain, &mut y);
    let (_, visible) = result2.unwrap();
    assert_eq!(visible, 4);
    assert_eq!(y, 26);
}

// ── cached_or_compute_lines ──

/// Helper: a simple compute function that returns a single short line.
fn compute_one_line() -> RenderedTurnLines {
    rendered(vec![Line::from("hello")])
}

#[test]
fn cached_or_compute_lines_cache_miss_stores_result() {
    let mut cache = vec![None];
    let rendered = cached_or_compute_lines(&mut cache, 0, &base_key(), compute_one_line);
    assert_eq!(rendered.lines.len(), 1, "should return computed lines");
    assert_eq!(
        rendered.height, 1,
        "single line at any viewport width has height 1"
    );
    assert_eq!(
        &*rendered.visual_offsets,
        &[1],
        "single short line should occupy one visual row"
    );
    assert_eq!(
        rendered.reasoning_header_idx, None,
        "no reasoning → no header index"
    );
    // Cache should be filled
    let cached = cache[0].as_ref().unwrap();
    assert_eq!(cached.key.width, 80);
    assert_eq!(cached.key.viewport_width, 100);
    assert_eq!(cached.rendered.height, 1);
    assert_eq!(cached.rendered.lines.len(), 1);
    assert_eq!(&*cached.rendered.visual_offsets, &[1]);
    assert!(
        !cached.key.reasoning_expanded,
        "cache should record the reasoning state it was rendered with"
    );
}

#[test]
fn cached_or_compute_lines_cache_hit_returns_stored_height() {
    let mut cache = vec![Some(cache_entry(
        base_key(),
        Arc::from(vec![Line::from("cached")]),
        42,
        Arc::from([99]),
    ))];
    // Cache hit — should return stored height without recomputing
    let rendered = cached_or_compute_lines(&mut cache, 0, &base_key(), || {
        panic!("should not be called on cache hit")
    });
    assert_eq!(rendered.height, 42, "should return cached height");
    assert_eq!(rendered.lines.len(), 1, "should return cached lines");
    assert_eq!(rendered.lines[0], Line::from("cached"));
    assert_eq!(
        &*rendered.visual_offsets,
        &[99],
        "should return cached offsets"
    );
    assert_eq!(
        rendered.reasoning_header_idx, None,
        "should return the cached header index"
    );
}

#[test]
fn cached_or_compute_lines_cache_hit_arc_shares_allocation() {
    let stored = Arc::from(vec![Line::from("shared")]);
    let stored_ptr = Arc::as_ptr(&stored);
    let mut cache = vec![Some(cache_entry(base_key(), stored, 7, Arc::from([1])))];
    let returned = cached_or_compute_lines(&mut cache, 0, &base_key(), || {
        panic!("should not recompute")
    });
    assert_eq!(
        Arc::as_ptr(&returned.lines),
        stored_ptr,
        "returned Arc should point to the same allocation as cache entry"
    );
}

#[test]
fn cached_or_compute_lines_width_mismatch_recomputes() {
    let mut cache = vec![Some(cache_entry(
        RenderCacheKey {
            width: 40, // different from requested width 80
            ..base_key()
        },
        Arc::from(vec![Line::from("stale")]),
        99,
        Arc::from([1]),
    ))];
    let compute_called = std::cell::Cell::new(false);
    let rendered = cached_or_compute_lines(&mut cache, 0, &base_key(), || {
        compute_called.set(true);
        rendered(vec![Line::from("fresh")])
    });
    assert!(compute_called.get(), "should recompute on width mismatch");
    assert_eq!(rendered.lines[0], Line::from("fresh"));
    // Height of a single "fresh" line at viewport width 100 is 1
    assert_eq!(rendered.height, 1);
    assert_eq!(
        &*rendered.visual_offsets,
        &[1],
        "offsets recomputed for fresh lines"
    );
    // Cache should be updated
    let cached = cache[0].as_ref().unwrap();
    assert_eq!(cached.key.width, 80);
    assert_eq!(cached.key.viewport_width, 100);
    assert_eq!(cached.rendered.lines[0], Line::from("fresh"));
    assert_eq!(&*cached.rendered.visual_offsets, &[1]);
}

#[test]
fn cached_or_compute_lines_viewport_width_mismatch_recomputes() {
    let mut cache = vec![Some(cache_entry(
        RenderCacheKey {
            viewport_width: 40, // different from requested viewport_width 100
            ..base_key()
        },
        Arc::from(vec![Line::from("stale")]),
        99,
        Arc::from([1]),
    ))];
    let compute_called = std::cell::Cell::new(false);
    let rendered = cached_or_compute_lines(&mut cache, 0, &base_key(), || {
        compute_called.set(true);
        rendered(vec![Line::from("fresh")])
    });
    assert!(
        compute_called.get(),
        "should recompute on viewport_width mismatch"
    );
    assert_eq!(rendered.lines[0], Line::from("fresh"));
    let cached = cache[0].as_ref().unwrap();
    assert_eq!(cached.key.viewport_width, 100);
}

#[test]
fn cached_or_compute_lines_turn_id_mismatch_recomputes() {
    let mut cache = vec![Some(cache_entry(
        RenderCacheKey {
            turn_id: 7, // cached entry is for turn 7
            ..base_key()
        },
        Arc::from(vec![Line::from("stale")]),
        99,
        Arc::from([1]),
    ))];
    // Request turn_id 42 at the same index — should be a miss.
    let compute_called = std::cell::Cell::new(false);
    let rendered = cached_or_compute_lines(
        &mut cache,
        0,
        &RenderCacheKey {
            turn_id: 42,
            ..base_key()
        },
        || {
            compute_called.set(true);
            rendered(vec![Line::from("fresh")])
        },
    );
    assert!(compute_called.get(), "should recompute on turn_id mismatch");
    assert_eq!(rendered.lines[0], Line::from("fresh"));
    let cached = cache[0].as_ref().unwrap();
    assert_eq!(
        cached.key.turn_id, 42,
        "cache entry should be updated to new turn_id"
    );
}

#[test]
fn cached_or_compute_lines_out_of_range_index_does_not_store() {
    let mut cache = vec![None]; // length 1
    // Request index 5 which is out of range
    let rendered = cached_or_compute_lines(&mut cache, 5, &base_key(), compute_one_line);
    assert_eq!(
        rendered.lines.len(),
        1,
        "should still return computed result"
    );
    assert_eq!(rendered.height, 1);
    assert_eq!(&*rendered.visual_offsets, &[1]);
    // Cache should remain unchanged (all entries still None since index 5 doesn't exist)
    assert!(
        cache[0].is_none(),
        "original cache entry should be untouched"
    );
}

#[test]
fn cached_or_compute_lines_height_matches_lines_height() {
    let mut cache = vec![None];
    let lines = vec![
        Line::from("line one"),
        Line::from("line two"),
        Line::from("line three"),
    ];
    let expected_h = lines_height(&lines, 80);
    let rendered = cached_or_compute_lines(
        &mut cache,
        0,
        &RenderCacheKey {
            width: 70, // content_width
            viewport_width: 80,
            ..base_key()
        },
        || rendered(lines.clone()),
    );
    assert_eq!(
        rendered.height, expected_h,
        "returned height should match lines_height"
    );
    assert_eq!(
        *rendered.visual_offsets.last().unwrap(),
        expected_h,
        "last offset should equal total visual height"
    );
    let cached = cache[0].as_ref().unwrap();
    assert_eq!(
        cached.rendered.height, expected_h,
        "stored height should match"
    );
    assert_eq!(
        *cached.rendered.visual_offsets.last().unwrap(),
        expected_h,
        "stored last offset should equal total height"
    );
}

#[test]
fn cached_or_compute_lines_none_slot_treated_as_miss() {
    let mut cache: Vec<Option<RenderedCache>> = vec![None];
    let compute_called = std::cell::Cell::new(false);
    let rendered = cached_or_compute_lines(&mut cache, 0, &base_key(), || {
        compute_called.set(true);
        compute_one_line()
    });
    assert!(compute_called.get(), "should compute when slot is None");
    assert!(cache[0].is_some(), "should fill the slot");
    drop(rendered.lines);
}

#[test]
fn cached_or_compute_lines_reasoning_expanded_mismatch_recomputes() {
    let mut cache = vec![Some(cache_entry(
        RenderCacheKey {
            reasoning_expanded: false, // cached as collapsed
            ..base_key()
        },
        Arc::from(vec![Line::from("stale")]),
        99,
        Arc::from([1]),
    ))];
    // Request with reasoning expanded — should be a miss.
    let compute_called = std::cell::Cell::new(false);
    let rendered = cached_or_compute_lines(
        &mut cache,
        0,
        &RenderCacheKey {
            reasoning_expanded: true,
            ..base_key()
        },
        || {
            compute_called.set(true);
            rendered(vec![Line::from("fresh")])
        },
    );
    assert!(
        compute_called.get(),
        "should recompute when reasoning_expanded differs"
    );
    assert_eq!(rendered.lines[0], Line::from("fresh"));
    let cached = cache[0].as_ref().unwrap();
    assert!(
        cached.key.reasoning_expanded,
        "cache entry should record the new reasoning state"
    );
}

#[test]
fn cached_or_compute_lines_tool_results_collapsed_mismatch_recomputes() {
    let mut cache = vec![Some(RenderedCache {
        key: RenderCacheKey {
            tool_results_collapsed: vec![true], // cached as collapsed
            ..base_key()
        },
        rendered: RenderedTurn {
            lines: Arc::from(vec![Line::from("stale")]),
            height: 99,
            visual_offsets: Arc::from([1]),
            joins: Arc::from([LineJoin::Break]),
            content_ranges: Arc::from([Some((0, 5))]),
            chrome_ranges: Arc::from([LineChrome::default()]),
            reasoning_header_idx: None,
            tool_result_header_idxs: vec![0],
            response_start_idx: None,
        },
    })];
    // Request with the result expanded — should be a miss.
    let compute_called = std::cell::Cell::new(false);
    let rendered = cached_or_compute_lines(
        &mut cache,
        0,
        &RenderCacheKey {
            tool_results_collapsed: vec![false],
            ..base_key()
        },
        || {
            compute_called.set(true);
            rendered(vec![Line::from("fresh")])
        },
    );
    assert!(
        compute_called.get(),
        "should recompute when tool_results_collapsed differs"
    );
    assert_eq!(rendered.lines[0], Line::from("fresh"));
    let cached = cache[0].as_ref().unwrap();
    assert_eq!(
        cached.key.tool_results_collapsed,
        vec![false],
        "cache entry should record the new collapse state"
    );
}

#[test]
fn cached_or_compute_lines_content_version_mismatch_recomputes() {
    // The cache key carries a per-turn content version so a rebuild can
    // never reuse a rendering of a turn whose content grew behind the
    // key's other fields (a tool-result chunk appended between a rebuild
    // and the streaming fast path).  Same turn/widths/collapse state, but
    // a newer content version — must be a miss.
    let mut cache = vec![Some(cache_entry(
        RenderCacheKey {
            content_version: 1, // cached from an earlier chunk
            ..base_key()
        },
        Arc::from(vec![Line::from("stale")]),
        99,
        Arc::from([1]),
    ))];
    // Request with a bumped content version — should be a miss.
    let compute_called = std::cell::Cell::new(false);
    let rendered = cached_or_compute_lines(
        &mut cache,
        0,
        &RenderCacheKey {
            content_version: 2,
            ..base_key()
        },
        || {
            compute_called.set(true);
            rendered(vec![Line::from("fresh")])
        },
    );
    assert!(
        compute_called.get(),
        "should recompute when content_version differs"
    );
    assert_eq!(rendered.lines[0], Line::from("fresh"));
    let cached = cache[0].as_ref().unwrap();
    assert_eq!(
        cached.key.content_version, 2,
        "cache entry should record the new content version"
    );
}

// ── compute_visual_offsets ─────────────────────────────────

#[test]
fn compute_visual_offsets_single_line_fits() {
    let lines = vec![Line::from("hello")];
    let offsets = compute_visual_offsets(&lines, 80);
    assert_eq!(&*offsets, &[1], "short line at wide viewport = 1 row");
}

#[test]
fn compute_visual_offsets_single_line_wraps() {
    let long = "a".repeat(200);
    let lines = vec![Line::from(long)];
    let offsets = compute_visual_offsets(&lines, 80);
    assert_eq!(&*offsets, &[3], "200 chars at 80-wide wraps to 3 rows");
}

#[test]
fn compute_visual_offsets_empty_lines_count_as_one_row_each() {
    let lines = vec![Line::from(""), Line::from(""), Line::from("")];
    let offsets = compute_visual_offsets(&lines, 80);
    assert_eq!(&*offsets, &[1, 2, 3], "each empty line = 1 visual row");
}

#[test]
fn compute_visual_offsets_mixed_lines() {
    let lines = vec![
        Line::from("short"),
        Line::from(""),              // 1 row
        Line::from("x".repeat(150)), // 2 rows at 80
    ];
    let offsets = compute_visual_offsets(&lines, 80);
    assert_eq!(&*offsets, &[1, 2, 4]);
}

#[test]
fn compute_visual_offsets_empty_slice() {
    let lines: Vec<Line<'static>> = vec![];
    let offsets = compute_visual_offsets(&lines, 80);
    assert!(offsets.is_empty(), "no lines → no offsets");
}

#[test]
fn compute_visual_offsets_zero_width_each_line_zero() {
    let lines = vec![Line::from("hello"), Line::from("world")];
    let offsets = compute_visual_offsets(&lines, 0);
    // At width 0 every line contributes 0 visual rows, so each
    // cumulative entry stays 0 (same length as lines).
    assert_eq!(&*offsets, &[0, 0], "zero width → each entry = 0");
}

// ── partition_point mapping (visual row → line index) ──────

#[test]
fn partition_point_finds_line_at_row_zero() {
    let offsets = [2, 5, 7];
    assert_eq!(offsets.partition_point(|&o| o == 0), 0);
}

#[test]
fn partition_point_finds_line_in_middle() {
    let offsets = [2, 5, 7];
    // row 3 falls in the second line (offset 2 < 3, offset 5 > 3)
    assert_eq!(offsets.partition_point(|&o| o <= 3), 1);
}

#[test]
fn partition_point_finds_line_at_exact_boundary() {
    let offsets = [2, 5, 7];
    // row 2 is the last visual row of line 0 — still maps to line 0
    assert_eq!(offsets.partition_point(|&o| o <= 2), 1);
    // row 5 maps to line 2
    assert_eq!(offsets.partition_point(|&o| o <= 5), 2);
}

#[test]
fn partition_point_past_end_returns_len() {
    let offsets = [2, 5, 7];
    assert_eq!(offsets.partition_point(|&o| o <= 7), 3);
    assert_eq!(offsets.partition_point(|&o| o <= 99), 3);
}

#[test]
fn partition_point_empty_offsets_returns_zero() {
    let offsets: [usize; 0] = [];
    assert_eq!(offsets.partition_point(|&o| o == 0), 0);
    assert_eq!(offsets.partition_point(|&o| o <= 99), 0);
}
