use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use syntect::easy::HighlightLines;
use tai_proto::SessionMessage;
use tai_tui::{MarkdownAlignment, MarkdownBlock, MarkdownDocument, MarkdownInline, StreamingText};

use crate::cache::GlobalLruCache;
use crate::syntax::{highlight_theme, syntax_set, to_ratatui_color};

/// Highlight a code snippet into styled ratatui lines.
///
/// * `language` – the language hint from the markdown fenced-code info string
///   (e.g. `Some("rust")`).  `None` or an unrecognised token falls back to
///   plain text (no highlighting).
/// * `code` – the raw source code.
///
/// Results are memoized in a bounded global cache so that `total_history_height`
/// (which re-renders every item after each mutation) does not trigger syntect's
/// regex engine repeatedly for the same code block.  The cache has a fixed
/// capacity; when full, new entries are dropped silently — the existing entries
/// for the most common code blocks stay warm.
fn highlight_code(language: Option<&str>, code: &str) -> Vec<Line<'static>> {
    // Memoize highlighted results so re-rendering does not re-run syntect.
    // LRU eviction naturally keeps the most frequently seen code blocks
    // cached, replacing the old manual HashMap+cap approach.
    static CACHE: GlobalLruCache<(String, String), Vec<Line<'static>>, 200> = GlobalLruCache::new();

    let key = (language.unwrap_or("").to_string(), code.to_string());

    CACHE.get_or_insert_with(&key, || {
        let ss = syntax_set();

        // Look up the syntax definition by the language token.  If the
        // token isn't recognised (or was omitted), use the built-in
        // "Plain Text" syntax which emits a single unstyled span per line.
        let syntax = language
            .and_then(|lang| ss.find_syntax_by_token(lang))
            .unwrap_or_else(|| ss.find_syntax_plain_text());

        let theme = highlight_theme();
        let mut highlighter = HighlightLines::new(syntax, theme);
        let mut result = Vec::with_capacity(code.len().max(1));

        for line in code.split('\n') {
            let Ok(ranges) = highlighter.highlight_line(line, ss) else {
                // If highlighting fails for a line, emit it as plain text.
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

pub(crate) fn session_message_lines(message: &SessionMessage, width: u16) -> Vec<Line<'static>> {
    match message {
        SessionMessage::SystemText { content } => {
            labeled_text_lines("system", content, Color::DarkGray)
        }
        SessionMessage::UserText { content } => markdown_lines(content, width),
        SessionMessage::AssistantText {
            content, reasoning, ..
        } => {
            let mut lines = Vec::new();
            if let Some(reasoning_text) = reasoning
                .as_deref()
                .filter(|value| !value.trim().is_empty())
            {
                heading_line(&mut lines, "Reasoning", Color::DarkGray);
                lines.extend(markdown_lines(reasoning_text, width));
            }
            heading_line(&mut lines, "Response", Color::Cyan);
            lines.extend(markdown_lines(content, width));
            lines
        }
        SessionMessage::AssistantToolUse {
            content,
            tool_calls,
            reasoning,
            ..
        } => {
            let mut lines = Vec::new();
            if let Some(reasoning_text) = reasoning
                .as_deref()
                .filter(|value| !value.trim().is_empty())
            {
                heading_line(&mut lines, "Reasoning", Color::DarkGray);
                lines.extend(markdown_lines(reasoning_text, width));
            }
            if let Some(content) = content.as_deref().filter(|value| !value.trim().is_empty()) {
                heading_line(&mut lines, "Response", Color::Cyan);
                lines.extend(markdown_lines(content, width));
            }
            lines.push(Line::from(Span::styled(
                format!(
                    "tool-call: {}",
                    humfmt::list(
                        &tool_calls
                            .iter()
                            .map(|call| format!("{}({})", call.name, call.arguments_json))
                            .collect::<Vec<_>>(),
                    )
                ),
                Style::default().fg(Color::Yellow),
            )));
            lines
        }
        SessionMessage::ToolResult {
            name,
            content,
            is_error,
            ..
        } => {
            let (label, color) = if *is_error {
                ("tool error", Color::Red)
            } else {
                ("tool result", Color::Reset)
            };
            // Header line with the label and tool name.
            let mut lines = vec![Line::from(Span::styled(
                format!("{label}: {name}"),
                Style::default().fg(color),
            ))];
            if !content.is_empty() {
                // Separate the body from the header with a blank line.
                lines.push(Line::from(Span::styled(String::new(), Style::default())));
                // Non-error results are rendered as markdown so that code
                // blocks produced by tools such as run_riscv (which wraps
                // formatted Rust source in ```rust fences) get syntect-based
                // syntax highlighting.  Errors remain plain text.
                let body = if *is_error {
                    plain_text_lines(content)
                } else {
                    markdown_lines(content, width)
                };
                lines.extend(body);
            }
            lines
        }
        // DisplayedImage is intercepted by App::push_session_message and
        // converted directly to HistoryItem::Image, so this arm should never
        // be reached at runtime — it exists only for exhaustive matching.
        SessionMessage::DisplayedImage(_) => vec![],
        _ => vec![],
    }
}

pub(crate) fn streaming_text_lines(text: &StreamingText, width: u16) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(Span::styled(
        format!("[{}]", text.request_id),
        Style::default().fg(Color::DarkGray),
    ))];

    if !text.reasoning.is_empty() {
        heading_line(&mut lines, "Reasoning", Color::DarkGray);
        lines.extend(plain_text_lines(&text.reasoning));
    }

    if !text.answer.is_empty() {
        heading_line(&mut lines, "Response", Color::Cyan);
        lines.extend(markdown_lines(&text.answer, width));
    }

    if text.reasoning.is_empty() && text.answer.is_empty() {
        lines.push(Line::from(Span::styled(String::new(), Style::default())));
    }

    lines
}

// ── Internal helpers ──────────────────────────────────────────────────────

fn labeled_text_lines(label: &'static str, text: &str, color: Color) -> Vec<Line<'static>> {
    prefixed_lines(label, plain_text_lines(text), color)
}

fn prefixed_lines(
    label: &'static str,
    body: Vec<Line<'static>>,
    color: Color,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    append_section(&mut lines, label, body, color);
    lines
}

/// Push a standalone heading line (e.g. "Reasoning:" or "Response:") with a
/// blank separator before it if there is already content above.
///
/// Why a standalone heading instead of the old `append_section` approach
/// (which prefixed the label to the first content line)?  Because the new
/// assistant-message visual style renders headings on their own row, and
/// streaming text should match the non-streaming layout so that section
/// headers look consistent whether or not the stream has finished.
fn heading_line(lines: &mut Vec<Line<'static>>, heading: &'static str, color: Color) {
    if !lines.is_empty() {
        lines.push(Line::from(Span::styled(String::new(), Style::default())));
    }
    lines.push(Line::from(Span::styled(
        format!("{heading}:"),
        Style::default().fg(color),
    )));
}

fn append_section(
    lines: &mut Vec<Line<'static>>,
    label: &'static str,
    body: Vec<Line<'static>>,
    color: Color,
) {
    // Add a blank line to separate this section from the previous content.
    if !lines.is_empty() {
        lines.push(Line::from(Span::styled(String::new(), Style::default())));
    }
    let mut body_iter = body.into_iter();
    if let Some(first) = body_iter.next() {
        let label_text = format!("{label}: ");
        // Move spans out of `first` instead of cloning — body is consumed
        // by into_iter() so no other code needs the original.
        let mut first_spans = first.spans;
        first_spans.insert(0, Span::styled(label_text, Style::default().fg(color)));
        lines.push(Line::from(first_spans));
    } else {
        lines.push(Line::from(Span::styled(
            format!("{label}:"),
            Style::default().fg(color),
        )));
    }
    lines.extend(body_iter);
}

pub(crate) fn markdown_lines(markdown: &str, width: u16) -> Vec<Line<'static>> {
    let document = MarkdownDocument::parse(markdown);
    let mut lines = Vec::new();
    render_markdown_blocks(&document.blocks, &mut lines, 0, width as usize);
    if lines.is_empty() {
        lines.push(Line::from(Span::styled(String::new(), Style::default())));
    }
    // Trim trailing empty lines
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
            // Opening fence with language hint
            let header = language
                .as_deref()
                .map(|value| format!("```{value}"))
                .unwrap_or_else(|| "```".to_string());
            lines.push(indented_line(indent, header));

            // Syntax-highlighted code lines
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

            // Closing fence
            lines.push(indented_line(indent, "```".to_string()));
        }
        MarkdownBlock::BlockQuote(blocks) => {
            let mut quoted = Vec::new();
            render_markdown_blocks(blocks, &mut quoted, 0, width);
            for line in quoted {
                let mut spans = line.spans.clone();
                // Prepend "> " to the first span of the quoted line
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

/// Create a `Line` with an indentation prefix made of spaces, followed by
/// the given text as a default-styled span.
fn indented_line(indent: usize, text: String) -> Line<'static> {
    let mut spans = Vec::new();
    if indent > 0 {
        spans.push(Span::styled(" ".repeat(indent), Style::default()));
    }
    spans.push(Span::styled(text, Style::default()));
    Line::from(spans)
}

/// Create a `Line` by prepending an indentation prefix (in spaces) to an
/// existing set of spans.
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
            // After a flush, we're on a fresh line so no separator needed
            // unless this is not the first word.
            *needs_separator = i > 0;
        }

        // Re-check: if the word itself doesn't fit on a fresh line,
        // split it grapheme-by-grapheme.
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
        // Link destination appends without space (the format already has one).
        current.push(Span::styled(dest_text, Style::default()));
        *current_width += dest_width;
    }
    *needs_separator = true;
}
fn wrapped_line_height(line: &Line<'_>, width: usize) -> usize {
    if width == 0 {
        return 0;
    }
    // Use Line::width() directly instead of line.to_string() + display_width
    // to avoid allocating a temporary String from concatenating all spans.
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

        // At least one span should have a non-default foreground colour.
        let has_colour = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .any(|s| matches!(s.style.fg, Some(Color::Rgb(_, _, _))));
        assert!(has_colour, "highlighted Rust should have coloured spans");
    }

    #[test]
    fn highlight_code_unknown_language_produces_output() {
        // An unrecognised language token should still produce output lines
        // without panicking.  syntect may apply a fallback syntax, so we
        // merely verify that we get at least one line.
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
        // At width 40, 100 chars → 3 wrapped rows (ceil(100/40))
        assert_eq!(lines_height(&lines, 40), 3);
    }

    #[test]
    fn lines_height_multiple_lines() {
        let lines = vec![Line::from("short"), Line::from("a".repeat(50))];
        // width=30: short=1 row, 50 chars=2 rows → total 3
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
        // Should have: ```rust line, highlighted code line, ``` line
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

    // ── inlines_to_lines ─────────────────────────────────────────────────

    #[test]
    fn inlines_to_lines_simple_text() {
        let inlines = vec![MarkdownInline::Text("hello".to_string())];
        let result = inlines_to_lines(&inlines, 0, None, 80);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].to_string(), "hello");
    }

    #[test]
    fn inlines_to_lines_with_indent_and_prefix() {
        let inlines = vec![MarkdownInline::Text("world".to_string())];
        let result = inlines_to_lines(&inlines, 2, Some("# ".to_string()), 80);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].to_string(), "  # world");
    }

    #[test]
    fn inlines_to_lines_handles_line_break() {
        let inlines = vec![
            MarkdownInline::Text("a".to_string()),
            MarkdownInline::LineBreak,
            MarkdownInline::Text("b".to_string()),
        ];
        let result = inlines_to_lines(&inlines, 0, None, 80);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].to_string(), "a");
        assert_eq!(result[1].to_string(), "b");
    }

    #[test]
    fn inlines_to_lines_word_wraps_at_width() {
        let inlines = vec![MarkdownInline::Text("hello world foo bar baz".to_string())];
        // Width 10: each word fits individually but they wrap.
        let result = inlines_to_lines(&inlines, 0, None, 10);
        // "hello " (6) + "world" (5) = 11 > 10 → wrap after "hello"
        // First line: "hello" (5), second line: "world" (5)
        // But then "world " + "foo " = 10, fits. Then "bar" would need to check.
        // Let me just verify there are multiple lines.
        assert!(
            result.len() > 1,
            "should wrap at narrow width, got {} lines: {:?}",
            result.len(),
            result
        );
    }

    #[test]
    fn inlines_to_lines_code_block_does_not_wrap() {
        let inlines = vec![MarkdownInline::Code("long_code_token".to_string())];
        // Width 5: the single code token should still be emitted (it may overflow as
        // a single word, but should not split at spaces).
        let result = inlines_to_lines(&inlines, 0, None, 5);
        assert_eq!(result.len(), 1);
        assert!(result[0].to_string().contains("long_code_token"));
    }

    #[test]
    fn inlines_to_lines_wraps_with_prefix_on_first_line_only() {
        let inlines = vec![MarkdownInline::Text(
            "aaa bbb ccc ddd eee fff ggg".to_string(),
        )];
        // Width 10, indent 2, prefix "# ". So first line has "  # " (4 chars)
        // leaving 6 chars for content. Second (wrapped) line has "  " (2 chars)
        // leaving 8 chars for content.
        let result = inlines_to_lines(&inlines, 2, Some("# ".to_string()), 10);
        assert!(
            result.len() >= 2,
            "should wrap, got {} lines: {:?}",
            result.len(),
            result
        );
        // First line starts with "  # " (prefix only on first line)
        assert!(
            result[0].to_string().starts_with("  # "),
            "first line should have prefix: {:?}",
            result[0].to_string()
        );
        // Subsequent lines start with indent-only (no prefix)
        for line in result.iter().skip(1) {
            let text = line.to_string();
            assert!(
                !text.starts_with("  # "),
                "continuation line should not have prefix: {:?}",
                text
            );
        }
    }

    // ── to_ratatui_color ─────────────────────────────────────────────────

    #[test]
    fn to_ratatui_color_opaque() {
        let c = to_ratatui_color(syntect::highlighting::Color {
            r: 255,
            g: 0,
            b: 0,
            a: 255,
        });
        assert_eq!(c, Color::Rgb(255, 0, 0));
    }

    #[test]
    fn to_ratatui_color_transparent() {
        let c = to_ratatui_color(syntect::highlighting::Color {
            r: 255,
            g: 0,
            b: 0,
            a: 0,
        });
        assert_eq!(c, Color::Reset);
    }

    #[test]
    fn to_ratatui_color_semi_transparent() {
        let c = to_ratatui_color(syntect::highlighting::Color {
            r: 255,
            g: 0,
            b: 0,
            a: 100,
        });
        assert_eq!(c, Color::Reset, "alpha < 128 → Reset");
    }

    // ── ToolResult rendering (markdown support) ──────────────────────────

    #[test]
    fn tool_result_with_code_block_gets_syntax_highlighting() {
        let msg = SessionMessage::ToolResult {
            call_id: "0".to_string(),
            name: "run_riscv".to_string(),
            content: "```rust\nfn main() {}\n```\n\nhello".to_string(),
            is_error: false,
        };
        let lines = session_message_lines(&msg, 80);
        // Line 0: header "tool result: run_riscv"
        assert_eq!(lines[0].to_string(), "tool result: run_riscv");
        // Line 1: blank separator; Line 2: ```rust
        assert!(
            lines[2].to_string().contains("```rust"),
            "{}",
            lines[2].to_string()
        );
        // The highlighted line for "fn main() {}" should have colour spans
        let has_colour = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .any(|s| matches!(s.style.fg, Some(Color::Rgb(_, _, _))));
        assert!(
            has_colour,
            "Rust code in tool result should have coloured spans"
        );
        // The closing fence and the output should be present
        let all_text: String = lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(all_text.contains("```"), "should contain closing fence");
        assert!(
            all_text.contains("hello"),
            "should contain execution output"
        );
    }

    #[test]
    fn tool_result_plain_text_still_renders() {
        let msg = SessionMessage::ToolResult {
            call_id: "0".to_string(),
            name: "echo".to_string(),
            content: "hello world".to_string(),
            is_error: false,
        };
        let lines = session_message_lines(&msg, 80);
        assert_eq!(lines[0].to_string(), "tool result: echo");
        assert!(lines.len() >= 2, "should have body line");
        let body: String = lines[1..]
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(body.contains("hello world"), "body should contain the text");
    }

    #[test]
    fn tool_result_error_stays_plain_text() {
        let msg = SessionMessage::ToolResult {
            call_id: "0".to_string(),
            name: "run_riscv".to_string(),
            content: "```rust\nfn main() {}\n```\ncrash!".to_string(),
            is_error: true,
        };
        let lines = session_message_lines(&msg, 80);
        assert_eq!(lines[0].to_string(), "tool error: run_riscv");
        // Error results should NOT have syntax highlighting — they should
        // render the content verbatim (no colour spans from syntect).
        let has_syntax_colour = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .any(|s| matches!(s.style.fg, Some(Color::Rgb(_, _, _))));
        assert!(
            !has_syntax_colour,
            "error tool result should not have coloured spans"
        );
        let body: String = lines[1..]
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(body.contains("```rust"), "error body should be verbatim");
    }

    #[test]
    fn assistant_text_with_reasoning_renders_reasoning_section() {
        let msg = SessionMessage::AssistantText {
            content: "The answer is 42.".into(),
            reasoning: Some("Let me think step by step.".into()),
            token_usage: None,
        };
        let lines = session_message_lines(&msg, 80);
        // The "Reasoning" heading should be on its own line
        assert_eq!(
            lines[0].to_string(),
            "Reasoning:",
            "first line should be the Reasoning heading: {:?}",
            lines[0].to_string()
        );
        // The reasoning body should be present
        let all_text: String = lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            all_text.contains("Let me think step by step."),
            "reasoning text should appear"
        );
        // The "Response" heading should appear before the answer
        assert!(
            all_text.contains("Response:"),
            "Response heading should appear"
        );
        // The answer text should appear after the Reasoning section
        assert!(
            all_text.contains("The answer is 42."),
            "answer text should appear"
        );
    }

    // ── UserText rendering (markdown) ──────────────────────────────────

    #[test]
    fn user_text_renders_plain_text_through_markdown_pipeline() {
        let msg = SessionMessage::UserText {
            content: "hello world".into(),
        };
        let lines = session_message_lines(&msg, 80);
        assert_eq!(lines.len(), 1, "plain text should produce one line");
        assert_eq!(
            lines[0].to_string(),
            "hello world",
            "content should be preserved verbatim"
        );
    }

    #[test]
    fn user_text_renders_code_block_with_syntax_highlighting() {
        let msg = SessionMessage::UserText {
            content: "```rust\nfn main() {}\n```".into(),
        };
        let lines = session_message_lines(&msg, 80);
        // Should have: ```rust, highlighted code line, ```
        assert!(
            lines.len() >= 3,
            "code block should produce at least 3 lines, got {}",
            lines.len()
        );
        assert!(
            lines[0].to_string().contains("```rust"),
            "first line should be the opening fence"
        );
        // The highlighted line should have colour spans from syntect
        let has_syntax_colour = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .any(|s| matches!(s.style.fg, Some(Color::Rgb(_, _, _))));
        assert!(
            has_syntax_colour,
            "Rust code in user text should have coloured spans"
        );
        assert!(
            lines.last().unwrap().to_string().contains("```"),
            "last line should be the closing fence"
        );
    }

    #[test]
    fn user_text_empty_produces_single_empty_line() {
        let msg = SessionMessage::UserText { content: "".into() };
        let lines = session_message_lines(&msg, 80);
        assert!(!lines.is_empty(), "should not return empty vec");
        assert_eq!(lines[0].width(), 0, "empty input → empty line");
    }

    #[test]
    fn user_text_multi_line_markdown_renders_all_blocks() {
        let msg = SessionMessage::UserText {
            content: "# heading\n\nparagraph\n\n- item1\n- item2".into(),
        };
        let lines = session_message_lines(&msg, 80);
        let all_text: String = lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(all_text.contains("# heading"), "should render heading");
        assert!(all_text.contains("paragraph"), "should render paragraph");
        assert!(all_text.contains("item1"), "should render list item");
        assert!(all_text.contains("item2"), "should render second list item");
    }

    #[test]
    fn assistant_text_without_reasoning_skips_reasoning_section() {
        let msg = SessionMessage::AssistantText {
            content: "Just an answer.".into(),
            reasoning: None,
            token_usage: None,
        };
        let lines = session_message_lines(&msg, 80);
        // First line should be the "Response" heading
        assert_eq!(
            lines[0].to_string(),
            "Response:",
            "first line should be the Response heading: {:?}",
            lines[0].to_string()
        );
        let all_text: String = lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            all_text.contains("Just an answer."),
            "answer text should appear"
        );
        assert!(
            !all_text.contains("reasoning"),
            "no reasoning section when reasoning is None"
        );
    }
}
