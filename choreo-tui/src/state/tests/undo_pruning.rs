//! `handle_turns_undone` pruning of the per-turn override and content-version
//! maps so they stay bounded by the live (non-undone) turn set.

use crate::test_util::test_app;
use choreo_client_core::TurnEventHandler;
use choreo_proto::{ToolResultRecord, Turn};

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
