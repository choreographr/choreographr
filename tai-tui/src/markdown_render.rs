use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use syntect::easy::HighlightLines;
use tai_proto::Turn;
use tai_tui::{MarkdownAlignment, MarkdownBlock, MarkdownDocument, MarkdownInline};

use crate::cache::GlobalLruCache;
use crate::syntax::{highlight_theme, syntax_set, to_ratatui_color};

fn highlight_code(language: Option<&str>, code: &str) -> Vec<Line<'static>> {
    static CACHE: GlobalLruCache<(String, String), Vec<Line<'static>>, 200> = GlobalLruCache::new();

    let key = (language.unwrap_or("").to_string(), code.to_string());

    CACHE.get_or_insert_with(&key, || {
        let ss = syntax_set();

        let syntax = language
            .and_then(|lang| ss.find_syntax_by_token(lang))
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
pub(crate) fn render_turn_lines(turn: &Turn, content_width: u16) -> Vec<Line<'static>> {
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
    let has_assistant = turn.assistant_text.is_some()
        || turn.assistant_reasoning.is_some()
        || !turn.tool_calls.is_empty();
    if has_assistant {
        let mut body: Vec<Line<'static>> = Vec::new();

        // Reasoning sub-section
        if let Some(ref reasoning) = turn.assistant_reasoning {
            let trimmed = reasoning.trim();
            if !trimmed.is_empty() {
                heading_line(&mut body, "Reasoning", Color::DarkGray);
                body.extend(plain_text_lines(trimmed));
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

        // Tool call labels
        for tc in &turn.tool_calls {
            body.push(Line::from(Span::styled(
                format!("tool: {}({})", tc.name, tc.arguments_json),
                Style::default().fg(Color::Yellow),
            )));
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

        // Header line
        body.push(Line::from(Span::styled(
            format!("{label}: {}", tr.name),
            Style::default().fg(accent),
        )));

        if !tr.content.is_empty() {
            body.push(Line::from(Span::styled(String::new(), Style::default())));
            // Non-error results use markdown; errors stay plain text.
            if tr.is_error {
                body.extend(plain_text_lines(&tr.content));
            } else {
                body.extend(markdown_lines(&tr.content, content_width));
            }
        }

        let margin = add_margin_lines(body, content_width, accent, None);
        all_lines.extend(margin.0);
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
    let bg_shade = Color::Rgb(60, 60, 60);
    let gray = Style::default().bg(bg_shade);
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
        spans.extend(line.spans);
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

fn format_timestamp(ts_secs: i64) -> String {
    if ts_secs <= 0 {
        return "-".to_string();
    }

    use chrono::{Local, TimeZone};

    let dt = match Local.timestamp_opt(ts_secs, 0) {
        chrono::LocalResult::Single(dt) => dt,
        _ => return "-".to_string(),
    };

    let now = Local::now();
    if dt.date_naive() == now.date_naive() {
        dt.format("%H:%M").to_string()
    } else {
        dt.format("%Y-%m-%d %H:%M").to_string()
    }
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
        let lines = render_turn_lines(&turn, 80);
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
        let lines = render_turn_lines(&turn, 80);
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
        let lines = render_turn_lines(&turn, 80);
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
            text.contains("Let me think..."),
            "reasoning body should appear"
        );
        assert!(text.contains("Response:"), "response header should appear");
        assert!(
            text.contains("The answer is 42."),
            "response body should appear"
        );
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
        let lines = render_turn_lines(&turn, 80);
        let text = lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            text.contains("tool: read_file"),
            "tool call label should appear: {text}"
        );
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
            }],
            displayed_images: vec![],
        };
        let lines = render_turn_lines(&turn, 80);
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
            }],
            displayed_images: vec![],
        };
        let lines = render_turn_lines(&turn, 80);
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
        let lines = render_turn_lines(&turn, 80);
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
        let lines = render_turn_lines(&turn, 80);
        let text = lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("Hello"), "user block should appear");
        assert!(text.contains("Hi there!"), "assistant block should appear");
    }
}
