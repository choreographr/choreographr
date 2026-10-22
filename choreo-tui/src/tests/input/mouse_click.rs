use crate::connection::handle_terminal_event;
use crate::state::*;
use crate::tests::support::test_app;
use choreo_proto::ClientMessage;
use crossterm::event::{Event, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

// ── Mouse click positions the input cursor ───────────────────

/// Send a left-click at terminal (column, row) through the event pipeline.
fn click_input(
    app: &mut App,
    tx: &crossbeam_channel::Sender<ClientMessage>,
    column: u16,
    row: u16,
) {
    handle_terminal_event(
        Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }),
        app,
        tx,
    )
    .expect("handle mouse click");
}

#[test]
fn mouse_click_in_input_box_positions_cursor() {
    let (tx, _rx) = crossbeam_channel::unbounded();
    let mut app = test_app();
    app.last_terminal_size = Some((80, 24));
    app.input.text = "hello".to_string();
    app.input.cursor = 0;
    app.update_viewport_from_terminal_size();

    // 80x24, no status/error, help shown: the input box occupies rows 20..23
    // with content on row 21, and content starts at column INPUT_PAD (2).
    // Column 4 is therefore content column 2 → cursor between 'h' and 'e'.
    click_input(&mut app, &tx, 4, 21);
    assert_eq!(app.input.cursor, 2);
}

#[test]
fn mouse_click_in_input_box_left_padding_clamps_to_start() {
    let (tx, _rx) = crossbeam_channel::unbounded();
    let mut app = test_app();
    app.last_terminal_size = Some((80, 24));
    app.input.text = "hello".to_string();
    app.input.cursor = 4;
    app.update_viewport_from_terminal_size();

    // Column 1 is inside the left padding (before INPUT_PAD=2).
    click_input(&mut app, &tx, 1, 21);
    assert_eq!(app.input.cursor, 0);
}

#[test]
fn mouse_click_in_input_box_past_line_end_clamps_to_end() {
    let (tx, _rx) = crossbeam_channel::unbounded();
    let mut app = test_app();
    app.last_terminal_size = Some((80, 24));
    app.input.text = "hello".to_string();
    app.input.cursor = 0;
    app.update_viewport_from_terminal_size();

    // Column 79 is well past the text's right edge → cursor at end.
    click_input(&mut app, &tx, 79, 21);
    assert_eq!(app.input.cursor, 5);
}

#[test]
fn mouse_click_on_input_box_border_does_not_move_cursor() {
    let (tx, _rx) = crossbeam_channel::unbounded();
    let mut app = test_app();
    app.last_terminal_size = Some((80, 24));
    app.input.text = "hello".to_string();
    app.input.cursor = 1;
    app.update_viewport_from_terminal_size();

    // Row 20 is the top border, row 22 the bottom border — neither should
    // reposition the cursor.
    click_input(&mut app, &tx, 4, 20);
    assert_eq!(app.input.cursor, 1);
    click_input(&mut app, &tx, 4, 22);
    assert_eq!(app.input.cursor, 1);
}

#[test]
fn mouse_click_in_input_box_second_line_of_multiline_text() {
    let (tx, _rx) = crossbeam_channel::unbounded();
    let mut app = test_app();
    app.last_terminal_size = Some((80, 24));
    app.input.text = "abc\ndef".to_string();
    app.input.cursor = 0;
    app.update_viewport_from_terminal_size();

    // Two content lines → box occupies rows 19..22 (content rows 20, 21).
    click_input(&mut app, &tx, 3, 20); // content row 0, col 1 → 'b' in "abc"
    assert_eq!(app.input.cursor, 1);
    click_input(&mut app, &tx, 3, 21); // content row 1, col 1 → 'e' in "def"
    assert_eq!(app.input.cursor, 5);
}

#[test]
fn mouse_click_in_input_box_wrapped_line() {
    let (tx, _rx) = crossbeam_channel::unbounded();
    let mut app = test_app();
    app.last_terminal_size = Some((80, 24));
    // Two words totalling 101 display columns: at inner width 76 this wraps
    // as line 0 = 76 'a's (bytes 0..76), line 1 = 24 'b's (bytes 77..101).
    app.input.text = "a".repeat(76) + " " + &"b".repeat(24);
    app.input.cursor = 0;
    app.update_viewport_from_terminal_size();

    // Two content lines → box occupies rows 19..22 (content rows 20, 21).
    click_input(&mut app, &tx, 30, 20); // visual line 0, content col 28
    assert_eq!(app.input.cursor, 28);
    click_input(&mut app, &tx, 10, 21); // visual line 1, content col 8
    assert_eq!(app.input.cursor, 77 + 8);
}
