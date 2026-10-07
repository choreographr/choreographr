use super::super::*;

// ── copy-chrome negatives (chrome is emitted, never inferred) ─────────

#[test]
fn plain_bar_text_has_no_chrome() {
    // Chrome is emitted *explicitly* by the block-quote bar and the code box,
    // so a row whose text merely contains `│ ` (a literal pipe in prose) is
    // never mistaken for chrome — it carries none and copies in full.  This is
    // the guard against the old string/colour re-scan.
    let (lines, _joins, chrome) = markdown_lines_joined("a \u{2502} b", 80);
    assert_eq!(lines.len(), chrome.len());
    assert!(
        chrome.iter().all(LineChrome::is_empty),
        "prose containing a bar must have no chrome: {chrome:#?}"
    );
}

#[test]
fn diff_gutter_has_no_chrome() {
    // A ` ```diff ` fence renders side-by-side rows with their own `│` gutter.
    // That gutter is diff *content*, not block-quote/box chrome, so every diff
    // row carries empty chrome and a copy reproduces it verbatim.
    let md = "```diff\ndiff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1 +1 @@\n-old\n+new\n```";
    let (lines, _joins, chrome) = markdown_lines_joined(md, 80);
    assert!(
        lines.iter().any(|l| l.to_string().contains('\u{2502}')),
        "side-by-side diff must have a `│` gutter: {lines:#?}"
    );
    assert!(
        chrome.iter().all(LineChrome::is_empty),
        "the diff gutter must not be chrome: {chrome:#?}"
    );
}

#[test]
fn non_quote_non_box_rows_have_no_chrome() {
    // Chrome is emitted only by the block-quote bar, the code box, and data
    // tables.  Every other markdown construct — headings, paragraphs (with
    // emphasis, inline code, and a literal `│`), lists, rules — must carry no
    // chrome, so its content range is exactly what it was before chrome
    // existed.  (Table rows are covered by `table_rows_carry_border_chrome`.)
    let md = "# Heading\n\nA paragraph with `code`, **bold**, a literal \u{2502} bar, and a\nsecond line.\n\n\
              - one\n- two\n\n\
              ---\n";
    let (lines, _joins, chrome) = markdown_lines_joined(md, 60);
    assert_eq!(lines.len(), chrome.len(), "chrome must align with lines");
    for (i, c) in chrome.iter().enumerate() {
        assert!(c.is_empty(), "row {i} unexpectedly has chrome: {lines:#?}");
    }
}

#[test]
fn table_rows_carry_border_chrome() {
    // A data table records each `│` border column (and the whole of each
    // frame/separator rule) as chrome, so the selection keeps the cell text and
    // drops the frame.  The gaps between the border chrome runs are the cells.
    let md = "| a | b |\n|---|---|\n| 1 | 2 |";
    let (lines, _joins, chrome) = markdown_lines_joined(md, 60);
    assert_eq!(lines.len(), chrome.len());
    assert!(
        chrome.iter().any(|c| !c.is_empty()),
        "table rows must carry border chrome: {chrome:#?}"
    );
}

#[test]
fn wrapped_table_cell_joins_with_space() {
    // A cell that wraps records a space-join on its continuation row, so a
    // normal selection over the table rejoins the cell to its original text.
    let md = "| Key | Value |\n|-----|-------|\n| k | alpha beta gamma delta epsilon |";
    let (_lines, joins, _chrome) = markdown_lines_joined(md, 30);
    assert!(
        joins.contains(&LineJoin::Space),
        "a wrapped table cell must record a space-join: {joins:?}"
    );
}

#[test]
fn markdown_blockquote_bar_is_excluded_from_copy_range() {
    let (body, body_joins, body_chrome) = markdown_lines_joined("> hello world", 40);
    let body_width = body[0].width(); // "│ hello world" = 13
    let (_lines, _rows, ranges, _joins, chrome) =
        add_margin_lines(body, body_joins, body_chrome, 40, Color::Green, None);
    // Row 2 is the single content row (separator, padding, content, …).  The
    // base range spans the whole content interval at the gutter offset (5); the
    // two-column bar is recorded as chrome shifted into that same column space
    // (5..7), so the selectable result still starts after the bar.
    assert_eq!(ranges[2], Some((5, 5 + body_width)));
    assert_eq!(chrome[2].intervals(), &[(5, 7)]);
}

