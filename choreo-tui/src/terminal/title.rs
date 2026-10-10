//! OSC 2 window/tab title.
//!
//! The TUI names the terminal window/tab after itself and, once a session is
//! attached, after that session: `choreo-tui — <session title>`. The title is
//! set with the OSC 2 sequence (`ESC ] 2 ; <text> ST`) and cleared (set to
//! empty) on suspend and exit so the TUI does not leave a stale name behind.
//!
//! Session titles are daemon/LLM-derived, so [`sanitize`] strips every control
//! byte (C0, DEL, C1) and every bidirectional-formatting character before the
//! title reaches the terminal — a raw `ESC`/`ST` byte would terminate or corrupt
//! the OSC sequence, and a bidi override would let a hostile title visually
//! reorder or spoof the window title — and caps the length so a pathologically
//! long title cannot emit an unbounded escape.

use super::{is_bidi_control, is_control_char, osc, write};

/// The program name shown in the window title when no session is attached.
const APP_NAME: &str = "choreo-tui";

/// Upper bound on the sanitized OSC 2 title length (in characters). OSC 2 is
/// free-form with no protocol limit and a session title can be long; capping
/// keeps the escape small and bounds what a daemon/LLM-supplied title can
/// emit. (The OSC 7501 record's `title` has its own, tighter protocol byte
/// cap — see `terminal::status` — so the two deliberately do not share a
/// constant.)
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

/// Strip control bytes and bidi-formatting controls, then cap the length of an
/// OSC 2 title.
///
/// Removes C0 (`U+0000..=U+001F`), DEL (`U+007F`), C1 (`U+0080..=U+009F`), and
/// bidirectional-formatting characters, then caps at [`MAX_TITLE_CHARS`].
pub(crate) fn sanitize(text: &str) -> String {
    text.chars()
        .filter(|c| !is_control_char(*c) && !is_bidi_control(*c))
        .take(MAX_TITLE_CHARS)
        .collect()
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
    fn sanitize_strips_bidi_overrides() {
        // An RLO (U+202E) and an isolate (U+2066) are removed; the text survives.
        assert_eq!(sanitize("a\u{202e}b\u{2066}c"), "abc");
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
