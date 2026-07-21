use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use syntect::easy::HighlightLines;
use tai_proto::Turn;
use tai_tui::{MarkdownAlignment, MarkdownBlock, MarkdownDocument, MarkdownInline};

use crate::cache::GlobalLruCache;
use crate::diff_render::try_render_diff_content;
use crate::render::{BG_SHADE, format_timestamp};
use crate::syntax::{highlight_theme, syntax_set, to_ratatui_color};
use tracing::warn;

fn find_syntax<'a>(
    ss: &'a syntect::parsing::SyntaxSet,
    lang: &str,
) -> Option<&'a syntect::parsing::SyntaxReference> {
    ss.find_syntax_by_token(lang).or_else(move || match lang {
        "typescript" | "tsx" | "mts" | "cts" => ss.find_syntax_by_token("javascript"),
        "vue" | "svelte" => ss.find_syntax_by_token("html"),
        _ => None,
    })
}

fn highlight_code(language: Option<&str>, code: &str) -> Vec<Line<'static>> {
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

// ── Public API ────────────────────────────────────────────────────────────

pub(crate) fn plain_text_lines(text: &str) -> Vec<Line<'static>> {
    if text.is_empty() {
        vec![Line::from(Span::styled(String::new(), Style::default()))]
    } else {
        text.split('\n')
            .map(|line| Line::from(Span::styled(line.to_string(), Style::default())))
            .collect()
    }
}

/// Render ANSI-escape-coded text as styled ratatui lines, wrapping at `width`.
/// Falls back to [`plain_text_lines`] on parse failure.
fn ansi_lines(text: &str, width: u16) -> Vec<Line<'static>> {
    use ansi_to_tui::IntoText as _;

    let width = width as usize;

    match text.as_bytes().into_text() {
        Ok(t) => {
            let mut result: Vec<Line<'static>> = Vec::new();
            for line in &t.lines {
                if width == 0 || line.width() <= width {
                    let spans: Vec<Span<'static>> = line
                        .spans
                        .iter()
                        .map(|span| Span::styled(span.content.to_string(), span.style))
                        .collect();
                    result.push(Line::from(spans));
                } else {
                    // Word-wrap this over-long line at width.
                    wrap_styled_line(line, width, &mut result);
                }
            }
            if result.is_empty() {
                vec![Line::from(Span::styled(String::new(), Style::default()))]
            } else {
                result
            }
        }
        Err(e) => {
            warn!(
                error = %e,
                text_len = text.len(),
                "failed to parse ANSI escape codes, falling back to plain text"
            );
            plain_text_lines(text)
        }
    }
}

/// Word-wrap a pre-styled ratatui line so that no output line exceeds `max_width`.
///
/// Walks the line's styled spans left-to-right, splitting at word (whitespace)
/// boundaries. If a single word is wider than `max_width` it is split by grapheme
/// cluster via [`split_word_to_width`].
fn wrap_styled_line(
    line: &ratatui::text::Line<'_>,
    max_width: usize,
    out: &mut Vec<Line<'static>>,
) {
    // ── 1. Tokenize the line into (style, text, is_space) triplets ───────
    //
    // We split each span's content at whitespace boundaries so that we can
    // later break at word boundaries.  Spaces are kept as separate tokens so
    // they can be dropped at line-start or line-end.
    struct StyledToken {
        text: String,
        style: Style,
        is_space: bool,
    }

    let mut tokens: Vec<StyledToken> = Vec::new();

    for span in &line.spans {
        let s = span.content.as_ref();
        let mut current = String::new();
        // Track whether the current run is whitespace or non-whitespace.
        let mut in_space = false;
        for ch in s.chars() {
            let ch_is_space = ch.is_whitespace();
            if ch_is_space != in_space && !current.is_empty() {
                // Finished a run — push the accumulated token.
                tokens.push(StyledToken {
                    text: std::mem::take(&mut current),
                    style: span.style,
                    is_space: in_space,
                });
            }
            current.push(ch);
            in_space = ch_is_space;
        }
        if !current.is_empty() {
            tokens.push(StyledToken {
                text: current,
                style: span.style,
                is_space: in_space,
            });
        }
    }

    if tokens.is_empty() {
        out.push(Line::from(vec![Span::styled(
            String::new(),
            Style::default(),
        )]));
        return;
    }

    // ── 2. Word-wrap the token stream onto lines of at most max_width ──
    let mut line_spans: Vec<Span<'static>> = Vec::new();
    let mut line_width = 0usize;
    // Did we just add a space at the end?  We keep at most one trailing space
    // so that flush + re-start doesn't introduce a leading space.
    let mut trailing_space = false;

    // Helper: split an over-long word across lines, used when the word alone
    // does not fit on the current (possibly just-flushed) line.
    let push_split_word = |text: &str,
                           style: Style,
                           max_width: usize,
                           out: &mut Vec<Line<'static>>,
                           line_spans: &mut Vec<Span<'static>>,
                           line_width: &mut usize|
     -> () {
        let chunks = split_word_to_width(text, max_width);
        for (ci, chunk) in chunks.iter().enumerate() {
            if ci > 0 {
                out.push(Line::from(std::mem::take(line_spans)));
                *line_width = 0;
            }
            let cw = display_width(chunk);
            line_spans.push(Span::styled(chunk.clone(), style));
            *line_width += cw;
        }
    };

    for token in &tokens {
        if token.is_space {
            // Collapse runs of whitespace to a single space.
            if !line_spans.is_empty() && !trailing_space {
                line_spans.push(Span::styled(" ".to_string(), token.style));
                line_width += 1;
                trailing_space = true;
            }
            continue;
        }

        let word_width = display_width(&token.text);

        if line_width + word_width <= max_width {
            // Fits on the current line.
            trailing_space = false;
            line_spans.push(Span::styled(token.text.clone(), token.style));
            line_width += word_width;
        } else if line_spans.is_empty() {
            // The word alone is too wide for the empty line — split it.
            trailing_space = false;
            push_split_word(
                &token.text,
                token.style,
                max_width,
                &mut *out,
                &mut line_spans,
                &mut line_width,
            );
        } else {
            // Flush the current line and start a fresh line with this word.
            out.push(Line::from(std::mem::take(&mut line_spans)));
            line_width = 0;
            trailing_space = false;

            if word_width <= max_width {
                line_spans.push(Span::styled(token.text.clone(), token.style));
                line_width = word_width;
            } else {
                push_split_word(
                    &token.text,
                    token.style,
                    max_width,
                    &mut *out,
                    &mut line_spans,
                    &mut line_width,
                );
            }
        }
    }

    if !line_spans.is_empty() {
        out.push(Line::from(line_spans));
    }
}

