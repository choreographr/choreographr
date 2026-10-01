use super::super::*;
// `format_timestamp` previously reached the tests through the façade's import
// hub; after the turn-assembly split it lives with the renderer, so import it
// directly here.
use crate::render::format_timestamp;

// ── render_turn_lines ────────────────────────────────────────────────

#[test]
#[expect(clippy::assert_is_empty)] // this clippy version wants assert_ne!(v, [] as [...]) here — worse
fn render_turn_lines_error_shows_red_header() {
    let turn = Turn {
        created_at: choreo_proto::TimestampMs::now(),
        undone: false,
        error: Some("something went wrong".into()),
        user_text: None,
        assistant_text: None,
        assistant_reasoning: None,
        tool_calls: vec![],
        token_usage: None,
        tool_results: vec![],
        displayed_images: vec![],
        reasoning_artifact: None,
        reasoning_producer: None,
    };
    let lines = render_turn_lines(&turn, 80, 85, false, &[]).lines;
    assert!(!lines.is_empty());
    let text = lines
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("Error: something went wrong"));
}

#[test]
fn render_turn_lines_error_wraps_long_message() {
    // The error block is drawn into a non-wrapping history Paragraph, so
    // long error text (e.g. provider JSON) must be pre-wrapped at the
    // content width — otherwise it clips at the viewport edge mid-token.
    let long = "client error (402): request failed with status 402: \
{\"error\":{\"message\":\"Insufficient Balance\",\"type\":\"unknown_error\",\"code\":\"invalid_request_error\"}}";
    let turn = Turn {
        created_at: choreo_proto::TimestampMs::now(),
        undone: false,
        error: Some(long.to_string()),
        user_text: None,
        assistant_text: None,
        assistant_reasoning: None,
        tool_calls: vec![],
        token_usage: None,
        tool_results: vec![],
        displayed_images: vec![],
        reasoning_artifact: None,
        reasoning_producer: None,
    };
    let rendered = render_turn_lines(&turn, 40, 45, false, &[]);
    assert!(
        rendered.lines.len() > 1,
        "a long error must wrap into multiple lines, got {} lines",
        rendered.lines.len()
    );
    // Every rendered line fits the content width (the non-wrapping
    // Paragraph's invariant), and concatenating them reproduces the
    // original header text verbatim — nothing clipped, nothing dropped.
    for line in &rendered.lines {
        assert!(line.width() <= 40, "line overflows: {line:?}");
    }
    let joined: String = rendered
        .lines
        .iter()
        .map(ToString::to_string)
        .collect::<String>();
    assert_eq!(joined, format!("Error: {long}"));
    assert!(joined.contains("invalid_request_error"));
}

#[test]
fn render_turn_lines_error_shows_user_text_above() {
    // A failed request's turn carries the user's message plus the error.
    // The transcript must show both — the user text first, then the red
    // error block — so the failure has its context.
    let turn = Turn {
        created_at: choreo_proto::TimestampMs::now(),
        undone: false,
        error: Some("Insufficient Balance".into()),
        user_text: Some("hi".into()),
        assistant_text: None,
        assistant_reasoning: None,
        tool_calls: vec![],
        token_usage: None,
        tool_results: vec![],
        displayed_images: vec![],
        reasoning_artifact: None,
        reasoning_producer: None,
    };
    let rendered = render_turn_lines(&turn, 80, 85, false, &[]);
    let texts: Vec<String> = rendered.lines.iter().map(ToString::to_string).collect();
    let user_idx = texts
        .iter()
        .position(|t| t.contains("hi"))
        .expect("user text");
    let error_idx = texts
        .iter()
        .position(|t| t.contains("Error: Insufficient Balance"))
        .expect("error block");
    assert!(
        user_idx < error_idx,
        "user text must render above the error block"
    );
    assert!(
        rendered.reasoning_header_idx.is_none() && rendered.tool_result_header_idxs.is_empty(),
        "an error turn has no reasoning/tool-result metadata"
    );
}

#[test]
fn render_turn_lines_error_sanitizes_hostile_body() {
    // The error body is provider-controlled bytes: OSC clipboard writes /
    // control chars must render as inert escaped text, never reach the
    // terminal as live sequences (same sink defense as tool output).
    let turn = Turn {
        created_at: choreo_proto::TimestampMs::now(),
        undone: false,
        error: Some("boom\u{1b}]52;c;evil\u{7}".into()),
        user_text: None,
        assistant_text: None,
        assistant_reasoning: None,
        tool_calls: vec![],
        token_usage: None,
        tool_results: vec![],
        displayed_images: vec![],
        reasoning_artifact: None,
        reasoning_producer: None,
    };
    let rendered = render_turn_lines(&turn, 80, 85, false, &[]);
    let joined: String = rendered.lines.iter().map(ToString::to_string).collect();
    assert!(
        !joined.contains('\u{1b}'),
        "no live ESC may survive: {joined:?}"
    );
    assert!(!joined.contains('\u{7}'), "BEL must be escaped: {joined:?}");
    assert!(
        joined.contains("\\u{1b}"),
        "OSC ESC must render as escaped text: {joined:?}"
    );
}

