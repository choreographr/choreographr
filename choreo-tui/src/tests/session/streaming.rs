use crate::connection::handle_daemon_message;
use crate::state::*;
use crate::tests::support::test_app;
use choreo_client_core::TurnEventHandler;
use choreo_proto::{
    DaemonMessage, DaemonMessageType, DisplayedImageRecord, ImageMetadata, SessionEvent,
    SessionStatus, TokenUsage, Turn,
};

// ── multi-session streaming: switching keeps accumulated content ──

/// A turn whose assistant text was streamed in via `OutputChunk`.
fn streamed_turn(user_text: &str, assistant_text: &str) -> Turn {
    Turn {
        created_at: choreo_proto::TimestampMs::now(),
        undone: false,
        error: None,
        user_text: Some(user_text.to_string()),
        assistant_text: Some(assistant_text.to_string()),
        assistant_reasoning: None,
        tool_calls: vec![],
        token_usage: None,
        tool_results: vec![],
        displayed_images: vec![],
        reasoning_artifact: None,
        reasoning_producer: None,
    }
}

/// The empty placeholder the daemon's attach snapshot carries for an
/// in-flight turn (what `start_turn` inserts on the main session thread
/// before the worker owns the live content).
fn placeholder_turn(user_text: &str) -> Turn {
    Turn {
        created_at: choreo_proto::TimestampMs::now(),
        undone: false,
        error: None,
        user_text: Some(user_text.to_string()),
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

#[expect(clippy::assert_is_empty)] // plain is_empty asserts read best here
#[test]
fn reset_for_session_switch_preserves_accumulated_streaming_state() {
    let mut app = test_app();
    // While viewing session 0, session 7's content accumulated via the
    // all-activity subscription: a streamed turn, its request→turn routing
    // entry, the active-request set, a live token estimate, and a reasoning
    // preference.
    {
        let display = app.display_for(7);
        display
            .view
            .insert_or_replace(5, streamed_turn("user q", "partial answer"));
        display.view.request_to_turn.insert(99, 5);
        display.active.insert(99);
        display.live_output_tokens = 42;
        display.reasoning_override.insert(5, true);
    }

    app.reset_for_session_switch(7);

    let display = app.display_for(7);
    // Live state survives the switch…
    assert_eq!(
        display.view.turns.len(),
        1,
        "accumulated turns must survive the switch"
    );
    assert_eq!(
        display
            .view
            .turns
            .get(&5)
            .unwrap()
            .assistant_text
            .as_deref(),
        Some("partial answer"),
        "streamed content must survive the switch"
    );
    assert_eq!(
        display.view.request_to_turn.get(&99),
        Some(&5),
        "request→turn routing must survive the switch"
    );
    assert!(
        display.active.contains(&99),
        "active request set must survive"
    );
    assert_eq!(display.live_output_tokens, 42);
    assert_eq!(display.reasoning_override.get(&5), Some(&true));
    // …while transient render state is reset and queued for a full rebuild.
    assert!(display.markers_dirty);
    assert!(display.visible_turn_ids.is_empty());
    assert!(display.height_prefix.is_empty());
    assert!(display.progress_dirty);
}

#[test]
fn reset_for_session_switch_preserves_scroll_position() {
    let mut app = test_app();
    // A short viewport so a handful of turns makes the history scrollable.
    app.history_viewport = HistoryViewport {
        width: 80,
        height: 5,
    };
    app.active_session_id = Some(1);
    app.attached_session_id = Some(1);
    // Session 1 accumulates a conversation long enough to scroll.
    {
        let display = app.display_for(1);
        for id in 0..6u32 {
            display
                .view
                .insert_or_replace(id, streamed_turn("question", "an answer that wraps"));
        }
    }
    app.compute_total_height_and_markers();
    // The user scrolls up to re-read earlier turns…
    app.scroll_up(2);
    let saved = app.effective_scroll();
    assert!(saved > 0, "session 1 should be scrolled up");

    // …switches to a session they have never opened: it opens at the bottom.
    // Mirror the UI loop's pre-render order: it clamps scroll *before* the
    // draw-time height rebuild, which must not clobber the preserved offset
    // (the cleared height cache makes `max_scroll_offset` read 0 there).
    app.reset_for_session_switch(2);
    app.clamp_scroll_state();
    app.compute_total_height_and_markers();
    assert_eq!(
        app.effective_scroll(),
        0,
        "a session visited for the first time opens at the bottom"
    );

    // …and returns to session 1: the reading position is remembered.
    app.reset_for_session_switch(1);
    // The daemon's attach snapshot lands after the switch and marks the display
    // content-dirty.  The height cache was just cleared, so the rebuild must
    // treat this as a baseline (not anchor the offset against a phantom old
    // height) and leave the preserved offset intact.
    app.active_display().unwrap().mark_content_changed();
    app.clamp_scroll_state();
    app.compute_total_height_and_markers();
    assert_eq!(
        app.effective_scroll(),
        saved,
        "session 1's scroll position must be remembered across the switch"
    );
}

/// The content line at the top of the history viewport, from the raw scroll
/// state (the thing that must stay fixed when the user returns).
fn top_content_line(app: &App) -> usize {
    app.total_history_height()
        .saturating_sub(app.effective_scroll())
        .saturating_sub(app.history_viewport.height as usize)
}

#[test]
fn session_switch_preserves_content_across_viewport_height_change() {
    let mut app = test_app();
    app.history_viewport = HistoryViewport {
        width: 80,
        height: 8,
    };
    app.active_session_id = Some(1);
    app.attached_session_id = Some(1);
    {
        let display = app.display_for(1);
        for id in 0..8u32 {
            display
                .view
                .insert_or_replace(id, streamed_turn("question", "an answer that wraps"));
        }
    }
    app.compute_total_height_and_markers();
    app.scroll_up(3);
    let anchored = top_content_line(&app);
    assert!(
        app.effective_scroll() > 0,
        "session 1 should be scrolled up"
    );

    // Leave and return with a SHORTER history viewport — exactly what happens
    // when the help/status bands reflow the layout on attach.  A raw
    // from-bottom offset would slide the content; the anchor must not.
    app.reset_for_session_switch(2);
    app.clamp_scroll_state();
    app.compute_total_height_and_markers();
    app.reset_for_session_switch(1);
    app.history_viewport.height = 6;
    app.clamp_scroll_state();
    app.compute_total_height_and_markers();

    assert_eq!(
        top_content_line(&app),
        anchored,
        "the same content must stay at the top of the viewport"
    );
}

#[test]
fn session_switch_keeps_anchored_content_when_background_content_grows() {
    let mut app = test_app();
    app.history_viewport = HistoryViewport {
        width: 80,
        height: 8,
    };
    app.active_session_id = Some(1);
    app.attached_session_id = Some(1);
    {
        let display = app.display_for(1);
        for id in 0..8u32 {
            display
                .view
                .insert_or_replace(id, streamed_turn("question", "an answer that wraps"));
        }
    }
    app.compute_total_height_and_markers();
    app.scroll_up(3);
    let anchored = top_content_line(&app);

    // Leave; while away the session keeps streaming (a new turn lands via the
    // all-activity subscription).
    app.reset_for_session_switch(2);
    app.clamp_scroll_state();
    app.compute_total_height_and_markers();
    {
        let display = app.display_for(1);
        display
            .view
            .insert_or_replace(8, streamed_turn("follow-up", "another answer"));
        display.mark_content_changed();
    }

    // Return: the anchored content is still at the top (growth is below it).
    app.reset_for_session_switch(1);
    app.clamp_scroll_state();
    app.compute_total_height_and_markers();
    assert_eq!(
        top_content_line(&app),
        anchored,
        "background growth must not shift the anchored content"
    );
}

#[test]
fn session_switch_at_bottom_follows_new_background_content() {
    let mut app = test_app();
    app.history_viewport = HistoryViewport {
        width: 80,
        height: 8,
    };
    app.active_session_id = Some(1);
    app.attached_session_id = Some(1);
    {
        let display = app.display_for(1);
        for id in 0..8u32 {
            display
                .view
                .insert_or_replace(id, streamed_turn("question", "an answer that wraps"));
        }
    }
    app.compute_total_height_and_markers();
    // No scroll: the user left the session pinned to the bottom.
    assert_eq!(app.effective_scroll(), 0);

    app.reset_for_session_switch(2);
    app.clamp_scroll_state();
    app.compute_total_height_and_markers();
    {
        let display = app.display_for(1);
        display
            .view
            .insert_or_replace(8, streamed_turn("follow-up", "another answer"));
        display.mark_content_changed();
    }

    app.reset_for_session_switch(1);
    app.clamp_scroll_state();
    app.compute_total_height_and_markers();
    assert_eq!(
        app.effective_scroll(),
        0,
        "a bottom-pinned session must follow new content on return"
    );
}

#[test]
fn handle_session_state_keeps_accumulated_live_turn_over_snapshot_placeholder() {
    let mut app = test_app();
    app.attached_session_id = Some(7);
    let (tx, _rx) = crossbeam_channel::unbounded();

    // Accumulated via the all-activity subscription: turn 5 is live-streaming.
    {
        let display = app.display_for(7);
        display
            .view
            .insert_or_replace(5, streamed_turn("user q", "streamed answer so far"));
        display.view.request_to_turn.insert(99, 5);
        display.active.insert(99);
    }

    // Daemon attach snapshot: turn 3 is finished, turn 5 is the in-flight
    // placeholder (assistant_text is None).
    let mut snapshot_turns = std::collections::BTreeMap::new();
    snapshot_turns.insert(3, streamed_turn("old q", "finished answer"));
    snapshot_turns.insert(5, placeholder_turn("user q"));

    handle_daemon_message(
        DaemonMessage::broadcast(DaemonMessageType::Session {
            session_id: Some(7),
            event: SessionEvent::SessionState {
                title: None,
                selected_model: None,
                parent_session_id: None,
                working_dir: None,
                turns: snapshot_turns,
                active_tool_groups: vec![],
                token_usage: None,
                context_window: None,
                last_prompt_tokens: None,
                reasoning_effort: None,
                reasoning_capability: None,
                status: SessionStatus::Inactive,
            },
        }),
        &mut app,
        &tx,
    )
    .expect("handle_daemon_message should succeed");

    let display = app.display_for(7);
    // The in-flight turn keeps its live content — the placeholder must not win.
    assert_eq!(
        display
            .view
            .turns
            .get(&5)
            .unwrap()
            .assistant_text
            .as_deref(),
        Some("streamed answer so far"),
        "accumulated live content must survive the attach snapshot merge"
    );
    // Finished turns come from the snapshot.
    assert_eq!(
        display
            .view
            .turns
            .get(&3)
            .unwrap()
            .assistant_text
            .as_deref(),
        Some("finished answer")
    );
    // Request routing and the active set survive the merge.
    assert_eq!(display.view.request_to_turn.get(&99), Some(&5));
    assert!(display.active.contains(&99));
}

#[test]
fn done_for_background_session_does_not_pollute_attached_display() {
    let mut app = test_app();
    app.attached_session_id = Some(0);
    let (tx, _rx) = crossbeam_channel::unbounded();
    // The attached session already has its own token usage.
    app.display_for(0).token_usage = Some(TokenUsage {
        input_tokens: 1,
        output_tokens: 2,
        total_tokens: 3,
        cached_tokens: 0,
        cache_write_tokens: 0,
    });
    // Session 7 (background, streamed via SubscribeAllActivity) has an
    // in-flight request that is about to finish.
    {
        let display = app.display_for(7);
        display.view.request_to_turn.insert(50, 5);
        display.active.insert(50);
    }

    handle_daemon_message(
        DaemonMessage::broadcast(DaemonMessageType::Session {
            session_id: Some(7),
            event: SessionEvent::Done {
                stream_id: 50,
                token_usage: Some(TokenUsage {
                    input_tokens: 99,
                    output_tokens: 99,
                    total_tokens: 99,
                    cached_tokens: 0,
                    cache_write_tokens: 0,
                }),
                last_prompt_tokens: Some(99),
            },
        }),
        &mut app,
        &tx,
    )
    .expect("handle_daemon_message should succeed");

    // The attached display is untouched — no progress-bar clobber from a
    // background session finishing.
    assert_eq!(
        app.display_for(0).token_usage,
        Some(TokenUsage {
            input_tokens: 1,
            output_tokens: 2,
            total_tokens: 3,
            cached_tokens: 0,
            cache_write_tokens: 0,
        })
    );
    assert!(!app.display_for(0).progress_dirty);
    // The background session's own display got the token usage and its
    // request was cleaned up via the generic handle_done dispatch.
    assert_eq!(
        app.display_for(7).token_usage,
        Some(TokenUsage {
            input_tokens: 99,
            output_tokens: 99,
            total_tokens: 99,
            cached_tokens: 0,
            cache_write_tokens: 0,
        })
    );
    assert!(!app.display_for(7).active.contains(&50));
    assert!(!app.display_for(7).view.request_to_turn.contains_key(&50));
}

#[test]
fn handle_session_status_changed_updates_attached_status() {
    let mut app = test_app();
    assert!(app.attached_status.is_none());

    // With no attached session, status should not be cached.
    app.handle_session_status_changed(42, &SessionStatus::Responding, 1_705_314_000_000);
    assert!(app.attached_status.is_none());

    // Once attached, a status change for that session should be cached.
    app.attached_session_id = Some(42);
    app.handle_session_status_changed(42, &SessionStatus::Responding, 1_705_314_000_000);
    assert_eq!(app.attached_status, Some(SessionStatus::Responding));

    // A status change for a different session should not overwrite.
    app.handle_session_status_changed(99, &SessionStatus::Sleeping, 1_705_314_000_000);
    assert_eq!(app.attached_status, Some(SessionStatus::Responding));

    // A subsequent change for the attached session should update.
    app.handle_session_status_changed(
        42,
        &SessionStatus::ToolCall("test".into()),
        1_705_314_000_000,
    );
    assert_eq!(
        app.attached_status,
        Some(SessionStatus::ToolCall("test".into()))
    );
}

#[test]
fn handle_turn_appended_with_displayed_image_populates_rendered_images() {
    let mut app = test_app();
    let metadata = ImageMetadata {
        mime_type: "image/png".to_string(),
        width: 640,
        height: 480,
        byte_len: 100,
        alt: Some("test image".to_string()),
    };
    let turn = Turn {
        created_at: choreo_proto::TimestampMs::now(),
        undone: false,
        error: None,
        user_text: Some("generate an image".to_string()),
        assistant_text: None,
        assistant_reasoning: None,
        tool_calls: vec![],
        token_usage: None,
        tool_results: vec![],
        displayed_images: vec![DisplayedImageRecord {
            metadata: metadata.clone(),
            data: vec![0u8; 100],
            tool_call_id: None,
        }],
        reasoning_artifact: None,
        reasoning_producer: None,
    };
    app.handle_turn_appended(0, 1, turn);

    let images = app.rendered_images.get(&0).and_then(|m| m.get(&1)).unwrap();
    assert_eq!(images.len(), 1);
    let img = images.get(&crate::state::ImageSlot::Displayed(0)).unwrap();
    assert_eq!(img.metadata.mime_type, "image/png");
    assert_eq!(img.metadata.width, 640);
    assert_eq!(img.metadata.height, 480);
    assert_eq!(img.data.len(), 100);
    assert!(img.pending_job.is_none());
    assert!(img.protocols.is_empty());
}
