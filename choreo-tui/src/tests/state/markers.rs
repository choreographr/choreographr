//! User-text turn marker computation (content line and virtual slot).

use crate::state::SessionDisplayState;
use crate::tests::support::{insert_turn, test_app};
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

/// The stored marker slot and the scrollbar occupancy bitmap must never
/// disagree, even after the total height grows without a marker rebuild.
///
/// This is the invariant the scrollbar click handler depends on: it tests each
/// marker's stored `virtual_slot` against the clicked half-slots, so that slot
/// has to be the one the draw-time bitmap placed the marker at — otherwise a
/// click on a visible marker misses and falls through to a track jump. The
/// streaming fast path grows the total (`height_prefix`) while skipping
/// `rebuild_markers` for the last visible turn, so the two must stay consistent
/// through both the no-rebuild and the catch-up-rebuild frames.
#[test]
fn marker_slots_stay_in_sync_after_total_height_drift() {
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 20;
    let viewport = app.history_viewport;
    let virtual_track = 2 * viewport.height as usize;

    insert_turn(&mut app, 0, "a", "first answer");
    insert_turn(&mut app, 1, "b", "second answer");
    insert_turn(&mut app, 2, "c", "third answer");
    app.rebuild_height_prefix();

    // Settle the bitmap the renderer would draw from this frame.
    app.active_display().unwrap().ensure_marker_slots(&viewport);
    let total_old = app.total_history_height();
    let drawn: Vec<usize> = {
        let display = app.active_display_ref().unwrap();
        display.markers.iter().map(|m| m.virtual_slot).collect()
    };
    assert_eq!(drawn.len(), 3, "one marker per user-text turn");

    // Simulate the streaming fast path for the last visible turn: the height
    // model grows, but `rebuild_markers` is skipped (nothing marks the bitmap
    // dirty), so the total moves while the stored slots keep the old
    // denominator.
    let delta = total_old;
    {
        let display = app.active_display().unwrap();
        let last = display.turn_heights.len() - 1;
        display.turn_heights[last] += delta;
        for prefix in &mut display.height_prefix[last..] {
            *prefix += delta;
        }
    }
    let total_new = app.total_history_height();
    assert!(total_new > total_old, "the drift grew the total");

    // No rebuild is due, so the frame reuses the bitmap: the stored slots did
    // not move and every marker's bit is still set where it is stored. A click
    // at the drawn row therefore still finds its marker.
    app.active_display().unwrap().ensure_marker_slots(&viewport);
    {
        let display = app.active_display_ref().unwrap();
        let slots_now: Vec<usize> = display.markers.iter().map(|m| m.virtual_slot).collect();
        assert_eq!(
            slots_now, drawn,
            "stored slots must stay put when no bitmap rebuild is due"
        );
        for marker in &display.markers {
            assert!(
                display
                    .marker_slots
                    .get(marker.virtual_slot)
                    .copied()
                    .unwrap_or(false),
                "bitmap must be set at the stored slot {}",
                marker.virtual_slot
            );
        }
    }

    // A rebuild is now due (structural change or track change). Every marker is
    // recomputed from its content line against the *current* total, the stored
    // slot is written back in the same pass that sets the bitmap bit, so the two
    // agree again — now at the new scale.
    {
        let display = app.active_display().unwrap();
        display.marker_slots_dirty = true;
        display.ensure_marker_slots(&viewport);
    }
    let display = app.active_display_ref().unwrap();
    for marker in &display.markers {
        let expected =
            SessionDisplayState::marker_slot(marker.content_line, virtual_track, total_new);
        assert_eq!(
            marker.virtual_slot, expected,
            "stored slot must be recomputed against the current total={total_new}"
        );
        assert!(
            display
                .marker_slots
                .get(marker.virtual_slot)
                .copied()
                .unwrap_or(false),
            "bitmap must be set at the freshly stored slot {}",
            marker.virtual_slot
        );
    }
    // No bitmap bit may sit outside a stored slot: the bitmap is derived from
    // exactly these markers, so a stale bit would be a phantom marker.
    for (slot, &set) in display.marker_slots.iter().enumerate() {
        if set {
            assert!(
                display.markers.iter().any(|m| m.virtual_slot == slot),
                "bitmap bit {slot} does not correspond to any marker's stored slot"
            );
        }
    }
    // The drift really did rescale at least one marker (a marker with content
    // on line 0 legitimately stays at slot 0).
    assert!(
        display
            .markers
            .iter()
            .zip(&drawn)
            .any(|(m, &old)| m.virtual_slot != old),
        "the catch-up rebuild should rescale the slots against the grown total"
    );
}
