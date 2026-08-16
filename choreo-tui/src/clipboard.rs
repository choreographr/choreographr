//! OSC 52 clipboard writes.
//!
//! The TUI copies selected text to the system clipboard through the terminal
//! emulator itself, using the OSC 52 escape sequence (`ESC ] 52 ; <sel> ;
//! <base64> ST`).  The terminal mediates the write, so:
//!
//! - It works over SSH and tmux — the clipboard written is the *local* one,
//!   exactly like terminal-native selection (a system-clipboard crate would
//!   write to the *remote* host's clipboard instead).
//! - It is a no-op on terminals that do not support OSC 52 (e.g. macOS
//!   Terminal.app): the sequence is silently ignored, never an error.
//! - The terminal can refuse the write (kitty/iTerm2 permission prompts)
//!   without affecting the TUI at all.
//! - Selections larger than [`MAX_OSC52_BYTES`] are refused up front (the
//!   caller surfaces a "too large" status): OSC 52 payloads are base64 (a
//!   4/3 expansion), and several terminals cap or drop very large paste
//!   sequences, so writing a multi-megabyte sequence would stall the UI loop
//!   for a paste the terminal would likely discard anyway.
//!
//! This mirrors the existing `terminal_progress` module's OSC 9;4 usage:
//! build the sequence as a string, write it to stdout, ignore errors.  A
//! write is only ever triggered by a user-initiated mouse-up over their own
//! selection, so a hostile LLM/tool output can never inject a clipboard
//! write through this path.

use base64::Engine as _;
use std::io::Write;

/// Maximum size (in bytes of text) of a selection written via OSC 52.
const MAX_OSC52_BYTES: usize = 1 << 20; // 1 MiB of text

/// Whether `text` fits under the OSC 52 size cap.  Split out so the
/// threshold is unit-testable without writing to a real stdout.
fn within_osc52_limit(text: &str) -> bool {
    text.len() <= MAX_OSC52_BYTES
}

/// Build the OSC 52 sequence that asks the terminal to place `text` on the
/// system clipboard (`c` selection).  Pure function so the byte layout is
/// unit-testable without a terminal.
pub(crate) fn build_osc52(text: &str) -> String {
    let encoded = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    format!("\x1b]52;c;{encoded}\x1b\\")
}

/// Copy `text` to the system clipboard via OSC 52.
///
/// Returns `true` when the sequence was written; `false` (without writing)
/// when the text exceeds [`MAX_OSC52_BYTES`], so the caller can report a
/// "too large to copy" status instead of pretending the copy succeeded.
/// Writes to stdout exactly like `terminal_progress::update_terminal_progress`.
/// Errors are swallowed on purpose: an unsupported or denying terminal must
/// degrade to "nothing happened", never disturb the UI loop.
pub(crate) fn copy_to_clipboard(text: &str) -> bool {
    if !within_osc52_limit(text) {
        return false;
    }
    let seq = build_osc52(text);
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    let _ = write!(handle, "{seq}");
    let _ = handle.flush();
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_osc52_empty_text() {
        // An empty selection is never copied (the caller gates on non-empty),
        // but the encoder must still produce a well-formed sequence.
        assert_eq!(build_osc52(""), "\x1b]52;c;\x1b\\");
    }

    #[test]
    fn build_osc52_base64_payload() {
        // "hello" base64-encodes to "aGVsbG8="; the sequence wraps it in the
        // OSC 52 clipboard write form with ST (ESC \) terminators.
        assert_eq!(build_osc52("hello"), "\x1b]52;c;aGVsbG8=\x1b\\");
    }

    #[test]
    fn build_osc52_multiline_text() {
        // Embedded newlines pass through the base64 encoding untouched.
        assert_eq!(
            build_osc52("line one\nline two"),
            "\x1b]52;c;bGluZSBvbmUKbGluZSB0d28=\x1b\\"
        );
    }

    #[test]
    fn build_osc52_utf8_round_trips() {
        // Non-ASCII (e.g. code snippets with emoji) survives the encode.
        let seq = build_osc52("→ 😀");
        assert!(seq.starts_with("\x1b]52;c;"));
        assert!(seq.ends_with("\x1b\\"));
        // Prefix is ESC ] 5 2 ; c ; = 7 bytes; suffix ST = 2 bytes.
        let b64 = &seq[7..seq.len() - 2];
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .expect("payload must be valid base64");
        assert_eq!(String::from_utf8(decoded).unwrap(), "→ 😀");
    }

    #[test]
    fn osc52_size_cap_boundary() {
        // Text at the cap is accepted; one byte over is refused (the caller
        // shows a "too large" status instead of stalling the UI on a
        // multi-megabyte escape sequence terminals may drop anyway).
        assert!(within_osc52_limit(""));
        assert!(within_osc52_limit("x".repeat(MAX_OSC52_BYTES).as_str()));
        assert!(!within_osc52_limit(
            "x".repeat(MAX_OSC52_BYTES + 1).as_str()
        ));
    }
}
