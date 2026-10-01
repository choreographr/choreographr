use super::super::*;

// ── markdown_lines ───────────────────────────────────────────────────

#[test]
fn markdown_lines_empty() {
    let result = markdown_lines("", 80);
    assert!(!result.is_empty(), "should not return empty vec");
    assert_eq!(result[0].width(), 0);
}

#[test]
fn markdown_lines_paragraph() {
    let result = markdown_lines("hello world", 80);
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].to_string(), "hello world");
}

#[test]
fn markdown_lines_code_block() {
    let md = "```rust\nfn main() {}\n```";
    let result = markdown_lines(md, 80);
    // Top border, language row, blank padding row, code row, bottom border.
    assert!(result.len() >= 5, "code block should have at least 5 lines");
    // The ``` fences are never rendered — a table-style rounded border closes
    // the box instead.
    let first = result[0].to_string();
    assert!(
        first.starts_with('╭') && first.ends_with('╮') && first.contains('─'),
        "top border: {first:?}"
    );
    let last = result.last().unwrap().to_string();
    assert!(
        last.starts_with('╰') && last.ends_with('╯') && last.contains('─'),
        "bottom border: {last:?}"
    );
    let text = result
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!text.contains("```"), "fences must not render: {text}");
    assert!(text.contains("rust"), "language tag should render: {text}");
    assert!(text.contains("fn main() {}"), "{text}");
}

#[test]
fn markdown_lines_diff_fence_renders_as_diff() {
    // A ` ```diff ` fence is the opt-in signal: the interior is handed to
    // the diff renderer, and the fence lines themselves are consumed.
    let md = "```diff\ndiff --git a/file.txt b/file.txt\n--- a/file.txt\n+++ \
b/file.txt\n@@ -1 +1 @@\n-old\n+new\n```";
    let result = markdown_lines(md, 80);
    let text = result
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    // Side-by-side artifacts (width 80 ≥ MIN_SIDEBYSIDE_WIDTH 40).
    assert!(text.contains("+++ b/"), "{text}");
    assert!(text.contains('│'), "{text}");
    // No literal fence remains.
    assert!(!text.contains("```"), "fence must be consumed: {text}");
}

#[test]
fn markdown_lines_diff_fence_with_junk_falls_back_to_literal_fence() {
    // A ` ```diff ` tag around non-diff content must not render as a bogus
    // diff — the renderer falls back to the literal code block so the raw
    // text always stays visible (fail-closed).
    let md = "```diff\njust some words\n```";
    let result = markdown_lines(md, 80);
    let text = result
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    // The junk interior stays visible (in a code box tagged `diff`), but it
    // must never be dragged into the side-by-side diff renderer.
    assert!(
        text.contains("just some words"),
        "raw interior must stay visible: {text}"
    );
    // The junk renders as a table-style code box (the literal-fence fallback),
    // not a side-by-side diff: the box's rounded corner is present and no
    // `+++ b/` diff header appears.
    assert!(
        text.contains('╭'),
        "fallback should render the code box: {text}"
    );
    assert!(
        !text.contains("+++ b/"),
        "no diff artifacts expected: {text}"
    );
}

#[test]
fn markdown_lines_code_block_no_language() {
    let md = "```\nplain code\n```";
    let result = markdown_lines(md, 80);
    // No language tag → no label row and no blank row: top border, the code,
    // bottom border.
    assert!(result.len() >= 3, "rows: {result:#?}");
    let first = result[0].to_string();
    assert!(first.starts_with('╭'), "top border: {first:?}");
    // The code row is the box's first interior row: `│ plain code │`.
    assert!(
        result[1].to_string().contains("plain code"),
        "{:#?}",
        result[1]
    );
    let text = result
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!text.contains("```"), "fences must not render: {text}");
}

#[test]
fn code_box_copy_ranges_trim_chrome() {
    let (lines, _joins, chrome) = markdown_lines_joined("```rust\nfn main() {}\n```", 80);
    // Row 0 top border, 1 language tag, 2 blank padding, 3 code, 4 bottom
    // border (the fence interior's trailing newline is stripped, so there is no
    // trailing blank row).
    assert_eq!(lines.len(), 5, "rows: {lines:#?}");
    // Box width: the widest interior row ("fn main() {}", 12 columns) plus the
    // `│ ` / ` │` frame (4 columns).  Rows are viewport-bounded, so the width
    // fits `u16` (the chrome interval's column unit).
    let box_width = u16::try_from(lines[0].width()).expect("box width fits u16");
    assert_eq!(box_width, 16, "box hugs the code: {lines:#?}");
    // The border rows are pure chrome — every cell excluded, so a selection
    // over them yields nothing.
    assert_eq!(
        chrome[0].intervals(),
        &[(0, box_width)],
        "top border is chrome"
    );
    assert_eq!(
        chrome[4].intervals(),
        &[(0, box_width)],
        "bottom border is chrome"
    );
    // The language row keeps exactly the tag selectable: the leading `│ ` and
    // the trailing pad + ` │` are chrome ("rust" occupies columns 2..6).
    assert_eq!(chrome[1].intervals(), &[(0, 2), (6, box_width)]);
    // The blank padding row's `│` borders are chrome, but the interior spaces
    // stay non-chrome so the row still copies as a genuinely blank line.
    assert_eq!(chrome[2].intervals(), &[(0, 2), (14, box_width)]);
    // The code row keeps exactly the code selectable, frame and pad trimmed.
    assert_eq!(chrome[3].intervals(), &[(0, 2), (14, box_width)]);
}

