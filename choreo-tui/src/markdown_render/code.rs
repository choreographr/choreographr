//! Fenced code-block rendering: syntect highlighting, the bordered code box,
//! and the ` ```diff ` opt-in that hands a fence interior to the diff renderer.

use super::{
    Color, GlobalLruCache, HighlightLines, Line, LineChrome, LineJoin, Modifier, Span, Style,
    TABLE_BORDERS, debug, display_width, grapheme_chunks, highlight_theme, syntax_set,
    to_ratatui_color, try_render_diff_content, wrap_styled_line_joined,
};

pub(crate) fn find_syntax<'a>(
    ss: &'a syntect::parsing::SyntaxSet,
    lang: &str,
) -> Option<&'a syntect::parsing::SyntaxReference> {
    ss.find_syntax_by_token(lang).or_else(move || match lang {
        "typescript" | "tsx" | "mts" | "cts" => ss.find_syntax_by_token("javascript"),
        "vue" | "svelte" => ss.find_syntax_by_token("html"),
        _ => None,
    })
}

pub(crate) fn highlight_code(language: Option<&str>, code: &str) -> Vec<Line<'static>> {
    static CACHE: GlobalLruCache<(String, String), Vec<Line<'static>>, 200> = GlobalLruCache::new();

    let key = (language.unwrap_or("").to_string(), code.to_string());

    CACHE.get_or_insert_with(&key, || {
        let ss = syntax_set();

        let syntax = language
            .and_then(|lang| find_syntax(ss, lang))
            .unwrap_or_else(|| ss.find_syntax_plain_text());

        let theme = highlight_theme();
        let mut highlighter = HighlightLines::new(syntax, theme);
        let mut result = Vec::with_capacity(code.len().max(1));

        for line in code.split('\n') {
            let Ok(ranges) = highlighter.highlight_line(line, ss) else {
                result.push(Line::from(Span::styled(line.to_string(), Style::default())));
                continue;
            };

            let spans: Vec<Span<'static>> = ranges
                .into_iter()
                .map(|(style, text)| {
                    Span::styled(
                        text.to_string(),
                        Style::default().fg(to_ratatui_color(style.foreground)),
                    )
                })
                .collect();

            result.push(Line::from(spans));
        }

        result
    })
}

