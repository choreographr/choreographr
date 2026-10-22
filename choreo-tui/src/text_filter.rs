//! The terminal-safety filter: one fused pass that makes untrusted text safe to
//! measure and draw in a terminal cell grid.
//!
//! Three sinks share it. Two of them *escape*, and they differ only in how a
//! complete SGR colour sequence is treated. [`normalize_ansi_text`] is the
//! *ANSI* sink: it keeps `ESC [ params m` verbatim, because the caller hands the
//! text to an ANSI parser that turns it into styling.
//! [`normalize_display_text`] is the plain sink: nothing downstream parses
//! escape sequences, so it escapes them like any other control character. The
//! third, [`sanitize_pasted_text`], is the *editing* ingest rule: text bound for
//! the input buffer is **rewritten** instead — control characters dropped, tabs
//! expanded, a CR folded to `\n` — with nothing escaped, so the bytes stored are
//! the bytes drawn (see below).
//!
//! Both escape-normalizers escape every other control character — including a
//! lone CR, which would let hostile content overwrite its own rendered line (a
//! CRLF pair is folded to a single `\n` first) — the line/paragraph separators
//! U+2028/U+2029, and every
//! Unicode *format* character except the joiners U+200C/U+200D. That last set is
//! the shared [`is_unsafe_unicode`] policy owned by `choreo-sanitize`, so this
//! sink classifies exactly the characters the daemon's sanitizers and the OSC
//! field strippers do. Tabs pass the per-char predicate and are expanded to
//! spaces in the same pass.
//!
//! The invariant the whole pass protects: the text this crate *measures* has to
//! be exactly the text ratatui *draws*. `unicode-width` counts a control
//! character as one column — a `\t`, or any other C0 char inside a string —
//! while ratatui filters it out of the drawn grapheme stream, so a row carrying
//! one measures wider than it renders: a code box's right `│` jogs left on every
//! tab-indented line, and its indentation vanishes. Escaping the control
//! characters and expanding the tabs leaves no character whose measured and
//! drawn widths disagree.
//!
//! Escaping happens *before* the ANSI sink's `contains("\x1b[")` gate, so only
//! genuine (complete) SGR sequences ever reach the parser.
//!
//! **Why the editing rule drops instead of escaping.** The two display sinks own
//! only the drawing, so they may rewrite text on its way to a cell grid. The
//! input buffer is the opposite case: its bytes are exactly what the daemon
//! receives, and the cursor is a byte offset into them, so the stored text has
//! to *be* the drawn text — an injected `\u{1b}` would be characters the user
//! never typed, and a cursor could not be trusted to land correctly among them.
//! Dropping what the terminal cannot draw and expanding tabs gives that identity
//! for free. This closes the last surface outside the measure==draw invariant:
//! once a paste is sanitized, the editable buffer can contain no character whose
//! measured and drawn widths disagree, so the editing exemption costs no
//! measurement honesty.
//!
//! Column rule, in full (the wrapping/height math depends on it):
//!
//! * a `\n` resets the column to 0, so each logical line counts from its own
//!   start (the tab stops a terminal would use);
//! * a kept SGR sequence advances it by 0, because the sequence draws no cell;
//!   counting its bytes would under-pad a tab that follows a colour code;
//! * an escaped char advances it by the display width of the *escape text* it
//!   produced (`\u{1b}` is six columns of literal text), because that text is
//!   what is drawn;
//! * every other kept char advances it by its own display width.
//!
//! One bounded divergence is accepted as the cost of a char-wise pass: the column
//! is counted with `UnicodeWidthChar::width`, while ratatui and the wrapping math
//! use `UnicodeWidthStr::width`, and the two disagree for an emoji-presentation
//! sequence (`❤️` = U+2764 U+FE0F measures 1 char-wise but 2 as a string), so a
//! tab following one can pad a single column short.

use std::borrow::Cow;

use choreo_sanitize::is_unsafe_unicode;

/// How [`normalize_impl`] treats a complete SGR colour sequence.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SgrPolicy {
    /// Keep `ESC [ params m` verbatim, because the caller will hand the text to
    /// an ANSI parser that turns it into styling.
    Keep,
    /// Escape it like any other control character, because nothing downstream
    /// parses escape sequences — the ESC byte would render as literal text
    /// anyway, and its column would be measured but never drawn.
    Escape,
}

/// Normalize ANSI-escape-coded tool-result content: escape every control
/// character, keep complete SGR colour sequences (`ESC [ params m`) verbatim so
/// ANSI coloring still works, and expand tabs — all in one scan with one output
/// allocation.
///
/// This is the *sink* defense: it protects the terminal from every tool at
/// once, including the streaming shell/VM tools whose raw output the daemon
/// deliberately does not escape (colors are a feature). The daemon separately
/// escapes the same char classes at the source for the line-oriented tools
/// and for the LLM transcript; this filter is what makes raw content safe to
/// draw. Escaping happens *before* the caller's `contains("\x1b[")` gate so
/// that only genuine SGR sequences ever reach the ANSI parser.
pub(crate) fn normalize_ansi_text(text: &str) -> String {
    normalize_impl(text, SgrPolicy::Keep)
}