#[test]
fn render_turn_lines_quote_bar_is_not_copyable() {
    let turn = Turn {
        created_at: choreo_proto::TimestampMs::now(),
        undone: false,
        error: None,
        user_text: Some("> quoted line".into()),
        assistant_text: None,
        assistant_reasoning: None,
        tool_calls: vec![],
        token_usage: None,
        tool_results: vec![],
        displayed_images: vec![],
        reasoning_artifact: None,
        reasoning_producer: None,
    };
    let rendered = render_turn_lines(&turn, 40, 40, false, &[]);
    let row = rendered
        .lines
        .iter()
        .position(|l| l.to_string().contains("│ quoted line"))
        .expect("quote row");
    // The `┃` gutter puts message content at column 5; the base range spans the
    // whole content, and the two-column quote bar is recorded as chrome at
    // (5, 7), so a drag-copy skips the bar.
    let base = rendered.content_ranges[row].expect("content row");
    assert_eq!(base.0, 5);
    assert_eq!(rendered.chrome_ranges[row].intervals(), &[(5, 7)]);
}

#[test]
fn render_turn_lines_tool_markdown_quote_bar_is_not_copyable() {
    let turn = Turn {
        created_at: choreo_proto::TimestampMs::now(),
        undone: false,
        error: None,
        user_text: None,
        assistant_text: None,
        assistant_reasoning: None,
        tool_calls: vec![],
        token_usage: None,
        tool_results: vec![choreo_proto::ToolResultRecord {
            call_id: "c".into(),
            name: "pdf_to_markdown".into(),
            content: "> quoted".into(),
            is_error: false,
            invocation_description: String::new(),
            image: None,
        }],
        displayed_images: vec![],
        reasoning_artifact: None,
        reasoning_producer: None,
    };
    // pdf_to_markdown is not a quiet tool, so the result is expanded by default.
    let rendered = render_turn_lines(&turn, 80, 80, false, &[false]);
    let row = rendered
        .lines
        .iter()
        .position(|l| l.to_string().contains("│ quoted"))
        .expect("quote body row");
    // Tool bodies are unboxed: the base range starts at column 0, and the
    // leading two-column bar is recorded as chrome at (0, 2).
    let base = rendered.content_ranges[row].expect("content row");
    assert_eq!(base.0, 0);
    assert_eq!(rendered.chrome_ranges[row].intervals(), &[(0, 2)]);
}

// ── LineJoin copy metadata ────────────────────────────────────────────

#[test]
fn wrapped_paragraph_joins_with_space() {
    // A paragraph that wraps onto three rows records Break for its first
    // row and Space for each continuation — the copy re-inserts the
    // separating space the reflow consumed.
    let text = "the quick brown fox jumps over the lazy dog and runs far away";
    let (lines, joins, _chrome) = markdown_lines_joined(text, 21);
    assert_eq!(lines.len(), 3, "paragraph must wrap to three rows");
    assert_eq!(
        joins,
        vec![LineJoin::Break, LineJoin::Space, LineJoin::Space]
    );
    // Reassembling the rows with the recorded joins reproduces the input.
    let mut out = String::new();
    for (i, line) in lines.iter().enumerate() {
        if i > 0 && joins[i] == LineJoin::Space {
            out.push(' ');
        }
        out.push_str(&line.to_string());
    }
    assert_eq!(out, text);
}

#[test]
fn paragraphs_break_between_blocks() {
    let md = "one paragraph here\n\nanother paragraph there";
    let (lines, joins, _chrome) = markdown_lines_joined(md, 80);
    assert_eq!(lines.len(), 3, "two paragraphs plus a blank spacer");
    assert_eq!(
        joins,
        vec![LineJoin::Break, LineJoin::Break, LineJoin::Break]
    );
}