/// Render a fenced code block as a **bordered box** styled exactly like the
/// markdown table frame: rounded corners (`╭ ╮ ╰ ╯`), `─` horizontals, and `│`
/// verticals, all in the default terminal style (no background colour).  The
/// box hugs the code — `inner = max(widest code row, language tag)` — with one
/// column of padding inside each `│`; the literal triple-backtick fence markers
/// are never emitted.  When a language tag is given it is shown bold on the
/// box's first interior row, followed by one blank padding row; without a tag
/// the code starts on the first interior row.
fn render_code_box(
    language: Option<&str>,
    code: &str,
    lines: &mut Vec<Line<'static>>,
    joins: &mut Vec<LineJoin>,
    chrome: &mut Vec<LineChrome>,
    indent: usize,
    width: usize,
) {
    // Drop the single trailing newline the fence interior normally carries
    // (the `\n` before the closing fence).  Left in, it splits into a final
    // empty line that would render as a spurious blank interior row above the
    // bottom border.  A deliberately blank last line (two newlines) survives,
    // since only one suffix is stripped.
    let code = code.strip_suffix('\n').unwrap_or(code);
    // The box must fit the block's available width.  Its frame costs four
    // columns — `│ ` on the left and ` │` on the right — so the interior code
    // area (and hence the box) is capped at `block_avail - 4`.
    let block_avail = width.saturating_sub(indent).max(2);
    let code_avail = block_avail.saturating_sub(4);

    // Highlight, then wrap every row that exceeds the code area so the box
    // never overflows.  Each row keeps the [`LineJoin`] the wrapper recorded so
    // a wrapped source line still rejoins when copied.
    let mut code_rows: Vec<(Line<'static>, LineJoin)> = Vec::new();
    for hl_line in highlight_code(language, code) {
        if hl_line.width() > code_avail {
            let mut wrapped: Vec<Line<'static>> = Vec::new();
            let mut wrapped_joins: Vec<LineJoin> = Vec::new();
            wrap_styled_line_joined(&hl_line, code_avail, &mut wrapped, &mut wrapped_joins);
            for (wi, wl) in wrapped.into_iter().enumerate() {
                // `wrapped_joins` is produced in lockstep with `wrapped`, so
                // `wi` is in bounds; a hard break is the safe fallback.
                let join = wrapped_joins.get(wi).copied().unwrap_or(LineJoin::Break);
                code_rows.push((wl, join));
            }
        } else {
            code_rows.push((hl_line, LineJoin::Break));
        }
    }

    // The box hugs the widest interior row (code or language tag) and is capped
    // at `code_avail` so the whole frame fits the block's available width.
    let code_max = code_rows.iter().map(|(l, _)| l.width()).max().unwrap_or(0);
    let label = language.filter(|tag| !tag.is_empty());
    let label_width = label.map_or(0, display_width);
    let inner = code_max.max(label_width).min(code_avail);
    let box_width = inner + 4;

    // The tag is drawn inside the frame, so cap it to the interior width: an
    // absurdly long language tag must not push its row past the (already
    // width-capped) frame.  A normal tag is shorter than `inner` and passes
    // through untouched; only the over-long case is truncated at a grapheme
    // boundary, and only an empty interior drops it.
    let tag_text: String = match label {
        Some(tag) if label_width > inner && inner > 0 => grapheme_chunks(tag, inner, 1)
            .into_iter()
            .next()
            .unwrap_or_default(),
        Some(_) if inner == 0 => String::new(),
        Some(tag) => tag.to_string(),
        None => String::new(),
    };

    // The rounded corners come from the table renderer's shared frame glyphs, so
    // the box and the tables draw one identical frame.
    let (top_left, top_right) = (TABLE_BORDERS.top_left, TABLE_BORDERS.top_right);
    let (bottom_left, bottom_right) = (TABLE_BORDERS.bottom_left, TABLE_BORDERS.bottom_right);

    // Push one physical row: the block's outer indent (always 0 today) sits to
    // the left of the box, outside its frame.  `row_chrome` records the row's
    // non-selectable intervals in the row's own column space (the indent
    // included), for the assembly layer to translate by its prefix.
    let mut emit = |mut row: Vec<Span<'static>>, join: LineJoin, row_chrome: LineChrome| {
        if indent > 0 {
            row.insert(0, Span::styled(" ".repeat(indent), Style::default()));
        }
        lines.push(Line::from(row));
        joins.push(join);
        chrome.push(row_chrome);
    };

    // Chrome for a *border* row: the whole rule is non-selectable, so a
    // selection over it yields nothing.
    let border_chrome = || {
        let mut c = LineChrome::default();
        c.push(indent, indent + box_width);
        c
    };

    // ── Top border: `╭` + `─`×(inner + 2) + `╮` ──
    emit(
        vec![Span::styled(
            format!("{top_left}{}{top_right}", "─".repeat(inner + 2)),
            Style::default(),
        )],
        LineJoin::Break,
        border_chrome(),
    );

    // ── Language tag + one blank padding row (only when a tag is given) ──
    if label.is_some() {
        let tag_width = display_width(&tag_text);
        let mut row = vec![
            Span::styled("│ ".to_string(), Style::default()),
            // The tag is a label, not content: bold distinguishes it from the
            // code without a colour that could clash with the syntax colours.
            Span::styled(
                tag_text,
                Style::default()
                    .fg(Color::Gray)
                    .add_modifier(Modifier::BOLD),
            ),
        ];
        if inner > tag_width {
            row.push(Span::styled(
                " ".repeat(inner - tag_width),
                Style::default(),
            ));
        }
        row.push(Span::styled(" │".to_string(), Style::default()));
        // The `│ ` / ` │` frame-plus-padding runs are chrome; the trailing pad
        // after the tag is chrome too, so the label copies as the bare tag.
        let mut c = LineChrome::default();
        c.push(indent, indent + 2);
        c.push(indent + 2 + tag_width, indent + box_width);
        emit(row, LineJoin::Break, c);

        // The blank padding row: its borders are chrome, but the interior is
        // spaces, so the assembly classifies it as blank content and the
        // selection copies a genuinely blank line (never the frame).
        emit(
            vec![
                Span::styled("│ ".to_string(), Style::default()),
                Span::styled(" ".repeat(inner), Style::default()),
                Span::styled(" │".to_string(), Style::default()),
            ],
            LineJoin::Break,
            {
                let mut c = LineChrome::default();
                c.push(indent, indent + 2);
                c.push(indent + 2 + inner, indent + box_width);
                c
            },
        );
    }

    // ── Code rows: `│ ` + code padded to the inner width + ` │` ──
    for (row_line, join) in code_rows {
        let mut spans = row_line.spans;
        // The word-wrapper can leave the separator space that triggered a
        // wrap on the row it broke off, making that row one column wider than
        // the code area.  Drop trailing whitespace-only spans so the row fits
        // the interior exactly — without this the row's right `│` would jut one
        // column past the frame.  The row's `Space` copy-join re-inserts that
        // separator at the seam, so nothing is lost from a copy.
        let mut content_width: usize = spans.iter().map(Span::width).sum();
        if content_width > inner {
            while spans.last().is_some_and(|s| s.content.trim().is_empty()) {
                spans.pop();
            }
            content_width = spans.iter().map(Span::width).sum();
        }
        let mut row = Vec::with_capacity(spans.len() + 3);
        row.push(Span::styled("│ ".to_string(), Style::default()));
        // The code keeps its syntect foreground colours; the box adds no
        // background of its own.
        row.extend(spans);
        if inner > content_width {
            row.push(Span::styled(
                " ".repeat(inner - content_width),
                Style::default(),
            ));
        }
        row.push(Span::styled(" │".to_string(), Style::default()));
        // The `│ ` / ` │` runs are chrome, and so is the right-hand fill that
        // pads the code out to the box width — the code text between them (and
        // only it) stays selectable, so a copy never grabs the frame or the
        // pad.  An *empty* code line is the exception: its interior is all
        // padding, so marking that padding as chrome would cover the whole row
        // and classify it as pure chrome, dropping an interior blank line from
        // a copy.  Leaving the padding non-chrome instead makes the row *blank
        // content* (mirroring the language-tag padding row), so the blank line
        // is copied as a genuinely blank line.
        let mut c = LineChrome::default();
        c.push(indent, indent + 2);
        if content_width == 0 {
            c.push(indent + 2 + inner, indent + box_width);
        } else {
            c.push(indent + 2 + content_width, indent + box_width);
        }
        emit(row, join, c);
    }

    // ── Bottom border: `╰` + `─`×(inner + 2) + `╯` ──
    emit(
        vec![Span::styled(
            format!("{bottom_left}{}{bottom_right}", "─".repeat(inner + 2)),
            Style::default(),
        )],
        LineJoin::Break,
        border_chrome(),
    );
}