/// Normalize untrusted display text for the renderers — the same fused pass as
/// [`normalize_ansi_text`], but with escape sequences escaped instead of kept,
/// because no ANSI parser will ever see this content.
///
/// Markdown source is exactly such content — the block renderer turns it into
/// styled `Line`s itself, so a surviving `ESC [ 3 1 m` would draw as the
/// literal text `[31m` while still measuring as four columns, desyncing the row
/// it sits in from everything measured around it.  Escaping it renders the
/// sequence inert (`\u{1b}[31m`) *and* keeps measurement honest.
///
/// The whole pass exists for one reason — the text this crate *measures* has to
/// be exactly the text ratatui *draws* (see the module docs for the invariant
/// and the resulting failure).  Escaping the control characters and expanding
/// the tabs leaves no character whose measured and drawn widths disagree.
///
/// Runs *before* markdown is parsed, so every block renderer (paragraphs,
/// code boxes, tables, diff fences) sees the same normalized source.  Content
/// that is already clean borrows: the common case costs one scan and no
/// allocation.
pub(crate) fn normalize_display_text(text: &str) -> Cow<'_, str> {
    if !text.contains('\t') && text.chars().all(terminal_keeps) {
        return Cow::Borrowed(text);
    }
    Cow::Owned(normalize_impl(text, SgrPolicy::Escape))
}

/// The body shared by [`normalize_ansi_text`] and [`normalize_display_text`]:
/// one scan that escapes what [`terminal_keeps`] rejects *and* expands tabs, so
/// the slow path allocates once instead of twice.
///
/// Fusing the two steps is safe precisely because tab expansion's only carried
/// state is the print column of the current logical line: the escaping decision
/// for a char is local (plus the CSI lookahead), so a column can be advanced in
/// the same step that emits the char's final on-screen text.  The full column
/// rule is in the module docs.
///
/// Iterates the input with a `Peekable<Chars>` (no intermediate `Vec<char>`),
/// so the per-chunk cost during streaming stays O(chunk) with one output
/// allocation plus one reused ESC-sequence buffer — it is called inside the
/// incremental renderer for the in-flight turn on every streamed chunk.
fn normalize_impl(text: &str, sgr_policy: SgrPolicy) -> String {
    let mut out = String::with_capacity(text.len());
    // Print column within the current logical line — the only state the pass
    // carries, which is why a normalizer resumed at a line start is complete
    // (the incremental streaming cache relies on exactly that).
    let mut col = 0usize;
    let mut chars = text.chars().peekable();
    // Reused ESC-sequence assembly buffer: one heap allocation for all the
    // color sequences in a chunk, not one per sequence (ANSI-heavy shell
    // output can contain hundreds).
    let mut seq = String::new();
    while let Some(c) = chars.next() {
        // Fold CRLF to a single LF: a carriage return followed by a line feed
        // is a normal line ending, but passing the `\r` through would put a
        // control char in the rendered cell stream (crossterm prints it to the
        // terminal). Folding — the same normalization the daemon's line
        // sanitizers apply — keeps the pair off the wire entirely. A *lone*
        // CR (the overwrite vector) is escaped below.
        if c == '\r' && chars.peek() == Some(&'\n') {
            out.push('\n');
            chars.next(); // consume the '\n'
            col = 0;
            continue;
        }
        // Keep a complete SGR sequence (ESC [ params m) verbatim so ANSI
        // coloring survives; every other use of ESC is escaped. A non-SGR
        // CSI (`\x1b[2J`) or OSC (`\x1b]…`) therefore renders as the inert
        // `\u{1b}` followed by literal text instead of reaching the
        // terminal as a live control sequence.
        if c == '\u{1b}' && sgr_policy == SgrPolicy::Keep && chars.peek() == Some(&'[') {
            chars.next(); // consume '['
            seq.clear();
            seq.push('\u{1b}');
            seq.push('[');
            let mut sgr = false;
            loop {
                match chars.peek().copied() {
                    // Intermediate bytes are 0x30-0x3F (digits, ';', ':', …).
                    Some(n) if (0x30..=0x3f).contains(&(n as u32)) => {
                        seq.push(n);
                        chars.next();
                    }
                    // Final byte 0x40-0x7E; only 'm' is SGR.
                    Some(n) if (0x40..=0x7e).contains(&(n as u32)) => {
                        seq.push(n);
                        chars.next();
                        sgr = n == 'm';
                        break;
                    }
                    // EOF or a non-CSI byte — not a CSI sequence at all.
                    _ => break,
                }
            }
            if sgr {
                // Valid SGR — keep the whole sequence verbatim.  It draws no
                // cell, so the column stays where it was.
                out.push_str(&seq);
            } else {
                // Non-SGR ESC use: render the ESC inert and the consumed
                // `[` + intermediate bytes as plain text (`seq` is
                // ESC + '[' + … , so everything after the leading ESC byte
                // is kept; `.get(1..)` is total because `seq` always starts
                // with the one-byte ESC).  All of it is drawn, so all of it
                // advances the column: the six chars of `\u{1b}` plus the
                // tail.
                out.push_str("\\u{1b}");
                let tail = seq.get(1..).unwrap_or("");
                out.push_str(tail);
                col += 6 + tail.len();
            }
            continue;
        }
        match c {
            // Expand to the next tab stop of the current line before emitting,
            // so the spaces count as the columns they occupy.
            '\t' => {
                let pad = TAB_STOP - (col % TAB_STOP);
                out.extend(std::iter::repeat_n(' ', pad));
                col += pad;
            }
            '\n' => {
                out.push('\n');
                col = 0;
            }
            _ if terminal_keeps(c) => {
                out.push(c);
                col += unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
            }
            _ => {
                // `escape_default` emits ASCII only, so the byte count it adds
                // is exactly the number of columns it draws.
                let before = out.len();
                out.extend(c.escape_default());
                col += out.len() - before;
            }
        }
    }
    out
}

