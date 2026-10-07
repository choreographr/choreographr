//! Draw-time selection highlight.
//!
//! [`apply_selection_to_lines`] restyles the covered display-column range of a
//! turn's visible line slice with the selection background, without mutating
//! the render cache.  It consumes the same row→line→column mapping and the
//! same chrome subtraction the copy uses (see [`super::extract`]), so what is
//! highlighted is exactly what gets copied.  The whole-selection table
//! highlight is computed once per frame by the caller
//! ([`super::table::table_highlight`]) and passed in, so the per-frame cost of
//! the table scan is O(selection span) rather than O(visible turns × span).

use crate::markdown_render::LineChrome;
use crate::state::App;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use super::{
    TableHighlight, content_range_for_row, selectable_intervals, selection_bounds_for_line,
    selection_range,
};

/// Background color for the in-progress selection highlight.
///
/// A solid, dedicated color rather than `Modifier::REVERSED`: the history's
/// turns carry explicit `BG_SHADE` backgrounds, so reverse-video would
/// depend on the terminal's swap semantics (and can render dark-on-dark on
/// shaded cells).  A fixed background color reads as a selection on both the
/// shaded turns and the plain text between them, like a terminal's own
/// selection.
pub(crate) const SELECTION_BG: Color = Color::Rgb(0x2F, 0x5F, 0xAF);

/// The render-cache slice of one turn that the draw-time highlight styles:
/// the turn's first content row plus the parallel per-line arrays (visual
/// offsets, content column ranges, chrome intervals) aligned with its rendered
/// lines.  Grouped so the render seam threads one value instead of five
/// positional arguments.
pub(crate) struct TurnSlice<'a> {
    /// The turn's first content row (its `height_prefix` predecessor entry).
    pub(crate) turn_start: usize,
    /// Cumulative visual-row offsets, one entry per rendered line.
    pub(crate) text_offsets: &'a [usize],
    /// Per-line base content column ranges (`None` for pure-chrome rows).
    pub(crate) content_ranges: &'a [Option<(usize, usize)>],
    /// Per-line renderer-emitted copy-chrome intervals.
    pub(crate) chrome_ranges: &'a [LineChrome],
}

/// Apply the selection highlight to the visible slice of one turn's lines.
///
/// Called from `render_history` for the visible semantic-line slice of each
/// turn.  For every line occupying any selected *content* line, the covered
/// column ranges (the row's content range minus its renderer-emitted chrome,
/// translated into the line's own column space) are restyled with the
/// selection background.  The render cache is never mutated: lines are
/// restyled at draw time only.
///
/// `table` is the whole-selection [`TableHighlight`], built **once per frame**
/// by `render_history` before the visible-turn loop and threaded in here.  It
/// is viewport-independent (it scans the selection's content lines, not the
/// visible rows), so recomputing it per visible turn would cost O(visible
/// turns × selection span) for nothing — passing the one value keeps the
/// per-frame cost O(selection span).
pub(crate) fn apply_selection_to_lines(
    app: &App,
    table: &TableHighlight,
    turn: &TurnSlice<'_>,
    line_start: usize,
    lines: &mut [Line<'static>],
) {
    // The slice fields are all `Copy`; destructuring by value keeps the body
    // reading exactly as if the values arrived as separate arguments.
    let TurnSlice {
        turn_start,
        text_offsets,
        content_ranges,
        chrome_ranges,
    } = *turn;
    let Some((anchor, head)) = selection_range(app) else {
        return;
    };
    let vp = app.history_viewport;
    let (start_line, end_line) = (anchor.0.min(head.0), anchor.0.max(head.0));
    for (k, line) in lines.iter_mut().enumerate() {
        let li = line_start + k;
        let row_lo = li
            .checked_sub(1)
            .and_then(|i| text_offsets.get(i))
            .copied()
            .unwrap_or(0);
        let Some(&row_hi) = text_offsets.get(li) else {
            continue;
        };
        // Every line occupies exactly one visual row in practice (pre-wrapped
        // at content_width < viewport width); the inner loop generalizes to
        // multi-row lines defensively.
        for vr in row_lo..row_hi {
            // The selection lives in content space, so a line's content line
            // (`turn_start + vr`) is compared directly against the selection
            // range — no screen-row conversion.  That is exactly what makes
            // the selection survive scrolling: the endpoints stay pinned to
            // the text, and this re-evaluates against the current scroll
            // every frame.  (The old screen-row math — a signed
            // `scroll + vh - total` offset that had to handle the negative
            // overflow case — is gone entirely.)
            let content_line = turn_start + vr;
            // Table cells highlight their selected sub-bands; a table row is
            // never highlighted as a plain line rectangle.
            if table.lines.contains(&content_line) {
                if let Some(bands) = table.ranges.get(&content_line) {
                    *line = style_line_selection_ranges(line, bands);
                }
                if row_hi - row_lo <= 1 {
                    break;
                }
                continue;
            }
            if content_line < start_line || content_line > end_line {
                continue;
            }
            let (col_lo, col_hi) = selection_bounds_for_line(anchor, head, content_line);
            // Translate the viewport columns into the semantic line's own
            // column space and clamp to its meaningful content — the exact
            // mapping extraction uses (`content_range_for_row`), so the
            // highlight and the copy can never disagree about which cells
            // are selected.  Pure-chrome rows and rows outside the line's
            // content range stay unhighlighted.
            let Some((line_idx, base)) = content_range_for_row(
                text_offsets,
                content_ranges,
                vr,
                col_lo,
                col_hi,
                vp.width as usize,
            ) else {
                continue;
            };
            // Subtract the row's chrome from the clamped base: the highlight is
            // the union of the selectable sub-intervals, styled in one pass.
            // A chromeless row yields `base` unchanged.  A missing entry
            // (cache drift) falls back to no chrome.
            let chrome = chrome_ranges.get(line_idx).cloned().unwrap_or_default();
            let selectable = selectable_intervals(base, &chrome);
            if selectable.is_empty() {
                continue;
            }
            // Style the disjoint selected sub-intervals in a single pass so a
            // row whose cells are separated by chrome (a table row, a quote
            // nested in a list) is restyled once, not once per interval.
            *line = style_line_selection_ranges(line, &selectable);
            // A real (single-visual-row) line is fully covered by its one row.
            if row_hi - row_lo <= 1 {
                break;
            }
        }
    }
}

/// Restyle the display-column range `[col_lo, col_hi)` of a line with the
/// selection highlight (a solid [`SELECTION_BG`] background), splitting
/// spans at grapheme boundaries so a selection can never split a ZWJ emoji
/// or combining sequence.  `col_hi` of `usize::MAX` means "to the end of
/// the line".
///
/// Single-range convenience wrapper over [`style_line_selection_ranges`],
/// kept for the unit tests; the production highlight path always calls the
/// multi-range form.
#[cfg(test)]
pub(crate) fn style_line_selection(
    line: &Line<'static>,
    col_lo: usize,
    col_hi: usize,
) -> Line<'static> {
    style_line_selection_ranges(line, &[(col_lo, col_hi)])
}