#[test]
fn code_box_uses_table_corner_glyphs() {
    let (lines, _joins, chrome) = markdown_lines_joined("```x\nlet x = 1;\n```", 80);
    let top = lines[0].to_string();
    assert!(
        top.starts_with(TABLE_BORDERS.top_left),
        "top border must open with the table corner glyph: {top:?}"
    );
    assert!(
        top.ends_with(TABLE_BORDERS.top_right),
        "top border must close with the table corner glyph: {top:?}"
    );
    let bottom = lines.last().unwrap().to_string();
    assert!(
        bottom.starts_with(TABLE_BORDERS.bottom_left),
        "bottom border must open with the table corner glyph: {bottom:?}"
    );
    assert!(
        bottom.ends_with(TABLE_BORDERS.bottom_right),
        "bottom border must close with the table corner glyph: {bottom:?}"
    );
    // The border rows are pure chrome: nothing selectable, and no span paints a
    // background (the box has no fill).
    let border_width = u16::try_from(lines[0].width()).expect("border width fits u16");
    assert_eq!(chrome[0].intervals(), &[(0, border_width)]);
    assert_eq!(chrome[lines.len() - 1].intervals(), &[(0, border_width)]);
    for line in &lines {
        assert!(
            line.spans.iter().all(|s| s.style.bg.is_none()),
            "the code box must not paint a background: {line:#?}"
        );
    }
}

#[test]
fn code_box_rows_stay_uniform_when_wrapping() {
    // Regression: a wrapped code line whose first chunk exactly fills the code
    // area could keep a trailing separator space from the word-wrapper, ending
    // up one column wider than the box interior, so its right `│` jutted past
    // the frame.  Every row of a box — borders included — must be one width.
    let samples = [
        "aaaa bbbb cccc dddd eeee ffff gggg hhhh iiii jjjj kkkk llll mmmm",
        "The quick brown fox jumps over the lazy dog near the river bank today",
        "Always write inline comments around new code explaining how it works",
        "always_write_inline_comments_around_new_code_explaining_how_it_works",
    ];
    for code in samples {
        for width in 20u16..80 {
            let md = format!("```text\n{code}\n```");
            let (lines, _joins, _chrome) = markdown_lines_joined(&md, width);
            let w = lines[0].width();
            for (i, line) in lines.iter().enumerate() {
                assert_eq!(
                    line.width(),
                    w,
                    "width {width} row {i} is not the box width ({w}): {line:#?}"
                );
            }
        }
    }
}

#[test]
fn code_box_language_tag_is_bold() {
    let (lines, _joins, _chrome) = markdown_lines_joined("```rust\nlet x = 1;\n```", 80);
    // Row 1 is the language tag row.
    let label_row = &lines[1];
    assert!(
        label_row.spans.iter().any(|s| {
            s.content.as_ref() == "rust" && s.style.add_modifier.contains(Modifier::BOLD)
        }),
        "language tag should be bold: {label_row:#?}"
    );
}

#[test]
fn code_box_hugs_code_width() {
    // A single short line with no language tag: the box is the code (3 cols)
    // plus the `│ ` / ` │` frame (4) = 7, and every row spans exactly that
    // width.
    let (lines, _joins, _chrome) = markdown_lines_joined("```\nabc\n```", 80);
    // Top border, the code, bottom border.
    assert_eq!(lines.len(), 3, "no tag → no label/blank rows: {lines:#?}");
    for line in &lines {
        assert_eq!(line.width(), 7, "box should hug the code: {line:#?}");
    }
}

// ── code block wrapping ───────────────────────────────────────────────

#[test]
fn code_block_wraps_long_line() {
    let long = "x".repeat(200);
    let md = format!("```rust\n{long}\n```");
    let result = markdown_lines(&md, 40);
    // The code content should be wrapped. Each wrapped segment should be
    // at most 40 columns wide.
    for line in &result {
        let text = line.to_string();
        // Skip fence lines
        if text.starts_with("```") {
            continue;
        }
        assert!(
            line.width() <= 40,
            "wrapped code line width {} exceeds 40: {text:?}",
            line.width()
        );
    }
    // Count non-fence lines to verify wrapping actually happened.
    let content_line_count = result
        .iter()
        .filter(|l| !l.to_string().starts_with("```"))
        .count();
    assert!(
        content_line_count > 3,
        "long code line should wrap into {content_line_count} lines, expected > 3"
    );
}