pub(crate) fn lines_height(lines: &[Line<'_>], width: u16) -> usize {
    let width = width as usize;
    if width == 0 {
        return 0;
    }

    if lines.len() <= 1 && lines.iter().all(|line| line.width() == 0) {
        return 1;
    }

    lines
        .iter()
        .map(|line| wrapped_line_height(line, width))
        .sum::<usize>()
        .max(1)
}

/// Render a complete Turn as styled lines suitable for the chat history.
/// Each section (user, assistant, tool results) is wrapped in the margin
/// pattern (top separator, padding, content, padding, bottom separator)
/// with role-specific accent colors.
pub(crate) fn render_turn_lines(
    turn: &Turn,
    content_width: u16,
    tool_content_width: u16,
) -> Vec<Line<'static>> {
    let mut all_lines: Vec<Line<'static>> = Vec::new();

    // ── Error block ──────────────────────────────────────────
    if let Some(ref err) = turn.error {
        let header = format!("Error: {err}");
        let lines = vec![Line::from(Span::styled(
            header,
            Style::default().fg(Color::Red),
        ))];
        all_lines.extend(lines);
        return all_lines;
    }

    // ── User text block (green accent) ───────────────────────
    if let Some(ref text) = turn.user_text {
        let body = markdown_lines(text, content_width);
        let timestamp_ms = Some(turn.created_at.as_millis());
        let margin = add_margin_lines(body, content_width, Color::Green, timestamp_ms);
        all_lines.extend(margin.0);
    }

    // ── Assistant response block (blue accent) ───────────────
    let has_assistant = turn.assistant_text.is_some() || turn.assistant_reasoning.is_some();
    if has_assistant {
        let mut body: Vec<Line<'static>> = Vec::new();

        // Reasoning sub-section
        if let Some(ref reasoning) = turn.assistant_reasoning {
            let trimmed = reasoning.trim();
            if !trimmed.is_empty() {
                heading_line(&mut body, "Reasoning", Color::DarkGray);
                body.extend(markdown_lines(trimmed, content_width));
            }
        }

        // Response sub-section
        if let Some(ref text) = turn.assistant_text {
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                heading_line(&mut body, "Response", Color::Cyan);
                body.extend(markdown_lines(trimmed, content_width));
            }
        }

        // If we have content, wrap with margin lines (no timestamp).
        if !body.is_empty() {
            let margin = add_margin_lines(body, content_width, Color::Blue, None);
            all_lines.extend(margin.0);
        }
    }

    // ── Tool results block (red accent if error, gray otherwise) ─
    for tr in &turn.tool_results {
        let accent = if tr.is_error {
            Color::Red
        } else {
            Color::Reset
        };
        let label = if tr.is_error {
            "tool error"
        } else {
            "tool result"
        };

        let mut body: Vec<Line<'static>> = Vec::new();

        // Invocation description (rendered as markdown so code blocks
        // highlight properly) appears before the tool result/error label.
        if !tr.invocation_description.is_empty() {
            body.extend(markdown_lines(
                &tr.invocation_description,
                tool_content_width,
            ));
            body.push(Line::from(Span::styled(String::new(), Style::default())));
        }

        // Header line
        body.push(Line::from(Span::styled(
            format!("{label}: {}", tr.name),
            Style::default().fg(accent),
        )));

        if !tr.content.is_empty() {
            body.push(Line::from(Span::styled(String::new(), Style::default())));
            // Content with ANSI escape codes gets colored rendering.
            if tr.content.contains("\x1b[") {
                body.extend(ansi_lines(&tr.content, tool_content_width));
            } else if tr.is_error {
                body.extend(plain_text_lines(&tr.content));
            } else if let Some(diff_lines) =
                try_render_diff_content(&tr.content, tool_content_width)
            {
                body.extend(diff_lines);
            } else {
                body.extend(markdown_lines(&tr.content, tool_content_width));
            }
        }

        // Apply 2-column left/right indent so every row spans the full
        // area width with exactly 2 columns of right margin.
        for line in &mut body {
            line.spans.insert(0, Span::styled("  ", Style::default()));
            let content_sum: usize = line.spans.iter().skip(1).map(|s| s.width()).sum();
            let fill = (tool_content_width as usize).saturating_sub(content_sum);
            line.spans
                .push(Span::styled(" ".repeat(fill), Style::default()));
            line.spans.push(Span::styled("  ", Style::default()));
        }
        all_lines.extend(body);
    }

    // If no sections produced output, emit a blank line.
    if all_lines.is_empty() {
        all_lines.push(Line::from(Span::styled(String::new(), Style::default())));
    }

    all_lines
}

// ── Margin helpers (reused from current render system) ─────────────────

/// Structural rows: top separator, top padding, bottom padding, bottom separator.
pub(crate) const MARGIN_STRUCTURAL_ROWS: usize = 4;

