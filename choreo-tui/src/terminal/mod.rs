//! Shared terminal OSC (Operating System Command) emission.
//!
//! Every terminal escape the TUI writes directly to stdout is framed here and
//! sent through [`write`]: one lock/write/flush of stdout with errors
//! swallowed, so an unsupported or denying terminal degrades to "nothing
//! happened" instead of disturbing the UI loop. The submodules build the
//! individual sequences — OSC 9;4 progress ([`progress`]), OSC 52 clipboard
//! ([`clipboard`]), OSC 2 window title ([`title`]), and OSC 7501 program
//! status ([`status`]).
//!
//! This is a leaf module: it carries no view state and never depends on
//! `App`. Lifecycle orchestration that needs `App` — publishing the status
//! records, deduping the window title, clearing everything on suspend/exit —
//! lives in `connection`.

use std::io::Write;

pub(crate) mod clipboard;
pub(crate) mod progress;
pub(crate) mod status;
pub(crate) mod title;

/// Frame an OSC escape sequence: `ESC ] code ; body ST`.
///
/// `ST` is the string terminator `ESC \` (0x1B 0x5C). `code` selects the
/// sequence family (2, 9;4, 52, 7501) and `body` is everything between the
/// code and the terminator.
pub(crate) fn osc(code: u16, body: &str) -> String {
    format!("\x1b]{code};{body}\x1b\\")
}

/// Write a prebuilt escape sequence to stdout.
///
/// This is the single place the TUI emits bytes to the terminal outside
/// ratatui's frame buffer, so every OSC family routes through it. Errors are
/// swallowed on purpose: an unsupported or denying terminal must never
/// surface as a UI error.
pub(crate) fn write(seq: &str) {
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    let _ = write!(handle, "{seq}");
    let _ = handle.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn osc_frames_code_and_body_with_st() {
        assert_eq!(osc(2, "hello"), "\x1b]2;hello\x1b\\");
        assert_eq!(osc(7501, "state=clear"), "\x1b]7501;state=clear\x1b\\");
        // An empty body still yields a well-formed sequence.
        assert_eq!(osc(2, ""), "\x1b]2;\x1b\\");
    }
}
