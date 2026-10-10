//! Streaming fast-path behaviour: dirty flags, incremental height recompute,
//! reasoning-header movement, and the incremental response cache.

use super::insert_turn;
use crate::markdown_render::render_turn_lines;
use crate::state::App;
use crate::test_util::test_app;
use choreo_client_core::TurnEventHandler;
use choreo_proto::{OutputStream, ToolResultRecord, Turn};
use std::borrow::Cow;

// ── streaming (incremental update) ──

#[test]
fn mark_streaming_changed_sets_flags() {
    let mut app = test_app();
    {
        let display = app.active_display_ref().unwrap();
        assert!(!display.streaming_dirty);
        assert!(!display.content_dirty);
    }

    app.mark_streaming_changed();

    let display = app.active_display_ref().unwrap();
    assert!(display.streaming_dirty, "streaming_dirty should be set");
    assert!(display.content_dirty, "content_dirty should be set");
}

#[test]
fn mark_content_changed_resets_streaming_turn_index() {
    let mut app = test_app();
    let display = app.active_display().unwrap();
    display.markers_dirty = false;
    display.streaming_turn_index = Some(0);

    display.mark_content_changed();

    assert!(display.markers_dirty, "markers_dirty should be set");
    assert!(display.content_dirty, "content_dirty should be set");
    assert!(
        display.streaming_turn_index.is_none(),
        "streaming_turn_index should be cleared"
    );
}
#[test]
fn streaming_update_without_turn_index_falls_back() {
    let mut app = test_app();
    insert_turn(&mut app, 0, "hello", "world");
    app.rebuild_height_prefix();

    let display = app.active_display_ref().unwrap();
    let old_total = display.total_history_height();
    assert!(old_total > 0);

    // Simulate streaming without a streaming_turn_index.
    // Capture viewport before mutable borrow.
    let viewport = app.history_viewport;
    let display = app.active_display().unwrap();
    display.streaming_turn_index = None;
    display.streaming_dirty = true;
    display.content_dirty = true;

    let total = display.compute_total_height_and_markers(&viewport);

    assert!(!display.streaming_dirty, "streaming_dirty cleared");
    assert!(!display.content_dirty, "content_dirty cleared");
    assert_eq!(total, old_total, "full rebuild produces same total");
}

#[test]
fn streaming_update_recalculates_turn_height() {
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 200;

    insert_turn(&mut app, 0, "hello", "world");
    app.rebuild_height_prefix();

    let display = app.active_display_ref().unwrap();
    let before_height = display.turn_heights[0];
    let before_total = display.total_history_height();

    // Simulate streaming: append to assistant_text.
    let viewport = app.history_viewport;
    let display = app.active_display().unwrap();
    let turn = display.view.turns.get_mut(&0).unwrap();
    turn.assistant_text
        .as_mut()
        .unwrap()
        .push_str("\n\nnew streaming content");
    display.streaming_turn_index = Some(0);
    display.streaming_dirty = true;
    display.content_dirty = true;

    let total = display.compute_total_height_and_markers(&viewport);

    assert!(
        display.turn_heights[0] > before_height,
        "turn height should increase after content added"
    );
    assert!(
        total >= before_total,
        "total height should increase or stay same"
    );
    assert!(!display.streaming_dirty, "streaming_dirty cleared");
    assert!(!display.content_dirty, "content_dirty cleared");
}

#[test]
fn streaming_answer_moves_reasoning_header_range() {
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 200;

    // A turn with reasoning only (no response yet), actively streaming.
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
    {
        let display = app.active_display().unwrap();
        display.view.insert_or_replace(1, turn);
        display.view.request_to_turn.insert(7, 1);
    }
    app.rebuild_height_prefix();

    // Before the answer: reasoning is the only content, so the header
    // sits at the top of the assistant block.
    let initial_start = app.active_display_ref().unwrap().turn_layouts[0]
        .reasoning_header_range
        .expect("header range should exist")
        .0;

    // First Answer chunk auto-collapses the reasoning and places the
    // response above the header.
    app.handle_request_stream(0, 7, OutputStream::Answer, Cow::Borrowed("Response text."));
    app.compute_total_height_and_markers();

    let (start, end) = app.active_display_ref().unwrap().turn_layouts[0]
        .reasoning_header_range
        .expect("header range should remain after auto-collapse");
    assert!(
        start > initial_start,
        "header should move below the streaming response ({initial_start} -> {start})"
    );
    assert!(start < end, "header range must be non-empty");
}

