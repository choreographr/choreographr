//! Data-table rendering: the nushell-style rounded frame, column sizing,
//! cell wrapping, and alignment.

use super::{
    Line, LineChrome, LineJoin, MarkdownAlignment, MarkdownInline, Modifier, Span, Style,
    display_width, indented_line, indented_styled_line, render_math_pretty, split_word_to_width,
};
// ── Table rendering ───────────────────────────────────────────────────────

/// The box-drawing glyphs for a data table's outer frame.
///
/// Only the four outer corners differ from a square table — nushell's
/// "rounded" preset keeps the `│`/`─` strokes and the `┬`/`┴`/`├`/`┤`/`┼`
/// T-junctions identical and rounds only the frame's corners, so those
/// junctions stay plain literals at their use sites below.
pub(crate) struct TableBorders {
    // The four rounded corners are `pub(crate)` so the fenced-code box
    // (`render_code_box`) draws its frame from the very same glyphs the tables
    // use — one source of truth for the rounded frame.  The junctions stay
    // private: only the tables draw them.
    pub(crate) top_left: char,
    top_mid: char,
    pub(crate) top_right: char,
    pub(crate) bottom_left: char,
    bottom_mid: char,
    pub(crate) bottom_right: char,
}

pub(crate) const TABLE_BORDERS: TableBorders = TableBorders {
    top_left: '╭',
    top_mid: '┬',
    top_right: '╮',
    bottom_left: '╰',
    bottom_mid: '┴',
    bottom_right: '╯',
};

pub(crate) fn render_table_lines(
    alignments: &[MarkdownAlignment],
    header: &[Vec<MarkdownInline>],
    rows: &[Vec<Vec<MarkdownInline>>],
    indent: usize,
    width: usize,
) -> (Vec<Line<'static>>, Vec<LineJoin>, Vec<LineChrome>) {
    let column_count = alignments
        .len()
        .max(header.len())
        .max(rows.iter().map(Vec::len).max().unwrap_or(0));
    if column_count == 0 {
        return (
            vec![Line::from(Span::styled(String::new(), Style::default()))],
            vec![LineJoin::Break],
            vec![LineChrome::default()],
        );
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
                // `widths` has one entry per normalized column, and `row` was
                // normalized to exactly that column count, so `index` is in
                // bounds.
                if let Some(w) = widths.get_mut(index) {
                    *w = (*w).max(display_width(line));
                }
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
    let mut joins = Vec::new();
    let mut chrome = Vec::new();
    lines.push(table_border_line(
        TABLE_BORDERS.top_left,
        TABLE_BORDERS.top_mid,
        TABLE_BORDERS.top_right,
        &widths,
        indent,
    ));
    joins.push(LineJoin::Break);
    chrome.push(LineChrome::default());
    // The header row is the table's first row and the only one drawn bold —
    // the same emphasis nushell gives its column headers.
    let (header_lines, header_joins, header_chrome) = table_rows
        .first()
        .map(|row| {
            render_table_row_wrapped(row, &widths, &header_alignment, indent, Modifier::BOLD)
        })
        .unwrap_or_default();
    lines.extend(header_lines);
    joins.extend(header_joins);
    chrome.extend(header_chrome);
    lines.push(table_separator_line(&widths, indent));
    joins.push(LineJoin::Break);
    chrome.push(LineChrome::default());
    for (index, row) in table_rows.iter().enumerate().skip(1) {
        let (row_lines, row_joins, row_chrome) =
            render_table_row_wrapped(row, &widths, &header_alignment, indent, Modifier::empty());
        lines.extend(row_lines);
        joins.extend(row_joins);
        chrome.extend(row_chrome);
        if index < table_rows.len() - 1 {
            // Inter-row junctions stay square: nushell's rounded preset
            // rounds only the outer corners, never the T-junctions.
            lines.push(table_border_line('├', '┼', '┤', &widths, indent));
            joins.push(LineJoin::Break);
            chrome.push(LineChrome::default());
        }
    }
    lines.push(table_border_line(
        TABLE_BORDERS.bottom_left,
        TABLE_BORDERS.bottom_mid,
        TABLE_BORDERS.bottom_right,
        &widths,
        indent,
    ));
    joins.push(LineJoin::Break);
    chrome.push(LineChrome::default());
    (lines, joins, chrome)
}

pub(crate) fn normalized_alignments(
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

pub(crate) fn normalize_table_row(row: &[Vec<MarkdownInline>], column_count: usize) -> Vec<String> {
    (0..column_count)
        .map(|index| {
            row.get(index)
                .map(|cell| inline_plain_text(cell))
                .unwrap_or_default()
        })
        .collect()
}

pub(crate) fn shrink_column_widths(widths: &mut [usize], budget: usize) {
    let min_width = 3usize;
    while widths.iter().sum::<usize>() > budget {
        if let Some((index, _)) = widths
            .iter()
            .enumerate()
            .filter(|(_, width)| **width > min_width)
            .max_by_key(|(_, width)| **width)
        {
            // The index comes from enumerating `widths` itself, so it is
            // always in bounds; `saturating_sub` cannot saturate because the
            // filter above guarantees `width > min_width`.
            if let Some(width) = widths.get_mut(index) {
                *width = width.saturating_sub(1);
            }
        } else {
            break;
        }
    }
}

pub(crate) fn table_border_line(
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

/// The rule between the header row and the body.
///
/// A uniform `├───┼───┤` for every column: column alignment is expressed by
/// the cells' padding (`pad_aligned`), not by the `:───` / `:───:` / `───:`
/// marks the GFM delimiter row uses. Those source-level marks used to be
/// echoed into the rendered rule, which reads as stray punctuation next to a
/// nushell-style frame; the rule is now plain.
pub(crate) fn table_separator_line(widths: &[usize], indent: usize) -> Line<'static> {
    let mut text = String::new();
    text.push('├');
    for (index, width) in widths.iter().enumerate() {
        // One `─` per display column of the cell plus its two padding cells,
        // matching `table_border_line`'s span so the junctions line up.
        text.push_str(&"─".repeat(*width + 2));
        text.push(if index + 1 == widths.len() {
            '┤'
        } else {
            '┼'
        });
    }
    indented_line(indent, text)
}

pub(crate) fn render_table_row_wrapped(
    row: &[String],
    widths: &[usize],
    alignments: &[MarkdownAlignment],
    indent: usize,
    modifier: Modifier,
) -> (Vec<Line<'static>>, Vec<LineJoin>, Vec<LineChrome>) {
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
            // `wrapped_cells`/`alignments` carry one entry per column of
            // `widths` (rows are normalized to the column count), so these
            // lookups are in bounds; `.get()` keeps them total.
            let cell_line = wrapped_cells
                .get(column_index)
                .and_then(|cell| cell.get(line_index))
                .map_or("", String::as_str);
            let Some(cell_width) = widths.get(column_index) else {
                continue;
            };
            text.push(' ');
            text.push_str(&pad_aligned(
                cell_line,
                *cell_width,
                alignments
                    .get(column_index)
                    .copied()
                    .unwrap_or(MarkdownAlignment::None),
            ));
            text.push(' ');
            text.push('│');
        }
        lines.push(indented_styled_line(indent, text, modifier));
    }
    // Every table row (visual or wrapped) is a distinct line in the copy:
    // the cell borders and padding are per-row rendering chrome that must
    // not be re-glueed into a paragraph.
    let joins = vec![LineJoin::Break; lines.len()];
    // No renderer-emitted chrome yet (borders are still handled by the copy
    // path); keep the buffer aligned with one default entry per row.
    let chrome = vec![LineChrome::default(); lines.len()];
    (lines, joins, chrome)
}

