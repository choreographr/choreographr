//! Display-width and height measurement: the width table every wrapper, the
//! code-box padding, and the history viewport's height math measure with.
//!
//! One rule governs everything here — a measurement is in *display columns*, the
//! cells ratatui would draw, never in chars or bytes. `unicode-width` is the
//! table, and the text reaching these functions is expected to have passed
//! [`crate::text_filter`] first, so no character it measures is one ratatui
//! filters out of the drawn grapheme stream.

use std::sync::Arc;

use super::super::Line;

/// Display width of `text`, in terminal columns.
pub(crate) fn display_width(text: &str) -> usize {
    unicode_width::UnicodeWidthStr::width(text)
}

/// Display width of one grapheme cluster, in terminal columns.
///
/// A cluster is at least as wide as its widest char: `unicode-width` reports a
/// combining mark as zero columns, but a cluster carrying one (a base plus its
/// marks, an emoji ZWJ sequence) draws as its base's width. Taking the maximum
/// of the cluster width and its widest member keeps an isolated combining mark
/// measuring one column rather than vanishing from the wrap budget.
pub(crate) fn grapheme_width(grapheme: &str) -> usize {
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

/// Total visual-row height of `lines` drawn into a `width`-column viewport.
///
/// Zero for a zero width (nothing is drawn). A single empty line still occupies
/// one row, so it reports 1 rather than 0.
pub(crate) fn lines_height(lines: &[Line<'_>], width: u16) -> usize {
    let width = width as usize;
    if width == 0 {
        return 0;
    }

    // A single zero-width line still occupies one row in the terminal.
    if lines
        .first()
        .is_some_and(|line| lines.len() == 1 && line.width() == 0)
    {
        return 1;
    }

    lines
        .iter()
        .map(|line| wrapped_line_height(line, width))
        .sum::<usize>()
}

/// Visual rows one rendered line occupies at `width` columns.
///
/// The renderer pre-wraps every line to the content width, so a line never
/// wraps more than `line_width.div_ceil(width)` times — the whole reason the
/// pre-wrap exists. An empty line still occupies one row.
pub(crate) fn wrapped_line_height(line: &Line<'_>, width: usize) -> usize {
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

/// Precompute the cumulative visual-row offset for every semantic line.
///
/// `visual_offsets[i]` = total visual rows covered by `lines[0..=i]`,
/// i.e. the sum of `wrapped_line_height` for each line up to and including `i`.
/// An empty slice is returned when `width` is 0 or when there are no lines.
///
/// The resulting array enables O(log n) visual-row → line-index lookups
/// via `partition_point`.
pub(crate) fn compute_visual_offsets(lines: &[Line<'_>], width: u16) -> Arc<[usize]> {
    let w = width as usize;
    let mut offsets = Vec::with_capacity(lines.len());
    let mut acc = 0;
    for line in lines {
        let h = if w == 0 {
            0
        } else {
            wrapped_line_height(line, w)
        };
        acc += h;
        offsets.push(acc);
    }
    Arc::from(offsets)
}

/// Build a right-aligned ordered-list marker: the number is left-padded with
/// spaces to `number_width` display columns so the ones digits stack vertically
/// across the list (the "9" of item 9 sits above the "0" of item 10, not above
/// its "1").  The ". " suffix is a fixed two columns, so every marker is exactly
/// `number_width + 2` wide and every item's content — and every continuation
/// line — starts at the same column.  The pad is pure alignment whitespace: it
/// carries no meaning and is simply what makes the whole list read as one block.
pub(crate) fn ordered_marker(number: usize, number_width: usize) -> String {
    format!("{number:>number_width$}. ")
}
