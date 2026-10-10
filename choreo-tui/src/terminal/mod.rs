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

/// Whether `c` is a C0 (`U+0000..=U+001F`), DEL (`U+007F`), or C1
/// (`U+0080..=U+009F`) control character.
///
/// These are the bytes the OSC 7501 protocol forbids in the decoded `title` /
/// `msg` text (a decoded control byte makes a terminal discard the whole
/// report) and the bytes a raw `ESC`/`ST` in a title could use to inject or
/// terminate a sequence, so every OSC family strips them before framing.
pub(crate) fn is_control_char(c: char) -> bool {
    matches!(u32::from(c), 0x00..=0x1F | 0x7F | 0x80..=0x9F)
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

    #[test]
    fn is_control_char_covers_c0_del_c1() {
        assert!(is_control_char('\u{1b}')); // ESC (C0)
        assert!(is_control_char('\u{07}')); // BEL (C0)
        assert!(is_control_char('\u{7f}')); // DEL
        assert!(is_control_char('\u{9d}')); // OSC (C1)
        assert!(!is_control_char('a'));
        assert!(!is_control_char('\u{2014}')); // em dash
    }
}
