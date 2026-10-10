//! Scrollbar notch math, scrollbar up/down steps, track-row mapping, and
//! `scroll_to_content_line`.

use crate::test_util::test_app;
use choreo_proto::Turn;

// ── scroll_to_content_line ──

#[test]
fn scroll_to_content_line_scrolls_to_content_line() {
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 5;

    for i in 0..5u32 {
        let turn = Turn {
            created_at: choreo_proto::TimestampMs::now(),
            undone: false,
            error: None,
            user_text: Some(format!("user text {i}")),
            assistant_text: Some(format!("assistant text {i}")),
            assistant_reasoning: None,
            tool_calls: vec![],
            token_usage: None,
            tool_results: vec![],
            displayed_images: vec![],
            reasoning_artifact: None,
            reasoning_producer: None,
        };
        let display = app.active_display().unwrap();
        display.view.insert_or_replace(i, turn);
    }
    app.rebuild_height_prefix();

    app.scroll_to_content_line(0);
    assert_eq!(app.effective_scroll(), app.max_scroll_offset());
}

// ── scrollbar_notch ──

#[test]
fn scrollbar_notch_no_content() {
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 20;
    assert_eq!(app.scrollbar_notch(), 1);
}

#[test]
fn scrollbar_notch_track_one() {
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 1;
    let display = app.active_display().unwrap();
    display.height_prefix.push(50);
    // max_scroll = 50 - 1 = 49, virtual_track = 2, notch = ceil(49 / 2) = 25
    assert_eq!(app.scrollbar_notch(), 25);
}

#[test]
fn scrollbar_notch_ceiling_division() {
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 50;
    let display = app.active_display().unwrap();
    display.height_prefix.push(150);
    // max_scroll = 150 - 50 = 100, virtual_track = 100, notch = ceil(100 / 100) = 1
    assert_eq!(app.scrollbar_notch(), 1);
}

#[test]
fn scrollbar_notch_rounds_up() {
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 30;
    let display = app.active_display().unwrap();
    display.height_prefix.push(105);
    // max_scroll = 105 - 30 = 75, virtual_track = 60, notch = ceil(75 / 60) = 2
    assert_eq!(app.scrollbar_notch(), 2);
}

// ── scrollbar_scroll_up / scrollbar_scroll_down ──

#[test]
fn scrollbar_scroll_up_increases_scroll_by_notch() {
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 10;
    let display = app.active_display().unwrap();
    display.height_prefix.push(110);
    // max_scroll = 100, virtual_track = 20, notch = 5
    display.history_scroll.scroll = 0;
    let before = app.effective_scroll();

    app.scrollbar_scroll_up();

    assert_eq!(app.effective_scroll(), before + 5);
}

#[test]
fn scrollbar_scroll_up_clamps_at_max_scroll() {
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 10;
    let display = app.active_display().unwrap();
    display.height_prefix.push(110);
    // max_scroll = 100, virtual_track = 20, notch = 5
    display.history_scroll.scroll = 100;

    app.scrollbar_scroll_up();

    assert_eq!(app.effective_scroll(), 100);
}

#[test]
fn scrollbar_scroll_down_decreases_scroll_by_notch() {
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 10;
    let display = app.active_display().unwrap();
    display.height_prefix.push(110);
    // max_scroll = 100, virtual_track = 20, notch = 5
    display.history_scroll.scroll = 100;
    let before = app.effective_scroll();

    app.scrollbar_scroll_down();

    assert_eq!(app.effective_scroll(), before - 5);
}

#[test]
fn scrollbar_scroll_down_clamps_at_zero() {
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 10;
    let display = app.active_display().unwrap();
    display.height_prefix.push(110);
    // max_scroll = 100, virtual_track = 20, notch = 5
    display.history_scroll.scroll = 5;

    app.scrollbar_scroll_down();

    assert_eq!(app.effective_scroll(), 0);
}

// ── scroll_to_track_row ──

#[test]
fn scroll_to_track_row_at_bottom() {
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 20;
    let display = app.active_display().unwrap();
    display.height_prefix.push(110);
    // max_scroll = 90, denom = 19
    display.history_scroll.scroll = 90;

    app.scroll_to_track_row(0, 20);

    assert_eq!(app.effective_scroll(), 90);
}

#[test]
fn scroll_to_track_row_at_top() {
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 20;
    let display = app.active_display().unwrap();
    display.height_prefix.push(110);
    // max_scroll = 90, denom = 19
    display.history_scroll.scroll = 0;

    app.scroll_to_track_row(19, 20);

    assert_eq!(app.effective_scroll(), 0);
}

#[test]
fn scroll_to_track_row_midpoint() {
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 10;
    let display = app.active_display().unwrap();
    display.height_prefix.push(110);
    // max_scroll = 100, denom = 9

    app.scroll_to_track_row(4, 10);

    assert_eq!(app.effective_scroll(), 56);
}

#[test]
fn scroll_to_track_row_zero_viewport() {
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 0;
    let display = app.active_display().unwrap();
    display.height_prefix.push(110);
    display.history_scroll.scroll = 42;

    app.scroll_to_track_row(0, 0);

    assert_eq!(app.effective_scroll(), 42);
}

#[test]
fn scroll_to_track_row_track_one() {
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 10;
    let display = app.active_display().unwrap();
    display.height_prefix.push(110);
    display.history_scroll.scroll = 42;

    app.scroll_to_track_row(0, 1);

    assert_eq!(app.effective_scroll(), 42);
}

#[test]
fn scroll_to_track_row_mouse_row_clamped() {
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 20;
    let display = app.active_display().unwrap();
    display.height_prefix.push(110);
    // max_scroll = 90, denom = 19
    display.history_scroll.scroll = 0;

    app.scroll_to_track_row(30, 20);

    assert_eq!(app.effective_scroll(), 0);
}

// ── scroll_to_content_line ──

#[test]
fn scroll_to_content_line_idempotent_when_already_visible() {
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 200;

    let turn = Turn {
        created_at: choreo_proto::TimestampMs::now(),
        undone: false,
        error: None,
        user_text: Some("hello".into()),
        assistant_text: Some("world".into()),
        assistant_reasoning: None,
        tool_calls: vec![],
        token_usage: None,
        tool_results: vec![],
        displayed_images: vec![],
        reasoning_artifact: None,
        reasoning_producer: None,
    };
    let display = app.active_display().unwrap();
    display.view.insert_or_replace(0, turn);
    app.rebuild_height_prefix();

    let before = app.effective_scroll();
    app.scroll_to_content_line(0);
    assert_eq!(app.effective_scroll(), before);
}

#[test]
fn scroll_to_content_line_large_content_line_saturates() {
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 5;

    for i in 0..5u32 {
        let turn = Turn {
            created_at: choreo_proto::TimestampMs::now(),
            undone: false,
            error: None,
            user_text: Some(format!("user text {i}")),
            assistant_text: Some(format!("assistant text {i}")),
            assistant_reasoning: None,
            tool_calls: vec![],
            token_usage: None,
            tool_results: vec![],
            displayed_images: vec![],
            reasoning_artifact: None,
            reasoning_producer: None,
        };
        let display = app.active_display().unwrap();
        display.view.insert_or_replace(i, turn);
    }
    app.rebuild_height_prefix();

    app.scroll_to_content_line(9999);
    assert_eq!(app.effective_scroll(), 0);
}
