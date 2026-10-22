use super::super::*;
use super::sweep_code_space;

// ── ansi_lines ──────────────────────────────────────────────────────

#[test]
fn ansi_lines_colors() {
    let result = ansi_lines("\x1b[31mhello\x1b[0m", 80);
    assert_eq!(result.len(), 1, "should produce one line");
    let has_red = result[0]
        .spans
        .iter()
        .any(|s| s.style.fg == Some(Color::Red));
    assert!(has_red, "ANSI red should translate to ratatui red fg");
}

#[test]
fn ansi_lines_fallback_on_junk() {
    let result = ansi_lines("\x1b[z", 80); // incomplete/invalid ANSI sequence
    assert_eq!(result.len(), 1, "junk bytes should fall back to one line");
    // The fallback (plain_text_lines) produces spans with default style.
    let all_default = result[0].spans.iter().all(|s| s.style == Style::default());
    assert!(all_default, "fallback output should have default style");
}

#[test]
fn ansi_lines_empty() {
    let result = ansi_lines("", 80);
    assert_eq!(result.len(), 1, "empty input → one line");
    assert_eq!(result[0].width(), 0, "line should be empty");
}

// ── normalize_ansi_text ──────────────────────────────────────────────

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

// ── fused normalizer against the two-pass composition ─────────────────

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

#[test]
fn normalize_display_text_escapes_sgr_sequences() {
    // Content no ANSI parser will see (markdown source, which this renderer
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

#[test]
fn normalize_ansi_text_keeps_sgr_sequences() {
    // Genuine SGR color sequences survive the filter verbatim so the
    // ANSI renderer below can still colorize shell/VM output.
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

#[test]
fn terminal_keep_policy_sweeps_all_chars() {
    // The sink keep-policy must agree with the shared spoofing predicate
    // for *every* char: keep tabs, newlines, printable ASCII, and safe
    // non-ASCII; escape every C0/C1 control (including CR — a lone carriage
    // return in rendered content would let a hostile result overwrite its
    // own line; CRLF pairs are folded by the filter before this predicate
    // runs) and every shared-unsafe Unicode char. The predicate's own
    // correctness against the Unicode tables is guarded by the code-space
    // sweep in choreo-sanitize; this sweep pins the TUI's per-char policy
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

#[test]
fn ansi_coloring_survives_the_terminal_filter() {
    // End-to-end: content with SGR passes through the filter and the ANSI
    // renderer still colorizes it — the filter must not break coloring.
    let filtered = normalize_ansi_text("\x1b[31mhello\x1b[0m");
    let result = ansi_lines(&filtered, 80);
    assert_eq!(result.len(), 1);
    let has_red = result[0]
        .spans
        .iter()
        .any(|s| s.style.fg == Some(Color::Red));
    assert!(
        has_red,
        "red SGR must survive the filter into ratatui styles"
    );
}

#[test]
fn ansi_lines_multi_line() {
    let result = ansi_lines("line1\nline2\nline3", 80);
    assert_eq!(
        result.len(),
        3,
        "ANSI text with newlines should produce one line per segment"
    );
}

#[test]
fn ansi_lines_wrapping() {
    // A single-line input that is wide enough to require wrapping.
    let long = "hello world ".repeat(20);
    let result = ansi_lines(&long, 40);
    assert!(
        result.len() > 1,
        "wide content should wrap into multiple lines, got {}",
        result.len()
    );
}

#[test]
fn ansi_lines_no_wrap_when_fits() {
    let result = ansi_lines("short", 80);
    assert_eq!(result.len(), 1, "short content should not wrap");
}

#[test]
fn ansi_lines_wrap_long_word() {
    // A single word wider than the wrap width should be split.
    let result = ansi_lines("superlongword", 5);
    assert!(result.len() > 1, "over-long word should wrap");
    assert!(
        result.iter().all(|l| l.width() <= 5),
        "every wrapped line must be ≤ 5 wide"
    );
}