pub(crate) fn wrap_cell_text(text: &str, width: usize) -> Vec<String> {
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

pub(crate) fn pad_aligned(text: &str, width: usize, alignment: MarkdownAlignment) -> String {
    let text_width = display_width(text);
    if text_width >= width {
        return text.to_string();
    }
    let remaining = width - text_width;
    let (left, right) = match alignment {
        MarkdownAlignment::Right => (remaining, 0),
        MarkdownAlignment::Center => (remaining / 2, remaining.div_ceil(2)),
        MarkdownAlignment::Left | MarkdownAlignment::None => (0, remaining),
    };
    format!("{}{}{}", " ".repeat(left), text, " ".repeat(right))
}

pub(crate) fn inline_plain_text(inlines: &[MarkdownInline]) -> String {
    let mut text = String::new();
    append_inline_plain_text(inlines, &mut text);
    text
}

pub(crate) fn append_inline_plain_text(inlines: &[MarkdownInline], text: &mut String) {
    for inline in inlines {
        match inline {
            MarkdownInline::Text(value) | MarkdownInline::Code(value) => text.push_str(value),
            MarkdownInline::InlineMath(value) | MarkdownInline::DisplayMath(value) => {
                // Table cells are rendered as plain text; pretty-print math so
                // it reads like the surrounding cell content instead of raw
                // LaTeX source.
                text.push_str(&render_math_pretty(value));
            }
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
                if destination.is_empty() {
                    text.push(']');
                } else {
                    text.push_str("] (");
                    text.push_str(destination);
                    text.push(')');
                }
            }
            MarkdownInline::LineBreak => text.push('\n'),
        }
    }
}