/// The *editing* ingest rule: rewrite pasted text so it can live in an input
/// buffer, which stores the bytes the daemon will receive and addresses the
/// cursor by byte offset.
///
/// Rewrites rather than escapes, which is what distinguishes it from the two
/// display sinks: an escaped control character would be text the user never
/// typed, and it would sit between the cursor and the glyph it points at. So:
///
/// * a CRLF pair folds to a single `\n`, and a lone `\r` becomes `\n` — a
///   carriage return cannot be stored in the buffer at all;
/// * each tab expands to spaces up to the next [`TAB_STOP`] column of the current
///   logical line, counted exactly as [`normalize_impl`] counts it (a `\n`
///   resets the column, every kept char advances it by its own display width);
/// * every character [`terminal_keeps`] rejects is dropped — all C0/C1 controls
///   and DEL, the line/paragraph separators, and the non-joiner Unicode format
///   class (bidi overrides, ZWSP, BOM, …). The joiners U+200C/U+200D survive, as
///   they do everywhere else;
/// * everything else is kept verbatim. Nothing is escaped: a `\u{1b}` the user
///   actually typed stays the characters it is.
///
/// With control characters gone and tabs expanded, the buffer holds no character
/// whose measured and drawn widths can disagree — so the prompt's wrapping,
/// which measures with `UnicodeWidthStr::width`, measures exactly what ratatui
/// draws, and the cursor's byte offset stays honest.
///
/// Clean text borrows: the common case costs one scan and no allocation.
pub(crate) fn sanitize_pasted_text(text: &str) -> Cow<'_, str> {
    // A tab or a CR always needs rewriting; anything else only if the per-char
    // predicate rejects it. `terminal_keeps` reports tabs as kept (the display
    // sinks expand them in the same pass), so this checks them separately.
    if !text.contains(['\t', '\r']) && text.chars().all(terminal_keeps) {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    // Print column within the current logical line — the same state, and the
    // same rule, as `normalize_impl`, so a tab lands on the same stop here as it
    // would in a displayed result.
    let mut col = 0usize;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        // A CR can never be stored: CRLF is a normal line ending and folds to a
        // single `\n`, and a lone CR becomes a newline too rather than reaching
        // the daemon as a byte that would overwrite the drawn line.
        if c == '\r' {
            if chars.peek() == Some(&'\n') {
                chars.next(); // consume the '\n' of the pair
            }
            out.push('\n');
            col = 0;
            continue;
        }
        match c {
            '\t' => {
                let pad = TAB_STOP - (col % TAB_STOP);
                out.extend(std::iter::repeat_n(' ', pad));
                col += pad;
            }
            '\n' => {
                out.push('\n');
                col = 0;
            }
            _ if terminal_keeps(c) => {
                out.push(c);
                col += unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
            }
            // Every control and spoofing char is dropped, not escaped — the
            // editing rule cannot expand what it stores (see the doc above).
            _ => {}
        }
    }
    Cow::Owned(out)
}

/// Standard terminal tab-stop interval.  No width library agrees with what
/// ratatui draws for a tab: `unicode-width` counts `\t` as **one** column
/// inside a string, while ratatui filters control characters out of the
/// grapheme stream when it draws, so the tab occupies **no** cell.  A literal
/// tab therefore desyncs every width this crate computes from the cells on
/// screen — a row measured one column wider than it draws — and it loses its
/// indentation as well.  Expanding tabs to spaces at 4-column stops (a common
/// editor convention) makes every downstream width computation (grapheme wrap,
/// `Line::width`, height `div_ceil`, fill padding, code-box padding) measure
/// exactly what ratatui draws, and keeps tab-aligned columns aligned — matching
/// what the raw bytes would have shown on a terminal with 4-wide tab stops.
const TAB_STOP: usize = 4;

