//! OSC 2 window/tab title.
//!
//! The TUI names the terminal window/tab after itself and, once a session is
//! attached, after that session: `choreo-tui — <session title>`. The title is
//! set with the OSC 2 sequence (`ESC ] 2 ; <text> ST`) and cleared (set to
//! empty) on suspend and exit so the TUI does not leave a stale name behind.
//!
//! Session titles are daemon/LLM-derived, so [`sanitize`] strips every control
//! byte (C0, DEL, C1) before the title reaches the terminal — a raw `ESC`/`ST`
//! byte in a title would otherwise terminate or corrupt the OSC sequence — and
//! caps the length so a pathologically long title cannot emit an unbounded
//! escape.

use super::{osc, write};

/// The program name shown in the window title when no session is attached.
const APP_NAME: &str = "choreo-tui";

/// Upper bound on the sanitized title length (in characters). The OSC 2 title
/// is free-form and a session title can be long; capping keeps the escape
/// small and bounds what a daemon/LLM-supplied title can emit.
const MAX_TITLE_CHARS: usize = 200;

/// Build the OSC 2 set-title sequence for `text` (sanitized and capped).
pub(crate) fn build(text: &str) -> String {
    osc(2, &sanitize(text))
}

/// Emit the OSC 2 title for `text`.
pub(crate) fn set(text: &str) {
    write(&build(text));
}

/// Emit the OSC 2 clear sequence (empty title), restoring the terminal's
/// default title handling.
pub(crate) fn clear() {
    write(&build(""));
}

/// Strip control bytes and cap the length of a title.
///
/// Removes C0 (`U+0000..=U+001F`), DEL (`U+007F`), and C1 (`U+0080..=U+009F`)
/// control characters, then caps at [`MAX_TITLE_CHARS`].
pub(crate) fn sanitize(text: &str) -> String {
    text.chars()
        .filter(|c| !is_control(*c))
        .take(MAX_TITLE_CHARS)
        .collect()
}

/// Whether `c` is a C0, DEL, or C1 control character.
fn is_control(c: char) -> bool {
    matches!(u32::from(c), 0x00..=0x1F | 0x7F | 0x80..=0x9F)
}

/// The window title text: the plain program name when no session title is
/// known, else `choreo-tui — <session title>`.
pub(crate) fn window_title(session_title: Option<&str>) -> String {
    match session_title {
        Some(title) if !title.is_empty() => format!("{APP_NAME} — {}", sanitize(title)),
        _ => APP_NAME.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_sets_osc2_title() {
        assert_eq!(build("hello"), "\x1b]2;hello\x1b\\");
    }

    #[test]
    fn build_with_empty_text_is_the_clear_form() {
        assert_eq!(build(""), "\x1b]2;\x1b\\");
    }

    #[test]
    fn sanitize_strips_c0_del_and_c1() {
        // ESC (0x1B), BEL (0x07), DEL (0x7F), and a C1 byte (0x9D) are all
        // removed; printable text (including non-ASCII) survives.
        let input = "a\u{1b}b\u{07}c\u{7f}d\u{9d}e\nf";
        assert_eq!(sanitize(input), "abcdef");
    }

    #[test]
    fn sanitize_caps_length() {
        let long = "x".repeat(MAX_TITLE_CHARS + 50);
        assert_eq!(sanitize(&long).chars().count(), MAX_TITLE_CHARS);
    }

    #[test]
    fn window_title_with_and_without_a_session_title() {
        assert_eq!(window_title(None), "choreo-tui");
        assert_eq!(window_title(Some("")), "choreo-tui");
        assert_eq!(
            window_title(Some("Fix the parser")),
            "choreo-tui — Fix the parser"
        );
    }

    #[test]
    fn window_title_sanitizes_the_session_title() {
        // A control byte in the session title never reaches the sequence.
        assert_eq!(
            window_title(Some("bad\u{1b}]2;evil")),
            "choreo-tui — bad]2;evil"
        );
    }
}
