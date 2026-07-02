use tai_proto::SessionMessage;
use tai_tui::{
    MarkdownAlignment, MarkdownBlock, MarkdownDocument, MarkdownInline, StreamingText,
};

pub(crate) fn plain_text_lines(text: &str) -> Vec<String> {
    if text.is_empty() {
        vec![String::new()]
    } else {
        text.split('\n').map(|line| line.to_string()).collect()
    }
}

pub(crate) fn lines_height(lines: &[String], width: u16) -> usize {
    let width = width as usize;
    if width == 0 {
        return 0;
    }

    if lines.len() <= 1 && lines.iter().all(|line| line.is_empty()) {
        return 1;
    }

    lines
        .iter()
        .map(|line| wrapped_line_height(line, width))
        .sum::<usize>()
        .max(1)
}

pub(crate) fn session_message_lines(message: &SessionMessage, width: u16) -> Vec<String> {
    match message {
        SessionMessage::SystemText { content } => labeled_plain_text_lines("system", content),
        SessionMessage::UserText { content } => labeled_plain_text_lines("user", content),
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
            let mut lines = vec![format!(
                "tool-call: {}",
                tool_calls
                    .iter()
                    .map(|call| format!("{}({})", call.name, call.arguments_json))
                    .collect::<Vec<_>>()
                    .join(", ")
            )];
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
            prefixed_lines(label, plain_text_lines(&format!("{name}: {content}")))
        }
    }
}

pub(crate) fn streaming_text_lines(text: &StreamingText, width: u16) -> Vec<String> {
    let mut lines = vec![format!("[{}]", text.request_id)];

    if !text.reasoning.is_empty() {
        append_section(&mut lines, "reasoning", plain_text_lines(&text.reasoning));
    }

    if !text.answer.is_empty() {
        append_section(&mut lines, "answer", markdown_lines(&text.answer, width));
    }

    if text.reasoning.is_empty() && text.answer.is_empty() {
        lines.push(String::new());
    }

    lines
}

fn labeled_plain_text_lines(label: &'static str, text: &str) -> Vec<String> {
    prefixed_lines(label, plain_text_lines(text))
}

fn prefixed_lines(label: &'static str, body: Vec<String>) -> Vec<String> {
    let mut lines = Vec::new();
    append_section(&mut lines, label, body);
    lines
}

fn append_section(lines: &mut Vec<String>, label: &'static str, body: Vec<String>) {
    let mut body_iter = body.into_iter();
    if let Some(first) = body_iter.next() {
        lines.push(format!("{label}: {first}"));
    } else {
        lines.push(format!("{label}:"));
    }
    lines.extend(body_iter);
}

pub(crate) fn markdown_lines(markdown: &str, width: u16) -> Vec<String> {
    let document = MarkdownDocument::parse(markdown);
    let mut lines = Vec::new();
    render_markdown_blocks(&document.blocks, &mut lines, 0, width as usize);
    if lines.is_empty() {
        lines.push(String::new());
    }
    while matches!(lines.last(), Some(line) if line.is_empty()) {
        lines.pop();
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

fn render_markdown_blocks(
    blocks: &[MarkdownBlock],
    lines: &mut Vec<String>,
    indent: usize,
    width: usize,
) {
    for (index, block) in blocks.iter().enumerate() {
        if index > 0 {
            lines.push(String::new());
        }
        render_markdown_block(block, lines, indent, width);
    }
}

fn render_markdown_block(
    block: &MarkdownBlock,
    lines: &mut Vec<String>,
    indent: usize,
    width: usize,
) {
    match block {
        MarkdownBlock::Paragraph(content) => lines.extend(inlines_to_lines(content, indent, None)),
        MarkdownBlock::Heading { level, content } => {
            let prefix = Some(format!("{} ", "#".repeat(*level as usize)));
            lines.extend(inlines_to_lines(content, indent, prefix));
        }
        MarkdownBlock::CodeBlock { language, code } => {
            let header = language
                .as_deref()
                .map(|value| format!("```{value}"))
                .unwrap_or_else(|| "```".to_string());
            lines.push(indented_line(indent, header));
            for line in code.split('\n') {
                lines.push(indented_line(indent, line.to_string()));
            }
            lines.push(indented_line(indent, "```".to_string()));
        }
        MarkdownBlock::BlockQuote(blocks) => {
            let mut quoted = Vec::new();
            render_markdown_blocks(blocks, &mut quoted, 0, width);
            for line in quoted {
                lines.push(indented_line(indent, format!("> {line}")));
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
                    lines.push(format!("{}{}{}", " ".repeat(indent), marker, first));
                } else {
                    lines.push(format!("{}{}", " ".repeat(indent), marker));
                }
                for line in rendered_iter {
                    lines.push(format!("{}{}", " ".repeat(continuation_indent), line));
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

fn render_table_lines(
    alignments: &[MarkdownAlignment],
    header: &[Vec<MarkdownInline>],
    rows: &[Vec<Vec<MarkdownInline>>],
    indent: usize,
    width: usize,
) -> Vec<String> {
    let column_count = alignments
        .len()
        .max(header.len())
        .max(rows.iter().map(Vec::len).max().unwrap_or(0));
    if column_count == 0 {
        return vec![String::new()];
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
) -> String {
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
) -> String {
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
) -> Vec<String> {
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

fn indented_line(indent: usize, text: String) -> String {
    if indent > 0 {
        format!("{}{}", " ".repeat(indent), text)
    } else {
        text
    }
}

fn inlines_to_lines(
    inlines: &[MarkdownInline],
    indent: usize,
    prefix: Option<String>,
) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    if indent > 0 {
        current.push_str(&" ".repeat(indent));
    }
    if let Some(prefix) = prefix {
        current.push_str(&prefix);
    }
    render_inlines_to_lines(inlines, &mut lines, &mut current, indent);
    lines.push(current);
    lines
}

fn render_inlines_to_lines(
    inlines: &[MarkdownInline],
    lines: &mut Vec<String>,
    current: &mut String,
    indent: usize,
) {
    for inline in inlines {
        match inline {
            MarkdownInline::Text(text) | MarkdownInline::Code(text) => current.push_str(text),
            MarkdownInline::Emphasis(content) | MarkdownInline::Strong(content) => {
                render_inlines_to_lines(content, lines, current, indent)
            }
            MarkdownInline::Link {
                content,
                destination,
            } => {
                render_inlines_to_lines(content, lines, current, indent);
                current.push_str(&format!(" ({destination})"));
            }
            MarkdownInline::Image { alt, destination } => {
                current.push_str("[image: ");
                render_inlines_to_lines(alt, lines, current, indent);
                current.push_str(&format!("] ({destination})"));
            }
            MarkdownInline::LineBreak => {
                lines.push(std::mem::take(current));
                if indent > 0 {
                    current.push_str(&" ".repeat(indent));
                }
            }
        }
    }
}

fn wrapped_line_height(line: &str, width: usize) -> usize {
    if width == 0 {
        return 0;
    }
    let line_width = display_width(line);
    if line_width == 0 {
        1
    } else {
        line_width.div_ceil(width)
    }
}
