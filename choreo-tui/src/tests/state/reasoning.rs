//! Reasoning-section layout and collapse state: header ranges, the
//! default-expanded derivation, `toggle_reasoning`,
//! `effective_reasoning_expanded`, and auto-collapse on the first answer chunk.

use crate::markdown_render::{LineChrome, LineJoin};
use crate::state::{App, ReasoningViewMode, RenderCacheKey, RenderedCache, RenderedTurn};
use crate::tests::support::{add_answered_turn, add_reasoning_turn, test_app};
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
            response_start_idx: None,
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
    assert!(display.view.start_request(7, 1));
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
    assert!(display.view.start_request(7, 1));
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

// ── bulk reasoning-visibility mode (Alt+Shift+R / /reasoning-toggle) ──
//
// The mode is the fallback a turn's visibility derives from; a per-turn click
// override still wins in either mode.  Collapsed hides every body (nothing
// streams live); Expanded re-applies the content default, so an answered turn
// stays collapsed and a still-streaming one shows its reasoning.

/// A turn whose reasoning is `reasoning` and, when `answer` is set, has already
/// produced response text.
fn reasoning_turn(reasoning: &str, answer: Option<&str>) -> Turn {
    Turn {
        created_at: choreo_proto::TimestampMs::now(),
        undone: false,
        error: None,
        user_text: Some("q".into()),
        assistant_text: answer.map(str::to_string),
        assistant_reasoning: Some(reasoning.to_string()),
        tool_calls: vec![],
        token_usage: None,
        tool_results: vec![],
        displayed_images: vec![],
        reasoning_artifact: None,
        reasoning_producer: None,
    }
}

#[test]
fn toggle_all_reasoning_expands_only_turns_without_a_response() {
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 40;
    // Fixture turns (they rebuild the height model, so the layouts are real):
    // turn 0 is still reasoning, turn 1 already produced its answer.
    add_reasoning_turn(&mut app, "think");
    add_answered_turn(&mut app, "think", "answer");
    let display = app.active_display().unwrap();
    // Start collapsed: that is the state the expand half switches out of.
    display.reasoning_mode = ReasoningViewMode::Collapsed;
    display.toggle_all_reasoning();

    let display = app.active_display().unwrap();
    assert_eq!(display.reasoning_mode, ReasoningViewMode::Expanded);
    assert!(
        display.effective_reasoning_expanded(0, true),
        "a reasoning-only turn expands"
    );
    assert!(
        !display.effective_reasoning_expanded(1, false),
        "a turn that already has a response stays collapsed"
    );
}

#[test]
fn toggle_all_reasoning_collapses_when_any_turn_is_expanded() {
    let mut app = test_app();
    app.history_viewport.width = 80;
    let display = app.active_display().unwrap();
    display
        .view
        .insert_or_replace(1, reasoning_turn("think", None));
    display
        .view
        .insert_or_replace(2, reasoning_turn("think", Some("answer")));
    assert_eq!(display.reasoning_mode, ReasoningViewMode::Expanded);

    // Turn 1 derives "expanded", so the transcript is not all-collapsed.
    display.toggle_all_reasoning();

    let display = app.active_display().unwrap();
    assert_eq!(display.reasoning_mode, ReasoningViewMode::Collapsed);
    assert!(!display.effective_reasoning_expanded(1, true));
    assert!(!display.effective_reasoning_expanded(2, true));
}

#[test]
fn toggle_all_reasoning_on_an_empty_session_expands() {
    // Vacuous truth: with no reasoning anywhere, "all collapsed" holds, so the
    // press switches to expanded (the mode is still recorded for turns that
    // arrive later).
    let mut app = test_app();
    let display = app.active_display().unwrap();
    display.reasoning_mode = ReasoningViewMode::Collapsed;

    display.toggle_all_reasoning();

    assert_eq!(
        app.active_display().unwrap().reasoning_mode,
        ReasoningViewMode::Expanded
    );
}

#[test]
fn toggle_all_reasoning_clears_manual_overrides() {
    let mut app = test_app();
    app.history_viewport.width = 80;
    let display = app.active_display().unwrap();
    display
        .view
        .insert_or_replace(1, reasoning_turn("think", None));
    // Collapsed mode, but the user clicked turn 1 open: not all collapsed.
    display.reasoning_mode = ReasoningViewMode::Collapsed;
    display.reasoning_override.insert(1, true);

    display.toggle_all_reasoning();

    let display = app.active_display().unwrap();
    assert_eq!(display.reasoning_mode, ReasoningViewMode::Collapsed);
    assert!(
        display.reasoning_override.is_empty(),
        "a bulk switch drops the per-turn preferences from the old mode"
    );
    assert!(!display.effective_reasoning_expanded(1, true));
}

#[test]
fn manual_click_overrides_the_collapsed_mode() {
    let mut app = test_app();
    app.history_viewport.width = 80;
    let display = app.active_display().unwrap();
    display
        .view
        .insert_or_replace(1, reasoning_turn("think", None));
    display.reasoning_mode = ReasoningViewMode::Collapsed;

    // Clicking the header still opens one turn in the collapsed mode.
    display.toggle_reasoning(1);

    let display = app.active_display().unwrap();
    assert_eq!(display.reasoning_override.get(&1), Some(&true));
    assert!(display.effective_reasoning_expanded(1, true));
}