/// Wrap content lines with a vertical accent bar on the left and dark-gray
/// background shading.
fn add_margin_lines(
    lines: Vec<Line<'static>>,
    content_width: u16,
    accent: Color,
    timestamp_ms: Option<i64>,
) -> (Vec<Line<'static>>, usize) {
    let gray = Style::default().bg(BG_SHADE);
    let no_shading = Style::default().bg(Color::Reset);
    let accent_line = Style::default().fg(accent).bg(Color::Reset);
    let total_width = content_width as usize + 9;
    let shaded_content = content_width as usize + 4;

    // Top separator: no shading
    let separator = Line::from(vec![Span::styled(" ".repeat(total_width), no_shading)]);

    // Padding row: shading on cols 3..W-3, no text
    let padding = Line::from(vec![
        Span::styled("  ", no_shading),
        Span::styled("┃", accent_line),
        Span::styled(" ".repeat(shaded_content), gray),
        Span::styled("  ", no_shading),
    ]);

    let mut result = Vec::with_capacity(lines.len() + MARGIN_STRUCTURAL_ROWS);
    result.push(separator);
    result.push(padding.clone());

    for line in lines {
        let fill = (content_width as usize).saturating_sub(line.width());

        let mut spans = vec![
            Span::styled("  ", no_shading),
            Span::styled("┃", accent_line),
            Span::styled("  ", gray),
        ];
        // Content spans — explicitly set bg so they display correctly even without
        // a paragraph-level background.
        spans.extend(
            line.spans
                .into_iter()
                .map(|s| Span::styled(s.content, s.style.bg(BG_SHADE))),
        );
        spans.push(Span::styled(" ".repeat(fill), gray));
        spans.push(Span::styled("  ", gray));
        spans.push(Span::styled("  ", no_shading));

        result.push(Line::from(spans));
    }

    result.push(padding);

    // Bottom separator: right-aligned timestamp (user messages only).
    if let Some(ms) = timestamp_ms {
        let ts_text = format_timestamp(ms / 1000);
        let ts_len = ts_text.len();
        let left_fill = total_width.saturating_sub(ts_len + 4);
        result.push(Line::from(vec![
            Span::styled(" ".repeat(left_fill), no_shading),
            Span::styled(ts_text, no_shading),
            Span::styled(" ".repeat(4), no_shading),
        ]));
    } else {
        result.push(Line::from(vec![Span::styled(
            " ".repeat(total_width),
            no_shading,
        )]));
    }

    let total_rows = result.len();
    (result, total_rows)
}

fn heading_line(lines: &mut Vec<Line<'static>>, heading: &'static str, color: Color) {
    if !lines.is_empty() {
        lines.push(Line::from(Span::styled(String::new(), Style::default())));
    }
    lines.push(Line::from(Span::styled(
        format!("{heading}:"),
        Style::default().fg(color),
    )));
}

pub(crate) fn markdown_lines(markdown: &str, width: u16) -> Vec<Line<'static>> {
    let document = MarkdownDocument::parse(markdown);
    let mut lines = Vec::new();
    render_markdown_blocks(&document.blocks, &mut lines, 0, width as usize);
    if lines.is_empty() {
        lines.push(Line::from(Span::styled(String::new(), Style::default())));
    }
    while matches!(lines.last(), Some(line) if line.width() == 0) {
        lines.pop();
    }
    if lines.is_empty() {
        lines.push(Line::from(Span::styled(String::new(), Style::default())));
    }
    lines
}

fn render_markdown_blocks(
    blocks: &[MarkdownBlock],
    lines: &mut Vec<Line<'static>>,
    indent: usize,
    width: usize,
) {
    for (index, block) in blocks.iter().enumerate() {
        if index > 0 {
            lines.push(Line::from(Span::styled(String::new(), Style::default())));
        }
        render_markdown_block(block, lines, indent, width);
    }
}

fn render_markdown_block(
    block: &MarkdownBlock,
    lines: &mut Vec<Line<'static>>,
    indent: usize,
    width: usize,
) {
    match block {
        MarkdownBlock::Paragraph(content) => {
            lines.extend(inlines_to_lines(content, indent, None, width))
        }
        MarkdownBlock::Heading { level, content } => {
            let prefix = Some(format!("{} ", "#".repeat(*level as usize)));
            lines.extend(inlines_to_lines(content, indent, prefix, width));
        }
        MarkdownBlock::CodeBlock { language, code } => {
            let header = language
                .as_deref()
                .map(|value| format!("```{value}"))
                .unwrap_or_else(|| "```".to_string());
            lines.push(indented_line(indent, header));

            let highlighted = highlight_code(language.as_deref(), code);
            for hl_line in highlighted {
                if indent > 0 {
                    let mut spans = vec![Span::styled(" ".repeat(indent), Style::default())];
                    spans.extend(hl_line.spans.clone());
                    lines.push(Line::from(spans));
                } else {
                    lines.push(hl_line);
                }
            }

            lines.push(indented_line(indent, "```".to_string()));
        }
        MarkdownBlock::BlockQuote(blocks) => {
            let mut quoted = Vec::new();
            render_markdown_blocks(blocks, &mut quoted, 0, width);
            for line in quoted {
                let mut spans = line.spans.clone();
                spans.insert(0, Span::styled("> ".to_string(), Style::default()));
                lines.push(indented_line_as_spans(indent, spans));
            }
        }
        MarkdownBlock::List {
            ordered,
            start,
            items,
        } => {
            for (index, item) in items.iter().enumerate() {
                let marker = if *ordered {
                    format!("{}. ", start + index)
                } else {
                    "• ".to_string()
                };
                let continuation_indent = indent + display_width(&marker);
                let mut rendered = Vec::new();
                render_markdown_blocks(item, &mut rendered, 0, width);
                let mut rendered_iter = rendered.into_iter();
                if let Some(first) = rendered_iter.next() {
                    let mut spans = vec![Span::styled(
                        format!("{}{}", " ".repeat(indent), marker),
                        Style::default(),
                    )];
                    spans.extend(first.spans.clone());
                    lines.push(Line::from(spans));
                } else {
                    lines.push(indented_line(indent, marker));
                }
                for line in rendered_iter {
                    lines.push({
                        let text = line.to_string();
                        Line::from(Span::styled(
                            format!("{}{}", " ".repeat(continuation_indent), text),
                            Style::default(),
                        ))
                    });
                }
            }
        }
        MarkdownBlock::Table {
            alignments,
            header,
            rows,
        } => lines.extend(render_table_lines(alignments, header, rows, indent, width)),
        MarkdownBlock::Rule => lines.push(indented_line(indent, "---".to_string())),
    }
}

