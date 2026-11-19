//! Data-table rendering: the nushell-style rounded frame, column sizing,
//! cell wrapping, and alignment.
//!
//! Cell content is rendered by [`inlines_to_lines`] — the same inline renderer
//! prose uses — so a cell keeps every inline style (bold, italic, code colour,
//! link emphasis) and wraps with the same [`LineJoin`] semantics the copy model
//! relies on. A table row's emphasis is therefore a property of its *cells*: the
//! header row's `BOLD` is handed to the inline renderer for each cell and never
//! applied to the assembled line, so the frame's borders and padding stay plain.

use super::{
    Line, LineChrome, LineJoin, MarkdownAlignment, MarkdownInline, Modifier, Span, Style,
    TableRowId, indented_line, indented_line_as_spans, inlines_to_lines,
};
use std::cell::Cell;

// ── Table identity ────────────────────────────────────────────────────────

thread_local! {
    /// Hands out each rendered data table a distinct ordinal.
    ///
    /// Thread-local, not process-global: rendering runs synchronously on the
    /// single UI thread (the render cache is rebuilt in place, never on a
    /// worker), so a counter local to that thread is enough to keep ordinals
    /// distinct for the whole session.  Were rendering ever moved onto a worker
    /// thread, the switch would need a shared atomic instead, or ids could
    /// collide across threads.
    static TABLE_ORDINAL: Cell<u32> = const { Cell::new(0) };
}

/// Allocate the next unique [`TableRowId::table`] ordinal for the current
/// render thread.
///
/// The id must be unique across the *whole* rendered session history, not just
/// within one `markdown_lines_joined` buffer: the selection detects a table's
/// contiguous line run by comparing ids across every turn, so two tables that
/// shared an id would be merged into one reading-order run.  A per-document line
/// offset cannot guarantee that — it repeats across turns, and across the
/// separate buffers a blockquote or list renders its nested blocks into — so a
/// monotonically increasing counter that never hands out the same value twice
/// is used instead.  It is thread-local (see [`TABLE_ORDINAL`]): valid because
/// every table in a session is rendered on the one UI thread.
pub(crate) fn next_table_id() -> u32 {
    TABLE_ORDINAL.with(|cell| {
        let id = cell.get();
        // `wrapping_add` is a theoretical guard only: exhausting the u32 space
        // would take billions of tables in one process.  It must not panic in a
        // release build if it ever did.
        cell.set(id.wrapping_add(1));
        id
    })
}

