//! Pointer-shape selection (OSC 22) over the Chat page's hover targets.
//!
//! The shape-selection function is exercised against each target the Chat page
//! hit-tests for clicks: the scrollbar column, the history pane's text, the
//! composer box, a clickable turn header, and the empty chrome around them.

use crate::selection::content_to_screen_row;
use crate::state::App;
use crate::terminal::pointer::PointerShape;
use crate::tests::support::{insert_turn, test_app};
use choreo_proto::Turn;

const TERM_W: u16 = 80;
const TERM_H: u16 = 24;

/// A Chat-page app whose viewport matches the real layout for an 80x24
/// terminal: the history pane's width minus the reserved scrollbar column, and
/// its height from the shared chat layout.
fn sized_chat_app() -> App {
    let mut app = test_app();
    app.last_terminal_size = Some((TERM_W, TERM_H));
    let history_area = app.chat_page_layout(TERM_W, TERM_H)[0];
    app.history_viewport.width = history_area.width.saturating_sub(1);
    app.history_viewport.height = history_area.height;
    app
}

/// The same app with a history tall enough to keep the scrollbar visible.
fn app_with_scrollbar() -> App {
    let mut app = sized_chat_app();
    for id in 0..20 {
        insert_turn(&mut app, id, "user text", "assistant response");
    }
    app.rebuild_height_prefix();
    assert!(
        app.scrollbar_visible(),
        "the fixture must overflow the viewport"
    );
    app
}

#[test]
fn scrollbar_column_is_a_click_target() {
    let mut app = app_with_scrollbar();
    let column = app.history_viewport.width;
    assert_eq!(
        app.pointer_shape_at(column, 0),
        PointerShape::Pointer,
        "the draggable scrollbar column must show the hand"
    );
    // One column left is the history pane itself, which is selectable text.
    assert_eq!(app.pointer_shape_at(column - 1, 0), PointerShape::Text);
}

#[test]
fn hidden_scrollbar_column_promises_nothing() {
    // Short history: no scrollbar is drawn, so the reserved column is blank and
    // must not show the hand (which would promise a click that does nothing).
    let mut app = sized_chat_app();
    insert_turn(&mut app, 0, "short", "short");
    app.rebuild_height_prefix();
    assert!(!app.scrollbar_visible());
    let column = app.history_viewport.width;
    assert_eq!(app.pointer_shape_at(column, 0), PointerShape::Default);
}

#[test]
fn history_text_is_selectable() {
    let mut app = app_with_scrollbar();
    assert_eq!(app.pointer_shape_at(0, 0), PointerShape::Text);
}

#[test]
fn composer_box_is_editable_text() {
    let mut app = sized_chat_app();
    let box_rect = app.input_box_rect(TERM_W, TERM_H);
    // Inside the box's top border and left padding: the whole box is the input.
    assert_eq!(
        app.pointer_shape_at(box_rect.x + 1, box_rect.y + 1),
        PointerShape::Text
    );
    assert_eq!(
        app.pointer_shape_at(box_rect.right() - 1, box_rect.bottom() - 1),
        PointerShape::Text
    );
}

#[test]
fn turn_header_is_a_click_target() {
    let mut app = sized_chat_app();
    let turn = Turn {
        created_at: choreo_proto::TimestampMs::now(),
        undone: false,
        error: None,
        user_text: None,
        assistant_text: Some("world".into()),
        assistant_reasoning: Some("think".into()),
        tool_calls: vec![],
        token_usage: None,
        tool_results: vec![],
        displayed_images: vec![],
        reasoning_artifact: None,
        reasoning_producer: None,
    };
    app.active_display()
        .expect("active display")
        .view
        .insert_or_replace(1, turn);
    app.rebuild_height_prefix();

    let (start, end) = app
        .active_display_ref()
        .and_then(|display| display.turn_layouts.first())
        .and_then(|layout| layout.reasoning_header_range)
        .expect("the only turn has a reasoning header");
    // The only turn starts at content line 0; the short history is anchored to
    // the bottom of the viewport, so map the header's content line to its row.
    let header_row = content_to_screen_row(&app, start).expect("header is visible");
    assert_eq!(app.pointer_shape_at(0, header_row), PointerShape::Pointer);
    // The reasoning body below the header is text again, not a control.
    let body_row = content_to_screen_row(&app, end).expect("body line is visible");
    assert_eq!(app.pointer_shape_at(0, body_row), PointerShape::Text);
}

#[test]
fn empty_chrome_promises_nothing() {
    let mut app = sized_chat_app();
    // The status bar is the bottom row, below the composer and outside the
    // history pane: no hover target at all.
    assert_eq!(app.pointer_shape_at(0, TERM_H - 1), PointerShape::Default);
}

#[test]
fn other_pages_have_no_hover_targets() {
    let mut app = app_with_scrollbar();
    app.page = crate::state::Page::SessionManager;
    assert_eq!(
        app.pointer_shape_at(app.history_viewport.width, 0),
        PointerShape::Default,
        "the Chat page's hit-tests do not apply while another page is shown"
    );
}

#[test]
fn sync_pointer_emits_only_on_change() {
    let mut app = app_with_scrollbar();
    // Nothing emitted yet: the default arrow goes out first, even with no
    // pointer position known.
    assert_eq!(app.sync_pointer(), Some(PointerShape::Default));
    assert_eq!(
        app.sync_pointer(),
        None,
        "an unchanged shape re-emits nothing"
    );

    app.pointer_hover = Some((0, 0));
    assert_eq!(app.sync_pointer(), Some(PointerShape::Text));
    assert_eq!(app.sync_pointer(), None);

    // The hover target disappears (the history now fits the viewport): the
    // shape resets from the recomputed target, with no mouse motion at all.
    let mut app2 = sized_chat_app();
    app2.pointer_hover = Some((0, 0));
    app2.term_pointer = Some(PointerShape::Text);
    insert_turn(&mut app2, 0, "short", "short");
    app2.rebuild_height_prefix();
    assert!(!app2.scrollbar_visible());
    app2.pointer_hover = Some((app2.history_viewport.width, 0));
    assert_eq!(
        app2.sync_pointer(),
        Some(PointerShape::Default),
        "a vanished scrollbar must reset the pointer to the arrow"
    );

    app.forget_pointer();
    assert_eq!(app.term_pointer, None);
    assert_eq!(app.pointer_hover, None);
    assert_eq!(
        app.sync_pointer(),
        Some(PointerShape::Default),
        "a forgotten cache re-emits"
    );
}
