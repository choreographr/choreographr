use super::super::*;

// ── plain_text_lines ─────────────────────────────────────────────────

#[test]
fn plain_text_lines_empty() {
    let result = plain_text_lines("", 80);
    assert_eq!(result.len(), 1, "empty input → one empty line");
    assert_eq!(result[0].width(), 0);
}

#[test]
fn plain_text_lines_single() {
    let result = plain_text_lines("hello", 80);
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].to_string(), "hello");
}

#[test]
fn plain_text_lines_multi() {
    let result = plain_text_lines("a\nb\nc", 80);
    assert_eq!(result.len(), 3);
    assert_eq!(result[0].to_string(), "a");
    assert_eq!(result[1].to_string(), "b");
    assert_eq!(result[2].to_string(), "c");
}

#[test]
fn plain_text_lines_wraps_long_line() {
    // A 200-char single-span line must wrap into ≤40-column lines — the
    // regression fixed by passing the content width: previously plain
    // tool output was emitted unwrapped and clipped at the viewport edge.
    let long = "x".repeat(200);
    let result = plain_text_lines(&long, 40);
    assert_eq!(result.len(), 5, "200 chars at 40 wide = 5 lines");
    for line in &result {
        assert!(
            line.width() <= 40,
            "wrapped line width {} exceeds 40",
            line.width()
        );
    }
    // Concatenation reproduces the input exactly (nothing dropped).
    let joined: String = result.iter().map(ToString::to_string).collect();
    assert_eq!(joined, long);
}

#[test]
fn plain_text_lines_wraps_at_word_boundary() {
    // Break at whitespace when it fits, keeping the whole word on the
    // next line — but never dropping content (the space stays as trailing
    // whitespace on the wrapped line, so concatenation is verbatim).
    let result = plain_text_lines("hello world", 6);
    let lines: Vec<String> = result.iter().map(ToString::to_string).collect();
    assert_eq!(lines, vec!["hello ", "world"]);
    let joined: String = result.iter().map(ToString::to_string).collect();
    assert_eq!(joined, "hello world", "content preserved verbatim");
}

#[test]
fn plain_text_lines_preserves_leading_whitespace() {
    // Indented plain output (code, aligned columns) must keep its
    // indentation when wrapped — no whitespace collapsing.
    let result = plain_text_lines("        let x = a_very_long_identifier;", 16);
    let joined: String = result.iter().map(ToString::to_string).collect();
    assert!(
        joined.starts_with("        let"),
        "leading indent must survive wrapping: {joined:?}"
    );
    for line in &result {
        assert!(
            line.width() <= 16,
            "wrapped line width {} exceeds 16",
            line.width()
        );
    }
}

#[test]
fn plain_text_lines_splits_oversized_word() {
    // A single word wider than the width is hard-split by grapheme.
    let result = plain_text_lines("abcdefghij", 3);
    let lines: Vec<String> = result.iter().map(ToString::to_string).collect();
    assert_eq!(lines, vec!["abc", "def", "ghi", "j"]);
    let joined: String = result.iter().map(ToString::to_string).collect();
    assert_eq!(joined, "abcdefghij");
}

#[test]
fn plain_text_lines_wide_grapheme_alone_on_line() {
    // A grapheme wider than the width (e.g. an emoji at width 1) must not
    // loop forever; it occupies its own over-wide line.
    let result = plain_text_lines("😀", 1);
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].to_string(), "😀");
}

#[test]
fn plain_text_lines_cjk_widths() {
    // Display width (not char count) drives wrapping: 4 CJK chars = 8
    // columns at width 4 → two lines of 2 chars each.
    let result = plain_text_lines("日本語文", 4);
    let lines: Vec<String> = result.iter().map(ToString::to_string).collect();
    assert_eq!(lines, vec!["日本", "語文"]);
}

#[test]
fn plain_text_lines_trailing_newline_keeps_blank_line() {
    // split('\n') semantics: a trailing newline yields a final blank
    // line, matching the old behavior.
    let result = plain_text_lines("a\n", 80);
    assert_eq!(result.len(), 2);
    assert_eq!(result[0].to_string(), "a");
    assert_eq!(result[1].to_string(), "");
}

// ── grapheme_chunks (shared hard-splitter) ───────────────────────────

#[test]
fn grapheme_chunks_exact_fit_flushes_immediately() {
    // A chunk that exactly fills the width is flushed so the next grapheme
    // starts a fresh chunk — same boundaries as wrap_plain_line's inline
    // hard-split (which cuts when the *next* grapheme would overflow).
    assert_eq!(grapheme_chunks("abcdefgh", 3, 0), ["abc", "def", "gh"]);
}

#[test]
fn grapheme_chunks_wide_grapheme_alone_on_line() {
    // A grapheme wider than the width occupies its own over-wide chunk.
    assert_eq!(grapheme_chunks("😀😀", 1, 0), ["😀", "😀"]);
}

#[test]
fn grapheme_chunks_floor_keeps_zero_width_graphemes() {
    // A leading combining mark is a zero-width grapheme.  With floor 1
    // (split_word_to_width) it still occupies a column of its own chunk;
    // with floor 0 (plain text) it merges invisibly.
    let run = "\u{301}ab";
    assert_eq!(grapheme_chunks(run, 1, 1), ["\u{301}", "a", "b"]);
    assert_eq!(grapheme_chunks(run, 1, 0), ["\u{301}a", "b"]);
}

#[test]
fn grapheme_chunks_empty_returns_one_empty_chunk() {
    assert_eq!(grapheme_chunks("", 5, 0), [""]);
}

// ── lines_height ────────────────────────────────────────────────────

#[test]
fn lines_height_simple() {
    let lines = vec![Line::from("hello")];
    assert_eq!(lines_height(&lines, 80), 1);
}

#[test]
fn lines_height_zero_width() {
    let lines = vec![Line::from("hello")];
    assert_eq!(lines_height(&lines, 0), 0);
}

#[test]
fn lines_height_wrapping() {
    let text = "x".repeat(100);
    let lines = vec![Line::from(text)];
    assert_eq!(lines_height(&lines, 40), 3);
}

#[test]
fn lines_height_multiple_lines() {
    let lines = vec![Line::from("short"), Line::from("a".repeat(50))];
    assert_eq!(lines_height(&lines, 30), 3);
}

#[test]
fn lines_height_empty() {
    let lines = vec![Line::from("")];
    assert_eq!(lines_height(&lines, 80), 1);
}

#[test]
fn lines_height_empty_slice_returns_zero() {
    let lines: Vec<Line<'static>> = vec![];
    assert_eq!(lines_height(&lines, 80), 0);
}

// ── display_width ────────────────────────────────────────────────────

#[test]
fn display_width_ascii() {
    assert_eq!(display_width("hello"), 5);
}

#[test]
fn display_width_unicode() {
    assert_eq!(display_width("café"), 4);
}

#[test]
fn display_width_empty() {
    assert_eq!(display_width(""), 0);
}
