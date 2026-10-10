//! User-text turn marker computation (content line and virtual slot).

use super::insert_turn;
use crate::test_util::test_app;
use choreo_proto::Turn;

// ── marker computation ──

#[test]
fn markers_empty_when_no_user_text_turns() {
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 20;

    let turn = Turn {
        created_at: choreo_proto::TimestampMs::now(),
        undone: false,
        error: None,
        user_text: None,
        assistant_text: Some("hello".into()),
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
        display.view.insert_or_replace(0, turn);
    }
    app.rebuild_height_prefix();

    let display = app.active_display_ref().unwrap();
    assert!(
        display.markers.is_empty(),
        "no markers should be created when no turn has user_text"
    );
}

#[test]
fn markers_created_for_each_user_text_turn() {
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 20;

    insert_turn(&mut app, 0, "user a", "assistant a");
    let turn_no_user = Turn {
        created_at: choreo_proto::TimestampMs::now(),
        undone: false,
        error: None,
        user_text: None,
        assistant_text: Some("assistant only".into()),
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
        display.view.insert_or_replace(1, turn_no_user);
    }
    insert_turn(&mut app, 2, "user c", "assistant c");

    app.rebuild_height_prefix();

    let display = app.active_display_ref().unwrap();
    assert_eq!(
        display.markers.len(),
        2,
        "expected 2 markers for 2 user-text turns"
    );
    assert!(
        display.markers[0].content_line < display.markers[1].content_line,
        "first user-text turn should appear before the second"
    );

    let total = display.total_history_height();
    for marker in &display.markers {
        assert!(
            marker.content_line < total,
            "marker content_line {0} should be < total history {total}",
            marker.content_line
        );
    }
}

#[test]
fn marker_virtual_slot_uses_final_total_height() {
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 20;
    let virtual_track = 2 * app.history_viewport.height as usize;

    insert_turn(&mut app, 0, "x", "y");
    insert_turn(&mut app, 1, "x", "y");
    app.rebuild_height_prefix();

    let display = app.active_display_ref().unwrap();
    let total = display.total_history_height();
    assert!(total > 0, "total history should be positive");

    let mut prev_end = 0usize;
    for (i, marker) in display.markers.iter().enumerate() {
        assert_eq!(
            marker.content_line, prev_end,
            "marker {i} content_line should equal the start of the turn"
        );
        if let Some(&end) = display.height_prefix.get(i) {
            prev_end = end;
        }

        let expected_slot = marker.content_line * virtual_track / total;
        assert_eq!(
            marker.virtual_slot, expected_slot,
            "marker {i} virtual_slot should use final total={total} as denominator"
        );
    }
}

#[test]
fn marker_virtual_slot_proportional_to_position() {
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 20;
    let virtual_track = 2 * app.history_viewport.height as usize;

    insert_turn(&mut app, 0, "a", "a");
    insert_turn(&mut app, 1, "b", "b");
    insert_turn(&mut app, 2, "c", "c");
    app.rebuild_height_prefix();

    let display = app.active_display_ref().unwrap();
    assert!(
        display.markers[0].virtual_slot <= display.markers[1].virtual_slot,
        "second marker slot should be >= first marker slot"
    );
    assert!(
        display.markers[1].virtual_slot <= display.markers[2].virtual_slot,
        "third marker slot should be >= second marker slot"
    );
    assert!(
        display.markers[2].virtual_slot < virtual_track,
        "last marker slot should be less than virtual_track={virtual_track}"
    );
}
