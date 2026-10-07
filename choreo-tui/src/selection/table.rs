//! Data-table selection: cell model, reading-order fill, and the table
//! highlight.
//!
//! A run of contiguous table lines (its data rows and frame/separator rules
//! all carry the same `TableRowId::table` id) is not selected as a plain line
//! rectangle: it is copied and highlighted by *reading-order fill*.  The
//! table's cells are read row-major (each row left-to-right, rows top to
//! bottom); the selection is the linear run of cells from the cell under the
//! anchor to the cell under the head, inclusive — every cell between copied
//! whole, the two end cells sliced at the anchor/head point, one cell per line,
//! blank-line separated.  [`table_highlight`] drives the draw-time highlight
//! from the same [`pick_table_run`] the copy uses, so the box shown is exactly
//! the box copied.

use crate::markdown_render::{LineChrome, LineJoin};
use crate::state::{App, SessionDisplayState};
use std::collections::{BTreeMap, BTreeSet};

use super::{
    cached_rendered_turn, resolve_line, selectable_intervals, selection_range,
    slice_line_columns_trimmed,
};

/// The table a content line belongs to (its `table` id), or `None` when the
/// line is not part of a table.  A table's data rows and its frame/separator
/// rules all report the same id, so a contiguous run of them is one table.
fn table_id_at(display: &SessionDisplayState, vp_width: usize, content_line: usize) -> Option<u32> {
    let (turn_idx, line_idx) = resolve_line(display, vp_width, content_line)?;
    let rendered = cached_rendered_turn(display, turn_idx, vp_width)?;
    rendered
        .chrome_ranges
        .get(line_idx)
        .and_then(LineChrome::table)
        .map(|id| id.table)
}

/// Walk every maximal run of contiguous table lines in the content-line range
/// `[start, end]` (inclusive), invoking `f(lo, hi)` for each run (`hi`
/// exclusive).  A run stops at the first line that is not the same table (or
/// is not a table at all).  Shared by the copy and the highlight so both see
/// exactly the same runs — the "walk contiguous lines with the same table id"
/// loop lives here only.
pub(crate) fn for_each_table_run(
    display: &SessionDisplayState,
    vp_width: usize,
    start: usize,
    end: usize,
    mut f: impl FnMut(usize, usize),
) {
    if start > end {
        return;
    }
    let mut content_line = start;
    while content_line <= end {
        let Some(table) = table_id_at(display, vp_width, content_line) else {
            content_line += 1;
            continue;
        };
        let run_start = content_line;
        content_line += 1;
        // Extend the run over every following line that is the same table;
        // `content_line` ends on the first line that is not (or past `end`).
        while content_line <= end && table_id_at(display, vp_width, content_line) == Some(table) {
            content_line += 1;
        }
        f(run_start, content_line);
    }
}

/// One display line of a data-table cell: where its text lives in the render
/// cache.
struct CellLine {
    /// The global content line this display line occupies.
    content_line: usize,
    turn_idx: usize,
    line_idx: usize,
    /// The cell's display-column band on this line (padding included).
    band: (usize, usize),
    /// How this display line glues to the cell's previous line (the renderer's
    /// per-cell copy-join): `Space` at a word-wrap seam, `Join` at a hard
    /// mid-word split, `Break` at the cell's first line or an embedded newline.
    join: LineJoin,
}

/// One data-table cell: the display lines it spans (its wrapped pieces, in
/// order).
struct TableCell {
    lines: Vec<CellLine>,
}

/// Build the row-major cells of the table run `[lo, hi)` from the render cache.
/// Frame/separator rules carry no cells and are skipped.
fn table_cells(
    display: &SessionDisplayState,
    vp_width: usize,
    lo: usize,
    hi: usize,
) -> Vec<TableCell> {
    let mut cells: Vec<TableCell> = Vec::new();
    let mut cur_row: Option<u32> = None;
    let mut row_start = 0usize;
    for content_line in lo..hi {
        let Some((turn_idx, line_idx)) = resolve_line(display, vp_width, content_line) else {
            continue;
        };
        let Some(rendered) = cached_rendered_turn(display, turn_idx, vp_width) else {
            continue;
        };
        let Some(chrome) = rendered.chrome_ranges.get(line_idx).cloned() else {
            continue;
        };
        let Some(id) = chrome.table() else {
            continue;
        };
        if id.is_rule() {
            continue;
        }
        let Some(base) = rendered.content_ranges.get(line_idx).copied().flatten() else {
            continue;
        };
        let bands = selectable_intervals(base, &chrome);
        if cur_row != Some(id.row) {
            cur_row = Some(id.row);
            row_start = cells.len();
            for _ in 0..bands.len() {
                cells.push(TableCell { lines: Vec::new() });
            }
        }
        for (col, &band) in bands.iter().enumerate() {
            let join = chrome
                .cell_joins()
                .get(col)
                .copied()
                .unwrap_or(LineJoin::Break);
            if let Some(cell) = cells.get_mut(row_start + col) {
                cell.lines.push(CellLine {
                    content_line,
                    turn_idx,
                    line_idx,
                    band,
                    join,
                });
            }
        }
    }
    cells
}