/// Map a table body row's positional index to its [`TableRowId::row`].
///
/// Body rows start at 1 (0 is the header), so `u32` is ample for any real
/// table.  An astronomically long one saturates to the largest non-sentinel
/// value rather than panicking or colliding with [`TableRowId::RULE`] (which
/// would merge those rows' cells in the selection).
fn body_row_index(index: usize) -> u32 {
    u32::try_from(index)
        .ok()
        .filter(|row| *row != TableRowId::RULE)
        .unwrap_or(TableRowId::RULE - 1)
}
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
    table_id: u32,
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
    let table_rows: Vec<Vec<&[MarkdownInline]>> =
        std::iter::once(normalize_table_row(header, column_count))
            .chain(
                rows.iter()
                    .map(|row| normalize_table_row(row, column_count)),
            )
            .collect();
    // `border_width`/`available`/`content_budget` are derived from the column
    // count alone, so they are settled before the cells are measured: `available`
    // is the widest a column could ever be, which is the width a cell is measured
    // at (see `cell_measure_width`).
    let border_width = column_count * 3 + 1;
    let available = width
        .saturating_sub(indent)
        .max(border_width + column_count);
    let content_budget = available.saturating_sub(border_width).max(column_count);
    let mut widths = vec![3usize; column_count];
    for row in &table_rows {
        for (index, cell) in row.iter().enumerate() {
            // `widths` has one entry per normalized column, and `row` was
            // normalized to exactly that column count, so `index` is in bounds.
            if let Some(w) = widths.get_mut(index) {
                *w = (*w).max(cell_measure_width(cell, available));
            }
        }
    }
    shrink_column_widths(&mut widths, content_budget);
    let header_alignment = normalized_alignments(alignments, column_count);
    let mut lines = Vec::new();
    let mut joins = Vec::new();
    let mut chrome = Vec::new();
    // Every frame/separator rule is a table line with no cells: it carries the
    // table id under the sentinel row so the selection tells it apart from a
    // non-table line without mistaking it for a data row.
    let rule_id = TableRowId {
        table: table_id,
        row: TableRowId::RULE,
    };
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
        rule_id,
    );
    // The header row is the table's first row and the only one whose *cells* are
    // drawn bold — the same emphasis nushell gives its column headers. The
    // emphasis is handed to the inline renderer per cell, so the frame's borders
    // and padding stay plain.
    if let Some(header_row) = table_rows.first() {
        let (header_lines, header_joins, header_chrome) = render_table_row_wrapped(
            header_row,
            &widths,
            &header_alignment,
            indent,
            Modifier::BOLD,
            TableRowId {
                table: table_id,
                row: 0,
            },
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
        rule_id,
    );
    for (index, row) in table_rows.iter().enumerate().skip(1) {
        // Body row `index` (0 is the header); `body_row_index` maps it to a
        // `u32` row id that is collision-free for any real table.
        let row_id = TableRowId {
            table: table_id,
            row: body_row_index(index),
        };
        let (row_lines, row_joins, row_chrome) = render_table_row_wrapped(
            row,
            &widths,
            &header_alignment,
            indent,
            Modifier::empty(),
            row_id,
        );
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
                rule_id,
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
        rule_id,
    );
    (lines, joins, chrome)
}