// ── Table rendering ───────────────────────────────────────────────────────

fn render_table_lines(
    alignments: &[MarkdownAlignment],
    header: &[Vec<MarkdownInline>],
    rows: &[Vec<Vec<MarkdownInline>>],
    indent: usize,
    width: usize,
) -> Vec<Line<'static>> {
    let column_count = alignments
        .len()
        .max(header.len())
        .max(rows.iter().map(Vec::len).max().unwrap_or(0));
    if column_count == 0 {
        return vec![Line::from(Span::styled(String::new(), Style::default()))];
    }
    let mut table_rows = Vec::with_capacity(rows.len() + 1);
    table_rows.push(normalize_table_row(header, column_count));
    table_rows.extend(
        rows.iter()
            .map(|row| normalize_table_row(row, column_count)),
    );
    let mut widths = vec![3usize; column_count];
    for row in &table_rows {
        for (index, cell) in row.iter().enumerate() {
            for line in cell.lines() {
                widths[index] = widths[index].max(display_width(line));
            }
        }
    }
    let border_width = column_count * 3 + 1;
    let available = width
        .saturating_sub(indent)
        .max(border_width + column_count);
    let content_budget = available.saturating_sub(border_width).max(column_count);
    shrink_column_widths(&mut widths, content_budget);
    let header_alignment = normalized_alignments(alignments, column_count);
    let mut lines = Vec::new();
    lines.push(table_border_line('┌', '┬', '┐', &widths, indent));
    lines.extend(render_table_row_wrapped(
        &table_rows[0],
        &widths,
        &header_alignment,
        indent,
    ));
    lines.push(table_separator_line(&widths, &header_alignment, indent));
    for (index, row) in table_rows.iter().enumerate().skip(1) {
        lines.extend(render_table_row_wrapped(
            row,
            &widths,
            &header_alignment,
            indent,
        ));
        if index < table_rows.len() - 1 {
            lines.push(table_border_line('├', '┼', '┤', &widths, indent));
        }
    }
    lines.push(table_border_line('└', '┴', '┘', &widths, indent));
    lines
}

fn normalized_alignments(
    alignments: &[MarkdownAlignment],
    column_count: usize,
) -> Vec<MarkdownAlignment> {
    (0..column_count)
        .map(|index| {
            alignments
                .get(index)
                .copied()
                .unwrap_or(MarkdownAlignment::None)
        })
        .collect()
}

fn normalize_table_row(row: &[Vec<MarkdownInline>], column_count: usize) -> Vec<String> {
    (0..column_count)
        .map(|index| {
            row.get(index)
                .map(|cell| inline_plain_text(cell))
                .unwrap_or_default()
        })
        .collect()
}

fn shrink_column_widths(widths: &mut [usize], budget: usize) {
    let min_width = 3usize;
    while widths.iter().sum::<usize>() > budget {
        if let Some((index, _)) = widths
            .iter()
            .enumerate()
            .filter(|(_, width)| **width > min_width)
            .max_by_key(|(_, width)| **width)
        {
            widths[index] -= 1;
        } else {
            break;
        }
    }
}

fn table_border_line(
    left: char,
    middle: char,
    right: char,
    widths: &[usize],
    indent: usize,
) -> Line<'static> {
    let mut text = String::new();
    text.push(left);
    for (index, width) in widths.iter().enumerate() {
        text.push_str(&"─".repeat(*width + 2));
        text.push(if index + 1 == widths.len() {
            right
        } else {
            middle
        });
    }
    indented_line(indent, text)
}

fn table_separator_line(
    widths: &[usize],
    alignments: &[MarkdownAlignment],
    indent: usize,
) -> Line<'static> {
    let mut text = String::new();
    text.push('├');
    for (index, width) in widths.iter().enumerate() {
        text.push_str(&alignment_rule_segment(*width, alignments[index]));
        text.push(if index + 1 == widths.len() {
            '┤'
        } else {
            '┼'
        });
    }
    indented_line(indent, text)
}

fn alignment_rule_segment(width: usize, alignment: MarkdownAlignment) -> String {
    let inner = width + 2;
    match alignment {
        MarkdownAlignment::Left => format!(":{}", "─".repeat(inner.saturating_sub(1))),
        MarkdownAlignment::Center => {
            if inner <= 2 {
                ":".repeat(inner)
            } else {
                format!(":{}:", "─".repeat(inner - 2))
            }
        }
        MarkdownAlignment::Right => format!("{}:", "─".repeat(inner.saturating_sub(1))),
        MarkdownAlignment::None => "─".repeat(inner),
    }
}

fn render_table_row_wrapped(
    row: &[String],
    widths: &[usize],
    alignments: &[MarkdownAlignment],
    indent: usize,
) -> Vec<Line<'static>> {
    let wrapped_cells: Vec<Vec<String>> = row
        .iter()
        .zip(widths.iter())
        .map(|(cell, width)| wrap_cell_text(cell, *width))
        .collect();
    let row_height = wrapped_cells.iter().map(Vec::len).max().unwrap_or(1).max(1);
    let mut lines = Vec::with_capacity(row_height);
    for line_index in 0..row_height {
        let mut text = String::new();
        text.push('│');
        for column_index in 0..widths.len() {
            let cell_line = wrapped_cells[column_index]
                .get(line_index)
                .map(String::as_str)
                .unwrap_or("");
            text.push(' ');
            text.push_str(&pad_aligned(
                cell_line,
                widths[column_index],
                alignments[column_index],
            ));
            text.push(' ');
            text.push('│');
        }
        lines.push(indented_line(indent, text));
    }
    lines
}

