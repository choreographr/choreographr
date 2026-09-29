//! Block-level markdown rendering: paragraphs, headings, code blocks, lists,
//! block quotes, tables, and rules into styled lines.

use super::{
    CODE_BG, CODE_PANEL_BOTTOM, CODE_PANEL_PAD, CODE_PANEL_TOP, Color, GlobalLruCache,
    HighlightLines, Line, LineJoin, MarkdownBlock, Modifier, QUOTE_BAR, QUOTE_BAR_COLOR, Span,
    Style, debug, display_width, ensure_blank_line_joined, heading_prefix, highlight_theme,
    inlines_to_lines, pad_marker, render_table_lines, syntax_set, to_ratatui_color,
    try_render_diff_content, wrap_styled_line_joined,
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

/// Render a fenced code block as a "panel": a solid [`CODE_BG`] rectangle that
/// hugs the code, with one column of padding on the left and right and a
/// half-row of padding at the top and bottom (drawn with half-block glyphs so
/// the panel colour has the same one-column thickness on all four sides).  The
/// literal triple-backtick fence markers are never emitted.  When a language
/// tag is given it is shown on the panel's first interior row, followed by one
/// blank padding row; without a tag the code starts on the first interior row.
fn render_code_panel(
    language: Option<&str>,
    code: &str,
    lines: &mut Vec<Line<'static>>,
    joins: &mut Vec<LineJoin>,
    indent: usize,
    width: usize,
) {
    // The whole panel must fit the block's available width; the code area is
    // the panel minus the 1-column pad on each side.
    let panel_avail = width.saturating_sub(indent).max(2);
    let code_avail = panel_avail.saturating_sub(2);

    // Highlight, then wrap every row that exceeds the code area so the panel
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

    // The panel hugs the widest row (code or language tag), plus one column of
    // padding each side, capped at the available width.
    let code_max = code_rows.iter().map(|(l, _)| l.width()).max().unwrap_or(0);
    let label = language.filter(|tag| !tag.is_empty());
    let label_width = label.map_or(0, display_width);
    let panel_width = (code_max.max(label_width) + 2).min(panel_avail);

    // A run of padding/fill: spaces tagged with the reserved foreground (never
    // seen, since spaces are blank) and the panel background, so the copy
    // machinery recognises it as chrome (see `copyable_columns`).
    let pad = |n: usize| {
        Span::styled(
            " ".repeat(n),
            Style::default().fg(CODE_PANEL_PAD).bg(CODE_BG),
        )
    };
    // Push one physical row: the block's outer indent (always 0 today) sits to
    // the left of the panel, outside its background.
    let mut push_row = |spans: Vec<Span<'static>>, join: LineJoin| {
        let mut row = Vec::with_capacity(spans.len() + usize::from(indent > 0));
        if indent > 0 {
            row.push(Span::styled(" ".repeat(indent), Style::default()));
        }
        row.extend(spans);
        lines.push(Line::from(row));
        joins.push(join);
    };

    // ── Top padding: panel colour in the row's lower half ──
    push_row(
        vec![Span::styled(
            CODE_PANEL_TOP.to_string().repeat(panel_width),
            Style::default().fg(CODE_BG),
        )],
        LineJoin::Break,
    );

    // ── Language tag + one blank padding row (only when a tag is given) ──
    if let Some(tag) = label {
        let mut row = vec![
            pad(1),
            Span::styled(
                tag.to_string(),
                Style::default().fg(Color::Gray).bg(CODE_BG),
            ),
        ];
        let used = 1 + display_width(tag);
        if panel_width > used {
            row.push(pad(panel_width - used));
        }
        push_row(row, LineJoin::Break);
        push_row(vec![pad(panel_width)], LineJoin::Break);
    }

    // ── Code rows ──
    for (row_line, join) in code_rows {
        let used = 1 + row_line.width();
        let mut row = Vec::with_capacity(row_line.spans.len() + 3);
        row.push(pad(1));
        row.extend(
            row_line
                .spans
                .into_iter()
                .map(|s| Span::styled(s.content, s.style.bg(CODE_BG))),
        );
        if panel_width > used {
            row.push(pad(panel_width - used));
        }
        push_row(row, join);
    }

    // ── Bottom padding: panel colour in the row's upper half ──
    push_row(
        vec![Span::styled(
            CODE_PANEL_BOTTOM.to_string().repeat(panel_width),
            Style::default().fg(CODE_BG),
        )],
        LineJoin::Break,
    );
}

