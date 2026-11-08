//! Block-level markdown rendering: the block dispatch (`render_markdown_block`)
//! over paragraphs, headings, block quotes, and rules, plus the shared
//! line-building helpers.  Fenced code blocks live in `code.rs` (syntect
//! highlighting, the bordered box, and the ` ```diff ` opt-in) and lists in
//! `list.rs`.

use super::{
    DocRender, Line, LineChrome, LineJoin, MarkdownBlock, Modifier, QUOTE_BAR, QUOTE_BAR_COLOR,
    QUOTE_BAR_WIDTH, Span, Style, ensure_blank_line_joined, heading_prefix, inlines_to_lines,
    next_table_id, render_code_block, render_list, render_table_lines,
};

// ── Public API ────────────────────────────────────────────────────────────

pub(crate) fn render_markdown_blocks(
    blocks: &[MarkdownBlock],
    lines: &mut Vec<Line<'static>>,
    joins: &mut Vec<LineJoin>,
    chrome: &mut Vec<LineChrome>,
    indent: usize,
    width: usize,
    opts: &DocRender<'_>,
) {
    for (index, block) in blocks.iter().enumerate() {
        if index > 0 {
            ensure_blank_line_joined(lines, joins, chrome);
        }
        // Headings get a *second* blank line for extra visual separation —
        // except when the heading is the first block (index 0) of the
        // document (or of a nested quote/list context), which must not be
        // preceded by blank lines.  `ensure_blank_line` above supplies the
        // first blank; this push adds the second.
        if index > 0 && matches!(block, MarkdownBlock::Heading { .. }) {
            lines.push(Line::from(Span::styled(String::new(), Style::default())));
            joins.push(LineJoin::Break);
            chrome.push(LineChrome::default());
        }
        // The first-row prefix belongs to the document's opening block alone;
        // every later block and every nested block renders without it.
        let block_opts = if index == 0 {
            *opts
        } else {
            opts.without_prefix()
        };
        match (block_opts.first_row_prefix, block) {
            // A flowing block (paragraph or heading) joins the prefix into its
            // own first text row, which is what lets every continuation row
            // keep the full width.
            (Some(_), block) if flows_into_first_row(block) => {
                render_markdown_block(block, lines, joins, chrome, indent, width, &block_opts);
            }
            // A first block that lays out fixed rows (a code box, list, or
            // table) or its own gutter (a quote) has no text row the prefix can
            // join.  Its rows are rendered `prefix.width()` narrower, then the
            // *whole block* is shifted right: the prefix opens the first row and
            // an equal-width blank pad opens every later row, so the block keeps
            // its internal alignment (a box's frame, a table's columns, a list's
            // markers, a quote's bar) while the prefix never overflows `width`.
            (Some(prefix), block) => {
                let start = lines.len();
                let shrunk = width.saturating_sub(prefix.width());
                let plain = block_opts.without_prefix();
                render_markdown_block(block, lines, joins, chrome, indent, shrunk, &plain);
                shift_rows_right(lines, chrome, start, prefix);
            }
            (None, block) => {
                render_markdown_block(block, lines, joins, chrome, indent, width, &block_opts);
            }
        }
    }
}

/// Whether a block's own text flows into wrapped rows that a leading span can
/// join — a paragraph or a heading — as opposed to a block whose rows are laid
/// out by rules of their own.
fn flows_into_first_row(block: &MarkdownBlock) -> bool {
    matches!(
        block,
        MarkdownBlock::Paragraph(_) | MarkdownBlock::Heading { .. }
    )
}

