//! Content-line resolution and per-line text extraction for a selection.
//!
//! Content lines are resolved one at a time through the turn height prefix
//! plus the render cache ([`content_line_to_turn_row`] → [`resolve_line`]),
//! the exact inverse of the click hit-testing.  [`extract_selection_text`]
//! walks the selected content lines, pulling each line's covered text through
//! the row→line→column mapping ([`content_range_for_row`]) and subtracting the
//! renderer's copy-chrome ([`selectable_intervals`]) so the copy is exactly the
//! selectable content; contiguous runs of table lines are instead copied by
//! reading-order cell fill (see [`super::table`]).  The draw-time highlight
//! consumes the same mapping and the same chrome subtraction, so the highlight
//! and the copy can never disagree.

use crate::markdown_render::{LineChrome, LineJoin};
use crate::state::{App, RenderedTurn, SessionDisplayState};
use smallvec::SmallVec;

use super::{
    for_each_table_run, pick_table_run, selection_bounds_for_line, selection_range,
    slice_line_columns,
};

/// One selected row's contribution to the copy: the text it covers, the
/// [`LineJoin`] the renderer recorded for it (how it glues to the row
/// before it), and its location in the render cache so the assembly loop
/// can detect two slots that are rows of the same semantic line (which
/// always concatenate directly).
struct ExtractedSlot {
    text: String,
    join: LineJoin,
    turn_idx: usize,
    line_idx: usize,
}

/// Map a global content line to `(turn_idx, visual_row)` in the render cache's
/// turn-local row space.
///
/// The height-prefix `partition_point` plus the `turn_start` subtraction that
/// both [`resolve_line`] and the extraction path need — the inverse of
/// `find_turn_at_row`'s screen mapping (the height prefix is the same
/// cumulative array that function binary searches).  `None` when the line is
/// past the end of the history.
fn content_line_to_turn_row(
    display: &SessionDisplayState,
    content_line: usize,
) -> Option<(usize, usize)> {
    if content_line >= display.total_history_height() {
        return None;
    }
    let turn_idx = display
        .height_prefix
        .partition_point(|&p| p <= content_line);
    let turn_start = turn_idx
        .checked_sub(1)
        .and_then(|prev| display.height_prefix.get(prev))
        .copied()
        .unwrap_or(0);
    let visual_row = content_line.saturating_sub(turn_start);
    Some((turn_idx, visual_row))
}

/// Resolve a global content line to `(turn_idx, line_idx)` in the render cache,
/// or `None` when it maps to no cached turn/line (past the end, cache drift).
pub(crate) fn resolve_line(
    display: &SessionDisplayState,
    vp_width: usize,
    content_line: usize,
) -> Option<(usize, usize)> {
    let (turn_idx, visual_row) = content_line_to_turn_row(display, content_line)?;
    let rendered = cached_rendered_turn(display, turn_idx, vp_width)?;
    let line_idx = rendered
        .visual_offsets
        .partition_point(|&o| o <= visual_row);
    if line_idx >= rendered.lines.len() {
        return None;
    }
    Some((turn_idx, line_idx))
}

/// Resolve one content line of the selection to the text slice it covers and
/// the copy-join metadata recorded for its rendered row.
///
/// Returns `(text, join, turn_idx, line_idx)` where `join` is the row's
/// [`LineJoin`] (how it glues to the row *before* it) and `(turn_idx,
/// line_idx)` locates the row in the render cache for adjacency checks.
fn text_and_join_for_content_line(
    display: &SessionDisplayState,
    vp_width: usize,
    content_line: usize,
    col_start: usize,
    col_end: usize,
) -> Option<(String, LineJoin, usize, usize)> {
    // Map the global content line to a visible turn and the turn-local
    // visual row.
    let (turn_idx, visual_row) = content_line_to_turn_row(display, content_line)?;
    let rendered = cached_rendered_turn(display, turn_idx, vp_width)?;
    // Pin the parallel-array invariant the extraction relies on (the same
    // one `cached_or_compute_lines` asserts on the rebuild path): a drift
    // here would silently degrade a copy to newline-joined rows via the
    // `unwrap_or(LineJoin::Break)` fallback below, so catch it in debug
    // builds at the consumer instead of letting it slip through.
    debug_assert_eq!(
        rendered.lines.len(),
        rendered.content_ranges.len(),
        "content ranges must align with the rendered lines"
    );
    debug_assert_eq!(
        rendered.lines.len(),
        rendered.joins.len(),
        "joins must align with the rendered lines"
    );
    let (line_idx, base) = content_range_for_row(
        &rendered.visual_offsets,
        &rendered.content_ranges,
        visual_row,
        col_start,
        col_end,
        vp_width,
    )?;
    let join = rendered
        .joins
        .get(line_idx)
        .copied()
        .unwrap_or(LineJoin::Break);
    // `line_idx < rendered.lines.len()` by the parallel-array invariant
    // asserted above (debug builds) and `content_range_for_row`'s mapping;
    // bail to an empty selection if a cache drift ever breaks it.
    let line = rendered.lines.get(line_idx)?;
    // Cut the row's renderer-emitted chrome out of its base range so the copy
    // is content−chrome: a block-quote bar nested inside a list item is
    // dropped while the list marker before it is kept.  A row that records no
    // chrome yields `base` unchanged.  A missing entry (cache drift) falls
    // back to no chrome.
    let chrome = rendered
        .chrome_ranges
        .get(line_idx)
        .cloned()
        .unwrap_or_default();
    let selectable = selectable_intervals(base, &chrome);
    if selectable.is_empty() && base.0 < base.1 {
        // The clamped base was non-blank (a real content row) but the chrome
        // covered all of it: no copyable cells, just like a pure-chrome row.
        // A blank base (`(lo, lo)`) instead falls through to the empty slot
        // below, so a blank line inside the selection survives the copy.
        return None;
    }
    // Concatenate the selected slices of each selectable sub-interval in
    // order; the pieces rejoin directly because chrome (not whitespace) was
    // cut out between them.
    let mut text = String::new();
    for (lo, hi) in &selectable {
        text.push_str(&slice_line_columns(line, *lo, *hi));
    }
    Some((text, join, turn_idx, line_idx))
}

