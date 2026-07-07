use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use syntect::easy::HighlightLines;
use tai_proto::SessionMessage;
use tai_tui::{MarkdownAlignment, MarkdownBlock, MarkdownDocument, MarkdownInline, StreamingText};

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
    static HIGHLIGHT_CACHE: OnceLock<Mutex<HashMap<(String, String), Vec<Line<'static>>>>> =
        OnceLock::new();
    let cache = HIGHLIGHT_CACHE.get_or_init(|| Mutex::new(HashMap::new()));

    let key = (language.unwrap_or("").to_string(), code.to_string());

    // Fast path: return a clone of the cached result without running syntect.
    {
        let guard = cache.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(cached) = guard.get(&key) {
            return cached.clone();
        }
    }

    let ss = syntax_set();

    // Look up the syntax definition by the language token.  If the token
    // isn't recognised (or was omitted), use the built-in "Plain Text"
    // syntax which emits a single unstyled span per line.
    let syntax = language
        .and_then(|lang| ss.find_syntax_by_token(lang))
        .unwrap_or_else(|| ss.find_syntax_plain_text());

    let theme = highlight_theme();
    let mut highlighter = HighlightLines::new(syntax, theme);
    let mut result = Vec::with_capacity(code.len().max(1));

    for line in code.split('\n') {
        let Ok(ranges) = highlighter.highlight_line(line, ss) else {
            // If highlighting fails for a line, emit it as plain text.
            result.push(Line::from(Span::styled(
                line.to_string(),
                Style::default(),
            )));
            continue;
        };

        let spans: Vec<Span<'static>> = ranges
            .into_iter()
            .map(|(style, text)| {
                Span::styled(text.to_string(), Style::default().fg(to_ratatui_color(style.foreground)))
            })
            .collect();

        result.push(Line::from(spans));
    }

    // Bounded cache: allows up to 200 entries.  When full, new entries are
    // not inserted so the most frequently seen code blocks stay cached.
    const MAX_CACHE_ENTRIES: usize = 200;
    {
        let mut guard = cache.lock().unwrap_or_else(|e| e.into_inner());
        if guard.len() < MAX_CACHE_ENTRIES {
            guard.insert(key, result.clone());
        }
    }

    result
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
        SessionMessage::SystemText { content } => labeled_text_lines("system", content),
        SessionMessage::UserText { content } => labeled_text_lines("user", content),
        SessionMessage::AssistantText { content } => {
            let body = markdown_lines(content, width);
            prefixed_lines("assistant", body)
        }
        SessionMessage::AssistantToolUse {
            content,
            tool_calls,
            reasoning_content,
            reasoning,
            reasoning_text,
        } => {
            let mut lines = vec![Line::from(Span::styled(
                format!(
                    "tool-call: {}",
                    tool_calls
                        .iter()
                        .map(|call| format!("{}({})", call.name, call.arguments_json))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                Style::default(),
            ))];
            if let Some(reasoning_text) = reasoning_content
                .as_deref()
                .or(reasoning.as_deref())
                .or(reasoning_text.as_deref())
                .filter(|value| !value.trim().is_empty())
            {
                append_section(&mut lines, "reasoning", plain_text_lines(reasoning_text));
            }
            if let Some(content) = content.as_deref().filter(|value| !value.trim().is_empty()) {
                append_section(&mut lines, "content", markdown_lines(content, width));
            }
            lines
        }
        SessionMessage::ToolResult {
            name,
            content,
            is_error,
            ..
        } => {
            let label = if *is_error {
                "tool error"
            } else {
                "tool result"
            };
            // Header line with the label and tool name.
            let mut lines = vec![Line::from(Span::styled(
                format!("{label}: {name}"),
                Style::default(),
            ))];
            if !content.is_empty() {
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
        SessionMessage::DisplayedImage(record) => {
            vec![Line::from(Span::styled(
                format!(
                    "[displayed image: {} ({}x{})]",
                    record.metadata.mime_type,
                    record.metadata.width,
                    record.metadata.height,
                ),
                Style::default(),
            ))]
        }
    }
}

pub(crate) fn streaming_text_lines(text: &StreamingText, width: u16) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(Span::styled(
        format!("[{}]", text.request_id),
        Style::default(),
    ))];

    if !text.reasoning.is_empty() {
        append_section(&mut lines, "reasoning", plain_text_lines(&text.reasoning));
    }

    if !text.answer.is_empty() {
        append_section(&mut lines, "answer", markdown_lines(&text.answer, width));
    }

    if text.reasoning.is_empty() && text.answer.is_empty() {
        lines.push(Line::from(Span::styled(String::new(), Style::default())));
    }

    lines
}

// ── Internal helpers ──────────────────────────────────────────────────────

fn labeled_text_lines(label: &'static str, text: &str) -> Vec<Line<'static>> {
    prefixed_lines(label, plain_text_lines(text))
}

fn prefixed_lines(label: &'static str, body: Vec<Line<'static>>) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    append_section(&mut lines, label, body);
    lines
}