/// Locate a selection point `(content_line, col)` in the cell map: the cell's
/// index and the `(wrap_line, column)` within it.  A column landing on a `│`
/// border (or past the last band) snaps to the nearest cell.  `None` when no
/// cell owns that display line.
fn locate_cell(
    cells: &[TableCell],
    display: &SessionDisplayState,
    vp_width: usize,
    content_line: usize,
    col: usize,
) -> Option<(usize, (usize, usize))> {
    let (turn_idx, line_idx) = resolve_line(display, vp_width, content_line)?;
    let mut best: Option<(usize, usize, (usize, usize))> = None;
    for (index, cell) in cells.iter().enumerate() {
        for (wrap, line) in cell.lines.iter().enumerate() {
            if line.turn_idx != turn_idx || line.line_idx != line_idx {
                continue;
            }
            if col >= line.band.0 && col < line.band.1 {
                return Some((index, (wrap, col)));
            }
            if best.is_none() || col >= line.band.1 {
                best = Some((index, wrap, line.band));
            }
        }
    }
    best.map(|(index, wrap, band)| (index, (wrap, col.clamp(band.0, band.1))))
}

/// The trimmed pieces of one cell (one per display line it has text on), each
/// paired with its global content line, the display-column range of its
/// non-whitespace content (the highlight range), and the cell-line copy-join
/// (how that line glues to the cell's previous line).  `from`/`to` slice the
/// cell at a `(wrap_line, column)`; `None` means the cell's own start/end.
///
/// The piece and its highlight range come from one grapheme walk
/// ([`super::text::slice_line_columns_trimmed`]) so the copied text and the
/// highlighted extent are measured identically and can never drift.
fn cell_pieces(
    display: &SessionDisplayState,
    vp_width: usize,
    cell: &TableCell,
    from: Option<(usize, usize)>,
    to: Option<(usize, usize)>,
) -> Vec<(usize, String, (usize, usize), LineJoin)> {
    let start = from.map_or(0, |(wrap, _)| wrap);
    let end = to.map_or(cell.lines.len().saturating_sub(1), |(wrap, _)| wrap);
    let mut pieces = Vec::new();
    for wrap in start..=end {
        let Some(line) = cell.lines.get(wrap) else {
            continue;
        };
        let Some(rendered) = cached_rendered_turn(display, line.turn_idx, vp_width) else {
            continue;
        };
        let Some(text_line) = rendered.lines.get(line.line_idx) else {
            continue;
        };
        let lo = match from {
            Some((w, c)) if w == wrap => c.max(line.band.0),
            _ => line.band.0,
        };
        let hi = match to {
            Some((w, c)) if w == wrap => c.min(line.band.1),
            _ => line.band.1,
        };
        let (piece, band) = slice_line_columns_trimmed(text_line, lo, hi);
        if !piece.is_empty() {
            pieces.push((line.content_line, piece, band, line.join));
        }
    }
    pieces
}

/// The reading-order-fill result for one table run: the copied text plus the
/// per-content-line highlight column ranges and the set of every data-row line
/// of the run (so the highlight never falls back to the plain line rectangle on
/// a table row).
pub(crate) struct TableRunPick {
    pub(crate) copy: String,
    pub(crate) highlights: Vec<(usize, (usize, usize))>,
    pub(crate) lines: Vec<usize>,
}

