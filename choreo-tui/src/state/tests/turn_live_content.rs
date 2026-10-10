//! `turn_has_live_content`: which locally-accumulated turn wins when merging an
//! attach snapshot.

use crate::state::turn::turn_has_live_content;
use choreo_proto::Turn;

// ── turn_has_live_content (attach snapshot merge) ──

fn empty_placeholder() -> Turn {
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

fn with_text(turn: &Turn, text: &str) -> Turn {
    let mut t = turn.clone();
    t.assistant_text = Some(text.into());
    t
}

#[test]
fn accumulated_live_content_beats_snapshot_placeholder() {
    let placeholder = empty_placeholder();
    let live = with_text(&placeholder, "streamed so far");
    // The accumulated turn has content the snapshot placeholder lacks.
    assert!(turn_has_live_content(&live, &placeholder));
    // But the placeholder never "wins" over a live turn.
    assert!(!turn_has_live_content(&placeholder, &live));
}

#[test]
fn snapshot_with_content_wins_over_accumulated() {
    let placeholder = empty_placeholder();
    let snapshot_final = with_text(&placeholder, "final answer from daemon");
    let accumulated = with_text(&placeholder, "earlier accumulated");
    // Both have content — the snapshot (daemon-canonical) wins.
    assert!(!turn_has_live_content(&accumulated, &snapshot_final));
    // Identical content: snapshot wins too (no clause triggers).
    let same = with_text(&placeholder, "same");
    assert!(!turn_has_live_content(&same, &same));
}

#[test]
fn reasoning_and_tool_content_also_count_as_live() {
    let placeholder = empty_placeholder();
    let mut reasoning = placeholder.clone();
    reasoning.assistant_reasoning = Some("thinking…".into());
    assert!(turn_has_live_content(&reasoning, &placeholder));

    let mut tool = placeholder.clone();
    tool.tool_calls.push(choreo_proto::AssistantToolCallRecord {
        call_id: "call_1".into(),
        name: "read_file".into(),
        arguments_json: "{}".into(),
    });
    assert!(turn_has_live_content(&tool, &placeholder));
}
