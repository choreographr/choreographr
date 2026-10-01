use super::super::*;

#[test]
fn render_turn_lines_reasoning_collapsed_with_response() {
    let turn = Turn {
        created_at: choreo_proto::TimestampMs::now(),
        undone: false,
        error: None,
        user_text: None,
        assistant_text: Some("Okay.".into()),
        assistant_reasoning: Some("Use **bold** for emphasis.".into()),
        tool_calls: vec![],
        token_usage: None,
        tool_results: vec![],
        displayed_images: vec![],
        reasoning_artifact: None,
        reasoning_producer: None,
    };
    let lines = render_turn_lines(&turn, 80, 85, false, &[]).lines;
    let text = lines
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    // Response present + reasoning collapsed: header shown, body hidden.
    assert!(text.contains("▶ Reasoning"), "header should be visible");
    assert!(
        !text.contains("Use **bold"),
        "reasoning body should NOT appear"
    );
    assert!(text.contains("Okay."), "response text should appear");
    assert!(
        !text.contains("**bold**"),
        "markdown bold syntax should not appear literally in output"
    );
    // The reasoning header appears below the response.
    assert!(
        text.find("Okay.") < text.find("▶ Reasoning"),
        "response should be rendered above the collapsed reasoning header"
    );
}

#[test]
fn render_turn_lines_reasoning_expanded_with_response() {
    let turn = Turn {
        created_at: choreo_proto::TimestampMs::now(),
        undone: false,
        error: None,
        user_text: None,
        assistant_text: Some("Okay.".into()),
        assistant_reasoning: Some("Use **bold** for emphasis.".into()),
        tool_calls: vec![],
        token_usage: None,
        tool_results: vec![],
        displayed_images: vec![],
        reasoning_artifact: None,
        reasoning_producer: None,
    };
    // User re-expanded the reasoning: the header points down and the
    // reasoning body appears BELOW the response.
    let lines = render_turn_lines(&turn, 80, 85, true, &[]).lines;
    let text = lines
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("▼ Reasoning"), "header should point down");
    assert!(
        text.contains("Use bold for emphasis."),
        "reasoning body should appear when expanded"
    );
    assert!(text.contains("Okay."), "response text should appear");
    assert!(
        !text.contains("**bold**"),
        "markdown bold syntax should not appear literally in output"
    );
    // Response first, then the header, then the reasoning body.
    assert!(
        text.find("Okay.") < text.find("▼ Reasoning")
            && text.find("▼ Reasoning") < text.find("Use bold for emphasis."),
        "response, header, and reasoning body should appear in that order"
    );
}

#[test]
fn render_turn_lines_reasoning_inline_code() {
    let turn = Turn {
        created_at: choreo_proto::TimestampMs::now(),
        undone: false,
        error: None,
        user_text: None,
        assistant_text: None,
        assistant_reasoning: Some("Use `code` inline.".into()),
        tool_calls: vec![],
        token_usage: None,
        tool_results: vec![],
        displayed_images: vec![],
        reasoning_artifact: None,
        reasoning_producer: None,
    };
    // No response text: reasoning defaults to expanded.
    let lines = render_turn_lines(&turn, 80, 85, true, &[]).lines;
    let text = lines
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        text.contains("▼ Reasoning"),
        "reasoning header should appear and point down"
    );
    assert!(text.contains("code"), "code content should appear");
    assert!(
        !text.contains("`code`"),
        "markdown inline code backticks should not appear literally"
    );
}

// ── reasoning_header_idx ──

#[test]
fn render_turn_lines_reasoning_header_idx_points_at_header_line() {
    let turn = Turn {
        created_at: choreo_proto::TimestampMs::now(),
        undone: false,
        error: None,
        user_text: Some("hello".into()),
        assistant_text: Some("response".into()),
        assistant_reasoning: Some("thinking".into()),
        tool_calls: vec![],
        token_usage: None,
        tool_results: vec![],
        displayed_images: vec![],
        reasoning_artifact: None,
        reasoning_producer: None,
    };
    // Turn with a response: reasoning collapsed by default.
    let rendered = render_turn_lines(&turn, 80, 85, false, &[]);
    let idx = rendered
        .reasoning_header_idx
        .expect("turn with reasoning must report a header index");
    assert!(
        idx < rendered.lines.len(),
        "header index must be within the rendered lines"
    );
    let header_line = rendered.lines[idx].to_string();
    assert!(
        header_line.contains("▶ Reasoning"),
        "line at the reported index should be the collapsed header: {header_line:?}"
    );
    // The header must sit below the response text.
    let response_idx = rendered
        .lines
        .iter()
        .position(|l| l.to_string().contains("response"))
        .expect("response text should be rendered");
    assert!(
        response_idx < idx,
        "response line ({response_idx}) should precede the header ({idx})"
    );
}