fn append_section(lines: &mut Vec<Line<'static>>, label: &'static str, body: Vec<Line<'static>>) {
    let mut body_iter = body.into_iter();
    if let Some(first) = body_iter.next() {
        let label_text = format!("{label}: ");
        // Move spans out of `first` instead of cloning — body is consumed
        // by into_iter() so no other code needs the original.
        let mut first_spans = first.spans;
        first_spans.insert(0, Span::styled(label_text, Style::default()));
        lines.push(Line::from(first_spans));
    } else {
        lines.push(Line::from(Span::styled(
            format!("{label}:"),
            Style::default(),
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
            lines.extend(inlines_to_lines(content, indent, None))
        }
        MarkdownBlock::Heading { level, content } => {
            let prefix = Some(format!("{} ", "#".repeat(*level as usize)));
            lines.extend(inlines_to_lines(content, indent, prefix));
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
            MarkdownInline::Text(value) | MarkdownInline::Code(value) => text.push_str(value),
            MarkdownInline::Emphasis(content) | MarkdownInline::Strong(content) => {
                append_inline_plain_text(content, text)
            }
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
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut current_spans: Vec<Span<'static>> = Vec::new();
    if indent > 0 {
        current_spans.push(Span::styled(" ".repeat(indent), Style::default()));
    }
    if let Some(prefix) = prefix {
        current_spans.push(Span::styled(prefix, Style::default()));
    }
    render_inlines_to_lines(inlines, &mut lines, &mut current_spans, indent);
    lines.push(Line::from(std::mem::take(&mut current_spans)));
    lines
}

fn render_inlines_to_lines(
    inlines: &[MarkdownInline],
    lines: &mut Vec<Line<'static>>,
    current: &mut Vec<Span<'static>>,
    indent: usize,
) {
    for inline in inlines {
        match inline {
            MarkdownInline::Text(text) | MarkdownInline::Code(text) => {
                current.push(Span::styled(text.clone(), Style::default()));
            }
            MarkdownInline::Emphasis(content) | MarkdownInline::Strong(content) => {
                render_inlines_to_lines(content, lines, current, indent)
            }
            MarkdownInline::Link {
                content,
                destination,
            } => {
                render_inlines_to_lines(content, lines, current, indent);
                current.push(Span::styled(
                    format!(" ({destination})"),
                    Style::default(),
                ));
            }
            MarkdownInline::Image { alt, destination } => {
                current.push(Span::styled("[image: ".to_string(), Style::default()));
                render_inlines_to_lines(alt, lines, current, indent);
                current.push(Span::styled(
                    format!("] ({destination})"),
                    Style::default(),
                ));
            }
            MarkdownInline::LineBreak => {
                lines.push(Line::from(std::mem::take(current)));
                if indent > 0 {
                    current.push(Span::styled(" ".repeat(indent), Style::default()));
                }
            }
        }
    }
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
        let has_colour = lines.iter().flat_map(|l| l.spans.iter()).any(|s| {
            matches!(s.style.fg, Some(Color::Rgb(_, _, _)))
        });
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
        let lines = vec![
            Line::from("short"),
            Line::from("a".repeat(50)),
        ];
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
        let result = inlines_to_lines(&inlines, 0, None);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].to_string(), "hello");
    }

    #[test]
    fn inlines_to_lines_with_indent_and_prefix() {
        let inlines = vec![MarkdownInline::Text("world".to_string())];
        let result = inlines_to_lines(&inlines, 2, Some("# ".to_string()));
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
        let result = inlines_to_lines(&inlines, 0, None);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].to_string(), "a");
        assert_eq!(result[1].to_string(), "b");
    }

    // ── to_ratatui_color ─────────────────────────────────────────────────

    #[test]
    fn to_ratatui_color_opaque() {
        let c = to_ratatui_color(syntect::highlighting::Color { r: 255, g: 0, b: 0, a: 255 });
        assert_eq!(c, Color::Rgb(255, 0, 0));
    }

    #[test]
    fn to_ratatui_color_transparent() {
        let c = to_ratatui_color(syntect::highlighting::Color { r: 255, g: 0, b: 0, a: 0 });
        assert_eq!(c, Color::Reset);
    }

    #[test]
    fn to_ratatui_color_semi_transparent() {
        let c = to_ratatui_color(syntect::highlighting::Color { r: 255, g: 0, b: 0, a: 100 });
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
        // Line 1: ```rust
        assert!(lines[1].to_string().contains("```rust"), "{}", lines[1].to_string());
        // The highlighted line for "fn main() {}" should have colour spans
        let has_colour = lines.iter().flat_map(|l| l.spans.iter()).any(|s| {
            matches!(s.style.fg, Some(Color::Rgb(_, _, _)))
        });
        assert!(has_colour, "Rust code in tool result should have coloured spans");
        // The closing fence and the output should be present
        let all_text: String = lines.iter().map(|l| l.to_string()).collect::<Vec<_>>().join("\n");
        assert!(all_text.contains("```"), "should contain closing fence");
        assert!(all_text.contains("hello"), "should contain execution output");
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
        let body: String = lines[1..].iter().map(|l| l.to_string()).collect::<Vec<_>>().join("\n");
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
        let has_syntax_colour = lines.iter().flat_map(|l| l.spans.iter()).any(|s| {
            matches!(s.style.fg, Some(Color::Rgb(_, _, _)))
        });
        assert!(!has_syntax_colour, "error tool result should not have coloured spans");
        let body: String = lines[1..].iter().map(|l| l.to_string()).collect::<Vec<_>>().join("\n");
        assert!(body.contains("```rust"), "error body should be verbatim");
    }
}