/// Shift every row the block at `[start..]` rendered right by `prefix`'s width,
/// so the block sits in the columns to the right of the prefix instead of under
/// it.  The prefix itself opens the first row (the collapse triangle); every
/// later row opens with an equal-width blank pad so the block's rows stay
/// mutually aligned.  The matching [`LineChrome`] intervals move with the text,
/// so a shifted block's copy metadata keeps pointing at its real columns.
fn shift_rows_right(
    lines: &mut [Line<'static>],
    chrome: &mut [LineChrome],
    start: usize,
    prefix: &Span<'static>,
) {
    let pad_width = prefix.width();
    if pad_width == 0 {
        return;
    }
    let pad = Span::styled(" ".repeat(pad_width), Style::default());
    if let Some(block_rows) = lines.get_mut(start..) {
        for (offset, row) in block_rows.iter_mut().enumerate() {
            let lead = if offset == 0 {
                prefix.clone()
            } else {
                pad.clone()
            };
            row.spans.insert(0, lead);
        }
    }
    if let Some(block_chrome) = chrome.get_mut(start..) {
        for row_chrome in block_chrome {
            let mut shifted = LineChrome::default();
            shifted.extend_shifted(row_chrome, pad_width);
            *row_chrome = shifted;
        }
    }
}

pub(crate) fn render_markdown_block(
    block: &MarkdownBlock,
    lines: &mut Vec<Line<'static>>,
    joins: &mut Vec<LineJoin>,
    chrome: &mut Vec<LineChrome>,
    indent: usize,
    width: usize,
    opts: &DocRender<'_>,
) {
    match block {
        MarkdownBlock::Paragraph(content) => {
            let (para_lines, para_joins, para_chrome) = inlines_to_lines(
                content,
                indent,
                opts.first_row_prefix.cloned().into_iter().collect(),
                width,
                Modifier::empty(),
            );
            lines.extend(para_lines);
            joins.extend(para_joins);
            chrome.extend(para_chrome);
        }
        MarkdownBlock::Heading { level, content } => {
            // Normalize the raw markdown level by the document-wide shift so
            // the first heading always renders as level 1 (see markdown_lines).
            let normalized = (*level as usize).saturating_sub(opts.heading_shift).max(1);
            // A document that opens with a heading carries the first-row
            // prefix in front of the heading's own decoration.  The prefix and
            // the decoration are distinct spans so each keeps its own style
            // (the triangle's gray, the decoration's inherited style).
            let mut prefix: Vec<Span<'static>> = Vec::new();
            if let Some(lead) = opts.first_row_prefix {
                prefix.push(lead.clone());
            }
            if let Some(deco) = heading_prefix(normalized) {
                prefix.push(Span::styled(deco, Style::default()));
            }
            // Headings are rendered bold + underlined for visual distinction.
            let (heading_lines, heading_joins, heading_chrome) = inlines_to_lines(
                content,
                indent,
                prefix,
                width,
                Modifier::BOLD | Modifier::UNDERLINED,
            );
            lines.extend(heading_lines);
            joins.extend(heading_joins);
            chrome.extend(heading_chrome);
        }
        MarkdownBlock::CodeBlock { language, code } => {
            // Fenced code rendering (the ` ```diff ` opt-in and the bordered
            // box) lives in `code.rs`.
            render_code_block(
                language.as_deref(),
                code,
                lines,
                joins,
                chrome,
                indent,
                width,
            );
        }
        MarkdownBlock::BlockQuote(blocks) => {
            let mut quoted = Vec::new();
            let mut quoted_joins = Vec::new();
            let mut quoted_chrome = Vec::new();
            // Content is rendered at (width - indent - 2) so that when the
            // `QUOTE_BAR` gutter and the outer indent are prepended on each
            // line the total stays within `width`.
            render_markdown_blocks(
                blocks,
                &mut quoted,
                &mut quoted_joins,
                &mut quoted_chrome,
                0,
                width.saturating_sub(indent + 2),
                &opts.without_prefix(),
            );
            for ((line, _inner_join), inner_chrome) in
                quoted.into_iter().zip(quoted_joins).zip(quoted_chrome)
            {
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
                // text marker.  It is recorded as chrome (below) so the
                // selection/copy starts *after* it.
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
                // The bar occupies columns `(indent, indent + QUOTE_BAR_WIDTH)`;
                // every inner chrome interval sits to its right, so shifting
                // them by the bar's width (plus the indent) records the bar as
                // chrome while preserving nested bars (a quote inside a quote).
                let mut row_chrome = LineChrome::default();
                row_chrome.push(indent, indent + QUOTE_BAR_WIDTH);
                row_chrome.extend_shifted(&inner_chrome, indent + QUOTE_BAR_WIDTH);
                chrome.push(row_chrome);
            }
        }
        MarkdownBlock::List {
            ordered,
            start,
            items,
        } => {
            // List layout (the shared marker column, the per-pair item spacing,
            // and the delimited-list margins) lives in `list.rs`.
            let (list_lines, list_joins, list_chrome) =
                render_list(*ordered, *start, items, indent, width, opts);
            lines.extend(list_lines);
            joins.extend(list_joins);
            chrome.extend(list_chrome);
        }
        MarkdownBlock::Table {
            alignments,
            header,
            rows,
        } => {
            // Allocate a session-unique table ordinal (see `next_table_id`):
            // the selection detects a table's contiguous run by comparing ids
            // across contiguous content lines, so the id must be collision-free
            // across the whole rendered history — a per-buffer line offset would
            // repeat across turns and across the separate buffers nested blocks
            // render into, letting two unrelated tables merge into one run.
            let table_id = next_table_id();
            let (table_lines, table_joins, table_chrome) =
                render_table_lines(alignments, header, rows, table_id, indent, width);
            lines.extend(table_lines);
            joins.extend(table_joins);
            chrome.extend(table_chrome);
        }
        MarkdownBlock::Rule => {
            lines.push(indented_line(indent, "---".to_string()));
            joins.push(LineJoin::Break);
            chrome.push(LineChrome::default());
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