/// Restyle several disjoint display-column ranges of a line with the
/// selection highlight in a single pass.  Ranges are half-open `[lo, hi)` in
/// display columns (`hi` of `usize::MAX` = end-of-line) and, by the
/// [`super::extract::selectable_intervals`] contract, ascending and
/// non-overlapping; each cut snaps to a grapheme boundary and the unselected
/// text keeps its span's original style.
///
/// The whole-selection table highlight and the plain per-line highlight both
/// route through here, so the multiple cell bands of one row are styled in one
/// walk rather than by cloning and re-splitting the line once per band.
pub(crate) fn style_line_selection_ranges(
    line: &Line<'static>,
    ranges: &[(usize, usize)],
) -> Line<'static> {
    if ranges.iter().all(|&(lo, hi)| lo >= hi) {
        return line.clone();
    }
    let mut out: Vec<Span<'static>> = Vec::with_capacity(line.spans.len() + 2);
    let mut col = 0usize;
    for span in &line.spans {
        let span_text = span.content.as_ref();
        let span_w = UnicodeWidthStr::width(span_text);
        if span_w == 0 {
            // Zero-width span (e.g. a control-char placeholder) — no cells to
            // highlight, keep it untouched.
            out.push(span.clone());
            continue;
        }
        let span_lo = col;
        let span_hi = col.saturating_add(span_w);
        col = span_hi;
        // This span's selected sub-intervals, in span-local columns, ascending.
        let mut cuts: Vec<(usize, usize)> = Vec::new();
        for &(lo, hi) in ranges {
            let a = lo.max(span_lo);
            let b = hi.min(span_hi);
            if a < b {
                cuts.push((a - span_lo, b - span_lo));
            }
        }
        if cuts.is_empty() {
            out.push(span.clone());
            continue;
        }
        cuts.sort_unstable();
        // Emit the span split at each cut, snapping every column to a grapheme
        // boundary; the gaps between cuts stay unselected.
        let mut cursor = 0usize;
        for (a, b) in cuts {
            if a > cursor {
                push_selection_piece(&mut out, span_text, cursor, a, span.style, false);
            }
            let a = a.max(cursor);
            if b > a {
                push_selection_piece(&mut out, span_text, a, b, span.style, true);
            }
            cursor = cursor.max(b);
        }
        if cursor < span_w {
            push_selection_piece(&mut out, span_text, cursor, span_w, span.style, false);
        }
    }
    Line::from(out)
}

/// Push one grapheme-snapped slice of `span_text` (span-local display columns
/// `[lo, hi)`) onto `out`, with the selection background when `selected`.
fn push_selection_piece(
    out: &mut Vec<Span<'static>>,
    span_text: &str,
    lo: usize,
    hi: usize,
    style: Style,
    selected: bool,
) {
    let a = crate::state::grapheme_offset_at_column(span_text, lo);
    let b = crate::state::grapheme_offset_at_column(span_text, hi);
    // The snap offsets come from `grapheme_offset_at_column` over the same
    // string, so both are char boundaries within `span_text` by construction;
    // `.get()` keeps the slice total and the swap guards monotonicity.
    let (a, b) = if a > b { (b, a) } else { (a, b) };
    let piece = span_text.get(a..b).unwrap_or("");
    if piece.is_empty() {
        return;
    }
    let style = if selected {
        style.bg(SELECTION_BG)
    } else {
        style
    };
    out.push(Span::styled(piece.to_owned(), style));
}
