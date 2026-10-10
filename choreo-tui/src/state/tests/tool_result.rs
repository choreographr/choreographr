//! Tool-result collapse state: defaults, overrides, cache invalidation, and the
//! per-result header ranges the renderer consumes.

use crate::markdown_render::{LineChrome, LineJoin};
use crate::state::{RenderCacheKey, RenderedCache, RenderedTurn};
use crate::test_util::test_app;
use choreo_proto::{ToolResultRecord, Turn};
use ratatui::text::Line;
use std::sync::Arc;

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