/// Whether a single char passes through the terminal filter unchanged: tabs,
/// newlines, printable ASCII, and safe non-ASCII. Everything else — every
/// C0/C1 control (the `Cc` category, exactly the predicate
/// [`crate::terminal::is_control_char`] applies to OSC payloads; a lone CR is
/// folded to `\n` by [`normalize_impl`] and [`sanitize_pasted_text`] before
/// this predicate runs), the line/paragraph separators, and the non-joiner
/// format-char spoofing class via the shared [`is_unsafe_unicode`] predicate
/// (owned by `choreo-sanitize`, the same policy the daemon's sanitizers use) —
/// is escaped by the display sinks and dropped by [`sanitize_pasted_text`]. SGR
/// sequences are handled separately by the display sinks (kept whole), so the
/// per-char predicate needs no ESC case.  Tabs are reported as kept because they
/// survive the classification step; both callers expand them to spaces in the
/// same pass.
fn terminal_keeps(c: char) -> bool {
    c == '\t'
        || c == '\n'
        || (c.is_ascii() && (' '..='~').contains(&c))
        || (!c.is_ascii() && !c.is_control() && !is_unsafe_unicode(c))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── tab expansion ────────────────────────────────────────────────

    #[test]
    fn normalize_ansi_text_no_tabs_returns_input() {
        // Common case (no tabs) must be a no-op, not a rewrite.
        let s = "plain text\nwithout tabs";
        assert_eq!(normalize_ansi_text(s), s);
    }

    #[test]
    fn normalize_ansi_text_leading_tab_becomes_four_spaces() {
        // A tab at column 0 advances to the next 4-column stop (column 4).
        assert_eq!(normalize_ansi_text("\tfoo"), "    foo");
    }

    #[test]
    fn normalize_ansi_text_mid_line_is_column_aware() {
        // "abc" sits at column 3; the next 4-column stop is 4 → 1 space,
        // not a fixed 4.
        assert_eq!(normalize_ansi_text("abc\tdef"), "abc def");
    }

    #[test]
    fn normalize_ansi_text_after_wide_char_tracks_display_columns() {
        // "日" occupies 2 columns; the next stop is 4 → 2 spaces.
        assert_eq!(normalize_ansi_text("日\tx"), "日  x");
    }

    #[test]
    fn normalize_ansi_text_at_tab_stop_adds_one_space() {
        // "1234567" fills column 7; a tab there advances 1 column to 8.
        assert_eq!(normalize_ansi_text("1234567\tx"), "1234567 x");
    }

    #[test]
    fn normalize_ansi_text_resets_column_per_line() {
        // Column tracking restarts after every newline, like a terminal.
        assert_eq!(normalize_ansi_text("a\tb\n\tc"), "a   b\n    c");
    }

    #[test]
    fn normalize_ansi_text_consecutive_tabs_chain() {
        // Two tabs at line start: col 0 → 4, then col 4 → 8.
        assert_eq!(normalize_ansi_text("\t\tfoo"), "        foo");
    }

    #[test]
    fn normalize_ansi_text_ignores_sgr_sequences_for_column_tracking() {
        // A complete SGR color sequence is invisible on screen: the column
        // must advance only past the visible chars, so a tab after a color
        // code pads to the correct 4-column stop instead of treating the
        // escape bytes as visible columns.  "abc" sits at column 3 (the
        // ESC[31m adds nothing) → 1 space.
        assert_eq!(
            normalize_ansi_text("\x1b[31mabc\tdef"),
            "\x1b[31mabc def",
            "SGR bytes must not count toward the column"
        );
        // Multi-param SGR and the reset form are handled the same way.
        assert_eq!(
            normalize_ansi_text("\x1b[1;32mab\tcd"),
            "\x1b[1;32mab  cd",
            "multi-param SGR must not count toward the column"
        );
        assert_eq!(
            normalize_ansi_text("\x1b[0m\tfoo"),
            "\x1b[0m    foo",
            "a tab right after a reset code pads from column 0"
        );
        // The sequence is preserved verbatim (the ANSI renderer needs it).
        assert!(normalize_ansi_text("\x1b[31mred\t").starts_with("\x1b[31mred"));
    }

    #[test]
    fn normalize_ansi_text_sgr_and_newline_interaction() {
        // Column tracking resets per line even when a color code spans lines:
        // each logical line starts its own tab-stop cycle.
        assert_eq!(
            normalize_ansi_text("\x1b[31mred\t\nblue\t"),
            "\x1b[31mred \nblue    "
        );
    }

    // ── fused normalizer against the two-pass composition ─────────────

    /// One CSI sequence starting at `chars[start]` (ESC), where `chars[start + 1]`
    /// is the `[`: returns the index one past the last char the reference filter
    /// consumes, and whether that made a complete SGR sequence.  A non-CSI byte or
    /// the end of input ends the attempt where it started, leaving that byte for the
    /// per-char pass — the documented rule.
    fn reference_csi(chars: &[char], start: usize) -> (usize, bool) {
        let mut i = start + 2;
        while let Some(&c) = chars.get(i) {
            let u = c as u32;
            if (0x30..=0x3f).contains(&u) {
                i += 1; // parameter byte
            } else if (0x40..=0x7e).contains(&u) {
                return (i + 1, c == 'm'); // final byte; only 'm' is SGR
            } else {
                break;
            }
        }
        (i, false)
    }

    /// Reference implementation of the composition the fused pass replaced: filter
    /// the input into one fresh `String`, then expand tabs over that string into a
    /// second one.
    ///
    /// Written independently of the fused loop — index-based over a `Vec<char>`, the
    /// tab pass re-reading the filtered text — so it catches a drift in either half
    /// instead of mirroring one.
    fn reference_normalize(text: &str, keep_sgr: bool) -> String {
        reference_expand_tabs(&reference_filter(text, keep_sgr))
    }

    /// The filtering half of [`reference_normalize`].
    fn reference_filter(text: &str, keep_sgr: bool) -> String {
        let chars: Vec<char> = text.chars().collect();
        let mut out = String::with_capacity(text.len());
        let mut i = 0;
        while i < chars.len() {
            let c = chars[i];
            if c == '\r' && chars.get(i + 1) == Some(&'\n') {
                out.push('\n');
                i += 2;
                continue;
            }
            if keep_sgr && c == '\u{1b}' && chars.get(i + 1) == Some(&'[') {
                let (end, sgr) = reference_csi(&chars, i);
                if sgr {
                    out.extend(chars.get(i..end).unwrap_or_default());
                } else {
                    out.push_str("\\u{1b}");
                    out.extend(chars.get(i + 1..end).unwrap_or_default());
                }
                i = end;
                continue;
            }
            if terminal_keeps(c) {
                out.push(c);
            } else {
                out.extend(c.escape_default());
            }
            i += 1;
        }
        out
    }

    /// The tab-expansion half of [`reference_normalize`], over already-filtered
    /// text: 4-column stops counted from each line's own start, with a kept SGR
    /// sequence advancing the column by nothing (it draws no cell).
    fn reference_expand_tabs(text: &str) -> String {
        let chars: Vec<char> = text.chars().collect();
        let mut out = String::with_capacity(text.len());
        let mut col = 0usize;
        let mut i = 0;
        while i < chars.len() {
            match chars[i] {
                '\n' => {
                    out.push('\n');
                    col = 0;
                    i += 1;
                }
                '\t' => {
                    let pad = TAB_STOP - (col % TAB_STOP);
                    out.extend(std::iter::repeat_n(' ', pad));
                    col += pad;
                    i += 1;
                }
                '\u{1b}' => {
                    out.push('\u{1b}');
                    i += 1;
                    if chars.get(i) == Some(&'[') {
                        out.push('[');
                        i += 1;
                        while let Some(&n) = chars.get(i) {
                            out.push(n);
                            i += 1;
                            if ('\u{40}'..='\u{7e}').contains(&n) {
                                break;
                            }
                        }
                    }
                }
                c => {
                    out.push(c);
                    // The renderer measures with `unicode-width`, so the reference
                    // has to count columns with the same table.
                    col += unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
                    i += 1;
                }
            }
        }
        out
    }

    /// Inputs that together reach every branch of the fused pass: each control
    /// class the filter distinguishes, every tab position a line can put one in, and
    /// the line-boundary cases that reset a tab-stop column.
    const NORMALIZE_CORPUS: &[(&str, &str)] = &[
        ("empty", ""),
        ("plain ascii", "plain ascii text, nothing to escape"),
        ("tab at column 0", "\tindent"),
        ("tab mid line", "abc\tdef"),
        ("tab after wide char", "日本語\tx"),
        ("tab after sgr", "\x1b[32mgreen\tpadding"),
        ("consecutive tabs", "\t\t\tdeep"),
        (
            "tab-indented fenced code",
            "```rust\n\tfn main() {\n\t\tbody();\n\t}\n```\n",
        ),
        ("crlf", "one\r\ntwo\r\nthree"),
        ("lone cr", "a\rb\r"),
        ("bel backspace nul", "a\x07b\x08c\0d"),
        ("bidi override", "a\u{202e}b"),
        ("line separators", "a\u{2028}b\u{2029}c"),
        ("unterminated csi", "pre\x1b[31"),
        ("incomplete csi at esc", "pre\x1b"),
        ("complete sgr run", "\x1b[1;31mbold red\x1b[0m"),
        (
            "mixed content",
            "tab\there\r\n\x1b[36mcyan\tpart\x1b[0m \u{202e} \x07\n\t日本語\tdone",
        ),
        (
            "long tab line crossing many stops",
            "a\tb\tc\td\te\tf\tg\th\ti\tj\tk\tl\tm\tn\to\tp\tq\tr\ts\tt\tu\tv\tw\tx\ty\tz\t尾",
        ),
    ];

    #[test]
    fn fused_normalizer_matches_the_filter_then_expand_composition() {
        // The fused pass must produce the *same bytes* as filtering into one String
        // and expanding tabs over it into another — for both SGR policies, over
        // every char class and tab position the renderer can meet.  This is the
        // safety net for the fusion: any drift in the escape set, the tab stops, the
        // column tracking (a wide char, a zero-width grapheme, a kept SGR sequence, a
        // just-escaped control char) shows up here as a byte difference.
        for (name, input) in NORMALIZE_CORPUS {
            assert_eq!(
                normalize_ansi_text(input),
                reference_normalize(input, true),
                "[{name}] fused SGR-keeping output drifted from the composition"
            );
            assert_eq!(
                normalize_display_text(input),
                std::borrow::Cow::<str>::Owned(reference_normalize(input, false)),
                "[{name}] fused SGR-escaping output drifted from the composition"
            );
        }
    }

    // ── normalize_display_text ───────────────────────────────────────

    #[test]
    fn normalize_display_text_escapes_sgr_sequences() {
        // Content no ANSI parser will see (markdown source, which the renderer
        // styles itself) must come out with no control character at all: a kept SGR
        // sequence would draw as the literal text `[31m` while still measuring four
        // columns, desyncing the row from everything around it.
        assert_eq!(
            normalize_display_text("\x1b[31mred\x1b[0m"),
            "\\u{1b}[31mred\\u{1b}[0m"
        );
        // Everything that is not ESC behaves exactly like the SGR-preserving filter,
        // tabs included: they expand at the 4-column stop (column 1 → 4 = 3 spaces).
        assert_eq!(normalize_display_text("a\u{202e}b"), "a\\u{202e}b");
        assert_eq!(normalize_display_text("a\tb\nc\r\n日"), "a   b\nc\n日");
    }

    #[test]
    fn normalize_display_text_borrows_clean_text_and_escapes_tabs() {
        // A response that is already clean — the overwhelmingly common case — costs
        // one scan and no allocation.
        assert!(matches!(
            normalize_display_text("plain 日本語 text\nwith lines"),
            std::borrow::Cow::Borrowed(_)
        ));
        // A tab expands at the 4-column stop *and* the control characters around it
        // are escaped, so nothing measured is invisible on screen.
        assert_eq!(normalize_display_text("\tif (x)"), "    if (x)");
        assert_eq!(normalize_display_text("a\x07b"), "a\\u{7}b");
        assert_eq!(normalize_display_text("\x1b[2J"), "\\u{1b}[2J");
    }

    #[test]
    fn normalize_display_text_is_idempotent() {
        // Normalized output is itself clean — no tab, no character outside
        // `terminal_keeps` — so re-normalizing it must return the same bytes and
        // take the borrowed fast path.  Widgets that normalize at more than one
        // layer (a body normalized before the markdown parse, then again by the
        // plain-text sink drawing a label built from it) rely on this: the second
        // pass is free and cannot double-escape.
        let once = normalize_display_text("a\u{7}\tb\u{202e}c\x1b[31m");
        let twice = normalize_display_text(&once);
        assert_eq!(twice, once, "re-normalizing clean text changes nothing");
        assert!(
            matches!(twice, std::borrow::Cow::Borrowed(_)),
            "clean text takes the borrowed fast path: {twice:?}"
        );
        // The empty string and text that was clean to begin with both round-trip.
        assert!(matches!(
            normalize_display_text(&normalize_display_text("plain text")),
            std::borrow::Cow::Borrowed(_)
        ));
    }

    // ── SGR passthrough and escape handling ───────────────────────────

    #[test]
    fn normalize_ansi_text_keeps_sgr_sequences() {
        // Genuine SGR color sequences survive the filter verbatim so the
        // ANSI renderer can still colorize shell/VM output.
        assert_eq!(
            normalize_ansi_text("\x1b[31mred\x1b[0m"),
            "\x1b[31mred\x1b[0m"
        );
        assert_eq!(
            normalize_ansi_text("\x1b[1;31m bold red \x1b[m"),
            "\x1b[1;31m bold red \x1b[m"
        );
    }

    #[test]
    fn normalize_ansi_text_escapes_osc_csi_and_controls() {
        // OSC (clipboard writes, title changes), non-SGR CSI (clear screen),
        // backspace, and BEL must never reach the terminal as live control
        // sequences — they render as inert escaped text instead.
        let osc = normalize_ansi_text("\x1b]52;c;evil\x07");
        assert!(osc.contains("\\u{1b}"), "OSC ESC must be escaped: {osc:?}");
        assert!(!osc.contains('\x1b'), "no live ESC may survive: {osc:?}");
        assert!(!osc.contains('\x07'), "BEL must be escaped: {osc:?}");

        let csi = normalize_ansi_text("\x1b[2J");
        assert_eq!(csi, "\\u{1b}[2J", "non-SGR CSI must render inert");

        let bs = normalize_ansi_text("a\x08b");
        assert_eq!(bs, "a\\u{8}b");

        // An unterminated ESC at end of input is escaped, not passed through.
        assert_eq!(normalize_ansi_text("tail\x1b"), "tail\\u{1b}");
    }

    #[test]
    fn normalize_ansi_text_escapes_bidi_and_separators_keeps_joiners() {
        // The spoofing class: bidi overrides and other invisible format chars
        // must not be able to reorder or hide rendered text; joiners are
        // legitimate in some scripts and pass through. Newlines/CJK stay and tabs
        // expand to the next 4-column stop;
        // a CRLF pair is a normal line ending and is folded through, while a
        // lone CR is escaped (a carriage return would let hostile content
        // overwrite its own rendered line).
        assert_eq!(normalize_ansi_text("a\u{202e}b"), "a\\u{202e}b");
        assert_eq!(normalize_ansi_text("a\u{200b}b"), "a\\u{200b}b");
        assert_eq!(normalize_ansi_text("a\u{2028}b"), "a\\u{2028}b");
        assert_eq!(normalize_ansi_text("a\u{200c}b"), "a\u{200c}b");
        assert_eq!(normalize_ansi_text("a\u{200d}b"), "a\u{200d}b");
        assert_eq!(normalize_ansi_text("a\tb\nc\n日本語"), "a   b\nc\n日本語");
        assert_eq!(
            normalize_ansi_text("a\tb\nc\r\n日本語"),
            "a   b\nc\n日本語",
            "CRLF must fold to a single LF"
        );
    }

    #[test]
    fn normalize_ansi_text_folds_crlf_but_escapes_lone_cr() {
        // CRLF is a normal line ending and folds to a single LF (no control
        // char reaches the rendered cell stream); a lone CR (which would
        // overwrite the rendered line) is escaped.
        assert_eq!(normalize_ansi_text("a\r\nb"), "a\nb");
        assert_eq!(normalize_ansi_text("a\rb"), "a\\rb");
        // A lone CR is escaped even when it precedes a folded CRLF pair.
        assert_eq!(normalize_ansi_text("a\r\r\nb"), "a\\r\nb");
        // CR at end of input (no following LF) is escaped.
        assert_eq!(normalize_ansi_text("a\r"), "a\\r");
        // No raw CR may survive the filter in any case — the sink defense
        // must keep control chars out of the terminal entirely.
        assert!(
            !normalize_ansi_text("a\r\nb\rc").contains('\r'),
            "filter output must never contain a raw CR"
        );
    }

    // ── the per-char keep policy, swept over the whole code space ─────

    /// Run `check` over every Unicode scalar value, sharded across the available
    /// cores.
    ///
    /// The sweep below is exhaustive by design (it pins the per-char keep policy
    /// against the shared spoofing predicate for *every* char) and pure CPU;
    /// sharding keeps it exhaustive while cutting its wall time to roughly one
    /// core's share. A panic in any shard propagates out of `scope` once every
    /// thread has joined.
    fn sweep_code_space(check: impl Fn(char) + Sync) {
        const TOTAL: u32 = 0x11_0000; // Unicode scalar range, excluding surrogates.
        let threads = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
        let shard = TOTAL.div_ceil(u32::try_from(threads).unwrap_or(1)).max(1);
        std::thread::scope(|scope| {
            let mut start = 0u32;
            while start < TOTAL {
                let end = (start + shard).min(TOTAL);
                let check = &check;
                scope.spawn(move || {
                    for cp in start..end {
                        if let Some(c) = char::from_u32(cp) {
                            check(c);
                        }
                    }
                });
                start = end;
            }
        });
    }

    #[test]
    fn terminal_keep_policy_sweeps_all_chars() {
        // The sink keep-policy must agree with the shared spoofing predicate
        // for *every* char: keep tabs, newlines, printable ASCII, and safe
        // non-ASCII; escape every C0/C1 control (including CR — a lone carriage
        // return in rendered content would let a hostile result overwrite its
        // own line; CRLF pairs are folded by the filter before this predicate
        // runs) and every shared-unsafe Unicode char. The predicate's own
        // correctness against the Unicode tables is guarded by the code-space
        // sweep in choreo-sanitize; this sweep pins the crate's per-char policy
        // (the SGR passthrough is handled separately and has its own tests).
        // Stated as the two directions of the policy rather than a copy of
        // `terminal_keeps`, so a change to the implementation that silently
        // keeps or escapes the wrong class is caught:
        //   - kept chars must be structural ASCII, printable ASCII, or safe
        //     non-ASCII — never a control or spoofing char;
        //   - every control (except TAB/LF), spoofing char, and unprintable
        //     ASCII byte must be escaped — nothing safe may leak.
        sweep_code_space(|c| {
            let keeps = terminal_keeps(c);
            let structural = matches!(c, '\t' | '\n');
            let printable_ascii = c.is_ascii() && (' '..='~').contains(&c);
            let safe_non_ascii = !c.is_ascii() && !c.is_control() && !is_unsafe_unicode(c);
            if keeps {
                assert!(
                    structural || printable_ascii || safe_non_ascii,
                    "kept char U+{:04X} violates the keep policy",
                    c as u32
                );
            } else {
                // Everything rejected is a control (all non-printable ASCII
                // is C0/DEL) or a shared spoofing char — nothing safe.
                assert!(
                    c.is_control() || is_unsafe_unicode(c),
                    "escaped char U+{:04X} is safe and should be kept",
                    c as u32
                );
            }
        });
    }

    // ── sanitize_pasted_text (the editing ingest rule) ────────────────

    #[test]
    fn sanitize_pasted_text_borrows_clean_text_unchanged() {
        // The overwhelmingly common paste — clean text — costs one scan and no
        // allocation, and comes back byte-identical.
        let clean = "plain 日本語 text\nwith lines";
        assert_eq!(sanitize_pasted_text(clean).as_ref(), clean);
        assert!(matches!(sanitize_pasted_text(clean), Cow::Borrowed(_)));
        assert!(matches!(sanitize_pasted_text(""), Cow::Borrowed(_)));
    }

    #[test]
    fn sanitize_pasted_text_drops_escapes_instead_of_escaping_them() {
        // The editing rule stores text the user will submit, so a control
        // character is *dropped* — never expanded into the `\u{1b}` escape text
        // the display sinks produce (that would be characters nobody typed).
        assert_eq!(sanitize_pasted_text("\x1b[31mred").as_ref(), "[31mred");
        assert_eq!(sanitize_pasted_text("a\x1bb").as_ref(), "ab");
        assert_eq!(sanitize_pasted_text("\x1b").as_ref(), "");
        assert_ne!(
            sanitize_pasted_text("\x1b[31m").as_ref(),
            "\\u{1b}[31m",
            "a pasted ESC must be dropped, not escaped"
        );
        // A literal `\u{1b}` the user actually typed is printable text and
        // passes through verbatim — nothing is re-escaped.
        assert_eq!(sanitize_pasted_text("\\u{1b}").as_ref(), "\\u{1b}");
    }

    #[test]
    fn sanitize_pasted_text_expands_tabs_to_the_next_stop() {
        // Tab stops follow the same 4-column rule the display sinks use.
        assert_eq!(sanitize_pasted_text("\tfoo").as_ref(), "    foo");
        // "abc" sits at column 3, so the next stop is 4 (one space).
        assert_eq!(sanitize_pasted_text("abc\tdef").as_ref(), "abc def");
        // "1234567" fills column 7; a tab there advances one column to 8.
        assert_eq!(sanitize_pasted_text("1234567\tx").as_ref(), "1234567 x");
        // Column tracking restarts after every newline.
        assert_eq!(sanitize_pasted_text("a\tb\n\tc").as_ref(), "a   b\n    c");
    }

    #[test]
    fn sanitize_pasted_text_expands_tab_after_wide_char() {
        // "日" occupies two columns, so a tab at column 2 pads two spaces to
        // reach the stop at 4.
        assert_eq!(sanitize_pasted_text("日\tx").as_ref(), "日  x");
    }

    #[test]
    fn sanitize_pasted_text_folds_carriage_returns() {
        // A CR cannot be stored: CRLF folds to one newline, and a lone CR
        // becomes a newline rather than overwriting the drawn line.
        assert_eq!(sanitize_pasted_text("one\r\ntwo").as_ref(), "one\ntwo");
        assert_eq!(sanitize_pasted_text("a\rb").as_ref(), "a\nb");
        assert_eq!(sanitize_pasted_text("a\r").as_ref(), "a\n");
        assert!(!sanitize_pasted_text("a\r\nb\rc").contains('\r'));
    }

    #[test]
    fn sanitize_pasted_text_drops_controls_and_separators() {
        // DEL, backspace and BEL leave no trace in the stored buffer.
        assert_eq!(sanitize_pasted_text("a\x7f\x08\x07b").as_ref(), "ab");
        // A bidi override, a zero-width space and a line separator are dropped
        // too, so a paste cannot smuggle an invisible reordering character into
        // the prompt.
        assert_eq!(sanitize_pasted_text("a\u{202e}b").as_ref(), "ab");
        assert_eq!(sanitize_pasted_text("a\u{2028}b").as_ref(), "ab");
        assert_eq!(sanitize_pasted_text("a\u{200b}b").as_ref(), "ab");
    }

    #[test]
    fn sanitize_pasted_text_keeps_the_joiners() {
        // U+200C/U+200D survive, exactly as they do at the display sinks.
        assert_eq!(sanitize_pasted_text("a\u{200c}b").as_ref(), "a\u{200c}b");
        assert_eq!(sanitize_pasted_text("a\u{200d}b").as_ref(), "a\u{200d}b");
    }

    #[test]
    fn sanitize_pasted_text_of_only_controls_is_empty() {
        // A paste of nothing but control characters sanitizes away entirely —
        // the caller then edits nothing at all.
        assert_eq!(sanitize_pasted_text("\x1b\x07\x7f\u{202e}").as_ref(), "");
        assert!(sanitize_pasted_text("\x00\x01\x02").is_empty());
    }
}