#[test]
fn collapsed_mode_hides_a_streaming_reasoning_body() {
    let mut app = test_app();
    app.history_viewport.width = 80;
    let display = app.active_display().unwrap();
    // A turn still waiting for its answer: the content default is EXPANDED.
    display
        .view
        .insert_or_replace(1, reasoning_turn("think", None));
    display.reasoning_mode = ReasoningViewMode::Collapsed;

    assert!(
        !display.effective_reasoning_expanded(1, true),
        "the collapsed mode hides even a still-streaming body"
    );
}

#[test]
fn reasoning_mode_is_per_session() {
    let mut app = test_app();
    app.display_for(0).reasoning_mode = ReasoningViewMode::Collapsed;

    // Another session keeps the default: the mode is display state, not global.
    assert_eq!(
        app.display_for(7).reasoning_mode,
        ReasoningViewMode::Expanded
    );
}

#[test]
fn whitespace_reasoning_is_not_an_expandable_block() {
    let mut app = test_app();
    app.history_viewport.width = 80;
    let display = app.active_display().unwrap();
    // Blank reasoning renders no header, so it must not count as expandable —
    // it would otherwise make the transcript "not all collapsed" forever.
    display
        .view
        .insert_or_replace(1, reasoning_turn("   ", None));
    display.reasoning_mode = ReasoningViewMode::Collapsed;

    display.toggle_all_reasoning();

    assert_eq!(
        app.active_display().unwrap().reasoning_mode,
        ReasoningViewMode::Expanded
    );
}

// ── collapsed reasoning chunks skip the re-render path ──

/// A turn whose reasoning is streaming and which has not produced an answer.
fn streaming_reasoning_turn(app: &mut App) {
    let display = app.display_for(0);
    display
        .view
        .insert_or_replace(1, reasoning_turn("thinking", None));
    assert!(display.view.start_request(7, 1));
}

#[test]
fn collapsed_reasoning_chunk_skips_version_bump_and_dirtying() {
    let mut app = test_app();
    app.history_viewport.width = 80;
    streaming_reasoning_turn(&mut app);
    {
        let display = app.display_for(0);
        display.reasoning_mode = ReasoningViewMode::Collapsed;
        // Settle the display: a frame has run, nothing is pending.
        display.content_dirty = false;
        display.streaming_dirty = false;
        display.markers_dirty = false;
    }
    let version_before = app.display_for(0).turn_content_version(1);

    app.handle_request_stream(0, 7, OutputStream::Reasoning, Cow::Borrowed(" more"));

    let display = app.display_for(0);
    assert_eq!(
        display.turn_content_version(1),
        version_before,
        "a hidden chunk must not invalidate the cached rendering"
    );
    assert!(
        !display.content_dirty,
        "a hidden chunk must not queue a rebuild"
    );
    assert!(
        !display.streaming_dirty,
        "a hidden chunk must not queue a streaming update"
    );
    assert_eq!(
        display.view.turns[&1].assistant_reasoning.as_deref(),
        Some("thinking more"),
        "the content is still accumulated so a later expand shows it all"
    );
}

#[test]
fn first_reasoning_chunk_invalidates_even_when_collapsed() {
    let mut app = test_app();
    app.history_viewport.width = 80;
    {
        let display = app.display_for(0);
        // No reasoning text yet, so this chunk ADDS the header row.
        display
            .view
            .insert_or_replace(1, reasoning_turn_placeholder());
        assert!(display.view.start_request(7, 1));
        display.reasoning_mode = ReasoningViewMode::Collapsed;
        display.content_dirty = false;
        display.streaming_dirty = false;
    }
    let version_before = app.display_for(0).turn_content_version(1);

    app.handle_request_stream(0, 7, OutputStream::Reasoning, Cow::Borrowed("think"));

    let display = app.display_for(0);
    assert_ne!(
        display.turn_content_version(1),
        version_before,
        "the first reasoning chunk adds the header and must invalidate"
    );
    assert!(display.content_dirty);
}

#[test]
fn expanded_reasoning_chunk_still_streams() {
    let mut app = test_app();
    app.history_viewport.width = 80;
    streaming_reasoning_turn(&mut app);
    {
        let display = app.display_for(0);
        display.content_dirty = false;
        display.streaming_dirty = false;
    }
    let version_before = app.display_for(0).turn_content_version(1);

    app.handle_request_stream(0, 7, OutputStream::Reasoning, Cow::Borrowed(" more"));

    let display = app.display_for(0);
    assert_ne!(
        display.turn_content_version(1),
        version_before,
        "a visible body grows live"
    );
    assert!(display.streaming_dirty);
}

/// A turn with neither reasoning nor response text — the placeholder the daemon
/// seeds for an in-flight request before the first chunk lands.
fn reasoning_turn_placeholder() -> Turn {
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
