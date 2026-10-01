use super::super::*;

#[test]
fn render_turn_lines_tool_calls() {
    let turn = Turn {
        created_at: choreo_proto::TimestampMs::now(),
        undone: false,
        error: None,
        user_text: None,
        assistant_text: None,
        assistant_reasoning: None,
        tool_calls: vec![choreo_proto::AssistantToolCallRecord {
            call_id: "call1".into(),
            name: "read_file".into(),
            arguments_json: r#"{"path":"/tmp/x"}"#.into(),
        }],
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
    // The turn has only tool_calls (no text, no reasoning), so no
    // assistant block is rendered. Tool calls are now only visible
    // through their streaming output and subsequent tool results.
    assert!(!text.contains("tool:"), "tool: label should not appear");
}

#[test]
fn render_turn_lines_quiet_tool_collapsed_by_default_hides_content() {
    // Quiet tools (read_file, http_request) default to
    // collapsed: the header row (triangle + invocation description) is
    // shown, but the label row and verbatim content are hidden behind
    // the triangle until the user expands the result.
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
    let lines = render_turn_lines(&turn, 80, 85, false, &[true]).lines;
    let text = lines
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    // Collapsed: triangle + description header only — no label row, no
    // verbatim content (the LLM still gets it; the user can expand).
    assert!(text.contains("▶ Reading file src/main.rs."), "{text}");
    assert!(!text.contains("tool result: read_file"), "{text}");
    assert!(!text.contains("file contents"), "{text}");
}

#[test]
fn render_turn_lines_quiet_tool_expanded_reveals_content() {
    // Expanding a quiet tool (user clicked the triangle) reveals the
    // label row and the verbatim content the old hard suppression
    // always hid.
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
    let lines = render_turn_lines(&turn, 80, 85, false, &[false]).lines;
    let text = lines
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("▼ Reading file src/main.rs."), "{text}");
    assert!(text.contains("tool result: read_file"), "{text}");
    assert!(text.contains("file contents"), "{text}");
}

#[test]
fn render_turn_lines_collapsed_shows_full_invocation_description() {
    // Collapsing hides the label row + verbatim content, but the whole
    // invocation description stays visible: continuation lines of a
    // multi-line description are part of the always-visible summary, so
    // the user sees the full context without expanding.
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
                content: "secret body".into(),
                is_error: false,
                invocation_description: "Running `sh` with an extremely long argument list that keeps going well past the wrap width and continues onto a second line of description text.".into(),
                image: None,
            }],
            displayed_images: vec![],
            reasoning_artifact: None,
            reasoning_producer: None,
        };
    let lines = render_turn_lines(&turn, 80, 85, false, &[true]).lines;
    let text = lines
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    // Header row opens the description; the wrapped tail is still
    // visible while the label row and content stay hidden.
    assert!(text.contains("▶ Running sh with"), "{text}");
    assert!(
        text.contains("second line of description text."),
        "full description must appear when collapsed: {text}"
    );
    assert!(!text.contains("tool result: sh"), "{text}");
    assert!(!text.contains("secret body"), "{text}");
}

#[test]
fn render_turn_lines_description_header_fits_content_width() {
    // The header prepends "▶ " to the first description line; the
    // description is wrapped two columns narrower than the content
    // width so the header row never overflows the viewport.
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
            content: "x".into(),
            is_error: false,
            invocation_description: "lorem ipsum dolor sit amet ".repeat(10),
            image: None,
        }],
        displayed_images: vec![],
        reasoning_artifact: None,
        reasoning_producer: None,
    };
    let lines = render_turn_lines(&turn, 40, 45, false, &[true]).lines;
    // tool_content_width = 45 → rows are padded to exactly 46 columns;
    // the header must never exceed that.
    assert!(
        lines[0].width() <= 46,
        "header row must fit the viewport width: {}",
        lines[0].width()
    );
}

