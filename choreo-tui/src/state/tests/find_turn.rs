//! `find_turn_at_row` click mapping, including the bottom-anchored
//! short-history case.

use crate::state::{App, find_turn_at_row};
use crate::test_util::test_app;
use choreo_proto::Turn;

// ── find_turn_at_row ──

#[test]
fn find_turn_at_row_returns_none_out_of_bounds() {
    let app = test_app();
    assert!(find_turn_at_row(&app, 999).is_none());
}

#[test]
fn find_turn_at_row_returns_turn_idx_and_offset() {
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
    display.view.insert_or_replace(1, turn);
    app.rebuild_height_prefix();

    // The history is shorter than the viewport, so content is anchored to
    // the bottom: content line 0 sits at screen row `vh - total`.
    let total = app.active_display().unwrap().total_history_height();
    #[expect(clippy::cast_possible_truncation)] // height-derived values fit u16
    let first_row = (app.history_viewport.height as usize - total) as u16;
    let (turn_idx, offset) = find_turn_at_row(&app, first_row).unwrap();
    assert_eq!(turn_idx, 0);
    assert_eq!(offset, 0);

    // Rows above the content are blank and must not map to a turn.
    assert!(find_turn_at_row(&app, first_row.saturating_sub(1)).is_none());
}

#[test]
fn find_turn_at_row_scrolled_history_maps_rows_correctly() {
    // Tall session with a scrollbar: scroll away from the bottom and
    // verify the mapping agrees with `render_history`'s bottom-up draw
    // order (content line `c` sits at screen row `vh - total + scroll + c`).
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 10;
    for i in 0..8 {
        let turn = Turn {
            created_at: choreo_proto::TimestampMs::now(),
            undone: false,
            error: None,
            user_text: Some(format!("user {i}")),
            assistant_text: Some(format!("assistant {i}")),
            assistant_reasoning: None,
            tool_calls: vec![],
            token_usage: None,
            tool_results: vec![],
            displayed_images: vec![],
            reasoning_artifact: None,
            reasoning_producer: None,
        };
        app.active_display()
            .unwrap()
            .view
            .insert_or_replace(i, turn);
    }
    app.rebuild_height_prefix();

    let total = app.active_display().unwrap().total_history_height();
    let vh = app.history_viewport.height as usize;
    assert!(
        total > vh,
        "test requires content taller than the viewport (scrollbar present)"
    );

    // Scroll partway up: max_scroll = total - vh.
    let scroll = (total - vh) / 2;
    app.scroll_to(scroll);
    assert_eq!(app.effective_scroll(), scroll);

    // The topmost visible content line is `total - scroll - vh`; the
    // bottom row of the viewport shows content line `total - scroll - 1`.
    let top_line = total - scroll - vh;
    let (idx, offset) = find_turn_at_row(&app, 0).expect("top row must map to a turn");
    assert_eq!(offset, top_line - turn_start(&app, idx));

    #[expect(clippy::cast_possible_truncation)] // viewport row fits u16
    let bottom_row = (vh - 1) as u16;
    let (idx_b, offset_b) = find_turn_at_row(&app, bottom_row).expect("bottom row must map");
    assert_eq!(
        offset_b,
        total - scroll - 1 - turn_start(&app, idx_b),
        "bottom row must map to the last visible content line"
    );
}

/// Content line where the turn at `turn_idx` starts (`height_prefix`
/// prefix-sum entry, 0 for the first turn).
fn turn_start(app: &App, turn_idx: usize) -> usize {
    app.active_display_ref()
        .and_then(|d| {
            turn_idx
                .checked_sub(1)
                .and_then(|prev| d.height_prefix.get(prev))
        })
        .copied()
        .unwrap_or(0)
}

#[test]
fn find_turn_at_row_short_history_anchors_content_at_bottom() {
    // Regression: when the history is shorter than the viewport (no
    // scrollbar shown), the renderer anchors the content at the bottom of
    // the viewport, but the click mapping assumed content always starts at
    // screen row 0.  The reasoning header (and image clicks) therefore
    // couldn't be hit on short sessions.
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 20;
    let turn = Turn {
        created_at: choreo_proto::TimestampMs::now(),
        undone: false,
        error: None,
        user_text: None,
        assistant_text: Some("Response text.".into()),
        assistant_reasoning: Some("Hidden thinking.".into()),
        tool_calls: vec![],
        token_usage: None,
        tool_results: vec![],
        displayed_images: vec![],
        reasoning_artifact: None,
        reasoning_producer: None,
    };
    app.active_display()
        .unwrap()
        .view
        .insert_or_replace(1, turn);
    app.rebuild_height_prefix();

    let (start, total) = {
        let display = app.active_display().unwrap();
        let (start, _end) = display.turn_layouts[0]
            .reasoning_header_range
            .expect("reasoning header range should exist");
        (start, display.total_history_height())
    };
    assert!(
        total < app.history_viewport.height as usize,
        "test requires a session too short to need the scrollbar"
    );

    // The header is drawn at screen row `vh - total + start` (bottom
    // anchored); clicking that row must resolve to the header's content
    // line `start`.
    #[expect(clippy::cast_possible_truncation)] // viewport row fits u16
    let screen_row = (app.history_viewport.height as usize - total + start) as u16;
    let (turn_idx, offset) = find_turn_at_row(&app, screen_row).expect("row must map to a turn");
    assert_eq!(turn_idx, 0);
    assert_eq!(offset, start);

    // The blank band above the content must not map to any turn.
    #[expect(clippy::cast_possible_truncation)] // viewport row fits u16
    let blank_row = (app.history_viewport.height as usize - total - 1) as u16;
    assert!(
        find_turn_at_row(&app, blank_row).is_none(),
        "empty rows above the content must not hit a turn"
    );
}