fn wrap_cell_text(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    for raw_line in text.split('\n') {
        let mut current = String::new();
        let mut current_width = 0;
        for word in raw_line.split_whitespace() {
            let word_width = display_width(word);
            let separator_width = usize::from(!current.is_empty());
            if current_width + separator_width + word_width <= width {
                if separator_width == 1 {
                    current.push(' ');
                    current_width += 1;
                }
                current.push_str(word);
                current_width += word_width;
            } else if current.is_empty() {
                lines.extend(split_word_to_width(word, width));
            } else {
                lines.push(std::mem::take(&mut current));
                current_width = 0;
                if word_width <= width {
                    current.push_str(word);
                    current_width = word_width;
                } else {
                    lines.extend(split_word_to_width(word, width));
                }
            }
        }
        if current.is_empty() {
            lines.push(String::new());
        } else {
            lines.push(current);
        }
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

fn split_word_to_width(word: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut current_width = 0;
    for grapheme in unicode_segmentation::UnicodeSegmentation::graphemes(word, true) {
        let grapheme_width = grapheme_width(grapheme).max(1);
        if !current.is_empty() && current_width + grapheme_width > width {
            chunks.push(std::mem::take(&mut current));
            current_width = 0;
        }
        current.push_str(grapheme);
        current_width += grapheme_width;
        if current_width >= width {
            chunks.push(std::mem::take(&mut current));
            current_width = 0;
        }
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    if chunks.is_empty() {
        chunks.push(String::new());
    }
    chunks
}

fn pad_aligned(text: &str, width: usize, alignment: MarkdownAlignment) -> String {
    let text_width = display_width(text);
    if text_width >= width {
        return text.to_string();
    }
    let remaining = width - text_width;
    let (left, right) = match alignment {
        MarkdownAlignment::Right => (remaining, 0),
        MarkdownAlignment::Center => (remaining / 2, remaining - (remaining / 2)),
        MarkdownAlignment::Left | MarkdownAlignment::None => (0, remaining),
    };
    format!("{}{}{}", " ".repeat(left), text, " ".repeat(right))
}

pub(crate) fn display_width(text: &str) -> usize {
    unicode_width::UnicodeWidthStr::width(text)
}

fn grapheme_width(grapheme: &str) -> usize {
    if grapheme.is_empty() {
        0
    } else {
        unicode_width::UnicodeWidthStr::width(grapheme).max(
            grapheme
                .chars()
                .map(|ch| unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0))
                .max()
                .unwrap_or(0),
        )
    }
}

fn inline_plain_text(inlines: &[MarkdownInline]) -> String {
    let mut text = String::new();
    append_inline_plain_text(inlines, &mut text);
    text
}

fn append_inline_plain_text(inlines: &[MarkdownInline], text: &mut String) {
    for inline in inlines {
        match inline {
            MarkdownInline::Text(value)
            | MarkdownInline::Code(value)
            | MarkdownInline::InlineMath(value)
            | MarkdownInline::DisplayMath(value) => text.push_str(value),
            MarkdownInline::Strikethrough(content)
            | MarkdownInline::Emphasis(content)
            | MarkdownInline::Strong(content) => append_inline_plain_text(content, text),
            MarkdownInline::Link {
                content,
                destination,
            } => {
                append_inline_plain_text(content, text);
                if !destination.is_empty() {
                    text.push_str(" (");
                    text.push_str(destination);
                    text.push(')');
                }
            }
            MarkdownInline::Image { alt, destination } => {
                text.push_str("[image: ");
                append_inline_plain_text(alt, text);
                if !destination.is_empty() {
                    text.push_str("] (");
                    text.push_str(destination);
                    text.push(')');
                } else {
                    text.push(']');
                }
            }
            MarkdownInline::LineBreak => text.push('\n'),
        }
    }
}

// ── Line-building helpers ─────────────────────────────────────────────────

fn indented_line(indent: usize, text: String) -> Line<'static> {
    let mut spans = Vec::new();
    if indent > 0 {
        spans.push(Span::styled(" ".repeat(indent), Style::default()));
    }
    spans.push(Span::styled(text, Style::default()));
    Line::from(spans)
}

fn indented_line_as_spans(indent: usize, mut spans: Vec<Span<'static>>) -> Line<'static> {
    if indent > 0 {
        spans.insert(0, Span::styled(" ".repeat(indent), Style::default()));
    }
    Line::from(spans)
}

fn inlines_to_lines(
    inlines: &[MarkdownInline],
    indent: usize,
    prefix: Option<String>,
    width: usize,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut current_spans: Vec<Span<'static>> = Vec::new();
    let mut current_width: usize = 0;
    if indent > 0 {
        current_spans.push(Span::styled(" ".repeat(indent), Style::default()));
        current_width += indent;
    }
    if let Some(ref prefix) = prefix {
        current_spans.push(Span::styled(prefix.clone(), Style::default()));
        current_width += display_width(prefix);
    }
    let mut needs_separator = false;
    render_inlines_to_lines(
        inlines,
        &mut lines,
        &mut current_spans,
        &mut current_width,
        &mut needs_separator,
        indent,
        width,
    );
    if !current_spans.is_empty() || lines.is_empty() {
        lines.push(Line::from(std::mem::take(&mut current_spans)));
    }
    lines
}

fn flush_line(
    lines: &mut Vec<Line<'static>>,
    current: &mut Vec<Span<'static>>,
    current_width: &mut usize,
    indent: usize,
) {
    lines.push(Line::from(std::mem::take(current)));
    *current_width = indent;
    if indent > 0 {
        current.push(Span::styled(" ".repeat(indent), Style::default()));
    }
}

fn render_inlines_to_lines(
    inlines: &[MarkdownInline],
    lines: &mut Vec<Line<'static>>,
    current: &mut Vec<Span<'static>>,
    current_width: &mut usize,
    needs_separator: &mut bool,
    indent: usize,
    width: usize,
) {
    for inline in inlines {
        match inline {
            MarkdownInline::Text(text) => {
                render_text_inline(
                    text,
                    lines,
                    current,
                    current_width,
                    needs_separator,
                    indent,
                    width,
                );
            }
            MarkdownInline::Code(text)
            | MarkdownInline::InlineMath(text)
            | MarkdownInline::DisplayMath(text) => {
                render_code_inline(
                    text,
                    lines,
                    current,
                    current_width,
                    needs_separator,
                    indent,
                    width,
                );
            }
            MarkdownInline::Strikethrough(content)
            | MarkdownInline::Emphasis(content)
            | MarkdownInline::Strong(content) => {
                render_style_inline(
                    content,
                    lines,
                    current,
                    current_width,
                    needs_separator,
                    indent,
                    width,
                );
            }
            MarkdownInline::Link {
                content,
                destination,
            } => {
                render_link_inline(
                    content,
                    destination,
                    lines,
                    current,
                    current_width,
                    needs_separator,
                    indent,
                    width,
                );
            }
            MarkdownInline::Image { alt, destination } => {
                let prefix_text = "[image: ";
                let prefix_width = display_width(prefix_text);
                let projected = *current_width + prefix_width;
                if projected > width && *current_width > indent {
                    flush_line(lines, current, current_width, indent);
                }
                current.push(Span::styled(prefix_text.to_string(), Style::default()));
                *current_width += prefix_width;

                render_inlines_to_lines(
                    alt,
                    lines,
                    current,
                    current_width,
                    needs_separator,
                    indent,
                    width,
                );

                let suffix = if !destination.is_empty() {
                    format!("] ({destination})")
                } else {
                    "]".to_string()
                };
                let suffix_width = display_width(&suffix);
                let projected = *current_width + suffix_width;
                if projected > width && *current_width > indent {
                    flush_line(lines, current, current_width, indent);
                }
                current.push(Span::styled(suffix, Style::default()));
                *current_width += suffix_width;
                *needs_separator = true;
            }
            MarkdownInline::LineBreak => {
                flush_line(lines, current, current_width, indent);
                *needs_separator = false;
            }
        }
    }
}

fn render_text_inline(
    text: &str,
    lines: &mut Vec<Line<'static>>,
    current: &mut Vec<Span<'static>>,
    current_width: &mut usize,
    needs_separator: &mut bool,
    indent: usize,
    width: usize,
) {
    let trimmed = text.trim_start();
    let ends_with_space = text.ends_with(' ') || text.ends_with('\t');
    let words: Vec<&str> = if trimmed.is_empty() {
        *needs_separator = true;
        return;
    } else {
        trimmed.split_whitespace().collect()
    };

    for (i, word) in words.iter().enumerate() {
        let word_width = display_width(word);
        let separator_width = usize::from(*needs_separator || i > 0);
        let projected = *current_width + separator_width + word_width;

        if projected > width && *current_width > indent {
            flush_line(lines, current, current_width, indent);
            *needs_separator = i > 0;
        }

        if *current_width + word_width > width && *current_width >= indent {
            flush_line(lines, current, current_width, indent);
            *needs_separator = false;
            let available = width.saturating_sub(*current_width);
            if word_width > available {
                let chunked = split_word_to_width(word, available);
                for (ci, chunk) in chunked.iter().enumerate() {
                    if ci > 0 {
                        flush_line(lines, current, current_width, indent);
                    }
                    current.push(Span::styled(chunk.clone(), Style::default()));
                    *current_width += display_width(chunk);
                }
                *needs_separator = true;
                continue;
            }
        }

        if (*needs_separator || i > 0) && !current.is_empty() && *current_width > indent {
            current.push(Span::styled(" ".to_string(), Style::default()));
            *current_width += 1;
        }
        current.push(Span::styled(word.to_string(), Style::default()));
        *current_width += word_width;
        *needs_separator = true;
    }

    if ends_with_space && !words.is_empty() {
        *needs_separator = true;
    }
}

fn render_code_inline(
    text: &str,
    lines: &mut Vec<Line<'static>>,
    current: &mut Vec<Span<'static>>,
    current_width: &mut usize,
    needs_separator: &mut bool,
    indent: usize,
    width: usize,
) {
    let word_width = display_width(text);
    let projected = *current_width + usize::from(*needs_separator) + word_width;
    if projected > width && *current_width > indent {
        flush_line(lines, current, current_width, indent);
    } else if *needs_separator && !current.is_empty() && *current_width > indent {
        current.push(Span::styled(" ".to_string(), Style::default()));
        *current_width += 1;
    }
    current.push(Span::styled(text.to_string(), Style::default()));
    *current_width += word_width;
    *needs_separator = true;
}

fn render_style_inline(
    content: &[MarkdownInline],
    lines: &mut Vec<Line<'static>>,
    current: &mut Vec<Span<'static>>,
    current_width: &mut usize,
    needs_separator: &mut bool,
    indent: usize,
    width: usize,
) {
    render_inlines_to_lines(
        content,
        lines,
        current,
        current_width,
        needs_separator,
        indent,
        width,
    );
    *needs_separator = true;
}

#[allow(clippy::too_many_arguments)]
fn render_link_inline(
    content: &[MarkdownInline],
    destination: &str,
    lines: &mut Vec<Line<'static>>,
    current: &mut Vec<Span<'static>>,
    current_width: &mut usize,
    needs_separator: &mut bool,
    indent: usize,
    width: usize,
) {
    render_inlines_to_lines(
        content,
        lines,
        current,
        current_width,
        needs_separator,
        indent,
        width,
    );
    if !destination.is_empty() {
        let dest_text = format!(" ({destination})");
        let dest_width = display_width(&dest_text);
        let projected = *current_width + dest_width;
        if projected > width && *current_width > indent {
            flush_line(lines, current, current_width, indent);
        }
        current.push(Span::styled(dest_text, Style::default()));
        *current_width += dest_width;
    }
    *needs_separator = true;
}

fn wrapped_line_height(line: &Line<'_>, width: usize) -> usize {
    if width == 0 {
        return 0;
    }
    let line_width = line.width();
    if line_width == 0 {
        1
    } else {
        line_width.div_ceil(width)
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Color;

    // ── find_syntax ──────────────────────────────────────────────────────

    #[test]
    fn find_syntax_rust() {
        let ss = syntax_set();
        let result = find_syntax(ss, "rust");
        assert!(result.is_some());
        assert_eq!(result.unwrap().name, "Rust");
    }

    #[test]
    fn find_syntax_typescript_maps_to_javascript() {
        let ss = syntax_set();
        let result = find_syntax(ss, "typescript");
        assert!(result.is_some());
        assert_eq!(result.unwrap().name, "JavaScript");
    }

    #[test]
    fn find_syntax_tsx_maps_to_javascript() {
        let ss = syntax_set();
        let result = find_syntax(ss, "tsx");
        assert!(result.is_some());
        assert_eq!(result.unwrap().name, "JavaScript");
    }

    #[test]
    fn find_syntax_vue_maps_to_html() {
        let ss = syntax_set();
        let result = find_syntax(ss, "vue");
        assert!(result.is_some());
        assert_eq!(result.unwrap().name, "HTML");
    }

    #[test]
    fn find_syntax_svelte_maps_to_html() {
        let ss = syntax_set();
        let result = find_syntax(ss, "svelte");
        assert!(result.is_some());
        assert_eq!(result.unwrap().name, "HTML");
    }

    #[test]
    fn find_syntax_unknown_returns_none() {
        let ss = syntax_set();
        let result = find_syntax(ss, "not-a-real-language-12345");
        assert!(result.is_none());
    }

    // ── highlight_code ───────────────────────────────────────────────────

    #[test]
    fn highlight_code_known_language_produces_coloured_spans() {
        let lines = highlight_code(Some("rust"), "fn main() {}");
        assert!(!lines.is_empty(), "should produce at least one line");

        let has_colour = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .any(|s| matches!(s.style.fg, Some(Color::Rgb(_, _, _))));
        assert!(has_colour, "highlighted Rust should have coloured spans");
    }

    #[test]
    fn highlight_code_unknown_language_produces_output() {
        let lines = highlight_code(Some("this-is-not-a-real-language"), "some text");
        assert!(!lines.is_empty(), "should still produce output");
    }

    #[test]
    fn highlight_code_none_language_uses_plain_text() {
        let lines = highlight_code(None, "plain text");
        assert!(!lines.is_empty());
    }

    #[test]
    fn highlight_code_empty_string() {
        let lines = highlight_code(Some("rust"), "");
        assert!(!lines.is_empty());
    }

    #[test]
    fn highlight_code_multi_line() {
        let lines = highlight_code(Some("python"), "def foo():\n    pass");
        assert_eq!(lines.len(), 2, "should have one line per code line");
    }

    // ── plain_text_lines ─────────────────────────────────────────────────

    #[test]
    fn plain_text_lines_empty() {
        let result = plain_text_lines("");
        assert_eq!(result.len(), 1, "empty input → one empty line");
        assert_eq!(result[0].width(), 0);
    }

    #[test]
    fn plain_text_lines_single() {
        let result = plain_text_lines("hello");
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].to_string(), "hello");
    }

    #[test]
    fn plain_text_lines_multi() {
        let result = plain_text_lines("a\nb\nc");
        assert_eq!(result.len(), 3);
        assert_eq!(result[0].to_string(), "a");
        assert_eq!(result[1].to_string(), "b");
        assert_eq!(result[2].to_string(), "c");
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
        assert!(result.len() >= 3, "code block should have at least 3 lines");
        assert_eq!(result[0].to_string(), "```rust");
        assert_eq!(result.last().unwrap().to_string(), "```");
    }

    #[test]
    fn markdown_lines_code_block_no_language() {
        let md = "```\nplain code\n```";
        let result = markdown_lines(md, 80);
        assert!(result.len() >= 3);
        assert_eq!(result[0].to_string(), "```");
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

    // ── render_turn_lines ────────────────────────────────────────────────

    #[test]
    fn render_turn_lines_error_shows_red_header() {
        let turn = Turn {
            created_at: tai_proto::TimestampMs::now(),
            undone: false,
            error: Some("something went wrong".into()),
            user_text: None,
            assistant_text: None,
            assistant_reasoning: None,
            tool_calls: vec![],
            token_usage: None,
            tool_results: vec![],
            displayed_images: vec![],
        };
        let lines = render_turn_lines(&turn, 80, 85);
        assert!(!lines.is_empty());
        let text = lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("Error: something went wrong"));
    }

    #[test]
    fn render_turn_lines_user_text() {
        let turn = Turn {
            created_at: tai_proto::TimestampMs::now(),
            undone: false,
            error: None,
            user_text: Some("hello world".into()),
            assistant_text: None,
            assistant_reasoning: None,
            tool_calls: vec![],
            token_usage: None,
            tool_results: vec![],
            displayed_images: vec![],
        };
        let lines = render_turn_lines(&turn, 80, 85);
        let text = lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("hello world"), "user text should appear");
    }

    #[test]
    fn render_turn_lines_assistant_text() {
        let turn = Turn {
            created_at: tai_proto::TimestampMs::now(),
            undone: false,
            error: None,
            user_text: None,
            assistant_text: Some("The answer is 42.".into()),
            assistant_reasoning: Some("Let me think...".into()),
            tool_calls: vec![],
            token_usage: None,
            tool_results: vec![],
            displayed_images: vec![],
        };
        let lines = render_turn_lines(&turn, 80, 85);
        let text = lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            text.contains("Reasoning:"),
            "reasoning header should appear"
        );
        assert!(
            text.contains("Let me think"),
            "reasoning body should appear"
        );
        assert!(text.contains("Response:"), "response header should appear");
        assert!(
            text.contains("The answer is 42."),
            "response body should appear"
        );
    }

    #[test]
    fn render_turn_lines_reasoning_bold_markdown() {
        let turn = Turn {
            created_at: tai_proto::TimestampMs::now(),
            undone: false,
            error: None,
            user_text: None,
            assistant_text: Some("Okay.".into()),
            assistant_reasoning: Some("Use **bold** for emphasis.".into()),
            tool_calls: vec![],
            token_usage: None,
            tool_results: vec![],
            displayed_images: vec![],
        };
        let lines = render_turn_lines(&turn, 80, 85);
        let text = lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            text.contains("Reasoning:"),
            "reasoning header should appear"
        );
        assert!(text.contains("bold"), "bold text content should appear");
        assert!(
            !text.contains("**bold**"),
            "markdown bold syntax should not appear literally in output"
        );
    }

    #[test]
    fn render_turn_lines_reasoning_inline_code() {
        let turn = Turn {
            created_at: tai_proto::TimestampMs::now(),
            undone: false,
            error: None,
            user_text: None,
            assistant_text: None,
            assistant_reasoning: Some("Use `code` inline.".into()),
            tool_calls: vec![],
            token_usage: None,
            tool_results: vec![],
            displayed_images: vec![],
        };
        let lines = render_turn_lines(&turn, 80, 85);
        let text = lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            text.contains("Reasoning:"),
            "reasoning header should appear"
        );
        assert!(text.contains("code"), "code content should appear");
        assert!(
            !text.contains("`code`"),
            "markdown inline code backticks should not appear literally"
        );
    }

    #[test]
    fn render_turn_lines_reasoning_whitespace_only() {
        let turn = Turn {
            created_at: tai_proto::TimestampMs::now(),
            undone: false,
            error: None,
            user_text: None,
            assistant_text: Some("Response text.".into()),
            assistant_reasoning: Some("   ".into()),
            tool_calls: vec![],
            token_usage: None,
            tool_results: vec![],
            displayed_images: vec![],
        };
        let lines = render_turn_lines(&turn, 80, 85);
        let text = lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !text.contains("Reasoning:"),
            "whitespace-only reasoning should not produce a Reasoning header"
        );
        assert!(text.contains("Response:"), "response header should appear");
    }

    #[test]
    fn render_turn_lines_reasoning_code_block() {
        let turn = Turn {
            created_at: tai_proto::TimestampMs::now(),
            undone: false,
            error: None,
            user_text: None,
            assistant_text: None,
            assistant_reasoning: Some("Here is code:\n```rust\nfn main() {}\n```".into()),
            tool_calls: vec![],
            token_usage: None,
            tool_results: vec![],
            displayed_images: vec![],
        };
        let lines = render_turn_lines(&turn, 80, 85);
        let text = lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            text.contains("Reasoning:"),
            "reasoning header should appear"
        );
        assert!(
            text.contains("fn main() {}"),
            "code block content should appear"
        );
        assert!(text.contains("```"), "code block fences should be visible");
    }

    #[test]
    fn render_turn_lines_tool_calls() {
        let turn = Turn {
            created_at: tai_proto::TimestampMs::now(),
            undone: false,
            error: None,
            user_text: None,
            assistant_text: None,
            assistant_reasoning: None,
            tool_calls: vec![tai_proto::AssistantToolCallRecord {
                call_id: "call1".into(),
                name: "read_file".into(),
                arguments_json: r#"{"path":"/tmp/x"}"#.into(),
            }],
            token_usage: None,
            tool_results: vec![],
            displayed_images: vec![],
        };
        let lines = render_turn_lines(&turn, 80, 85);
        let text = lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        // The turn has only tool_calls (no text, no reasoning), so no
        // assistant block is rendered. Tool calls are now only visible
        // through their streaming output and subsequent tool results.
        assert!(!text.contains("tool:"), "tool: label should not appear");
    }

    #[test]
    fn render_turn_lines_tool_results() {
        let turn = Turn {
            created_at: tai_proto::TimestampMs::now(),
            undone: false,
            error: None,
            user_text: None,
            assistant_text: None,
            assistant_reasoning: None,
            tool_calls: vec![],
            token_usage: None,
            tool_results: vec![tai_proto::ToolResultRecord {
                call_id: "call1".into(),
                name: "read_file".into(),
                content: "file contents".into(),
                is_error: false,
                invocation_description: String::new(),
            }],
            displayed_images: vec![],
        };
        let lines = render_turn_lines(&turn, 80, 85);
        let text = lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("tool result: read_file"));
        assert!(text.contains("file contents"));
    }

    #[test]
    fn render_turn_lines_tool_results_error() {
        let turn = Turn {
            created_at: tai_proto::TimestampMs::now(),
            undone: false,
            error: None,
            user_text: None,
            assistant_text: None,
            assistant_reasoning: None,
            tool_calls: vec![],
            token_usage: None,
            tool_results: vec![tai_proto::ToolResultRecord {
                call_id: "call1".into(),
                name: "run".into(),
                content: "command failed".into(),
                is_error: true,
                invocation_description: String::new(),
            }],
            displayed_images: vec![],
        };
        let lines = render_turn_lines(&turn, 80, 85);
        let text = lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("tool error: run"));
        assert!(text.contains("command failed"));
    }

    #[test]
    fn render_turn_lines_empty_turn_produces_blank_line() {
        let turn = Turn {
            created_at: tai_proto::TimestampMs::now(),
            undone: false,
            error: None,
            user_text: None,
            assistant_text: None,
            assistant_reasoning: None,
            tool_calls: vec![],
            token_usage: None,
            tool_results: vec![],
            displayed_images: vec![],
        };
        let lines = render_turn_lines(&turn, 80, 85);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].width(), 0);
    }

    #[test]
    fn render_turn_lines_user_with_assistant_renders_both_blocks() {
        let turn = Turn {
            created_at: tai_proto::TimestampMs::now(),
            undone: false,
            error: None,
            user_text: Some("Hello".into()),
            assistant_text: Some("Hi there!".into()),
            assistant_reasoning: None,
            tool_calls: vec![],
            token_usage: None,
            tool_results: vec![],
            displayed_images: vec![],
        };
        let lines = render_turn_lines(&turn, 80, 85);
        let text = lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("Hello"), "user block should appear");
        assert!(text.contains("Hi there!"), "assistant block should appear");
    }

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
}