#[test]
fn tool_result_default_collapsed_quiet_and_error_rules() {
    // Quiet tools default collapsed; everything else — including error
    // results of quiet tools — defaults expanded.
    let mk = |name: &str, is_error: bool| choreo_proto::ToolResultRecord {
        call_id: "c".into(),
        name: name.into(),
        content: "x".into(),
        is_error,
        invocation_description: String::new(),
        image: None,
    };
    assert!(tool_result_default_collapsed(&mk("read_file", false)));
    assert!(tool_result_default_collapsed(&mk("http_request", false)));
    assert!(tool_result_default_collapsed(&mk("grep", false)));
    assert!(tool_result_default_collapsed(&mk(
        "retrieve_webpage",
        false
    )));
    assert!(tool_result_default_collapsed(&mk(
        "spawn_subsession",
        false
    )));
    assert!(tool_result_default_collapsed(&mk("list_sessions", false)));
    // Shell/exec family all default collapsed.
    assert!(tool_result_default_collapsed(&mk("sh", false)));
    assert!(tool_result_default_collapsed(&mk("nushell", false)));
    assert!(tool_result_default_collapsed(&mk("fish", false)));
    assert!(tool_result_default_collapsed(&mk("powershell", false)));
    assert!(tool_result_default_collapsed(&mk("exec", false)));
    // A non-quiet tool still defaults expanded.
    assert!(!tool_result_default_collapsed(&mk("find", false)));
    assert!(!tool_result_default_collapsed(&mk("read_file", true)));
    assert!(!tool_result_default_collapsed(&mk("sh", true)));
    assert!(!tool_result_default_collapsed(&mk("http_request", true)));
}

#[test]
fn render_turn_lines_tool_result_header_falls_back_to_label_without_description() {
    // Streaming stubs (and tools with no invocation description) carry
    // the standard label in the header row; expanding still shows the
    // content body, and the label is not duplicated.
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
            content: "progress".into(),
            is_error: false,
            invocation_description: String::new(),
            image: None,
        }],
        displayed_images: vec![],
        reasoning_artifact: None,
        reasoning_producer: None,
    };
    let join = |lines: Vec<ratatui::text::Line<'static>>| {
        lines
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    };
    let collapsed = render_turn_lines(&turn, 80, 85, false, &[true]);
    let text = join(collapsed.lines);
    assert!(text.contains("▶ tool result: run"), "{text}");
    assert!(!text.contains("progress"), "{text}");
    let expanded = render_turn_lines(&turn, 80, 85, false, &[false]);
    let text = join(expanded.lines);
    assert!(text.contains("▼ tool result: run"), "{text}");
    assert!(text.contains("progress"), "{text}");
}

#[test]
fn render_turn_lines_tool_result_header_idxs_aligned_and_stable() {
    // Each tool result reports a header index in tool_results order.  A
    // result's header index shifts when an earlier result's body grows or
    // shrinks (collapse changes body lengths), so the indexes describe the
    // rendered state they were computed with — exactly what the layout
    // ranges and the cache key need.
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
            choreo_proto::ToolResultRecord {
                call_id: "c1".into(),
                name: "read_file".into(),
                content: "a".into(),
                is_error: false,
                invocation_description: "Reading `a`.".into(),
                image: None,
            },
            choreo_proto::ToolResultRecord {
                call_id: "c2".into(),
                name: "sh".into(),
                content: "b".into(),
                is_error: false,
                invocation_description: "Running `b`.".into(),
                image: None,
            },
        ],
        displayed_images: vec![],
        reasoning_artifact: None,
        reasoning_producer: None,
    };
    let collapsed = render_turn_lines(&turn, 80, 85, false, &[true, true]);
    let expanded = render_turn_lines(&turn, 80, 85, false, &[false, false]);
    assert_eq!(collapsed.tool_result_header_idxs.len(), 2);
    assert_eq!(expanded.tool_result_header_idxs.len(), 2);
    // No other sections in this turn: the collapsed headers are the first
    // two lines, each carrying its triangle + description.
    assert_eq!(collapsed.tool_result_header_idxs[0], 0);
    assert_eq!(collapsed.tool_result_header_idxs[1], 1);
    assert!(collapsed.lines[0].to_string().contains("▶ Reading a."));
    assert!(collapsed.lines[1].to_string().contains("▶ Running b."));
    // Expanding the first result pushes the second result's header down
    // past the first result's body (label + content); the header indexes
    // must reflect that shift, pointing at the real header rows.
    assert_eq!(expanded.tool_result_header_idxs[0], 0);
    let second = expanded.tool_result_header_idxs[1];
    assert!(
        second > 1,
        "expanded first result pushes the second header down"
    );
    assert!(expanded.lines[second].to_string().contains("▼ Running b."));
    // Expanded has strictly more lines than collapsed (bodies added).
    assert!(collapsed.lines.len() < expanded.lines.len());
}

#[test]
#[expect(clippy::assert_is_empty)] // this clippy version wants assert_ne!(v, [] as [...]) here — worse
fn render_turn_lines_tool_result_header_idxs_empty_without_results() {
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
    let rendered = render_turn_lines(&turn, 80, 85, false, &[]);
    assert!(rendered.tool_result_header_idxs.is_empty());
}