#[test]
fn code_block_wrap_trailing_whitespace_stripped() {
    // A code line that *exactly* fills the box interior must not leave a
    // trailing whitespace character from the word-wrapper in a content span.
    let md = format!("```\n{}\n```", "a".repeat(30));
    let result = markdown_lines(&md, 30);
    // The box's frame glyphs and its whitespace padding are layout, not code;
    // skip them (frame glyph spans carry a glyph, padding spans are pure
    // whitespace) so the check sees only the code content spans.
    let is_frame = |s: &str| s.chars().any(|c| "│─╭╮╰╯".contains(c));
    for line in &result {
        for span in &line.spans {
            let content = span.content.as_ref();
            if is_frame(content) {
                continue;
            }
            if content.chars().any(|c| !c.is_whitespace()) {
                assert_eq!(
                    content.trim_end(),
                    content,
                    "code content span must not carry trailing whitespace: {content:?}"
                );
            }
        }
    }
}

#[test]
#[expect(clippy::assert_is_empty)] // this clippy version wants assert_ne!(v, [] as [...]) here — worse
fn code_block_no_wrap_when_fits() {
    let md = "```\nshort\n```";
    let result = markdown_lines(md, 80);
    assert!(!result.is_empty());
    // `result[0]` is the top border; the code row is next, framed as
    // `│ short │`.
    let code_line = result.get(1).expect("second line should be code");
    assert!(
        code_line.to_string().contains("short"),
        "code should not wrap when short: {code_line:?}"
    );
}

#[test]
fn code_block_indented_wrapping() {
    let long = "x".repeat(100);
    let md = format!("> ```\n> {long}\n> ```");
    let result = markdown_lines(&md, 40);
    // Each code content line in the blockquote should be ≤ 40 (indent 2 + "│ " prefix).
    for line in &result {
        let text = line.to_string();
        if text.starts_with(" ```") || text.starts_with("│ ```") || text.starts_with("│  ```") {
            continue;
        }
        assert!(
            line.width() <= 40,
            "indented code line width {} exceeds 40: {text:?}",
            line.width()
        );
    }
}

// ── Code box: copy classification edge cases ─────────────────────────

#[test]
fn code_box_interior_blank_line_is_blank_content() {
    // Regression: a blank line *inside* a fence renders as a box row whose only
    // non-chrome columns are the interior padding.  It must classify as *blank
    // content* (like the language-tag padding row), not pure chrome, so a copy
    // keeps the blank line instead of silently dropping it.
    let (lines, _joins, chrome) = markdown_lines_joined("```\na\n\nb\n```", 80);
    // rows: top border, "a", blank interior line, "b", bottom border.
    assert_eq!(lines.len(), 5, "rows: {lines:#?}");
    assert!(
        matches!(
            classify_row_content(&lines[2], &chrome[2]),
            RowContent::Blank
        ),
        "interior blank code line must be blank content: {:?}",
        chrome[2].intervals()
    );
    // The blank row's chrome is the frame only — the one-column interior is
    // left non-chrome so it reads as blank content.
    assert_eq!(chrome[2].intervals(), &[(0, 2), (3, 5)]);
}

#[test]
fn code_box_over_long_language_tag_is_capped_to_the_frame() {
    // A language tag wider than the available code area must be truncated to
    // the box interior, never pushing its row past the (width-capped) frame.
    let tag = "verylonglanguagetagname";
    let (lines, _joins, _chrome) = markdown_lines_joined(&format!("```{tag}\ncode\n```"), 20);
    for line in &lines {
        assert!(line.width() <= 20, "box row must fit the width: {line:#?}");
    }
    // Every row is the same width — the box still hugs its widest interior row.
    let w = lines[0].width();
    assert_eq!(w, 20, "box should use the full available width: {lines:#?}");
    assert!(
        lines.iter().all(|l| l.width() == w),
        "all box rows share one width: {lines:#?}"
    );
}

#[test]
fn chrome_covered_width_merges_overlapping_intervals() {
    // Overlapping intervals must not double-count a covered column.
    let mut overlap = LineChrome::default();
    overlap.push(0, 5);
    overlap.push(3, 8);
    assert_eq!(chrome_covered_width(&overlap), 8);

    // Touching intervals merge into one run.
    let mut touching = LineChrome::default();
    touching.push(0, 2);
    touching.push(2, 5);
    assert_eq!(chrome_covered_width(&touching), 5);

    // Empty / inverted intervals credit nothing.
    let mut degenerate = LineChrome::default();
    degenerate.push(4, 4);
    degenerate.push(7, 3);
    assert_eq!(chrome_covered_width(&degenerate), 0);
}