// ── Public API ────────────────────────────────────────────────────────────

pub(crate) fn render_markdown_blocks(
    blocks: &[MarkdownBlock],
    lines: &mut Vec<Line<'static>>,
    joins: &mut Vec<LineJoin>,
    indent: usize,
    width: usize,
    heading_shift: usize,
) {
    for (index, block) in blocks.iter().enumerate() {
        if index > 0 {
            ensure_blank_line_joined(lines, joins);
        }
        // Headings get a *second* blank line for extra visual separation —
        // except when the heading is the first block (index 0) of the
        // document (or of a nested quote/list context), which must not be
        // preceded by blank lines.  `ensure_blank_line` above supplies the
        // first blank; this push adds the second.
        if index > 0 && matches!(block, MarkdownBlock::Heading { .. }) {
            lines.push(Line::from(Span::styled(String::new(), Style::default())));
            joins.push(LineJoin::Break);
        }
        render_markdown_block(block, lines, joins, indent, width, heading_shift);
    }
}

pub(crate) fn render_markdown_block(
    block: &MarkdownBlock,
    lines: &mut Vec<Line<'static>>,
    joins: &mut Vec<LineJoin>,
    indent: usize,
    width: usize,
    heading_shift: usize,
) {
    match block {
        MarkdownBlock::Paragraph(content) => {
            let (para_lines, para_joins) =
                inlines_to_lines(content, indent, None, width, Modifier::empty());
            lines.extend(para_lines);
            joins.extend(para_joins);
        }
        MarkdownBlock::Heading { level, content } => {
            // Normalize the raw markdown level by the document-wide shift so
            // the first heading always renders as level 1 (see markdown_lines).
            let normalized = (*level as usize).saturating_sub(heading_shift).max(1);
            let prefix = heading_prefix(normalized);
            // Headings are rendered bold + underlined for visual distinction.
            let (heading_lines, heading_joins) = inlines_to_lines(
                content,
                indent,
                prefix.as_deref(),
                width,
                Modifier::BOLD | Modifier::UNDERLINED,
            );
            lines.extend(heading_lines);
            joins.extend(heading_joins);
        }
        MarkdownBlock::CodeBlock { language, code } => {
            // A ` ```diff ` fence is an explicit opt-in: the emitting tool
            // chose the markdown `diff` language tag, so the fence interior
            // is handed to the diff renderer instead of the generic code
            // block. The renderer is fed *fence interiors only* — the raw
            // `--- ` / `diff --git` auto-detection sniffs no longer run
            // against whole tool outputs, which is what used to misparse
            // `pdf_to_markdown`'s "--- UNTRUSTED …" delimiter as a diff path
            // header. If the interior does not parse as a diff (junk under
            // the tag) we fall through to the literal-fence code path below
            // so the raw text always stays visible.
            if language.as_deref() == Some("diff") {
                let diff_width = u16::try_from(width.saturating_sub(indent)).unwrap_or(u16::MAX);
                // Log the accept/fallback *decision* only — the fence interior
                // itself passes through the diff renderer and can contain
                // arbitrary tool output, so it is never logged here.
                debug!(fence = "diff", "rendering fenced diff interior");
                if let Some(diff_lines) = try_render_diff_content(code, diff_width) {
                    for line in diff_lines {
                        // Mirror the generic code path's indent handling so a
                        // fenced diff inside a blockquote/list stays inside its
                        // container and never overflows the width.
                        if indent > 0 {
                            let mut spans =
                                vec![Span::styled(" ".repeat(indent), Style::default())];
                            // `line` is consumed right after, so move its span
                            // Vec instead of cloning every span of every diff row.
                            spans.extend(line.spans);
                            lines.push(Line::from(spans));
                        } else {
                            lines.push(line);
                        }
                        // Every diff row is a distinct source line — never
                        // reflowed and never space-joined — so the copy
                        // reproduces the diff verbatim.
                        joins.push(LineJoin::Break);
                    }
                    return;
                }
                debug!(
                    fence = "diff",
                    "fence interior not a parseable diff; falling back to literal code block"
                );
            }

            render_code_panel(language.as_deref(), code, lines, joins, indent, width);
        }
        MarkdownBlock::BlockQuote(blocks) => {
            let mut quoted = Vec::new();
            let mut quoted_joins = Vec::new();
            // Content is rendered at (width - indent - 2) so that when the
            // `QUOTE_BAR` gutter and the outer indent are prepended on each
            // line the total stays within `width`.
            render_markdown_blocks(
                blocks,
                &mut quoted,
                &mut quoted_joins,
                0,
                width.saturating_sub(indent + 2),
                heading_shift,
            );
            for (line, _inner_join) in quoted.into_iter().zip(quoted_joins) {
                // Quoted text is dimmed with ITALIC so it reads as secondary to
                // the surrounding prose.  Inline code and math carry their own
                // foreground colour (Cyan/Yellow/Magenta), so they are left
                // upright — italicising them would smear the syntax
                // highlighting — and the leading bar (below) is the primary
                // visual cue that the block is a quote.
                let mut spans: Vec<Span<'static>> = line
                    .spans
                    .into_iter()
                    .map(|span| {
                        let style = if span.style.fg.is_none() {
                            span.style.add_modifier(Modifier::ITALIC)
                        } else {
                            span.style
                        };
                        Span::styled(span.content, style)
                    })
                    .collect();
                // The two-column muted bar replaces the old literal `"> "`
                // text marker.  `leading_quote_prefix` recognises this exact
                // span so the selection/copy can start *after* it.
                spans.insert(
                    0,
                    Span::styled(QUOTE_BAR.to_string(), Style::default().fg(QUOTE_BAR_COLOR)),
                );
                lines.push(indented_line_as_spans(indent, spans));
                // Every quoted row is a distinct line in the copy: the bar is
                // per-row rendering chrome, and re-glueing wrapped quote rows
                // into one line would merge the gutters into the text.
                // Copying proceeds row by row.
                joins.push(LineJoin::Break);
            }
        }
        MarkdownBlock::List {
            ordered,
            start,
            items,
        } => {
            // Render every item into its own buffer first so the whole list's
            // spacing can be decided as a unit: if the *majority* of items wrap
            // to more than one visual line, every item pair gets a blank line
            // (paragraph-style); otherwise the list renders tight with no gaps
            // between items.  A uniform rhythm per list reads better than the
            // old per-item spacing (where one long item created a single
            // lopsided gap in an otherwise tight list).
            //
            // The list also shares a single indentation unit: the width of the
            // *widest* marker (the item with the highest number, e.g. 4 columns
            // for "10. ").  Every marker is padded with trailing spaces up to
            // that width ("9. " -> "9.  ") so every item's *content* starts at
            // the same column, and every continuation line is indented to that
            // same column — so first lines and wrapped lines all line up as one
            // block regardless of how many digits each marker has.
            let max_marker_width = if *ordered {
                // Item numbers run start..=start + len - 1, so the widest marker
                // is always the last one — O(1) per list, no per-item scan.
                // `saturating_add` is cheap overflow hardening: CommonMark caps
                // marker digits at 9, so `start` is small today, but the marker
                // arithmetic must never be able to overflow (and panic in debug)
                // if a parser or future input ever allows a larger start.
                items.len().checked_sub(1).map_or(0, |last| {
                    display_width(&format!("{}. ", start.saturating_add(last)))
                })
            } else {
                display_width("• ")
            };
            let mut rendered_items: Vec<(String, Vec<Line<'static>>, Vec<LineJoin>)> =
                Vec::with_capacity(items.len());
            for (index, item) in items.iter().enumerate() {
                let marker = if *ordered {
                    // saturating_add: a huge literal list start must render,
                    // not overflow (see max_marker_width above).
                    format!("{}. ", start.saturating_add(index))
                } else {
                    "• ".to_string()
                };
                // Pad the marker to the list-wide width so first-line content
                // aligns with every other item's first line (and with the
                // continuation lines below).  Without this, "9. " content sits
                // one column left of its "10. " sibling — the wrapped lines
                // lined up, but the visible first line was still misaligned.
                let marker = pad_marker(&marker, max_marker_width);
                let mut rendered = Vec::new();
                let mut rendered_joins = Vec::new();
                // Content is rendered at (width - indent - max_marker_width):
                // with every marker padded to that width, a first line totals
                // exactly `width`, and continuation lines (indented to the same
                // column) fit as well.
                render_markdown_blocks(
                    item,
                    &mut rendered,
                    &mut rendered_joins,
                    0,
                    width.saturating_sub(indent + max_marker_width),
                    heading_shift,
                );
                rendered_items.push((marker, rendered, rendered_joins));
            }

            // Strict majority: more than half of the items must be multi-line
            // for the list to be spaced out.  A tie (e.g. 2 items, 1 wrapping)
            // stays tight because 1 * 2 == 2 is not > 2.
            let multi_line_count = rendered_items
                .iter()
                .filter(|(_, rendered, _)| rendered.len() > 1)
                .count();
            let spaced = multi_line_count * 2 > items.len();

            for (index, (marker, rendered, rendered_joins)) in
                rendered_items.into_iter().enumerate()
            {
                // All continuation lines align under the widest marker so wrapped
                // text lines up across the whole list.
                let continuation_indent = indent + max_marker_width;
                // Zip the item's rows with the joins their inner renderer
                // recorded: the two vectors stay in lockstep by construction,
                // so `joins` below needs no fallback.  The first row (which
                // carries the marker) is consumed with its join — each item
                // starts a fresh line, whatever the inner renderer said about
                // its first line is superseded.
                let mut zipped = rendered.into_iter().zip(rendered_joins);
                if let Some((first, _first_join)) = zipped.next() {
                    let mut spans = vec![Span::styled(
                        format!("{}{}", " ".repeat(indent), marker),
                        Style::default(),
                    )];
                    spans.extend(first.spans.clone());
                    lines.push(Line::from(spans));
                    joins.push(LineJoin::Break);
                } else {
                    lines.push(indented_line(indent, marker));
                    joins.push(LineJoin::Break);
                }
                for (line, join) in zipped {
                    let mut spans = vec![Span::styled(
                        " ".repeat(continuation_indent),
                        Style::default(),
                    )];
                    spans.extend(line.spans);
                    lines.push(Line::from(spans));
                    // Wrapped continuations inside the item rejoin with
                    // Space/Join exactly as the inner renderer recorded
                    // (their predecessor's text is the line above them).
                    joins.push(join);
                }

                // Blank line between items only when the list is spaced out as
                // a whole (majority of items wrap).  Uses ensure_blank_line so
                // consecutive blanks collapse into one (e.g. when a spaced
                // item ends with a nested list that already produced a blank).
                if index + 1 < items.len() && spaced {
                    ensure_blank_line_joined(lines, joins);
                }
            }

            // A list is delimited: emit a collapsing blank line after its
            // items.  Blocks between lists get their separation from the
            // *next* block's before-margin, but a nested list's successor is
            // the next item marker of the enclosing list, which never
            // receives a before-margin — without this margin the marker would
            // run flush against the nested list's last line.  ensure_blank_line
            // collapses this margin with any following before-margin, and
            // markdown_lines strips it when the list is the document's last
            // block, so the rule is invisible everywhere except the boundary
            // that was previously missing it.
            if !items.is_empty() {
                ensure_blank_line_joined(lines, joins);
            }
        }
        MarkdownBlock::Table {
            alignments,
            header,
            rows,
        } => {
            let (table_lines, table_joins) =
                render_table_lines(alignments, header, rows, indent, width);
            lines.extend(table_lines);
            joins.extend(table_joins);
        }
        MarkdownBlock::Rule => {
            lines.push(indented_line(indent, "---".to_string()));
            joins.push(LineJoin::Break);
        }
    }
}

// ── Line-building helpers ─────────────────────────────────────────────────

pub(crate) fn indented_line(indent: usize, text: String) -> Line<'static> {
    indented_styled_line(indent, text, Modifier::empty())
}

/// Like [`indented_line`], but applies `modifier` (e.g. `Modifier::BOLD` for a
/// table's header row) to the text span only — the indent stays unstyled so the
/// emphasis never bleeds into the leading margin.
pub(crate) fn indented_styled_line(
    indent: usize,
    text: String,
    modifier: Modifier,
) -> Line<'static> {
    let mut spans = Vec::new();
    if indent > 0 {
        spans.push(Span::styled(" ".repeat(indent), Style::default()));
    }
    spans.push(Span::styled(text, Style::default().add_modifier(modifier)));
    Line::from(spans)
}

pub(crate) fn indented_line_as_spans(
    indent: usize,
    mut spans: Vec<Span<'static>>,
) -> Line<'static> {
    if indent > 0 {
        spans.insert(0, Span::styled(" ".repeat(indent), Style::default()));
    }
    Line::from(spans)
}
