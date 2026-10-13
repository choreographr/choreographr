//! Streaming fast-path behaviour: dirty flags, incremental height recompute,
//! reasoning-header movement, and the incremental response cache.

use super::insert_turn;
use crate::markdown_render::render_turn_lines;
use crate::state::{App, ResponsePin};
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

// ── streaming response top auto-pin ──────────────────────────────────

/// A turn with a user prompt and no response yet, matching what `start_turn`
/// leaves on the client before the answer streams.
fn pending_turn(user_text: &str) -> Turn {
    Turn {
        created_at: choreo_proto::TimestampMs::now(),
        undone: false,
        error: None,
        user_text: Some(user_text.into()),
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

/// Stream `doc` one char at a time as an Answer, recomputing the height model
/// after every chunk exactly as the UI loop does per frame.
fn stream_answer(app: &mut App, session_id: u64, stream_id: u64, doc: &str) {
    for ch in doc.chars() {
        app.handle_request_stream(
            session_id,
            stream_id,
            OutputStream::Answer,
            Cow::Owned(ch.to_string()),
        );
        app.compute_total_height_and_markers();
    }
}

/// The content line currently drawn at the top of the history viewport.
fn viewport_top_line(app: &App) -> usize {
    let display = app.active_display_ref().expect("active display");
    display
        .total_history_height()
        .saturating_sub(app.history_viewport.height as usize)
        .saturating_sub(display.effective_scroll(&app.history_viewport))
}

#[test]
fn streaming_response_pins_its_top_at_viewport_top_once() {
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 10;
    {
        let display = app.active_display().unwrap();
        display.view.insert_or_replace(1, pending_turn("ask"));
        display.view.request_to_turn.insert(7, 1);
    }
    app.rebuild_height_prefix();
    assert_eq!(app.effective_scroll(), 0, "starts following the bottom");

    // Grow the response well past the viewport.
    stream_answer(&mut app, 0, 7, &"line\n".repeat(60));

    let row = {
        let display = app.active_display_ref().unwrap();
        assert_eq!(
            display.response_pin,
            ResponsePin::Pinned(1),
            "the response pins exactly once"
        );
        display.turn_layouts[0]
            .response_start_row
            .expect("response is present")
    };
    assert!(
        app.effective_scroll() > 0,
        "a pinned response leaves the bottom"
    );
    assert_eq!(
        viewport_top_line(&app),
        row,
        "the response's first line sits at the viewport top"
    );

    // Further growth keeps the response top pinned (it pins only once).
    stream_answer(&mut app, 0, 7, &"more\n".repeat(30));
    assert_eq!(
        app.active_display_ref().unwrap().response_pin,
        ResponsePin::Pinned(1),
        "the pin is not re-taken on later chunks"
    );
    assert_eq!(
        viewport_top_line(&app),
        row,
        "the response top stays at the viewport top as it grows below"
    );
}

#[test]
fn streaming_short_response_never_pins_and_keeps_following() {
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 10;
    {
        let display = app.active_display().unwrap();
        display.view.insert_or_replace(1, pending_turn("ask"));
        display.view.request_to_turn.insert(7, 1);
    }
    app.rebuild_height_prefix();

    stream_answer(&mut app, 0, 7, "short.");

    let display = app.active_display_ref().unwrap();
    assert_eq!(
        display.response_pin,
        ResponsePin::Armed(1),
        "a response shorter than the viewport stays armed, never pinned"
    );
    assert!(display.turn_layouts[0].response_start_row.is_some());
    assert_eq!(
        app.effective_scroll(),
        0,
        "a short response keeps following the bottom"
    );
}

#[test]
fn reasoning_only_streaming_never_pins() {
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 6;

    let turn = pending_turn("ask");
    {
        let display = app.active_display().unwrap();
        display.view.insert_or_replace(1, turn);
        display.view.request_to_turn.insert(7, 1);
    }
    app.rebuild_height_prefix();

    for ch in "thinking\n".repeat(40).chars() {
        app.handle_request_stream(0, 7, OutputStream::Reasoning, Cow::Owned(ch.to_string()));
        app.compute_total_height_and_markers();
    }

    let display = app.active_display_ref().unwrap();
    assert_eq!(
        display.response_pin,
        ResponsePin::Idle,
        "reasoning alone must never arm the response pin"
    );
    assert_eq!(
        app.effective_scroll(),
        0,
        "reasoning-only streaming keeps following the bottom"
    );
}

#[test]
fn manual_scroll_disarms_an_unpinned_response() {
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 6;
    // A tall prior turn so the history is scrollable from the start.
    insert_turn(&mut app, 0, "earlier", &"old\n".repeat(20));
    {
        let display = app.active_display().unwrap();
        display.view.insert_or_replace(1, pending_turn("ask"));
        display.view.request_to_turn.insert(7, 1);
    }
    app.rebuild_height_prefix();

    // First answer chunk arms the pin while the response is still short.
    app.handle_request_stream(0, 7, OutputStream::Answer, Cow::Borrowed("hi"));
    app.compute_total_height_and_markers();
    assert_eq!(
        app.active_display_ref().unwrap().response_pin,
        ResponsePin::Armed(1)
    );

    // The user scrolls manually: the pending pin is disarmed.
    app.scroll_up(1);
    assert_eq!(
        app.active_display_ref().unwrap().response_pin,
        ResponsePin::Idle,
        "a manual scroll disarms a not-yet-pinned response"
    );

    // A now-tall response must NOT pin — it would yank the user back up.
    stream_answer(&mut app, 0, 7, &"line\n".repeat(80));
    assert_eq!(
        app.active_display_ref().unwrap().response_pin,
        ResponsePin::Idle,
        "a disarmed response never pins"
    );
}

#[test]
fn pinned_response_keeps_its_position_across_a_session_switch() {
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 10;
    {
        let display = app.active_display().unwrap();
        display.view.insert_or_replace(1, pending_turn("ask"));
        display.view.request_to_turn.insert(7, 1);
    }
    app.rebuild_height_prefix();
    stream_answer(&mut app, 0, 7, &"line\n".repeat(60));

    let row = {
        let display = app.active_display_ref().unwrap();
        assert_eq!(display.response_pin, ResponsePin::Pinned(1));
        display.turn_layouts[0]
            .response_start_row
            .expect("response is present")
    };
    assert_eq!(viewport_top_line(&app), row);

    // Switch away and back (mirroring the UI loop's clamp-before-draw order).
    app.reset_for_session_switch(2);
    app.clamp_scroll_state();
    app.compute_total_height_and_markers();
    app.reset_for_session_switch(0);
    app.active_display().unwrap().mark_content_changed();
    app.clamp_scroll_state();
    app.compute_total_height_and_markers();

    let display = app.active_display_ref().unwrap();
    assert_eq!(
        display.response_pin,
        ResponsePin::Pinned(1),
        "the pin survives the switch (it never re-pins)"
    );
    assert_eq!(
        display.turn_layouts[0].response_start_row,
        Some(row),
        "the response top row is stable"
    );
    assert_eq!(
        viewport_top_line(&app),
        row,
        "the pinned response top is unchanged after the switch"
    );
}

#[test]
fn switching_into_a_background_streaming_response_pins_at_once() {
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 10;
    // The user is viewing session 2; session 1 streams an answer in the
    // background (routed by the all-activity subscription, never computed).
    app.active_session_id = Some(2);
    {
        let display = app.display_for(1);
        display.view.insert_or_replace(1, pending_turn("ask"));
        display.view.request_to_turn.insert(7, 1);
    }
    for ch in "line\n".repeat(80).chars() {
        app.handle_request_stream(1, 7, OutputStream::Answer, Cow::Owned(ch.to_string()));
    }
    assert_eq!(
        app.display_for(1).response_pin,
        ResponsePin::Armed(1),
        "a background response is armed but not pinned until viewed"
    );

    // Switch in: the single compute must pin it before any frame is drawn.
    app.reset_for_session_switch(1);
    app.clamp_scroll_state();
    app.compute_total_height_and_markers();

    let row = {
        let display = app.active_display_ref().unwrap();
        assert_eq!(display.response_pin, ResponsePin::Pinned(1));
        display.turn_layouts[0]
            .response_start_row
            .expect("response is present")
    };
    assert_eq!(
        viewport_top_line(&app),
        row,
        "switch-in pins the response top at the viewport top"
    );
}