#[test]
fn render_turn_lines_user_text() {
    let turn = Turn {
        created_at: choreo_proto::TimestampMs::now(),
        undone: false,
        error: None,
        user_text: Some("hello world".into()),
        assistant_text: None,
        assistant_reasoning: None,
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
    assert!(text.contains("hello world"), "user text should appear");
}

#[test]
fn render_turn_lines_assistant_text() {
    let turn = Turn {
        created_at: choreo_proto::TimestampMs::now(),
        undone: false,
        error: None,
        user_text: None,
        assistant_text: Some("The answer is 42.".into()),
        assistant_reasoning: Some("Let me think...".into()),
        tool_calls: vec![],
        token_usage: None,
        tool_results: vec![],
        displayed_images: vec![],
        reasoning_artifact: None,
        reasoning_producer: None,
    };
    // Default state for a turn with a response: reasoning collapsed.
    let lines = render_turn_lines(&turn, 80, 85, false, &[]).lines;
    let text = lines
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    // The collapsible header is always shown when reasoning exists.
    assert!(text.contains("Reasoning"), "reasoning header should appear");
    assert!(
        text.contains("▶"),
        "collapsed reasoning shows a right-pointing arrow"
    );
    assert!(
        !text.contains("Let me think"),
        "collapsed reasoning body should NOT appear"
    );
    assert!(
        !text.contains("Response:"),
        "response header should NOT appear"
    );
    assert!(
        text.contains("The answer is 42."),
        "response body should appear"
    );
    // Reasoning sits BELOW the response in the rendered output.
    assert!(
        text.find("The answer is 42.") < text.find("Reasoning"),
        "response should be rendered above the reasoning header"
    );
}

#[test]
fn render_turn_lines_tool_results_error() {
    let turn = Turn {
        created_at: choreo_proto::TimestampMs::now(),
        undone: false,
        error: None,
        user_text: None,
        assistant_text: None,
        assistant_reasoning: None,
        tool_calls: vec![],
        token_usage: None,
        tool_results: vec![choreo_proto::ToolResultRecord {
            call_id: "call1".into(),
            name: "run".into(),
            content: "command failed".into(),
            is_error: true,
            invocation_description: String::new(),
            image: None,
        }],
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
    assert!(text.contains("tool error: run"));
    assert!(text.contains("command failed"));
}

#[test]
fn render_turn_lines_empty_turn_produces_blank_line() {
    let turn = Turn {
        created_at: choreo_proto::TimestampMs::now(),
        undone: false,
        error: None,
        user_text: None,
        assistant_text: None,
        assistant_reasoning: None,
        tool_calls: vec![],
        token_usage: None,
        tool_results: vec![],
        displayed_images: vec![],
        reasoning_artifact: None,
        reasoning_producer: None,
    };
    let lines = render_turn_lines(&turn, 80, 85, false, &[]).lines;
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].width(), 0);
}

