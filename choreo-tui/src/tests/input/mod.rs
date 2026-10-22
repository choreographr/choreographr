//! `InputBuffer` and input-bar tests, one child module per subject — the
//! boundaries follow the banner sections the monolithic file was already
//! divided into, so a few keyboard tests sit beside the click tests they were
//! written with rather than beside their closest subject.
//!
//! `editing` covers cursor movement, the editing primitives (insert/delete/
//! word/line kills) and multi-line plus wrapped-line navigation; `click_offset`
//! holds the click-to-byte-offset mapping and the single-key Home/End/Enter
//! handling it was documented with; `chords` pins the readline/emacs key
//! chords, visual-line computation and word-boundary movement; `history` covers
//! Up/Down recall of submitted prompts and the history pane's click targets;
//! `mouse_click` places the cursor from a click in the input box; `scroll`
//! covers `scroll_offset`/`ensure_cursor_visible` and the input box geometry;
//! `kitty` pins the kitty-protocol SHIFT normalisation end to end; `ime` pins
//! the defences that keep mangled IME text events out of the buffer.

mod chords;
mod click_offset;
mod editing;
mod history;
mod ime;
mod kitty;
mod mouse_click;
mod scroll;
