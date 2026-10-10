//! Reasoning-section layout and collapse state: header ranges, the
//! default-expanded derivation, `toggle_reasoning`,
//! `effective_reasoning_expanded`, and auto-collapse on the first answer chunk.

use crate::markdown_render::{LineChrome, LineJoin};
use crate::state::{RenderCacheKey, RenderedCache, RenderedTurn};
use crate::test_util::test_app;
use choreo_client_core::TurnEventHandler;
use choreo_proto::{OutputStream, Turn};
use ratatui::text::Line;
use std::borrow::Cow;
use std::sync::Arc;

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
