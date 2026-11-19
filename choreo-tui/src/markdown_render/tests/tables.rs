use super::super::*;

// ── Cell content is styled by the shared inline renderer ──────────────
//
// A table cell goes through `inlines_to_lines`, the same renderer prose uses,
// so every inline style survives into the cell.  A row's emphasis belongs to its
// cells, not to the assembled line: the frame's glyphs and padding stay plain.

/// The span whose trimmed text is exactly `text`, or `None`.
fn span_with_text<'a>(line: &'a Line<'static>, text: &str) -> Option<&'a Span<'static>> {
    line.spans.iter().find(|span| span.content.trim() == text)
}

fn is_bold(span: &Span<'static>) -> bool {
    span.style.add_modifier.contains(Modifier::BOLD)
}

#[test]
fn table_cells_pretty_print_math() {
    let result = markdown_lines("| A | B |\n|---|---|\n| $x_i$ | $\\alpha$ |", 80);
    let rendered: Vec<String> = result.iter().map(ToString::to_string).collect();
    let rendered = rendered.join("\n");
    assert!(
        rendered.contains("xᵢ"),
        "inline math subscript in table cell: {rendered}"
    );
    assert!(rendered.contains("α"), "greek in table cell: {rendered}");
}

#[test]
fn table_body_cell_strong_renders_bold() {
    let lines = markdown_lines("| A | B |\n|---|---|\n| **pinned** | plain |", 40);
    let body = lines
        .iter()
        .find(|line| line.to_string().contains("pinned"))
        .expect("body row present");
    let bold = span_with_text(body, "pinned").expect("cell text renders");
    assert!(is_bold(bold), "a bold cell must render bold: {body:?}");
    let plain = span_with_text(body, "plain").expect("plain cell text renders");
    assert!(
        !is_bold(plain),
        "an unmarked cell must stay plain: {body:?}"
    );
}

#[test]
fn table_cell_emphasis_stacks_onto_the_header_emphasis() {
    // The header row's baseline emphasis is stacked (not replaced) by the
    // cell's own markup, so a bold-italic header cell is both.
    let lines = markdown_lines("| *both* | B |\n|---|---|\n| 1 | 2 |", 40);
    let header = lines
        .iter()
        .find(|line| line.to_string().contains("both"))
        .expect("header row present");
    let span = span_with_text(header, "both").expect("header cell text renders");
    assert!(
        span.style.add_modifier.contains(Modifier::BOLD)
            && span.style.add_modifier.contains(Modifier::ITALIC),
        "*both* in a header cell is bold and italic: {header:?}"
    );
}

#[test]
fn table_frame_spans_stay_plain_under_a_bold_header_row() {
    // The emphasis is handed to the inline renderer per cell, so it reaches the
    // cell text and nothing else: no `│` glyph, no padding space, and no indent
    // may inherit it.
    let lines = markdown_lines("  | A | B |\n  |---|---|\n  | 1 | 2 |", 40);
    let mut cell_text_seen = false;
    for line in &lines {
        for span in &line.spans {
            let content = span.content.as_ref();
            if content.contains(['│', '─']) || content.trim().is_empty() {
                assert!(
                    !is_bold(span),
                    "frame/padding span must not be bold: {content:?} in {line:?}"
                );
            } else if content.trim() == "A" {
                cell_text_seen = true;
                assert!(is_bold(span), "header cell text is bold: {line:?}");
            }
        }
    }
    assert!(cell_text_seen, "the header row renders its cell text");
}

#[test]
fn styled_wrapped_and_unbreakable_cells_keep_the_frame_square() {
    // Every line of a table renders to the same display width, so the `│`
    // columns stay aligned through a bold cell, a wrapped cell, inline code, and
    // a word too long for its column.
    let md = "| **Key** | *Value* |\n|---|---|\n| **alpha beta gamma delta** | `code` supercalifragilisticexpialidocious |";
    let lines = markdown_lines(md, 34);
    let widths: Vec<usize> = lines.iter().map(Line::width).collect();
    assert!(
        widths.windows(2).all(|pair| pair[0] == pair[1]),
        "every table line shares one width: {widths:?} in {:?}",
        lines.iter().map(ToString::to_string).collect::<Vec<_>>()
    );
}