#[test]
fn render_turn_lines_reasoning_header_idx_stable_across_expand_collapse() {
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
    let collapsed = render_turn_lines(&turn, 80, 85, false, &[]);
    let expanded = render_turn_lines(&turn, 80, 85, true, &[]);
    assert_eq!(
        collapsed.reasoning_header_idx, expanded.reasoning_header_idx,
        "the header index must not depend on the collapsed/expanded state"
    );
    assert!(collapsed.reasoning_header_idx.is_some());
}

#[test]
fn render_turn_lines_reasoning_header_idx_none_without_reasoning() {
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
    let rendered = render_turn_lines(&turn, 80, 85, false, &[]);
    assert!(
        rendered.reasoning_header_idx.is_none(),
        "no reasoning → no header index"
    );
}

#[test]
fn render_turn_lines_reasoning_header_idx_none_for_whitespace_only_reasoning() {
    let turn = Turn {
        created_at: choreo_proto::TimestampMs::now(),
        undone: false,
        error: None,
        user_text: None,
        assistant_text: Some("response".into()),
        assistant_reasoning: Some("   \n ".into()),
        tool_calls: vec![],
        token_usage: None,
        tool_results: vec![],
        displayed_images: vec![],
        reasoning_artifact: None,
        reasoning_producer: None,
    };
    let rendered = render_turn_lines(&turn, 80, 85, false, &[]);
    assert!(
        rendered.reasoning_header_idx.is_none(),
        "whitespace-only reasoning is treated as absent"
    );
}

#[test]
fn render_turn_lines_reasoning_whitespace_only() {
    let turn = Turn {
        created_at: choreo_proto::TimestampMs::now(),
        undone: false,
        error: None,
        user_text: None,
        assistant_text: Some("Response text.".into()),
        assistant_reasoning: Some("   ".into()),
        tool_calls: vec![],
        token_usage: None,
        tool_results: vec![],
        displayed_images: vec![],
        reasoning_artifact: None,
        reasoning_producer: None,
    };
    let lines = render_turn_lines(&turn, 80, 85, false, &[]).lines;
    let text = lines
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    // Whitespace-only reasoning is treated as absent: no header, and the
    // response renders as before.
    assert!(
        !text.contains("Reasoning"),
        "whitespace-only reasoning should not produce a header"
    );
    assert!(
        !text.contains("Response:"),
        "response header should NOT appear"
    );
    assert!(
        text.contains("Response text."),
        "response body should appear"
    );
}

#[test]
fn render_turn_lines_reasoning_code_block() {
    let turn = Turn {
        created_at: choreo_proto::TimestampMs::now(),
        undone: false,
        error: None,
        user_text: None,
        assistant_text: None,
        assistant_reasoning: Some("Here is code:\n```rust\nfn main() {}\n```".into()),
        tool_calls: vec![],
        token_usage: None,
        tool_results: vec![],
        displayed_images: vec![],
        reasoning_artifact: None,
        reasoning_producer: None,
    };
    // No response text: reasoning defaults to expanded.
    let lines = render_turn_lines(&turn, 80, 85, true, &[]).lines;
    let text = lines
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("▼ Reasoning"), "header should appear");
    assert!(
        text.contains("fn main() {}"),
        "code block content should appear"
    );
    assert!(
        !text.contains("```"),
        "fences are replaced by the code box: {text}"
    );
    assert!(
        text.contains('╭'),
        "the code box's top border should render: {text}"
    );
}

#[test]
fn reasoning_expanded_default_with_response_is_collapsed() {
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
    assert!(
        !reasoning_expanded_default(&turn),
        "response present → reasoning collapsed by default"
    );
}

#[test]
fn reasoning_expanded_default_without_response_is_expanded() {
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
    assert!(
        reasoning_expanded_default(&turn),
        "no response yet → reasoning expanded by default"
    );
}

#[test]
fn reasoning_expanded_default_without_reasoning_is_collapsed() {
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
    assert!(
        !reasoning_expanded_default(&turn),
        "no reasoning → no expanded section"
    );
}