/// Push a table frame or inter-row separator rule: its whole span is
/// non-selectable chrome, so a copy over it yields nothing, and it is a fresh
/// line in the copy.  It carries `rule_id` so the selection keeps recognising it
/// as part of the same table.
fn push_rule_row(
    lines: &mut Vec<Line<'static>>,
    joins: &mut Vec<LineJoin>,
    chrome: &mut Vec<LineChrome>,
    line: Line<'static>,
    rule_id: TableRowId,
) {
    let mut c = LineChrome::default();
    c.push(0, line.width());
    c.set_table(rule_id);
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

/// Normalize a source row to exactly `column_count` cells.
///
/// A short row's missing cells become empty slices, which the inline renderer
/// renders as one empty display line — the same blank the old plain-text padding
/// produced. The cells are borrowed rather than flattened to text: they are
/// rendered once the column widths are known, so their inline styles survive to
/// the point of emission.
pub(crate) fn normalize_table_row(
    row: &[Vec<MarkdownInline>],
    column_count: usize,
) -> Vec<&[MarkdownInline]> {
    (0..column_count)
        .map(|index| row.get(index).map_or(&[][..], Vec::as_slice))
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
/// the cells' padding (`pad_aligned_spans`), not by the `:───` / `:───:` /
/// `───:` marks the GFM delimiter row uses. Those source-level marks used to be
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
    row: &[&[MarkdownInline]],
    widths: &[usize],
    alignments: &[MarkdownAlignment],
    indent: usize,
    emphasis: Modifier,
    row_id: TableRowId,
) -> (Vec<Line<'static>>, Vec<LineJoin>, Vec<LineChrome>) {
    let rendered_cells: Vec<Vec<CellLine>> = row
        .iter()
        .zip(widths.iter())
        .map(|(cell, width)| render_cell(cell, *width, emphasis))
        .collect();
    let row_height = rendered_cells
        .iter()
        .map(Vec::len)
        .max()
        .unwrap_or(1)
        .max(1);
    // Every display line of a table row is a fresh line in the copy: the
    // reading-order fill reconstructs each cell from the per-cell copy-joins
    // recorded on the row's chrome (below), not from a row-level join, so the
    // row-level join this vector carries is always `Break`.
    let joins = vec![LineJoin::Break; row_height];
    let mut lines = Vec::with_capacity(row_height);
    let mut chrome = Vec::with_capacity(row_height);
    for line_index in 0..row_height {
        // The row is assembled from spans that each keep their own style: the
        // frame glyphs and the cell padding are `Style::default()`, while a
        // cell's spans carry whatever the inline renderer gave them (plus the
        // row's emphasis). The frame must never inherit the emphasis — a bold
        // header should read as bold text in a plain box, not a bold box.
        let mut spans: Vec<Span<'static>> = Vec::new();
        // Track the row's display columns so each `│` border column is recorded
        // as chrome: the selection keeps the cell text and drops the borders.
        let mut col = 0usize;
        let mut row_chrome = LineChrome::default();
        row_chrome.set_table(row_id);
        spans.push(Span::styled("│", Style::default()));
        row_chrome.push(indent + col, indent + col + 1);
        col += 1;
        for column_index in 0..widths.len() {
            // `rendered_cells`/`alignments` carry one entry per column of
            // `widths` (rows are normalized to the column count), so these
            // lookups are in bounds; `.get()` keeps them total.
            let cell_line = rendered_cells
                .get(column_index)
                .and_then(|cell| cell.get(line_index));
            let Some(cell_width) = widths.get(column_index) else {
                continue;
            };
            // Record how this cell's text on this row line glues to the same
            // cell's text on the line above — a word-wrap seam (`Space`), a hard
            // mid-word split (`Join`), the cell's first line or an embedded
            // newline (`Break`) — aligned with the cell's selectable band so the
            // selection's reading-order fill can rejoin a wrapped cell to its
            // original text without guessing.
            row_chrome.push_cell_join(cell_line.map_or(LineJoin::Break, |(_, join)| *join));
            let cell_spans: &[Span<'static>] = cell_line.map_or(&[], |(spans, _join)| spans);
            // The cell was rendered at exactly the column width, so each of its
            // lines must fit it; an over-wide one would desync the frame from
            // its border columns. The only way to exceed it is a grapheme
            // cluster wider than the column, which the cluster reflow cannot
            // split any further.
            debug_assert!(
                cell_spans.iter().map(Span::width).sum::<usize>() <= *cell_width,
                "table cell line wider than its column"
            );
            // One padding space, the aligned cell text, then one padding space
            // and the cell's trailing border.
            spans.push(Span::styled(" ", Style::default()));
            col += 1;
            let aligned = pad_aligned_spans(
                cell_spans,
                *cell_width,
                alignments
                    .get(column_index)
                    .copied()
                    .unwrap_or(MarkdownAlignment::None),
            );
            for span in aligned {
                col += span.width();
                spans.push(span);
            }
            spans.push(Span::styled(" ", Style::default()));
            col += 1;
            spans.push(Span::styled("│", Style::default()));
            row_chrome.push(indent + col, indent + col + 1);
            col += 1;
        }
        lines.push(indented_line_as_spans(indent, spans));
        chrome.push(row_chrome);
    }
    (lines, joins, chrome)
}

/// One rendered cell display line: its styled spans and how it glues to the
/// cell's previous line (the copy's per-cell join).
type CellLine = (Vec<Span<'static>>, LineJoin);

/// Render one cell's inlines at `width` through the shared inline renderer.
///
/// `emphasis` is the row's baseline modifier — `Modifier::BOLD` for a header
/// row, empty for a body row. Handing it to the inline renderer stacks it onto
/// every *cell* span (so a header cell's nested `*italic*` still becomes
/// bold+italic) instead of onto the assembled row line, which is what keeps the
/// frame's borders and padding plain.
///
/// The renderer's [`LineChrome`] output is discarded: inline content records no
/// chrome, and [`render_table_row_wrapped`] builds the table's own (border bands
/// plus per-cell copy joins). Merging the two would double-count the bands the
/// selection slices cells on.
pub(crate) fn render_cell(
    inlines: &[MarkdownInline],
    width: usize,
    emphasis: Modifier,
) -> Vec<CellLine> {
    let (lines, joins, _chrome) = inlines_to_lines(inlines, 0, Vec::new(), width, emphasis);
    lines
        .into_iter()
        .zip(joins)
        .map(|(line, join)| (line.spans, join))
        .collect()
}

/// The display width a cell needs on one line — the widest of its hard-broken
/// segments.
///
/// Rendering *is* the measurement: `width` is the widest a table line could ever
/// be (the whole available width, before [`shrink_column_widths`] trims the
/// columns down to the content budget), so a cell that still wraps here is one
/// no column could hold on a single line anyway. Each rendered line's width is
/// taken with its leading and trailing whitespace trimmed, because the renderer
/// can add padding of its own — display math is centred within `width` — and
/// that padding is not content.
///
/// Measuring through the renderer is the point: there is no second text
/// projection that could drift from what is actually emitted. Style does not
/// change [`Span::width`], so measuring with [`Modifier::empty`] and emitting
/// with the row's emphasis wrap identically.
pub(crate) fn cell_measure_width(inlines: &[MarkdownInline], width: usize) -> usize {
    render_cell(inlines, width, Modifier::empty())
        .iter()
        .map(|(spans, _join)| trimmed_line_width(spans))
        .max()
        .unwrap_or(0)
}

/// Display width of a rendered line's text with its leading and trailing
/// whitespace trimmed, computed straight from the spans with no intermediate
/// `String`.
///
/// Whitespace is matched with [`char::is_whitespace`] — the same rule
/// [`str::trim`] uses — and per-char widths are summed exactly as
/// [`unicode_width::UnicodeWidthStr::width`] does, so the result equals
/// `display_width(text.trim())` over the concatenated span text.
pub(crate) fn trimmed_line_width(spans: &[Span<'static>]) -> usize {
    let char_width = |ch: char| unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
    let total: usize = spans
        .iter()
        .flat_map(|span| span.content.chars())
        .map(char_width)
        .sum();
    let leading: usize = spans
        .iter()
        .flat_map(|span| span.content.chars())
        .take_while(|ch| ch.is_whitespace())
        .map(char_width)
        .sum();
    let trailing: usize = spans
        .iter()
        .rev()
        .flat_map(|span| span.content.chars().rev())
        .take_while(|ch| ch.is_whitespace())
        .map(char_width)
        .sum();
    // Subtracting both runs from the total is correct when there is any content;
    // an all-whitespace line makes `leading` and `trailing` each equal the total,
    // so the saturating steps clamp the double subtraction to zero.
    total.saturating_sub(leading).saturating_sub(trailing)
}

/// Pad `spans` to exactly `width` display columns according to `alignment`.
///
/// The padding is emitted as plain [`Style::default()`] space spans, so a row's
/// emphasis never reaches the frame: a padded header cell reads as bold text in
/// a plain box.
pub(crate) fn pad_aligned_spans(
    spans: &[Span<'static>],
    width: usize,
    alignment: MarkdownAlignment,
) -> Vec<Span<'static>> {
    let text_width: usize = spans.iter().map(Span::width).sum();
    if text_width >= width {
        return spans.to_vec();
    }
    let remaining = width - text_width;
    let (left, right) = match alignment {
        MarkdownAlignment::Right => (remaining, 0),
        MarkdownAlignment::Center => (remaining / 2, remaining.div_ceil(2)),
        MarkdownAlignment::Left | MarkdownAlignment::None => (0, remaining),
    };
    let mut padded = Vec::with_capacity(spans.len() + 2);
    if left > 0 {
        padded.push(Span::styled(" ".repeat(left), Style::default()));
    }
    padded.extend(spans.iter().cloned());
    if right > 0 {
        padded.push(Span::styled(" ".repeat(right), Style::default()));
    }
    padded
}
