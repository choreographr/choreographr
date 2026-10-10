//! Request lifecycle handlers: `handle_started`, `handle_done`, and the
//! `handle_failed` error-routing matrix.

use super::insert_turn;
use crate::state::App;
use crate::test_util::test_app;
use choreo_client_core::TurnEventHandler;

#[test]
fn handle_started_sets_streaming_turn_index() {
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 200;

    // Pre-populate turns so visible_turn_ids exist.
    insert_turn(&mut app, 10, "user", "assistant");
    insert_turn(&mut app, 20, "another user", "another assistant");
    app.rebuild_height_prefix();

    {
        let display = app.active_display_ref().unwrap();
        assert_eq!(display.visible_turn_ids.len(), 2);
        assert_eq!(display.visible_turn_ids[0], 10);
        assert_eq!(display.visible_turn_ids[1], 20);
        assert!(display.streaming_turn_index.is_none());
    }

    // handle_started now requires session_id
    app.handle_started(0, 1, 10, 100);

    let display = app.active_display_ref().unwrap();
    assert_eq!(
        display.streaming_turn_index,
        Some(0),
        "should find turn 10 at index 0"
    );
    assert_eq!(display.view.request_to_turn.get(&1), Some(&10));
    assert!(display.active.contains(&1));

    // Idempotent: the requester sees the targeted `Started` reply AND the
    // broadcast `Started` for the same run, so applying it twice must not
    // change the display's live-stream state.
    app.handle_started(0, 1, 10, 100);
    let display = app.active_display_ref().unwrap();
    assert_eq!(display.streaming_turn_index, Some(0));
    assert_eq!(display.view.request_to_turn.get(&1), Some(&10));
    assert!(display.active.contains(&1));
}

#[test]
fn handle_done_fires_full_rebuild() {
    let mut app = test_app();
    // test_app's default display lives on session 0; treat it as the
    // attached session so `handle_done` routes to it.
    app.attached_session_id = Some(0);
    app.history_viewport.width = 80;
    app.history_viewport.height = 200;

    insert_turn(&mut app, 10, "user", "assistant");
    app.rebuild_height_prefix();
    {
        let display = app.active_display().unwrap();
        display.markers_dirty = false;
        display.streaming_turn_index = Some(0);
        display.streaming_dirty = false;
        display.content_dirty = false;
    }

    app.handle_done(0, 1, None, None);

    let display = app.active_display_ref().unwrap();
    assert!(
        display.streaming_turn_index.is_none(),
        "streaming_turn_index should be cleared"
    );
    assert!(
        display.markers_dirty,
        "markers_dirty should be set (full rebuild)"
    );
    assert!(display.content_dirty, "content_dirty should be set");
}

#[test]
fn handle_failed_clears_streaming() {
    let mut app = test_app();
    // test_app's default display lives on session 0; treat it as attached
    // so the connection-level (`None`) resolution keeps routing to it.
    app.attached_session_id = Some(0);
    {
        let display = app.active_display().unwrap();
        display.streaming_turn_index = Some(0);
        display.streaming_dirty = false;
        display.content_dirty = false;
        display.markers_dirty = false;
    }

    app.handle_failed(None, 1, "oops".into());

    let display = app.active_display_ref().unwrap();
    assert!(display.streaming_turn_index.is_none());
    assert!(display.error.is_some());
    assert!(display.markers_dirty, "markers_dirty should be set");
    assert!(display.content_dirty, "content_dirty should be set");
}

#[test]
fn handle_failed_connection_level_resolves_to_attached_session_without_phantom_display() {
    // A connection-level "no session attached" failure arrives with
    // `session_id: None`.  It must land in the attached session's display
    // and must NOT create a phantom display entry.
    let mut app = App::new();
    app.attached_session_id = Some(42);
    app.active_session_id = Some(42);

    app.handle_failed(None, 7, "no session attached".into());

    let display = app.display_for(42);
    assert_eq!(display.error.as_deref(), Some("no session attached"));
    assert!(
        !app.session_displays.contains_key(&0),
        "a connection-level failure must not create a phantom session-0 display"
    );
}

#[test]
fn handle_failed_for_request_failure_does_not_write_global_error() {
    // A request-level failure (a real session id) renders its full error
    // as the turn's red block in the transcript; the global status/error
    // bar must not print it a second time.  The per-session display still
    // records it.
    let mut app = test_app();
    app.attached_session_id = Some(42);
    app.active_session_id = Some(42);
    assert!(app.error.is_none());

    app.handle_failed(
        Some(42),
        1,
        "client error (402): Insufficient Balance".into(),
    );

    assert_eq!(
        app.error, None,
        "a request failure's transcript block must not be duplicated on the status bar"
    );
    assert_eq!(
        app.display_for(42).error.as_deref(),
        Some("client error (402): Insufficient Balance"),
        "the per-session display records the failure"
    );
}

#[test]
fn handle_failed_connection_level_writes_global_error_for_attached_session() {
    // A `session_id: None` envelope marks a connection-level failure (e.g.
    // "no session attached"), which has no turn to render an error block
    // in: the global status/error bar is its only surface.
    let mut app = App::new();
    app.attached_session_id = Some(42);
    app.active_session_id = Some(42);
    assert!(app.error.is_none());

    app.handle_failed(None, 7, "no session attached".into());

    assert_eq!(app.error.as_deref(), Some("no session attached"));
    assert_eq!(
        app.display_for(42).error.as_deref(),
        Some("no session attached")
    );
    assert!(
        !app.session_displays.contains_key(&0),
        "a connection-level failure must not create a phantom session-0 display"
    );
}

#[test]
fn handle_failed_connection_level_without_attached_session_still_writes_global_error() {
    // A connection-level rejection with no attached session to resolve to
    // has no display to update, but the user must still see it on the
    // status line — there is no transcript block for it.
    let mut app = test_app();
    app.attached_session_id = None;
    assert!(app.error.is_none());

    app.handle_failed(None, 9, "no session attached".into());

    assert_eq!(app.error.as_deref(), Some("no session attached"));
}

#[test]
fn handle_failed_for_background_session_does_not_write_global_error() {
    // The TUI subscribes to all activity, so a background session's
    // request failure arrives too.  It must be recorded on that session's
    // display but must not clobber the global status/error bar the user
    // is looking at (same gating as the ModelSelected / ReasoningEffortSet
    // arms).
    let mut app = test_app();
    app.attached_session_id = Some(42);
    app.active_session_id = Some(42);
    assert!(app.error.is_none());

    app.handle_failed(Some(99), 3, "background failure".into());

    assert_eq!(
        app.error, None,
        "background failure must not write the global error bar"
    );
    assert_eq!(
        app.display_for(99).error.as_deref(),
        Some("background failure")
    );
}