#[test]
fn cell_measure_width_is_the_width_of_the_emitted_text() {
    // Column sizing renders the cell (at the widest a column could be) and trims
    // the result, so the measured width *is* the emitted text's width: a link,
    // a code span, and nested emphasis cannot be sized for one form and printed
    // as another.
    let inlines = vec![
        MarkdownInline::Strong(vec![MarkdownInline::Text("bold".to_string())]),
        MarkdownInline::Text(", ".to_string()),
        MarkdownInline::Emphasis(vec![MarkdownInline::Text("soft".to_string())]),
        MarkdownInline::Text(" and ".to_string()),
        MarkdownInline::Code("code()".to_string()),
        MarkdownInline::Text(" then ".to_string()),
        MarkdownInline::Link {
            content: vec![MarkdownInline::Text("site".to_string())],
            destination: "https://example.com".to_string(),
        },
    ];
    let measured = cell_measure_width(&inlines, 200);
    let emitted: String = render_cell(&inlines, 200, Modifier::empty())
        .iter()
        .map(|(spans, _join)| {
            spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join(" ");
    assert_eq!(
        measured,
        display_width(emitted.trim()),
        "measured width matches the emitted text: {emitted:?}"
    );
    assert!(
        emitted.contains("https://example.com"),
        "the link's destination is emitted, not just measured: {emitted:?}"
    );
}

#[test]
fn cell_with_display_math_and_a_link_stays_inside_the_frame() {
    // The two shapes whose measurement is not simply the source text: display
    // math is centred by the renderer, and a link prints its destination.  Both
    // still have to sit inside their column, or the frame desyncs.
    let md = "| A | B |\n|---|---|\n| $$x^2$$ | [site](https://example.com/a/long/path) |";
    let lines = markdown_lines(md, 36);
    let widths: Vec<usize> = lines.iter().map(Line::width).collect();
    assert!(
        widths.windows(2).all(|pair| pair[0] == pair[1]),
        "every table line shares one width: {widths:?} in {:?}",
        lines.iter().map(ToString::to_string).collect::<Vec<_>>()
    );
}

#[test]
fn cell_measure_width_trims_renderer_padding() {
    // Display math is centred by the renderer, which pads it with leading
    // spaces. That padding is not content, so the measured width is the
    // equation's own width — the allocation-free trim path and the emitted text
    // must agree on where the content ends.
    let inlines = vec![MarkdownInline::DisplayMath("x^2".to_string())];
    let measured = cell_measure_width(&inlines, 40);
    let emitted: String = render_cell(&inlines, 40, Modifier::empty())
        .iter()
        .map(|(spans, _join)| {
            spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(
        measured,
        display_width(emitted.trim()),
        "measured width ignores the centred-math padding: {emitted:?}"
    );
    assert!(
        measured < 40,
        "the measurement is the equation, not the full width: {measured}"
    );
}

#[test]
fn trimmed_line_width_matches_trimmed_display_width() {
    // The measurement trims renderer padding off each drawn line without
    // building an intermediate `String`. Prove the allocation-free path agrees
    // with `display_width(text.trim())` across leading/trailing spaces, an
    // all-whitespace line, a wide glyph, and a combining mark — so it cannot
    // drift from the semantics it replaces.
    let cases: [&[&str]; 4] = [
        &["  x²  "],
        &["   ", ""],
        &["", "héllo", " "],
        &["Ａ\u{0301} "],
    ];
    for parts in cases {
        let spans: Vec<Span<'static>> = parts
            .iter()
            .map(|part| Span::styled((*part).to_string(), Style::default()))
            .collect();
        let text = parts.concat();
        assert_eq!(
            trimmed_line_width(&spans),
            display_width(text.trim()),
            "trimmed width of {text:?}"
        );
    }
}

#[test]
fn a_link_wider_than_its_column_hard_splits_inside_the_cell() {
    // A destination longer than its column is hard-split mid-word inside the
    // cell, exactly as an over-wide token is split in prose: the frame stays
    // square, and the split is recorded as a per-cell `Join` so the copy
    // rejoins the URL without inventing a space.
    let md = "| Link |\n|---|\n| [site](https://example.com/a/very/long/path/that/does/not/fit) |";
    let (lines, _joins, chrome) = markdown_lines_joined(md, 30);
    let widths: Vec<usize> = lines.iter().map(Line::width).collect();
    assert!(
        widths.windows(2).all(|pair| pair[0] == pair[1]),
        "every table line shares one width: {widths:?} in {:?}",
        lines.iter().map(ToString::to_string).collect::<Vec<_>>()
    );
    assert!(
        chrome
            .iter()
            .any(|c| c.cell_joins().contains(&LineJoin::Join)),
        "the split URL records a per-cell mid-word join: {chrome:#?}"
    );
}

#[test]
fn table_body_row_without_markup_is_not_bold() {
    // The complement of the header rule: a plain body cell is plain, so the
    // frame's only emphasis is the header row's own.
    let lines = markdown_lines("| A | B |\n|---|---|\n| 1 | 2 |", 40);
    let body = lines
        .iter()
        .find(|line| line.to_string().contains('1'))
        .expect("body row present");
    assert!(
        !body.spans.iter().any(is_bold),
        "an unmarked body row is not bold: {body:?}"
    );
}

#[test]
fn table_uses_rounded_corners_and_a_plain_header_rule() {
    // nushell-style frame: rounded OUTER corners, square T-junctions, and a
    // uniform header rule — the GFM delimiter row's alignment colons are
    // expressed by cell padding, never echoed into the rendered rule.
    let md = "| Name | Role | Years |\n|:--|:--:|--:|\n| Ada Lovelace | Mathematician | 1842 |";
    let lines = markdown_lines(md, 60);
    let joined = lines
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");

    // Outer corners are rounded…
    for corner in ['╭', '╮', '╰', '╯'] {
        assert!(joined.contains(corner), "rounded corner {corner}: {joined}");
    }
    // …no square corner survives anywhere…
    for corner in ['┌', '┐', '└', '┘'] {
        assert!(!joined.contains(corner), "square corner {corner}: {joined}");
    }
    // …and the T-junctions stay square (only the frame's corners round).
    assert!(joined.contains('┬'), "top junction: {joined}");
    assert!(joined.contains('╯'), "bottom junction: {joined}");

    // The header rule is uniform: none of the source delimiter row's colons
    // may appear (the fixed cell text below contains no colon of its own).
    assert!(!joined.contains(':'), "no alignment colons: {joined}");

    // The header row — and only it — draws its cell text bold.
    let header = lines
        .iter()
        .find(|line| line.to_string().contains("Name"))
        .expect("header row present");
    let header_cell = span_with_text(header, "Name").expect("header cell text renders");
    assert!(is_bold(header_cell), "header cell text is bold: {header:?}");
    let body = lines
        .iter()
        .find(|line| line.to_string().contains("Ada Lovelace"))
        .expect("body row present");
    assert!(
        !body.spans.iter().any(is_bold),
        "body row must not be bold: {body:?}"
    );
}