/// Resolve a turn-local visual row and viewport column range to the
/// content-clamped display-column range of the semantic line it covers.
///
/// The single row→line→column mapping shared by the extraction path
/// ([`text_and_join_for_content_line`]) and the draw-time highlight
/// ([`super::highlight::apply_selection_to_lines`]), so the two can never
/// drift apart again — they already diverged twice (the screen-row offset bug
/// and the within-line column bug, both fixed by pinning exactly this
/// mapping).  `visual_offsets`/`content_ranges` are the turn's cached arrays,
/// aligned with its rendered lines (see the `debug_assert`s in
/// `cached_or_compute_lines`); `col_end == usize::MAX` means "to the end of
/// the line".  Returns `(line_idx, (lo, hi))` — the semantic line and its
/// selectable display-column range — or `None` when the row maps to no
/// selectable content (pure-chrome rows, image rows, rows past the end of
/// the text).  A *blank* content row (an empty `(lo, lo)` range) resolves
/// to that empty range rather than `None`: it is genuine content with no
/// characters (the renderer's blank spacer between markdown blocks), so the
/// extraction path records it as an empty slot and the blank line survives
/// the copy, while chrome stays `None` and contributes nothing.
pub(crate) fn content_range_for_row(
    visual_offsets: &[usize],
    content_ranges: &[Option<(usize, usize)>],
    visual_row: usize,
    col_start: usize,
    col_end: usize,
    vp_width: usize,
) -> Option<(usize, (usize, usize))> {
    // Map the turn-local visual row to a semantic line, then to the visual
    // row *within* that line (0 for every line in practice: lines are
    // pre-wrapped narrower than the viewport).
    let line_idx = visual_offsets.partition_point(|&o| o <= visual_row);
    if line_idx >= visual_offsets.len() {
        return None;
    }
    let line_start_row = line_idx
        .checked_sub(1)
        .and_then(|i| visual_offsets.get(i))
        .copied()
        .unwrap_or(0);
    let within_line = visual_row.saturating_sub(line_start_row);
    // Translate viewport columns into the semantic line's own column space:
    // visual row `within_line` of a line shows columns
    // `[within_line * vp_width, (within_line + 1) * vp_width)`.
    let base = within_line.saturating_mul(vp_width);
    let line_col_lo = base.saturating_add(col_start);
    let line_col_hi = if col_end == usize::MAX {
        usize::MAX
    } else {
        base.saturating_add(col_end)
    };
    // Clamp to the line's meaningful content so neither the highlight nor
    // the copy ever includes the box chrome (`┃` gutter, indents, trailing
    // fill) or pure-chrome rows.
    let content = content_ranges.get(line_idx).copied().flatten();
    match content {
        // A real content row: clamp the selected columns to its content
        // range; an empty overlap means the selection covers none of it.
        Some((clo, chi)) if clo < chi => {
            let lo = line_col_lo.max(clo);
            let hi = line_col_hi.min(chi);
            if lo >= hi {
                return None;
            }
            Some((line_idx, (lo, hi)))
        }
        // A *blank* content row — an empty `(lo, lo)` range: content with
        // no characters, e.g. the spacer the renderer leaves between
        // markdown blocks or genuinely blank lines inside tool output.
        // Deliberately NOT chrome: the extraction path turns it into an
        // empty slot (its `Break` join re-inserts the newline), so a blank
        // line inside the selected text is copied, not dropped.  The
        // highlight path is unaffected: `style_line_selection_ranges` no-ops
        // on an empty column range.  (An empty overlap on a non-blank row falls
        // through the first arm to `None` above; only the row's own empty
        // content range reaches this arm.)
        Some((clo, chi)) => Some((line_idx, (clo, chi))),
        // Pure chrome (box separators, padding, image blocks, past-end): no
        // content, no slot.
        None => None,
    }
}

