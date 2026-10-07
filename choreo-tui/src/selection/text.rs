//! Text slicing for a selection.
//!
//! Rendered lines carry their text in spans; a selection slice walks that text
//! by *display columns*, snapping every cut to a grapheme boundary
//! ([`slice_line_columns`]) so a selection can never split a ZWJ emoji or a
//! combining sequence.  [`slice_line_columns_trimmed`] adds, in the same walk,
//! the display-column range of the slice's trimmed (non-whitespace) content —
//! the highlight extent — measured the same grapheme-cluster way so it can
//! never drift from the copied text.

use crate::state::grapheme_offset_at_column;
use ratatui::text::Line;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// Concatenate a rendered line's spans into its plain text.
pub(crate) fn line_text(line: &Line<'_>) -> String {
    let mut text = String::new();
    for span in &line.spans {
        text.push_str(span.content.as_ref());
    }
    text
}

/// Slice a rendered line's text by display columns, snapping to grapheme
/// boundaries (a selection can never split a ZWJ emoji or combining mark).
pub(crate) fn slice_line_columns(line: &Line<'_>, col_lo: usize, col_hi: usize) -> String {
    let text = line_text(line);
    let width = UnicodeWidthStr::width(text.as_str());
    let lo = grapheme_offset_at_column(&text, col_lo.min(width));
    let hi = grapheme_offset_at_column(&text, col_hi.min(width));
    // Both offsets are grapheme (⇒ char) boundaries snapped by
    // `grapheme_offset_at_column`, and ordered after the swap below.
    let (start, end) = (lo.min(hi), lo.max(hi));
    text.get(start..end).unwrap_or("").to_string()
}

/// Slice a rendered line's text to the display-column range `[lo, hi)` and
/// return, in the same pass, the display-column range of the slice's trimmed
/// (non-whitespace) content.
///
/// The copy trims the slice and the highlight uses the trimmed extent, so the
/// two must be measured identically: both walk grapheme clusters and measure
/// each with [`UnicodeWidthStr::width`] (a ZWJ emoji family's cluster width is
/// *not* the sum of its parts), and only graphemes whose start column falls in
/// `[lo, hi)` are included — the same grapheme-snapped extent
/// [`slice_line_columns`] produces.  A char-by-char width sum would drift here
/// and highlight a wider band than the copy keeps.  Building only the selected
/// graphemes avoids materialising the whole line.
pub(crate) fn slice_line_columns_trimmed(
    line: &Line<'_>,
    lo: usize,
    hi: usize,
) -> (String, (usize, usize)) {
    let mut piece = String::new();
    let mut col = 0usize;
    let mut first: Option<usize> = None;
    let mut end = lo;
    for span in &line.spans {
        for grapheme in span.content.graphemes(true) {
            let width = UnicodeWidthStr::width(grapheme);
            let start = col;
            col += width;
            if width == 0 || start < lo || start >= hi {
                continue;
            }
            piece.push_str(grapheme);
            if grapheme.chars().any(|c| !c.is_whitespace()) {
                first.get_or_insert(start);
                end = col;
            }
        }
    }
    (piece.trim().to_string(), (first.unwrap_or(lo), end))
}
