//! `InputBuffer` key-handling tests: NUL rejection and Ctrl+Backspace clearing.

use crate::state::InputBuffer;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[test]
fn input_buffer_drops_nul_and_keeps_newline() {
    // crossterm 0.29 parses kitty-protocol IME "text events"
    // (`CSI 0;;<codepoints>u`) as Char('\0') with the composed text
    // dropped.  The guard in handle_key must refuse to insert the NUL
    // while still accepting a literal newline (legacy Ctrl+J = 0x0A).
    let mut buf = InputBuffer::new();
    assert!(!buf.handle_key(KeyEvent::new(KeyCode::Char('\0'), KeyModifiers::NONE)));
    assert!(buf.handle_key(KeyEvent::new(KeyCode::Char('\n'), KeyModifiers::NONE)));
    assert_eq!(buf.text, "\n");
}

#[test]
fn input_buffer_ctrl_backspace_clears_whole_buffer_from_any_cursor() {
    // Ctrl+Backspace empties the draft prompt outright, independent of
    // the cursor position — unlike Ctrl+U, which keeps the tail after
    // the cursor, and unlike plain Backspace, which deletes one grapheme.
    let mut buf = InputBuffer::new();
    buf.text = "hello world".to_string();
    // Cursor parked mid-text: clearing must not leave the trailing "world".
    buf.cursor = 6;
    buf.generation = 7;
    buf.scroll_offset = 3;

    let consumed = buf.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::CONTROL));

    assert!(consumed, "ctrl+backspace must be consumed");
    assert!(buf.text.is_empty(), "the whole draft must be cleared");
    assert_eq!(buf.cursor, 0);
    assert_eq!(buf.scroll_offset, 0, "clear resets the visible window");
    assert_ne!(buf.generation, 7, "clear must invalidate the lines cache");
}