/// The selectable display-column intervals of a row: `base` with every chrome
/// interval removed.
///
/// `base` is the row's clamped `content_ranges` entry — a half-open
/// `(lo, hi)` in the semantic line's own column space (already intersected
/// with the drag's viewport columns by [`content_range_for_row`]).  `chrome`
/// is the row's renderer-emitted [`LineChrome`]: the non-selectable intervals
/// *within* that same column space.  The result is the pieces of `base` left
/// once the chrome is cut out — i.e. `drag ∩ (content − chrome)` — a list
/// because chrome can name more than one interval (a block quote nested in a
/// list item records both its marker-relative and inner-bar intervals).
///
/// This is the **only** place the subtraction lives: the highlight
/// ([`super::highlight::apply_selection_to_lines`]) and the copy
/// ([`text_and_join_for_content_line`]) both consume this list, so they can
/// never disagree about which cells are selectable.  The returned intervals
/// are disjoint and in ascending order; an all-chrome (or blank) row yields
/// an empty list.
pub(crate) fn selectable_intervals(
    base: (usize, usize),
    chrome: &LineChrome,
) -> SmallVec<[(usize, usize); 2]> {
    let (base_lo, base_hi) = base;
    let mut out: SmallVec<[(usize, usize); 2]> = SmallVec::new();
    if base_lo >= base_hi {
        // A blank content row (`(lo, lo)`) has no cells to select.
        return out;
    }
    if chrome.is_empty() {
        // The overwhelmingly common case: no chrome, one interval.
        out.push((base_lo, base_hi));
        return out;
    }
    // Clip each chrome interval to the base range and drop the empties.  The
    // producers push chrome left-to-right, but normalise defensively so the
    // complement below is well-formed for any input.
    let mut clipped: SmallVec<[(usize, usize); 2]> = SmallVec::new();
    for &(clo, chi) in chrome.intervals() {
        let lo = usize::from(clo).max(base_lo);
        let hi = usize::from(chi).min(base_hi);
        if lo < hi {
            clipped.push((lo, hi));
        }
    }
    clipped.sort_unstable();
    // Merge touching/overlapping chrome so the gaps between them are the
    // selectable pieces.
    let mut merged: SmallVec<[(usize, usize); 2]> = SmallVec::new();
    for (lo, hi) in clipped {
        match merged.last_mut() {
            Some(last) if lo <= last.1 => last.1 = last.1.max(hi),
            _ => merged.push((lo, hi)),
        }
    }
    // Complement of `merged` within `[base_lo, base_hi)`: everything the chrome
    // did not cover, in ascending order.
    let mut cursor = base_lo;
    for (lo, hi) in merged {
        if cursor < lo {
            out.push((cursor, lo));
        }
        cursor = cursor.max(hi);
    }
    if cursor < base_hi {
        out.push((cursor, base_hi));
    }
    out
}

/// Read-only render-cache lookup for a turn's rendered lines.
///
/// Mirrors the key the renderer computes (see `render_history`): only an
/// entry with matching turn id and widths is reused, so a stale entry (e.g.
/// from before a resize) is treated as a miss and the row is skipped rather
/// than extracting text from the wrong wrapping.
pub(crate) fn cached_rendered_turn(
    display: &SessionDisplayState,
    turn_idx: usize,
    vp_width: usize,
) -> Option<&RenderedTurn> {
    let cached = display.render_cache.get(turn_idx)?.as_ref()?;
    let turn_id = display.visible_turn_ids.get(turn_idx).copied()?;
    if cached.key.turn_id != turn_id
        || cached.key.width as usize != vp_width.saturating_sub(9)
        || cached.key.viewport_width as usize != vp_width
    {
        return None;
    }
    Some(&cached.rendered)
}

/// Push the ordinary single-line slot for `content_line` (the non-table case),
/// or nothing when the line resolves to no copyable content.  The bound the
/// line covers comes from the shared anchor-fixed semantics
/// ([`selection_bounds_for_line`]).
fn push_plain_slot(
    slots: &mut Vec<ExtractedSlot>,
    display: &SessionDisplayState,
    vp_width: usize,
    anchor: (usize, u16),
    head: (usize, u16),
    content_line: usize,
) {
    let (col_lo, col_hi) = selection_bounds_for_line(anchor, head, content_line);
    if let Some((text, join, turn_idx, line_idx)) =
        text_and_join_for_content_line(display, vp_width, content_line, col_lo, col_hi)
    {
        slots.push(ExtractedSlot {
            text,
            join,
            turn_idx,
            line_idx,
        });
    }
}

