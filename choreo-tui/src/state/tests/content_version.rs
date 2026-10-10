//! Turn content-version bookkeeping: the cache key's content fingerprint, its
//! pruning on undo, and the stale-line regressions it guards.

use crate::test_util::test_app;
use choreo_client_core::TurnEventHandler;
use choreo_proto::{ToolResultRecord, Turn};

#[test]
fn streaming_chunk_after_mark_content_changed_stays_fresh_and_incremental() {
    // Regression for "scrollbar moves but the results stay stuck": a
    // mid-stream `mark_content_changed` (here simulated with a `Done` for
    // an unrelated request — the same shape as a `TurnAppended` or
    // `SessionState` interleaving between chunks, which happens more
    // often when another session is active) disarms the streaming fast
    // path (`streaming_dirty=false`, `streaming_turn_index=None`).
    //
    // Before the fix the next chunk was processed by the O(n) full
    // rebuild, whose content-blind cache key served the *pre-chunk* lines
    // — the visible results froze until the final `TurnAppended`
    // invalidated the slot.  The content-version key forces the rebuild
    // to recompute, and running the fast path first keeps chunk
    // processing incremental.
    let mut app = test_app();
    app.attached_session_id = Some(0);
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
        tool_results: vec![ToolResultRecord {
            call_id: "call-1".into(),
            name: "find".into(), // not quiet → expanded by default
            content: String::new(),
            is_error: false,
            invocation_description: "Running `b`.".into(),
            image: None,
        }],
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

    let version_before = app.active_display_ref().unwrap().turn_content_version(1);
    let height_before = app.active_display_ref().unwrap().turn_heights[0];

    // First chunk: fast path renders it live.
    app.handle_tool_result_chunk(0, 7, "call-1".into(), b"line one\n".to_vec());
    app.compute_total_height_and_markers();
    let version_after_chunk1 = app.active_display_ref().unwrap().turn_content_version(1);
    assert!(
        version_after_chunk1 > version_before,
        "chunk must bump the turn's content version"
    );

    // Mid-stream disarming event (unrelated Done): clears the streaming
    // flags and forces markers_dirty.
    app.handle_done(0, 99, None, None);
    assert!(
        app.active_display_ref().unwrap().markers_dirty,
        "Done must mark the display for a rebuild"
    );

    // Second chunk arrives while markers_dirty is still set.
    app.handle_tool_result_chunk(0, 7, "call-1".into(), b"line two\n".to_vec());
    app.compute_total_height_and_markers();

    let display = app.active_display_ref().unwrap();
    // The rebuild (or the fast path) must serve the *latest* content, not
    // the pre-second-chunk lines the content-blind key would have reused.
    let cached = display.render_cache[0].as_ref().expect("cache slot filled");
    let text: String = cached
        .rendered
        .lines
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        text.contains("line two"),
        "rebuild must not serve stale pre-chunk lines:\n{text}"
    );
    assert!(
        text.contains("line one"),
        "earlier chunk must still be present"
    );
    assert!(
        display.turn_heights[0] > height_before,
        "turn height must reflect the streamed content"
    );
    assert!(!display.streaming_dirty, "streaming flag consumed");
    assert!(!display.markers_dirty, "markers flag consumed");
    assert!(!display.content_dirty, "content flag consumed");
}

#[test]
fn rebuild_after_disarmed_chunk_serves_fresh_content() {
    // Regression for the content-version cache key: a chunk arrives, then
    // a `mark_content_changed` event (a `Done`/`TurnAppended`/`SessionState`
    // interleaving between the chunk and its render) disarms the fast
    // path *before* it can re-render — so the rebuild runs against a cache
    // entry rendered from pre-chunk content.  Without the content version
    // in the key the rebuild would reuse those stale lines; with it, the
    // mismatch forces a recompute.
    let mut app = test_app();
    app.attached_session_id = Some(0);
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
        tool_results: vec![ToolResultRecord {
            call_id: "call-1".into(),
            name: "find".into(), // not quiet → expanded by default
            content: String::new(),
            is_error: false,
            invocation_description: "Running `b`.".into(),
            image: None,
        }],
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

    // The cache now holds the pre-chunk rendering (empty result body).
    // A chunk appends content and bumps the version, but the fast path
    // has NOT run yet.
    app.handle_tool_result_chunk(0, 7, "call-1".into(), b"line one\n".to_vec());

    // The disarming event lands before the next render: streaming flags
    // are cleared, markers_dirty set — the rebuild path will run.
    app.handle_done(0, 99, None, None);

    app.compute_total_height_and_markers();

    let display = app.active_display_ref().unwrap();
    let cached = display.render_cache[0].as_ref().expect("cache slot filled");
    let text: String = cached
        .rendered
        .lines
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        text.contains("line one"),
        "rebuild must recompute the chunk's content (content-version key):\n{text}"
    );
    assert!(
        cached.key.content_version > 0,
        "cache entry must record the post-chunk version"
    );
}

#[test]
fn content_version_bumps_on_every_mutating_handler() {
    // The version is the cache key's content fingerprint: every handler
    // that changes a turn's rendered text must bump it, so a rebuild can
    // tell stale entries apart from current ones.
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
        tool_results: vec![ToolResultRecord {
            call_id: "call-1".into(),
            name: "find".into(),
            content: String::new(),
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
        display.view.request_to_turn.insert(7, 1);
    }
    app.rebuild_height_prefix();

    let v0 = app.active_display_ref().unwrap().turn_content_version(1);
    assert_eq!(v0, 0, "fresh turn starts at version 0");

    app.handle_tool_result_chunk(0, 7, "call-1".into(), b"one\n".to_vec());
    let v1 = app.active_display_ref().unwrap().turn_content_version(1);
    assert_eq!(v1, v0 + 1, "chunk bumps by one");

    app.handle_tool_result_chunk(0, 7, "call-1".into(), b"two\n".to_vec());
    let v2 = app.active_display_ref().unwrap().turn_content_version(1);
    assert_eq!(v2, v1 + 1, "every chunk bumps the version");

    // A replacement turn (the daemon's final TurnAppended) bumps too, so
    // a cached rendering of the accumulated version is never reused.
    let mut replacement = app.active_display_ref().unwrap().view.turns[&1].clone();
    replacement.tool_results[0].content.push_str("final\n");
    app.handle_turn_appended(0, 1, replacement);
    let v3 = app.active_display_ref().unwrap().turn_content_version(1);
    assert_eq!(v3, v2 + 1, "TurnAppended bumps the version");
}