/// Apply reading-order fill to one table run `[lo, hi)`.
///
/// The table's cells are read in row-major order (each row left-to-right, rows
/// top to bottom).  The selection is the linear run of cells from the cell under
/// the anchor to the cell under the head, inclusive: every cell between is
/// copied whole, and the two endpoints' cells are sliced at the anchor/head
/// point (their leading/trailing text is dropped).  Cells are separated by a
/// blank line.  An endpoint off the table clamps to the first/last cell.
pub(crate) fn pick_table_run(
    display: &SessionDisplayState,
    vp_width: usize,
    anchor: (usize, u16),
    head: (usize, u16),
    lo: usize,
    hi: usize,
) -> Option<TableRunPick> {
    let cells = table_cells(display, vp_width, lo, hi);
    if cells.is_empty() {
        return None;
    }
    // Endpoints resolved to (cell index, point); off-table endpoints clamp to
    // the first/last cell with no partial slice (`None`).
    let a = locate_cell(&cells, display, vp_width, anchor.0, anchor.1 as usize)
        .map_or((0, None), |(index, point)| (index, Some(point)));
    let b = locate_cell(&cells, display, vp_width, head.0, head.1 as usize)
        .map_or((cells.len() - 1, None), |(index, point)| {
            (index, Some(point))
        });
    // Reading-order range, normalised low → high; the low cell is sliced from
    // its endpoint, the high cell to its endpoint.
    let (lo_i, hi_i) = (a.0.min(b.0), a.0.max(b.0));
    let lo_point = if lo_i == a.0 { a.1 } else { b.1 };
    let hi_point = if hi_i == b.0 { b.1 } else { a.1 };
    let lines = cells
        .iter()
        .flat_map(|cell| cell.lines.iter().map(|line| line.content_line))
        .collect();
    let mut copy = String::new();
    let mut highlights = Vec::new();
    for index in lo_i..=hi_i {
        let Some(cell) = cells.get(index) else {
            continue;
        };
        let from = if index == lo_i { lo_point } else { None };
        let to = if index == hi_i { hi_point } else { None };
        let pieces = cell_pieces(display, vp_width, cell, from, to);
        if !copy.is_empty() {
            copy.push_str("\n\n");
        }
        // Rejoin the cell's display lines through the recorded per-cell copy
        // joins — a word-wrap seam re-inserts the single space the reflow
        // consumed, a hard mid-word split concatenates directly, an embedded
        // newline is a real break — and record each line's trimmed range for the
        // highlight (so the cell padding is neither copied nor shown as
        // selected).
        let mut first_piece = true;
        for (content_line, piece, band, join) in &pieces {
            if !first_piece {
                match join {
                    LineJoin::Break => copy.push('\n'),
                    LineJoin::Space => copy.push(' '),
                    LineJoin::Join => {}
                }
            }
            first_piece = false;
            copy.push_str(piece);
            if band.0 < band.1 {
                highlights.push((*content_line, *band));
            }
        }
    }
    if copy.is_empty() {
        None
    } else {
        Some(TableRunPick {
            copy,
            highlights,
            lines,
        })
    }
}

/// The table reading-order-fill highlight for the whole selection: the set of
/// every data-row content line each table run covers (so a table row never falls
/// back to the plain line rectangle) and the selected column ranges per line.
/// Both come from the same [`pick_table_run`] the copy uses, so the highlight
/// and the copy can never disagree.
///
/// Built **once per frame** (not per visible turn): the whole-selection scan is
/// viewport-independent, so `render_history` computes it a single time and
/// threads the same value through every visible turn's
/// [`super::highlight::apply_selection_to_lines`] call, keeping the per-frame
/// cost O(selection span) instead of O(visible turns × selection span).
pub(crate) struct TableHighlight {
    pub(crate) lines: BTreeSet<usize>,
    pub(crate) ranges: BTreeMap<usize, Vec<(usize, usize)>>,
}

/// Scan the whole selection once and build its [`TableHighlight`].
///
/// Returns an empty highlight when there is no active selection or no active
/// display.  The scan is viewport-independent (it walks the selection's content
/// lines, not the visible screen rows), which is exactly why the draw path can
/// hoist it out of the per-turn loop.
pub(crate) fn table_highlight(app: &App) -> TableHighlight {
    let mut lines = BTreeSet::new();
    let mut ranges: BTreeMap<usize, Vec<(usize, usize)>> = BTreeMap::new();
    let Some((anchor, head)) = selection_range(app) else {
        return TableHighlight { lines, ranges };
    };
    let Some(display) = app.active_display_ref() else {
        return TableHighlight { lines, ranges };
    };
    let vp_width = app.history_viewport.width as usize;
    let (start_line, end_line) = (anchor.0.min(head.0), anchor.0.max(head.0));
    for_each_table_run(display, vp_width, start_line, end_line, |lo, hi| {
        if let Some(pick) = pick_table_run(display, vp_width, anchor, head, lo, hi) {
            lines.extend(pick.lines);
            for (line_no, band) in pick.highlights {
                ranges.entry(line_no).or_default().push(band);
            }
        }
    });
    TableHighlight { lines, ranges }
}