/// Extract the plain text covered by the active selection rectangle.
///
/// Content lines are resolved one at a time through the height prefix + the
/// render cache.  Lines that resolve to no copyable content — pure-chrome
/// rows, image blocks, lines past the end — are skipped, so the copyable
/// region is exactly the region the highlight covers.  Consecutive rows that
/// are wrapped continuations of one original line (marked by the renderer's
/// per-line [`LineJoin`] metadata) are glued back together, so copying a
/// selected paragraph yields the original unwrapped text, not the display's
/// line-wrapped rows.  A run of contiguous table lines is instead copied by
/// reading-order cell fill (see [`pick_table_run`]).
pub(crate) fn extract_selection_text(app: &App) -> Option<String> {
    let (anchor, head) = selection_range(app)?;
    let display = app.active_display_ref()?;
    let vp_width = app.history_viewport.width as usize;
    let (start_line, end_line) = (anchor.0.min(head.0), anchor.0.max(head.0));

    // Collect one slot per selected row: its text, the [`LineJoin`] the row
    // was recorded with (how it glues to the row before it), and its
    // (turn_idx, line_idx) so adjacency can be checked across turns.  Rows
    // with no copyable content (pure chrome: box separators, padding,
    // image blocks, past-end) contribute no slot; blank *content* rows —
    // the renderer's blank spacers between markdown blocks, blank lines
    // inside tool output — contribute an empty slot, so a blank line inside
    // the selected text survives the copy as a blank line.
    //
    // Iterate the selection's *content* lines directly — no screen mapping,
    // which is exactly why the selection survives scrolling (the endpoints
    // are content-anchored, so this is scroll-independent).  The shared
    // table-run walk yields each contiguous run of one table's lines; the
    // gaps between runs are handled as ordinary single lines.
    let mut slots: Vec<ExtractedSlot> = Vec::new();
    let mut prev = start_line;
    for_each_table_run(display, vp_width, start_line, end_line, |lo, hi| {
        for content_line in prev..lo {
            push_plain_slot(&mut slots, display, vp_width, anchor, head, content_line);
        }
        if let Some(pick) = pick_table_run(display, vp_width, anchor, head, lo, hi) {
            // A table run is its own slot; `usize::MAX` identity never
            // matches a real line, so the assembly separates the run
            // from its neighbours and never trims it as a wrap.
            slots.push(ExtractedSlot {
                text: pick.copy,
                join: LineJoin::Break,
                turn_idx: usize::MAX,
                line_idx: usize::MAX,
            });
        }
        prev = hi;
    });
    for content_line in prev..=end_line {
        push_plain_slot(&mut slots, display, vp_width, anchor, head, content_line);
    }

    // Assemble the slots.  Each slot's join metadata says how the text glued
    // to its immediate predecessor when the renderer wrapped the original
    // line — except when the two slots are rows of the *same* semantic line
    // split across viewport rows (never in practice: content is pre-wrapped
    // narrower than the viewport), which always concatenate directly.
    let mut out = String::new();
    for (i, slot) in slots.iter().enumerate() {
        if i == 0 {
            out.push_str(&slot.text);
            continue;
        }
        // `i > 0` inside this loop, so the previous slot always exists; the
        // fallback duplicates the current slot's indices only if the iterator
        // were ever handed an inconsistent slice.
        let (prev_turn, prev_line) = slots
            .get(i - 1)
            .map_or((slot.turn_idx, slot.line_idx), |p| (p.turn_idx, p.line_idx));
        let join = if prev_turn == slot.turn_idx && prev_line == slot.line_idx {
            // Same semantic line across two viewport rows — contiguous text.
            LineJoin::Join
        } else {
            slot.join
        };
        match join {
            LineJoin::Break => out.push('\n'),
            LineJoin::Space => {
                // A word-boundary wrap: the reflow consumed the separating
                // whitespace.  Re-insert exactly one space, trimming any
                // whitespace the renderer left at the seam first (some
                // wrappers keep a placeholder space on the row, others drop
                // it — trim+insert handles both).  Indentation padding that
                // the renderer prepends to continuation rows (list items) is
                // alignment chrome, not text, so it is trimmed away too.
                let trimmed = out.trim_end().len();
                out.truncate(trimmed);
                out.push(' ');
            }
            LineJoin::Join => {
                // Direct concatenation — whitespace (if any) is already where
                // it belongs within the rows (plain-text wraps keep their
                // whitespace on the previous row; hard splits have none).
            }
        }
        if matches!(join, LineJoin::Space) {
            out.push_str(slot.text.trim_start());
        } else {
            out.push_str(&slot.text);
        }
    }
    if out.is_empty() { None } else { Some(out) }
}