/// Render a fenced code block into `lines`/`joins`/`chrome`.
///
/// A ` ```diff ` fence is an explicit opt-in: the emitting tool chose the
/// markdown `diff` language tag, so the fence interior is handed to the diff
/// renderer instead of the generic code block.  The renderer is fed *fence
/// interiors only* — the raw `--- ` / `diff --git` auto-detection sniffs no
/// longer run against whole tool outputs, which is what used to misparse
/// `pdf_to_markdown`'s "--- UNTRUSTED …" delimiter as a diff path header.  If
/// the interior does not parse as a diff (junk under the tag) we fall through to
/// the literal-fence code path so the raw text always stays visible.
pub(crate) fn render_code_block(
    language: Option<&str>,
    code: &str,
    lines: &mut Vec<Line<'static>>,
    joins: &mut Vec<LineJoin>,
    chrome: &mut Vec<LineChrome>,
    indent: usize,
    width: usize,
) {
    if language == Some("diff") {
        let diff_width = u16::try_from(width.saturating_sub(indent)).unwrap_or(u16::MAX);
        // Log the accept/fallback *decision* only — the fence interior itself
        // passes through the diff renderer and can contain arbitrary tool
        // output, so it is never logged here.
        debug!(fence = "diff", "rendering fenced diff interior");
        if let Some(diff_lines) = try_render_diff_content(code, diff_width) {
            for line in diff_lines {
                // Mirror the generic code path's indent handling so a fenced
                // diff inside a blockquote/list stays inside its container and
                // never overflows the width.
                if indent > 0 {
                    let mut spans = vec![Span::styled(" ".repeat(indent), Style::default())];
                    // `line` is consumed right after, so move its span Vec
                    // instead of cloning every span of every diff row.
                    spans.extend(line.spans);
                    lines.push(Line::from(spans));
                } else {
                    lines.push(line);
                }
                // Every diff row is a distinct source line — never reflowed and
                // never space-joined — so the copy reproduces the diff verbatim.
                joins.push(LineJoin::Break);
                chrome.push(LineChrome::default());
            }
            return;
        }
        debug!(
            fence = "diff",
            "fence interior not a parseable diff; falling back to literal code block"
        );
    }

    render_code_box(language, code, lines, joins, chrome, indent, width);
}
