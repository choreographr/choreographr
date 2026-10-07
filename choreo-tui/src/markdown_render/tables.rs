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
    push_rule_row(
        &mut lines,
        &mut joins,
        &mut chrome,
        table_border_line(
            TABLE_BORDERS.top_left,
            TABLE_BORDERS.top_mid,
            TABLE_BORDERS.top_right,
            &widths,
            indent,
        ),
    );
    // The header row is the table's first row and the only one drawn bold —
    // the same emphasis nushell gives its column headers.
    if let Some(header_row) = table_rows.first() {
        let (header_lines, header_joins, header_chrome) = render_table_row_wrapped(
            header_row,
            &widths,
            &header_alignment,
            indent,
            Modifier::BOLD,
        );
        lines.extend(header_lines);
        joins.extend(header_joins);
        chrome.extend(header_chrome);
    }
    push_rule_row(
        &mut lines,
        &mut joins,
        &mut chrome,
        table_separator_line(&widths, indent),
    );
    for (index, row) in table_rows.iter().enumerate().skip(1) {
        let (row_lines, row_joins, row_chrome) =
            render_table_row_wrapped(row, &widths, &header_alignment, indent, Modifier::empty());
        lines.extend(row_lines);
        joins.extend(row_joins);
        chrome.extend(row_chrome);
        if index < table_rows.len() - 1 {
            // Inter-row junctions stay square: nushell's rounded preset
            // rounds only the outer corners, never the T-junctions.
            push_rule_row(
                &mut lines,
                &mut joins,
                &mut chrome,
                table_border_line('├', '┼', '┤', &widths, indent),
            );
        }
    }
    push_rule_row(
        &mut lines,
        &mut joins,
        &mut chrome,
        table_border_line(
            TABLE_BORDERS.bottom_left,
            TABLE_BORDERS.bottom_mid,
            TABLE_BORDERS.bottom_right,
            &widths,
            indent,
        ),
    );
    (lines, joins, chrome)
}

/// Push a table frame or inter-row separator rule: its whole span is
/// non-selectable chrome, so a copy over it yields nothing, and it is a fresh
/// line in the copy.
fn push_rule_row(
    lines: &mut Vec<Line<'static>>,
    joins: &mut Vec<LineJoin>,
    chrome: &mut Vec<LineChrome>,
    line: Line<'static>,
) {
    let mut c = LineChrome::default();
    c.push(0, line.width());
    lines.push(line);
    joins.push(LineJoin::Break);
    chrome.push(c);
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
                .map_or_default(|cell| inline_plain_text(cell))
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
    let wrapped_cells: Vec<Vec<(String, LineJoin)>> = row
        .iter()
        .zip(widths.iter())
        .map(|(cell, width)| wrap_cell_text(cell, *width))
        .collect();
    let row_height = wrapped_cells.iter().map(Vec::len).max().unwrap_or(1).max(1);
    // Row copy-joins: the first row is a fresh line, and each continuation
    // joins as the wrap that produced it did — a word-boundary seam re-inserts
    // the one separating space, a hard mid-word split joins directly, an
    // embedded newline is a break.  When the cells on a line disagree, the
    // strongest wins, so a hard split is never merged with a space.
    let mut joins = Vec::with_capacity(row_height);
    // The first row is always a fresh line; continuations default to a
    // word-wrap seam (`Space`) and are upgraded below.
    joins.push(LineJoin::Break);
    joins.resize(row_height, LineJoin::Space);
    for (line_index, row_join) in joins.iter_mut().enumerate().skip(1) {
        for cell in &wrapped_cells {
            if let Some((_, cell_join)) = cell.get(line_index) {
                match cell_join {
                    LineJoin::Break => {
                        *row_join = LineJoin::Break;
                        break;
                    }
                    LineJoin::Join => {
                        if *row_join != LineJoin::Break {
                            *row_join = LineJoin::Join;
                        }
                    }
                    LineJoin::Space => {}
                }
            }
        }
    }
    let mut lines = Vec::with_capacity(row_height);
    let mut chrome = Vec::with_capacity(row_height);
    for line_index in 0..row_height {
        let mut text = String::new();
        // Track the row's display columns so each `│` border column is recorded
        // as chrome: the selection keeps the cell text and drops the borders.
        let mut col = 0usize;
        let mut row_chrome = LineChrome::default();
        text.push('│');
        row_chrome.push(indent + col, indent + col + 1);
        col += 1;
        for column_index in 0..widths.len() {
            // `wrapped_cells`/`alignments` carry one entry per column of
            // `widths` (rows are normalized to the column count), so these
            // lookups are in bounds; `.get()` keeps them total.
            let cell_line = wrapped_cells
                .get(column_index)
                .and_then(|cell| cell.get(line_index))
                .map_or("", |(text, _join)| text.as_str());
            let Some(cell_width) = widths.get(column_index) else {
                continue;
            };
            // One padding space, the aligned cell text, then one padding space
            // and the cell's trailing border.
            text.push(' ');
            col += 1;
            let padded = pad_aligned(
                cell_line,
                *cell_width,
                alignments
                    .get(column_index)
                    .copied()
                    .unwrap_or(MarkdownAlignment::None),
            );
            col += display_width(&padded);
            text.push_str(&padded);
            text.push(' ');
            col += 1;
            text.push('│');
            row_chrome.push(indent + col, indent + col + 1);
            col += 1;
        }
        lines.push(indented_styled_line(indent, text, modifier));
        chrome.push(row_chrome);
    }
    (lines, joins, chrome)
}

/// Wrap one cell's text to `width`, returning each display line paired with
/// the [`LineJoin`] describing how it glues to the line before it (so the copy
/// can rejoin a wrapped cell to its original text): `Break` for the first line
/// of a source segment, `Space` for a word-boundary wrap, `Join` for a hard
/// mid-word split.
pub(crate) fn wrap_cell_text(text: &str, width: usize) -> Vec<(String, LineJoin)> {
    let width = width.max(1);
    let mut lines: Vec<(String, LineJoin)> = Vec::new();
    for raw_line in text.split('\n') {
        let mut current = String::new();
        let mut current_width = 0;
        // The join of the line currently being built; a fresh source segment
        // is a break, a word-wrap seam becomes a space, a hard split joins.
        let mut current_join = LineJoin::Break;
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
            } else {
                // Flush the current line (the wrap is a word boundary → the
                // copy re-inserts the separating space) before placing `word`.
                if !current.is_empty() {
                    lines.push((std::mem::take(&mut current), current_join));
                    current_join = LineJoin::Space;
                    current_width = 0;
                }
                if word_width <= width {
                    current.push_str(word);
                    current_width = word_width;
                } else {
                    // The word alone exceeds the width: hard-split it, its
                    // chunks joined directly (no space exists in the source).
                    for (chunk_index, chunk) in
                        split_word_to_width(word, width).into_iter().enumerate()
                    {
                        if chunk_index > 0 {
                            lines.push((std::mem::take(&mut current), current_join));
                            current_join = LineJoin::Join;
                        }
                        current_width += display_width(&chunk);
                        current.push_str(&chunk);
                    }
                }
            }
        }
        lines.push((current, current_join));
    }
    if lines.is_empty() {
        lines.push((String::new(), LineJoin::Break));
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
