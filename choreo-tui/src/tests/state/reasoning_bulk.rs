//! Bulk reasoning visibility (`Alt+Shift+R` / `/reasoning-toggle`): the
//! per-session expand/collapse mode, its interaction with the per-turn click
//! override, and the streaming fast path a hidden reasoning body takes.
//!
//! The per-turn collapse state itself — `toggle_reasoning`,
//! `effective_reasoning_expanded` and the auto-collapse on the first answer
//! chunk — lives in the sibling `reasoning` module; this one owns the mode that
//! those derive *through*.

use crate::state::{App, ReasoningViewMode};
use crate::tests::support::{add_answered_turn, add_reasoning_turn, assistant_turn, test_app};
use choreo_client_core::TurnEventHandler;
use choreo_proto::OutputStream;
use std::borrow::Cow;

// The mode is the fallback a turn's visibility derives from; a per-turn click
// override still wins in either mode.  Collapsed hides every body (nothing
// streams live); Expanded re-applies the content default, so an answered turn
// stays collapsed and a still-streaming one shows its reasoning.

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
        .insert_or_replace(1, assistant_turn(Some("think"), None));
    display
        .view
        .insert_or_replace(2, assistant_turn(Some("think"), Some("answer")));
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
        .insert_or_replace(1, assistant_turn(Some("think"), None));
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
        .insert_or_replace(1, assistant_turn(Some("think"), None));
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
        .insert_or_replace(1, assistant_turn(Some("think"), None));
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
        .insert_or_replace(1, assistant_turn(Some("   "), None));
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
        .insert_or_replace(1, assistant_turn(Some("thinking"), None));
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
        // Neither reasoning nor response yet — the placeholder the daemon seeds
        // for an in-flight request, so this chunk ADDS the header row.
        display
            .view
            .insert_or_replace(1, assistant_turn(None, None));
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
