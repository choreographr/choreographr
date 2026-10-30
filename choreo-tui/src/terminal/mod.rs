//! Shared terminal OSC (Operating System Command) emission.
//!
//! Every terminal escape the TUI writes directly to stdout is framed here and
//! sent through [`write`]: one lock/write/flush of stdout with errors
//! swallowed, so an unsupported or denying terminal degrades to "nothing
//! happened" instead of disturbing the UI loop. The submodules build the
//! individual sequences — OSC 9;4 progress ([`progress`]), OSC 22 pointer shape
//! ([`pointer`]), OSC 52 clipboard ([`clipboard`]), OSC 2 window title
//! ([`title`]), and OSC 7501 program status ([`status`]).
//!
//! This is a leaf module: it carries no view state and never depends on
//! `App`. Lifecycle orchestration that needs `App` — publishing the status
//! records, deduping the window title and the pointer shape, clearing
//! everything on suspend/exit — lives in `connection`.
//!
//! **The strip policy is the grid's policy.** Both free-text OSC fields (the
//! OSC 2 title and the OSC 7501 `title`/`msg`) drop exactly the characters the
//! terminal grid filter escapes: [`is_control_char`] for the byte classes, plus
//! `choreo_sanitize::is_unsafe_unicode` — the shared, data-table-derived
//! predicate the grid reaches through [`crate::text_filter`] — for the spoofing
//! class and the line/paragraph separators. Classification is therefore
//! identical between the two consumers, and only the *action* differs: the grid
//! **escapes** (the text stays readable and keeps measuring exactly what it
//! draws), while an OSC payload **drops** (a payload body cannot carry an escape
//! sequence at all, and OSC 7501 forbids control bytes — a decoded one makes a
//! terminal discard the whole report).

use std::io::Write;

use choreo_sanitize::is_unsafe_unicode;

pub(crate) mod clipboard;
pub(crate) mod pointer;
pub(crate) mod progress;
pub(crate) mod status;
pub(crate) mod title;

/// Frame an OSC escape sequence: `ESC ] code ; body ST`.
///
/// `ST` is the string terminator `ESC \` (0x1B 0x5C). `code` selects the
/// sequence family (2, 9;4, 22, 52, 7501) and `body` is everything between the
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
    let _ = handle.write_all(seq.as_bytes());
    let _ = handle.flush();
}

/// Whether `c` is a C0 (`U+0000..=U+001F`), DEL (`U+007F`), or C1
/// (`U+0080..=U+009F`) control character.
///
/// These are the bytes the OSC 7501 protocol forbids in the decoded `title` /
/// `msg` text (a decoded control byte makes a terminal discard the whole
/// report) and the bytes a raw `ESC`/`ST` in a title could use to inject or
/// terminate a sequence, so every OSC family strips them before framing.
///
/// The name states the protocol rule this serves, but the classification is
/// std's `char::is_control` — the Unicode `Cc` general category, which is exactly
/// the C0/DEL/C1 set above. So it is by construction the same predicate the grid
/// filter applies (`text_filter::terminal_keeps`), rather than a second range
/// list that could drift from it. The rest of the strip set comes from
/// [`is_unsafe_display_char`].
pub(crate) fn is_control_char(c: char) -> bool {
    c.is_control()
}

/// Whether `c` must be stripped from free text shown outside the terminal grid
/// (an OSC 2 window title, an OSC 7501 `title`/`msg` value).
///
/// Delegates to the shared [`is_unsafe_unicode`] policy: every Unicode *format*
/// character other than the joiners U+200C/U+200D — bidi marks, overrides and
/// isolates, the zero-width space and word joiner, the soft hyphen, the BOM,
/// invisible operators, and any format char the Unicode tables gain — plus the
/// line/paragraph separators U+2028/U+2029 (which are not control chars, yet
/// would break the one-line status record). These reorder, hide, or fake
/// displayed text, so a hostile session title could visually spoof a window title
/// or a status record; the OSC 7501 spec's Security section calls for disarming
/// them before free text is shown outside the grid.
///
/// Using the shared predicate — rather than a hand-kept range list — is what
/// keeps this in step with the grid filter, which escapes the same characters:
/// the two consumers classify identically and differ only in escaping versus
/// dropping (see the module docs). The joiners stay because they affect ligation
/// rather than ordering or visibility and are required by scripts such as
/// Persian (ZWNJ) and Devanagari.
pub(crate) fn is_unsafe_display_char(c: char) -> bool {
    is_unsafe_unicode(c)
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

    #[test]
    fn is_unsafe_display_char_covers_the_spoofing_class_and_separators() {
        // Bidi marks, overrides and isolates are stripped.
        assert!(is_unsafe_display_char('\u{061c}')); // Arabic letter mark
        assert!(is_unsafe_display_char('\u{200e}')); // LRM
        assert!(is_unsafe_display_char('\u{200f}')); // RLM
        assert!(is_unsafe_display_char('\u{202e}')); // RLO
        assert!(is_unsafe_display_char('\u{2066}')); // LRI
        assert!(is_unsafe_display_char('\u{2069}')); // PDI
    }

    #[test]
    fn is_unsafe_display_char_strips_invisible_format_chars() {
        // The invisible-but-harmless-looking format class: the grid escapes
        // these, so the OSC fields must drop them too.
        assert!(is_unsafe_display_char('\u{200b}')); // zero-width space
        assert!(is_unsafe_display_char('\u{2060}')); // word joiner
        assert!(is_unsafe_display_char('\u{00ad}')); // soft hyphen
        assert!(is_unsafe_display_char('\u{feff}')); // BOM / ZWNBSP
    }

    #[test]
    fn is_unsafe_display_char_strips_line_and_paragraph_separators() {
        // U+2028/U+2029 are not control chars, yet a terminal renders them as
        // line breaks — a one-line status record must not carry either.
        assert!(is_unsafe_display_char('\u{2028}'));
        assert!(is_unsafe_display_char('\u{2029}'));
    }

    #[test]
    fn is_unsafe_display_char_keeps_joiners_and_ordinary_text() {
        // Zero-width joiners are legitimate (emoji ZWJ sequences, Persian
        // ZWNJ, Devanagari conjuncts).
        assert!(!is_unsafe_display_char('\u{200c}'));
        assert!(!is_unsafe_display_char('\u{200d}'));
        assert!(!is_unsafe_display_char('a'));
        assert!(!is_unsafe_display_char('日'));
        assert!(!is_unsafe_display_char('\u{2014}')); // em dash
    }
}