#[test]
fn streaming_tool_result_expanded_grows_collapsed_stays_flat() {
    // The streaming fast path re-renders the in-flight turn with the
    // effective per-result visibility every chunk: an expanded result's
    // body (and turn height) grows live, while a collapsed quiet result
    // stays a single header row no matter how much content streams in.
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 200;

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
                call_id: "quiet".into(),
                name: "read_file".into(), // quiet → collapsed by default
                content: String::new(),
                is_error: false,
                invocation_description: "Reading `a`.".into(),
                image: None,
            },
            ToolResultRecord {
                call_id: "loud".into(),
                name: "find".into(), // not quiet → expanded by default
                content: String::new(),
                is_error: false,
                invocation_description: "Running `b`.".into(),
                image: None,
            },
        ],
        displayed_images: vec![],
        reasoning_artifact: None,
        reasoning_producer: None,
    };
    {
        let display = app.active_display().unwrap();
        display.view.insert_or_replace(1, turn);
        display.view.request_to_turn.insert(7, 1);
    }
    app.rebuild_height_prefix();

    // Capture the header ranges and turn height in one borrow scope.
    let snapshot = |app: &mut App| {
        let display = app.active_display_ref().unwrap();
        let layout = &display.turn_layouts[0];
        (
            layout.tool_result_header_ranges[0],
            layout.tool_result_header_ranges[1],
            display.turn_heights[0],
        )
    };
    let (quiet_range, _loud_range, height_before) = snapshot(&mut app);
    assert_eq!(quiet_range, (0, 1), "collapsed result: single header row");

    // Stream a chunk into the *expanded* result: the turn must grow and
    // the collapsed result must keep its single-row header range.
    app.handle_tool_result_chunk(
        0,
        7,
        "loud".into(),
        b"line one\nline two\nline three\n".to_vec(),
    );
    app.compute_total_height_and_markers();

    let (quiet_range, loud_range, height_after) = snapshot(&mut app);
    assert!(
        height_after > height_before,
        "expanded result grows as content streams ({height_before} -> {height_after})"
    );
    assert_eq!(quiet_range, (0, 1), "collapsed result stays a single row");
    assert_eq!(loud_range, (1, 2), "expanded header still on its own row");

    // Now stream an even bigger chunk into the *collapsed* quiet result:
    // nothing visible changes — the body is hidden behind the triangle.
    let height_before_collapsed = snapshot(&mut app).2;
    app.handle_tool_result_chunk(
        0,
        7,
        "quiet".into(),
        b"hidden\nhidden\nhidden\nhidden\n".to_vec(),
    );
    app.compute_total_height_and_markers();
    let height_after_collapsed = snapshot(&mut app).2;
    assert_eq!(
        height_after_collapsed, height_before_collapsed,
        "collapsed result stays flat while its content streams"
    );
}
#[test]
fn streaming_update_preserves_height_prefix_invariant() {
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 200;

    insert_turn(&mut app, 0, "a", "b");
    insert_turn(&mut app, 1, "c", "d");
    insert_turn(&mut app, 2, "e", "f");
    app.rebuild_height_prefix();

    let viewport = app.history_viewport;
    let display = app.active_display().unwrap();
    let old_prefix = display.height_prefix.clone();
    let old_heights = display.turn_heights.clone();

    // Stream content into turn 1.
    let turn = display.view.turns.get_mut(&1).unwrap();
    turn.assistant_text
        .as_mut()
        .unwrap()
        .push_str("\n\nlots of new content that should increase height");
    display.streaming_turn_index = Some(1);
    display.streaming_dirty = true;
    display.content_dirty = true;

    display.compute_total_height_and_markers(&viewport);

    // Verify invariant: height_prefix[i] == sum(turn_heights[0..=i]).
    let mut accum = 0usize;
    for i in 0..display.turn_heights.len() {
        accum += display.turn_heights[i];
        assert_eq!(
            display.height_prefix[i], accum,
            "invariant failed at index {i}: height_prefix[i] should equal cumulative turn_heights"
        );
    }

    // Turn 0 height unchanged.
    assert_eq!(
        display.turn_heights[0], old_heights[0],
        "turn 0 height should not change"
    );
    assert_eq!(
        display.height_prefix[0], old_prefix[0],
        "height_prefix[0] should not change"
    );
    // Markers must also be correct after the streaming update.
    assert_eq!(
        display.markers[0].content_line, 0,
        "marker[0] content_line should be 0"
    );
    assert_eq!(
        display.markers[1].content_line, display.turn_heights[0],
        "marker[1] content_line should equal turn 0 height"
    );
    assert_eq!(
        display.markers[2].content_line,
        display.turn_heights[0] + display.turn_heights[1],
        "marker[2] content_line should reflect updated turn 1 height"
    );
}

#[test]
fn streaming_answer_reuses_the_incremental_response_cache() {
    // Streaming a paragraph-heavy response one char at a time through the
    // fast path must keep the response's committed markdown (so the total
    // bytes re-parsed stays far below the naive sum of every prefix) and
    // still leave the rendered cache matching a whole-response render.
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 200;

    let turn = Turn {
        created_at: choreo_proto::TimestampMs::now(),
        undone: false,
        error: None,
        user_text: Some("ask".into()),
        assistant_text: None,
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
        display.view.request_to_turn.insert(7, 1);
    }
    app.rebuild_height_prefix();

    let doc = "Hi.\n\n\
        One two three four five.\n\n\
        Six seven eight nine ten.\n\n\
        Eleven twelve thirteen.\n\n\
        Fourteen fifteen sixteen.\n";
    let mut naive_bytes = 0usize;
    let mut streamed = 0usize;
    for ch in doc.chars() {
        streamed += ch.len_utf8();
        naive_bytes += streamed;
        app.handle_request_stream(0, 7, OutputStream::Answer, Cow::Owned(ch.to_string()));
        app.compute_total_height_and_markers();
    }

    let display = app.active_display_ref().unwrap();
    let cache = display
        .streaming_response
        .as_ref()
        .expect("streaming fast path must populate the response cache");
    assert_eq!(cache.turn_id, 1, "cache is keyed to the streaming turn");
    assert!(
        cache.markdown.parsed_bytes * 2 < naive_bytes,
        "incremental parsed {} bytes vs {naive_bytes} naive — prefix not reused",
        cache.markdown.parsed_bytes
    );

    // The rendered cache entry must match a whole-response render exactly.
    let turn = &display.view.turns[&1];
    let cached = display.render_cache[0]
        .as_ref()
        .expect("streaming turn is cached");
    let full = render_turn_lines(turn, 71, 79, false, &[]);
    assert_eq!(&*cached.rendered.lines, full.lines.as_slice());
    assert_eq!(&*cached.rendered.joins, full.joins.as_slice());
}