#[test]
fn hard_split_word_joins_directly() {
    // A single word wider than the line is hard-split by grapheme; the
    // copy joins the pieces directly (no space exists in the original).
    let word = "supercalifragilisticexpialidocious";
    let (lines, joins, _chrome) = markdown_lines_joined(word, 10);
    assert!(lines.len() >= 3, "word must split across rows");
    assert_eq!(joins[0], LineJoin::Break, "first row is fresh");
    assert!(
        joins.iter().skip(1).all(|&j| j == LineJoin::Join),
        "every continuation is a mid-word split: {joins:?}"
    );
    let rejoin: String = lines.iter().map(ToString::to_string).collect();
    assert_eq!(rejoin, word, "direct concatenation reproduces the word");
}

#[test]
fn plain_text_wrap_joins_directly_and_preserves_whitespace() {
    // `wrap_plain_line` keeps the whitespace run on the previous row, so
    // the copy concatenates the rows directly — no invented space, and
    // the input is reproduced byte-for-byte (including internal runs).
    let text = "alpha   beta gamma  delta epsilon zeta";
    let (lines, joins) = plain_text_lines_joined(text, 10);
    assert!(lines.len() > 1, "line must wrap");
    assert_eq!(joins[0], LineJoin::Break);
    assert!(
        joins.iter().skip(1).all(|&j| j == LineJoin::Join),
        "every continuation is a direct join: {joins:?}"
    );
    let rejoin: String = lines.iter().map(ToString::to_string).collect();
    assert_eq!(rejoin, text, "direct concatenation reproduces the input");
}

#[test]
fn ansi_word_wrap_joins_with_space() {
    // ANSI-colored text wraps via `wrap_styled_line` (word-boundary
    // breaks consume the space), so continuations join with Space.
    let (lines, joins) = ansi_lines_joined("aaaa bbbb cccc dddd eeee", 10);
    assert_eq!(lines.len(), 3);
    assert_eq!(
        joins,
        vec![LineJoin::Break, LineJoin::Space, LineJoin::Space]
    );
}

#[test]
fn code_block_lines_break_but_wrapped_line_joins() {
    // Each source line of a code block is a fresh line; a wrapped
    // over-long source line records its own continuation joins.
    let md = "```text\nshort line\nverylongwordthatexceedsthewidth\n```";
    let (lines, joins, _chrome) = markdown_lines_joined(md, 20);
    // Top border | text | (blank) | short line | verylongwordth… (wrap row 1)
    // | …edsthewidth (wrap row 2) | bottom border.  The fence interior's trailing
    // newline is stripped, so there is no trailing blank code row.
    assert_eq!(lines.len(), 7);
    assert_eq!(
        joins,
        vec![
            LineJoin::Break, // top border
            LineJoin::Break, // language tag (text)
            LineJoin::Break, // blank padding row
            LineJoin::Break, // short line
            LineJoin::Break, // verylongwordth… (row 1 of the wrap)
            LineJoin::Join,  // …edsthewidth (hard split continuation)
            LineJoin::Break, // bottom border
        ]
    );
}

#[test]
fn render_turn_lines_joins_stay_aligned() {
    // Every produced row carries a join, and the copy metadata on the
    // assistant block rows matches the row count (the selection
    // machinery relies on the alignment).
    let turn = Turn {
        created_at: choreo_proto::TimestampMs::now(),
        undone: false,
        error: None,
        user_text: None,
        assistant_text: Some("a paragraph that is long enough to wrap across several rows".into()),
        assistant_reasoning: None,
        tool_calls: vec![],
        token_usage: None,
        tool_results: vec![],
        displayed_images: vec![],
        reasoning_artifact: None,
        reasoning_producer: None,
    };
    let rendered = render_turn_lines(&turn, 20, 24, false, &[]);
    assert_eq!(rendered.lines.len(), rendered.joins.len());
    assert_eq!(rendered.lines.len(), rendered.content_ranges.len());
    // The box chrome rows are fresh lines; at least one content row is a
    // wrapped continuation.
    assert!(rendered.joins.contains(&LineJoin::Space));
    assert!(rendered.joins.contains(&LineJoin::Break));
}