#[test]
fn render_turn_lines_user_with_assistant_renders_both_blocks() {
    let turn = Turn {
        created_at: choreo_proto::TimestampMs::now(),
        undone: false,
        error: None,
        user_text: Some("Hello".into()),
        assistant_text: Some("Hi there!".into()),
        assistant_reasoning: None,
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
    assert!(text.contains("Hello"), "user block should appear");
    assert!(text.contains("Hi there!"), "assistant block should appear");
}

#[test]
fn user_text_timestamp_rendered_in_milliseconds() {
    // Regression: the user-text timestamp was divided by 1000 before
    // being passed to format_timestamp (which takes milliseconds),
    // so every user message rendered as a 1970 date (e.g. "Jan 21 1970").
    let ts_ms = 1_705_314_000_000i64; // a plausible modern timestamp
    let (lines, _rows, _content_ranges, _joins, _chrome) = add_margin_lines(
        Vec::new(),
        Vec::new(),
        Vec::new(),
        80,
        Color::Green,
        Some(ts_ms),
    );
    let bottom = lines.last().expect("bottom separator line");
    let rendered = bottom.to_string();
    let expected = format_timestamp(ts_ms);
    assert!(
        rendered.contains(&expected),
        "bottom separator should render {expected:?}, got {rendered:?}"
    );
    assert!(
        !rendered.contains("1970"),
        "epoch-looking dates indicate the millis→seconds unit bug, got {rendered:?}"
    );
    // Position: right-aligned with a 1-column margin under the shaded
    // block.  content_width 80 → total_width 89; the shaded area's last
    // column is 86 and the 2-col right margin occupies 87–88, so the
    // timestamp's right edge must sit at column 85 (total_width − 4) — not
    // spilling into the margin or the scrollbar column.
    let ts_width = expected.len(); // format_timestamp emits ASCII only
    let ts_start = rendered.find(&expected).expect("timestamp position");
    assert_eq!(
        ts_start + ts_width,
        86,
        "timestamp right edge must be column total_width − 4 = 85"
    );
    assert_eq!(
        rendered.len(),
        89,
        "separator row spans exactly content_width + 9 columns"
    );
    assert!(
        rendered.ends_with("   "),
        "3 trailing blanks: 1 under the shading + the 2-col right margin"
    );
    // And the real render path (a user turn) must carry the same
    // timestamp into the bottom separator.
    let turn = Turn {
        created_at: choreo_proto::TimestampMs::now(),
        undone: false,
        error: None,
        user_text: Some("hello world".into()),
        assistant_text: None,
        assistant_reasoning: None,
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
    assert!(
        !text.contains("1970"),
        "rendered turn must not contain an epoch date:\n{text}"
    );
}

#[test]
fn margin_block_rows_indented_two_columns_on_both_sides() {
    // Message blocks carry a symmetric 2-column blank margin on each side:
    // a 2-column left margin before the `┃` gutter and a 2-column right
    // margin after the shaded box (before the scrollbar column), so every
    // message-block row spans exactly content_width + 9 columns.  With one
    // content line the row count is MARGIN_STRUCTURAL_ROWS(4) + 1 = 5:
    // separator, padding, content, padding, separator.
    let (lines, _rows, content_ranges, _joins, _chrome) = add_margin_lines(
        vec![Line::from("hello")],
        vec![LineJoin::Break],
        vec![LineChrome::default()],
        20,
        Color::Blue,
        None,
    );
    assert_eq!(lines.len(), 5, "sep + pad + content + pad + sep");
    let content = &lines[2];
    assert_eq!(
        content.width(),
        29,
        "content_width 20 + 9 chrome (2 margin + 1 gutter + 2 shade left, \
         2 shade + 2 margin right) = 29"
    );
    // The 2-column left margin precedes the gutter; the text starts after
    // the `"  ┃  "` gutter.
    assert_eq!(content.spans[0].content.as_ref(), "  ", "2-col left margin");
    assert!(content.spans[1].content.as_ref().starts_with('┃'));
    assert_eq!(content_ranges[2], Some((5, 10)), "text starts at column 5");
    // The 2-column right margin brings the row to exactly 29 columns.
    assert_eq!(
        content.spans.last().unwrap().content.as_ref(),
        "  ",
        "2-col right margin"
    );
    // Padding rows line up with the content rows exactly.
    assert_eq!(lines[1].width(), 29, "padding row matches content rows");
    assert_eq!(lines[3].width(), 29, "padding row matches content rows");
}

#[test]
fn assistant_block_has_no_duplicate_timestamp() {
    // The proto carries a single created_at per turn spanning both halves,
    // so the assistant block deliberately renders no timestamp — a second
    // one would duplicate the user message's time.  Only the user block's
    // bottom separator carries it (pinned by the position assertions in
    // `user_text_timestamp_rendered_in_milliseconds`).
    let turn = Turn {
        created_at: choreo_proto::TimestampMs::now(),
        undone: false,
        error: None,
        user_text: None,
        assistant_text: Some("Hi there!".into()),
        assistant_reasoning: None,
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
    let rendered_ts = format_timestamp(turn.created_at.as_millis());
    assert!(
        !text.contains(&rendered_ts),
        "assistant block must not duplicate the turn timestamp: {text}"
    );
}

#[test]
fn tool_result_rows_start_flush_and_end_one_column_short_of_scrollbar() {
    // Tool-result rows lost their 2-column left indent and now end with a
    // single blank column: every row is padded to exactly tool_content_width
    // + 1 columns and body content starts at column 0.
    let turn = Turn {
        created_at: choreo_proto::TimestampMs::now(),
        undone: false,
        error: None,
        user_text: None,
        assistant_text: None,
        assistant_reasoning: None,
        tool_calls: vec![],
        token_usage: None,
        tool_results: vec![choreo_proto::ToolResultRecord {
            call_id: "call1".into(),
            name: "sh".into(),
            content: "hello world".into(),
            is_error: false,
            invocation_description: "Running `echo hello`.".into(),
            image: None,
        }],
        displayed_images: vec![],
        reasoning_artifact: None,
        reasoning_producer: None,
    };
    let rendered = render_turn_lines(&turn, 80, 85, false, &[]);
    for line in &rendered.lines {
        assert!(
            line.width() <= 86,
            "no row may exceed tool_content_width + 1 = 86, got {}",
            line.width()
        );
    }
    // Body rows are padded to the full row width (content + fill + margin).
    assert!(
        rendered.lines.iter().any(|l| l.width() == 86),
        "filled rows must span exactly tool_content_width + 1 = 86 columns"
    );
    // Content starts at column 0 (the 2-column left indent was removed).
    assert!(
        rendered
            .content_ranges
            .iter()
            .any(|r| matches!(r, Some((0, _)))),
        "tool content must start at column 0, got {:#?}",
        rendered.content_ranges
    );
}
