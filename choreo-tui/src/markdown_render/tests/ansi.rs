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

// ── sanitize_for_terminal ────────────────────────────────────────────

#[test]
fn expand_tabs_no_tabs_returns_input() {
    // Common case (no tabs) must be a no-op, not a rewrite.
    let s = "plain text\nwithout tabs";
    assert_eq!(expand_tabs(s), s);
}

#[test]
fn expand_tabs_leading_tab_becomes_four_spaces() {
    // A tab at column 0 advances to the next 4-column stop (column 4).
    assert_eq!(expand_tabs("\tfoo"), "    foo");
}

#[test]
fn expand_tabs_mid_line_is_column_aware() {
    // "abc" sits at column 3; the next 4-column stop is 4 → 1 space,
    // not a fixed 4.
    assert_eq!(expand_tabs("abc\tdef"), "abc def");
}

#[test]
fn expand_tabs_after_wide_char_tracks_display_columns() {
    // "日" occupies 2 columns; the next stop is 4 → 2 spaces.
    assert_eq!(expand_tabs("日\tx"), "日  x");
}

#[test]
fn expand_tabs_at_tab_stop_adds_one_space() {
    // "1234567" fills column 7; a tab there advances 1 column to 8.
    assert_eq!(expand_tabs("1234567\tx"), "1234567 x");
}

#[test]
fn expand_tabs_resets_column_per_line() {
    // Column tracking restarts after every newline, like a terminal.
    assert_eq!(expand_tabs("a\tb\n\tc"), "a   b\n    c");
}

#[test]
fn expand_tabs_consecutive_tabs_chain() {
    // Two tabs at line start: col 0 → 4, then col 4 → 8.
    assert_eq!(expand_tabs("\t\tfoo"), "        foo");
}

#[test]
fn expand_tabs_ignores_sgr_sequences_for_column_tracking() {
    // A complete SGR color sequence is invisible on screen: the column
    // must advance only past the visible chars, so a tab after a color
    // code pads to the correct 4-column stop instead of treating the
    // escape bytes as visible columns.  "abc" sits at column 3 (the
    // ESC[31m adds nothing) → 1 space.
    assert_eq!(
        expand_tabs("\x1b[31mabc\tdef"),
        "\x1b[31mabc def",
        "SGR bytes must not count toward the column"
    );
    // Multi-param SGR and the reset form are handled the same way.
    assert_eq!(
        expand_tabs("\x1b[1;32mab\tcd"),
        "\x1b[1;32mab  cd",
        "multi-param SGR must not count toward the column"
    );
    assert_eq!(
        expand_tabs("\x1b[0m\tfoo"),
        "\x1b[0m    foo",
        "a tab right after a reset code pads from column 0"
    );
    // The sequence is preserved verbatim (the ANSI renderer needs it).
    assert!(expand_tabs("\x1b[31mred\t").starts_with("\x1b[31mred"));
}

#[test]
fn expand_tabs_sgr_and_newline_interaction() {
    // Column tracking resets per line even when a color code spans lines:
    // each logical line starts its own tab-stop cycle.
    assert_eq!(
        expand_tabs("\x1b[31mred\t\nblue\t"),
        "\x1b[31mred \nblue    "
    );
}

#[test]
fn sanitize_for_terminal_keeps_sgr_sequences() {
    // Genuine SGR color sequences survive the filter verbatim so the
    // ANSI renderer below can still colorize shell/VM output.
    assert_eq!(
        sanitize_for_terminal("\x1b[31mred\x1b[0m"),
        "\x1b[31mred\x1b[0m"
    );
    assert_eq!(
        sanitize_for_terminal("\x1b[1;31m bold red \x1b[m"),
        "\x1b[1;31m bold red \x1b[m"
    );
}

#[test]
fn sanitize_for_terminal_escapes_osc_csi_and_controls() {
    // OSC (clipboard writes, title changes), non-SGR CSI (clear screen),
    // backspace, and BEL must never reach the terminal as live control
    // sequences — they render as inert escaped text instead.
    let osc = sanitize_for_terminal("\x1b]52;c;evil\x07");
    assert!(osc.contains("\\u{1b}"), "OSC ESC must be escaped: {osc:?}");
    assert!(!osc.contains('\x1b'), "no live ESC may survive: {osc:?}");
    assert!(!osc.contains('\x07'), "BEL must be escaped: {osc:?}");

    let csi = sanitize_for_terminal("\x1b[2J");
    assert_eq!(csi, "\\u{1b}[2J", "non-SGR CSI must render inert");

    let bs = sanitize_for_terminal("a\x08b");
    assert_eq!(bs, "a\\u{8}b");

    // An unterminated ESC at end of input is escaped, not passed through.
    assert_eq!(sanitize_for_terminal("tail\x1b"), "tail\\u{1b}");
}

#[test]
fn sanitize_for_terminal_escapes_bidi_and_separators_keeps_joiners() {
    // The spoofing class: bidi overrides and other invisible format chars
    // must not be able to reorder or hide rendered text; joiners are
    // legitimate in some scripts and pass through. Tabs/newlines/CJK stay;
    // a CRLF pair is a normal line ending and is folded through, while a
    // lone CR is escaped (a carriage return would let hostile content
    // overwrite its own rendered line).
    assert_eq!(sanitize_for_terminal("a\u{202e}b"), "a\\u{202e}b");
    assert_eq!(sanitize_for_terminal("a\u{200b}b"), "a\\u{200b}b");
    assert_eq!(sanitize_for_terminal("a\u{2028}b"), "a\\u{2028}b");
    assert_eq!(sanitize_for_terminal("a\u{200c}b"), "a\u{200c}b");
    assert_eq!(sanitize_for_terminal("a\u{200d}b"), "a\u{200d}b");
    assert_eq!(sanitize_for_terminal("a\tb\nc\n日本語"), "a\tb\nc\n日本語");
    assert_eq!(
        sanitize_for_terminal("a\tb\nc\r\n日本語"),
        "a\tb\nc\n日本語",
        "CRLF must fold to a single LF"
    );
}

#[test]
fn sanitize_for_terminal_folds_crlf_but_escapes_lone_cr() {
    // CRLF is a normal line ending and folds to a single LF (no control
    // char reaches the rendered cell stream); a lone CR (which would
    // overwrite the rendered line) is escaped.
    assert_eq!(sanitize_for_terminal("a\r\nb"), "a\nb");
    assert_eq!(sanitize_for_terminal("a\rb"), "a\\rb");
    // A lone CR is escaped even when it precedes a folded CRLF pair.
    assert_eq!(sanitize_for_terminal("a\r\r\nb"), "a\\r\nb");
    // CR at end of input (no following LF) is escaped.
    assert_eq!(sanitize_for_terminal("a\r"), "a\\r");
    // No raw CR may survive the filter in any case — the sink defense
    // must keep control chars out of the terminal entirely.
    assert!(
        !sanitize_for_terminal("a\r\nb\rc").contains('\r'),
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
    let filtered = sanitize_for_terminal("\x1b[31mhello\x1b[0m");
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
