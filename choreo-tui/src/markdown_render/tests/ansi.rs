use super::super::*;
use crate::text_filter::normalize_ansi_text;

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
