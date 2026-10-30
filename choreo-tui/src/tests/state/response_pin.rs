//! The streaming response auto-pin: holding an in-flight response's top row at
//! the viewport top, arming/disarming, and surviving a session switch.
//!
//! These exercise the `pin` module's state machine through the same entry points
//! the UI loop uses (`handle_request_stream` + `compute_total_height_and_markers`),
//! so the pin is checked where it is actually decided.

use crate::state::{App, ResponsePin};
use crate::tests::support::{insert_turn, test_app};
use choreo_client_core::TurnEventHandler;
use choreo_client_core::dispatch::ToolCallEvent;
use choreo_proto::{OutputStream, ToolResultRecord, Turn};
use std::borrow::Cow;

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
        assert!(display.view.start_request(7, 1));
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
        "the response's top row sits at the viewport top"
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
        assert!(display.view.start_request(7, 1));
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
        assert!(display.view.start_request(7, 1));
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
        assert!(display.view.start_request(7, 1));
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
fn a_noop_manual_scroll_keeps_the_pin_armed() {
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 6;
    // A tall prior turn so the history is scrollable.
    insert_turn(&mut app, 0, "earlier", &"old\n".repeat(20));
    {
        let display = app.active_display().unwrap();
        display.view.insert_or_replace(1, pending_turn("ask"));
        assert!(display.view.start_request(7, 1));
    }
    app.rebuild_height_prefix();

    // Arm the pin, but do not move off the bottom: the response is short and the
    // viewport is still following the bottom (`scroll == 0`).
    app.handle_request_stream(0, 7, OutputStream::Answer, Cow::Borrowed("hi"));
    app.compute_total_height_and_markers();
    assert_eq!(
        app.active_display_ref().unwrap().response_pin,
        ResponsePin::Armed(1)
    );

    // `scroll_down` at the bottom is a no-op — it must not disarm the pin, so
    // the response still pins once it fills the viewport.
    app.scroll_down(1);
    assert_eq!(
        app.active_display_ref().unwrap().response_pin,
        ResponsePin::Armed(1),
        "a no-op scroll-down must not disarm the pin"
    );

    stream_answer(&mut app, 0, 7, &"line\n".repeat(80));
    assert_eq!(
        app.active_display_ref().unwrap().response_pin,
        ResponsePin::Pinned(1),
        "the still-armed response pins once it fills the viewport"
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
        assert!(display.view.start_request(7, 1));
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
        assert!(display.view.start_request(7, 1));
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

// ── multi-turn (tool-loop) runs ──────────────────────────────────────

/// A continuation turn in an agent run: no user text (the run is continuing),
/// no response yet.  The daemon appends these via its tool loop under the same
/// stream id as the first turn.
fn continuation_turn() -> Turn {
    Turn {
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
    }
}

#[test]
fn a_new_turn_supersedes_the_pinned_response_and_the_run_ends_pinned_on_it() {
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 10;
    {
        let display = app.active_display().unwrap();
        display.view.insert_or_replace(1, pending_turn("ask"));
        assert!(display.view.start_request(7, 1));
    }
    app.rebuild_height_prefix();

    // Turn 1 streams a long response and pins its top.
    stream_answer(&mut app, 0, 7, &"first\n".repeat(60));
    {
        let display = app.active_display_ref().unwrap();
        assert_eq!(display.response_pin, ResponsePin::Pinned(1));
        assert!(
            display.history_scroll.scroll > 0,
            "turn 1 is pinned off the bottom"
        );
    }

    // The run continues: the daemon appends turn 2 under the same stream id and
    // starts it.  The engaged pin is re-targeted and the viewport released.
    app.handle_turn_appended(0, 2, continuation_turn());
    app.handle_started(0, 7, 2, 0);
    {
        let display = app.active_display_ref().unwrap();
        assert_eq!(
            display.response_pin,
            ResponsePin::Idle,
            "the superseded pin is released"
        );
        assert_eq!(
            display.history_scroll.scroll, 0,
            "the viewport is released back to the bottom for the new turn"
        );
    }

    // Turn 2 streams its (final) response and pins in turn.
    stream_answer(&mut app, 0, 7, &"final\n".repeat(60));
    let (row, turn2_start) = {
        let display = app.active_display_ref().unwrap();
        assert_eq!(
            display.response_pin,
            ResponsePin::Pinned(2),
            "the final turn is the one pinned"
        );
        (
            display.turn_layouts[1]
                .response_start_row
                .expect("turn 2 has a response"),
            // `height_prefix[0]` is turn 1's total height — turn 2's start.
            display.height_prefix[0],
        )
    };
    assert_eq!(
        viewport_top_line(&app),
        turn2_start + row,
        "the final turn's top row sits at the viewport top"
    );
}

#[test]
fn a_manual_scroll_after_pinning_drops_the_pin() {
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 10;
    {
        let display = app.active_display().unwrap();
        display.view.insert_or_replace(1, pending_turn("ask"));
        assert!(display.view.start_request(7, 1));
    }
    app.rebuild_height_prefix();
    stream_answer(&mut app, 0, 7, &"line\n".repeat(60));
    assert_eq!(
        app.active_display_ref().unwrap().response_pin,
        ResponsePin::Pinned(1)
    );

    app.scroll_up(1);
    assert_eq!(
        app.active_display_ref().unwrap().response_pin,
        ResponsePin::Idle,
        "a manual scroll after pinning drops the engaged pin"
    );
}

#[test]
fn a_new_turn_after_a_manual_scroll_does_not_release_the_view() {
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 10;
    {
        let display = app.active_display().unwrap();
        display.view.insert_or_replace(1, pending_turn("ask"));
        assert!(display.view.start_request(7, 1));
    }
    app.rebuild_height_prefix();
    stream_answer(&mut app, 0, 7, &"line\n".repeat(60));

    // The user scrolls away from the pinned response — the pin is dropped, so
    // the viewport is theirs.
    app.scroll_up(2);
    let user_scroll = app.active_display_ref().unwrap().history_scroll.scroll;
    assert!(user_scroll > 0);

    // A new turn must NOT release the viewport back to the bottom over them.
    app.handle_turn_appended(0, 2, continuation_turn());
    app.handle_started(0, 7, 2, 0);
    assert_eq!(
        app.active_display_ref().unwrap().history_scroll.scroll,
        user_scroll,
        "a user who scrolled away is not yanked by the next turn"
    );

    // The new turn arms, but only ever pins from the bottom — not while the
    // user reads elsewhere.
    stream_answer(&mut app, 0, 7, &"final\n".repeat(60));
    assert_eq!(
        app.active_display_ref().unwrap().response_pin,
        ResponsePin::Armed(2),
        "the new turn stays armed while the user reads elsewhere"
    );
}

// ── release on content that follows the response ─────────────────────

/// A first turn whose tool result placeholder is already seeded, so a
/// `handle_tool_result_chunk` for `call_id` appends to it — the shape the
/// daemon broadcasts (`broadcast_turn_appended`) before the tools run.
fn turn_with_tool_result(user_text: &str, call_id: &str) -> Turn {
    Turn {
        created_at: choreo_proto::TimestampMs::now(),
        undone: false,
        error: None,
        user_text: Some(user_text.into()),
        assistant_text: None,
        assistant_reasoning: None,
        tool_calls: vec![],
        token_usage: None,
        tool_results: vec![ToolResultRecord {
            call_id: call_id.into(),
            // Not a quiet tool, so the result renders expanded by default and
            // its streamed content actually grows the turn.
            name: "find".into(),
            content: String::new(),
            is_error: false,
            invocation_description: "Running `x`.".into(),
            image: None,
        }],
        displayed_images: vec![],
        reasoning_artifact: None,
        reasoning_producer: None,
    }
}

fn tool_call_started(app: &mut App, call_id: &str) {
    app.handle_tool_call_event(
        0,
        7,
        ToolCallEvent::Started {
            call_id: call_id.into(),
            tool_name: "find".into(),
            arguments_json: "{}".into(),
            invocation_description: "Running `x`.".into(),
        },
    );
}

#[test]
fn a_tool_call_releases_a_pinned_response_and_returns_to_the_bottom() {
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 10;
    {
        let display = app.active_display().unwrap();
        display.view.insert_or_replace(1, pending_turn("ask"));
        assert!(display.view.start_request(7, 1));
    }
    app.rebuild_height_prefix();

    // A long response pins its top.
    stream_answer(&mut app, 0, 7, &"line\n".repeat(60));
    assert_eq!(
        app.active_display_ref().unwrap().response_pin,
        ResponsePin::Pinned(1)
    );
    assert!(
        app.effective_scroll() > 0,
        "the response is pinned off the bottom"
    );

    // The model then calls a tool: the pin is released and the viewport returns
    // to the bottom so the tool call and its output stream into view.
    tool_call_started(&mut app, "c1");
    assert_eq!(
        app.active_display_ref().unwrap().response_pin,
        ResponsePin::Idle,
        "a tool call releases the pinned response"
    );
    assert_eq!(
        app.active_display_ref().unwrap().history_scroll.scroll,
        0,
        "the viewport returns to the bottom for the tool call"
    );
}

#[test]
fn a_tool_result_chunk_releases_a_pinned_response_and_follows_the_output() {
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 10;
    {
        let display = app.active_display().unwrap();
        display
            .view
            .insert_or_replace(1, turn_with_tool_result("ask", "c1"));
        assert!(display.view.start_request(7, 1));
    }
    app.rebuild_height_prefix();

    stream_answer(&mut app, 0, 7, &"line\n".repeat(60));
    assert_eq!(
        app.active_display_ref().unwrap().response_pin,
        ResponsePin::Pinned(1)
    );

    // Tool output arrives: the pin is released and the view follows the bottom
    // as the output grows.
    app.handle_tool_result_chunk(0, 7, "c1".into(), b"out\n".repeat(80));
    app.compute_total_height_and_markers();
    assert_eq!(
        app.active_display_ref().unwrap().response_pin,
        ResponsePin::Idle,
        "tool output releases the pinned response"
    );
    assert_eq!(
        app.effective_scroll(),
        0,
        "the view follows the tool output from the bottom"
    );
}

#[test]
fn an_armed_short_response_that_a_tool_follows_never_pins() {
    // Regression: a short response never fills the viewport, so it stays
    // `Armed`; when a tool follows and its output grows the turn tall enough
    // that the finished response's top would pass the viewport top, a stale
    // `Armed` would fire and yank the view up onto that finished response.  The
    // tool content must release it instead.
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 8;
    {
        let display = app.active_display().unwrap();
        display
            .view
            .insert_or_replace(1, turn_with_tool_result("ask", "c1"));
        assert!(display.view.start_request(7, 1));
    }
    app.rebuild_height_prefix();

    // A short response: armed, but never engaged (the viewport still follows
    // the bottom).
    app.handle_request_stream(0, 7, OutputStream::Answer, Cow::Borrowed("hi"));
    app.compute_total_height_and_markers();
    assert_eq!(
        app.active_display_ref().unwrap().response_pin,
        ResponsePin::Armed(1),
        "a short response stays armed, never pinned"
    );
    assert_eq!(app.effective_scroll(), 0);

    // A tool follows and its output grows the turn well past the viewport.
    app.handle_tool_result_chunk(0, 7, "c1".into(), b"out\n".repeat(80));
    app.compute_total_height_and_markers();
    assert_eq!(
        app.active_display_ref().unwrap().response_pin,
        ResponsePin::Idle,
        "the armed trigger must not fire on the finished response"
    );
    assert_eq!(
        app.effective_scroll(),
        0,
        "the view follows the tool output from the bottom"
    );
}

#[test]
fn a_new_turn_releases_an_armed_response() {
    // A tool-less continuation (e.g. a truncation recovery) has no tool-call
    // event to release the pin; the new turn's `Started` must do it.
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 6;
    {
        let display = app.active_display().unwrap();
        display.view.insert_or_replace(1, pending_turn("ask"));
        assert!(display.view.start_request(7, 1));
    }
    app.rebuild_height_prefix();

    app.handle_request_stream(0, 7, OutputStream::Answer, Cow::Borrowed("hi"));
    app.compute_total_height_and_markers();
    assert_eq!(
        app.active_display_ref().unwrap().response_pin,
        ResponsePin::Armed(1)
    );

    app.handle_turn_appended(0, 2, continuation_turn());
    app.handle_started(0, 7, 2, 0);
    assert_eq!(
        app.active_display_ref().unwrap().response_pin,
        ResponsePin::Idle,
        "a new turn releases the armed trigger"
    );
}
