//! Ordered and unordered list layout: the shared marker column, per-item
//! rendering, the tight/spaced decision, and the delimited-list margins.

use super::{
    Line, LineChrome, LineJoin, MarkdownBlock, Span, Style, display_width,
    ensure_blank_line_joined, indented_line, ordered_marker, render_markdown_blocks,
};

/// One rendered list item: its padded marker, its rendered rows, and the
/// aligned per-row [`LineJoin`]/[`LineChrome`] metadata.  A named alias keeps
/// the four-element tuple out of the `rendered_items` declaration (past the
/// `clippy::type_complexity` bar) and gives the tuple one place to evolve.
type RenderedItem = (String, Vec<Line<'static>>, Vec<LineJoin>, Vec<LineChrome>);

/// Render a list block (`ordered` with item numbers running `start..`, or a
/// bullet list) into fresh `(lines, joins, chrome)` buffers, which the caller
/// appends to its own.
pub(crate) fn render_list(
    ordered: bool,
    start: usize,
    items: &[Vec<MarkdownBlock>],
    indent: usize,
    width: usize,
    heading_shift: usize,
) -> (Vec<Line<'static>>, Vec<LineJoin>, Vec<LineChrome>) {
    // Build the list into its own buffers: every item is rendered before any
    // output is emitted, so the whole list's spacing can be decided as a unit
    // (see the tight/spaced decision below).
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut joins: Vec<LineJoin> = Vec::new();
    let mut chrome: Vec<LineChrome> = Vec::new();

    // Render every item into its own buffer first so the whole list's
    // spacing can be decided as a unit: if the *majority* of items wrap
    // to more than one visual line, every item pair gets a blank line
    // (paragraph-style); otherwise the list renders tight with no gaps
    // between items.  A uniform rhythm per list reads better than the
    // old per-item spacing (where one long item created a single
    // lopsided gap in an otherwise tight list).
    //
    // The list shares a single indentation unit: the width of the
    // *widest* number plus the fixed ". " suffix for an ordered list
    // (4 columns for a list reaching "10. "), or the bullet marker's
    // width for an unordered list.  Ordered markers are right-aligned
    // within that digit column — the number is left-padded with spaces
    // ("9" -> " 9") — so the ones digits stack vertically (item 9's "9"
    // sits above item 10's "0", not its "1") while the ". " suffix and
    // the content that follows stay at a fixed column.  Every
    // continuation line is indented to that same column, so first lines
    // and wrapped lines all line up as one block.
    let (max_number_width, max_marker_width) = if ordered {
        // Item numbers run start..=start + len - 1, so the widest number
        // is always the last one — O(1) per list, no per-item scan.
        // `saturating_add` is cheap overflow hardening: CommonMark caps an
        // ordered-list marker at nine digits (so `start` is at most
        // 999,999,999 today), but the marker arithmetic must never be able to
        // overflow (and panic in debug) if a parser or future input ever
        // allows a larger start.
        let number_width = items.len().checked_sub(1).map_or(1, |last| {
            display_width(&start.saturating_add(last).to_string())
        });
        // ". " is two fixed columns after the number column.
        (number_width, number_width + 2)
    } else {
        (0, display_width("• "))
    };
    let mut rendered_items: Vec<RenderedItem> = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        let marker = if ordered {
            // saturating_add: a huge literal list start must render,
            // not overflow (see max_number_width above).
            ordered_marker(start.saturating_add(index), max_number_width)
        } else {
            "• ".to_string()
        };
        // Every marker is already exactly `max_marker_width` columns
        // wide (fixed digit column + ". ", or the bullet marker), so
        // first-line content aligns with every other item's first line
        // (and with the continuation lines below).
        let mut rendered = Vec::new();
        let mut rendered_joins = Vec::new();
        let mut rendered_chrome = Vec::new();
        // Content is rendered at (width - indent - max_marker_width):
        // with every marker padded to that width, a first line totals
        // exactly `width`, and continuation lines (indented to the same
        // column) fit as well.
        render_markdown_blocks(
            item,
            &mut rendered,
            &mut rendered_joins,
            &mut rendered_chrome,
            0,
            width.saturating_sub(indent + max_marker_width),
            heading_shift,
        );
        rendered_items.push((marker, rendered, rendered_joins, rendered_chrome));
    }

    // Strict majority: more than half of the items must be multi-line
    // for the list to be spaced out.  A tie (e.g. 2 items, 1 wrapping)
    // stays tight because 1 * 2 == 2 is not > 2.
    let multi_line_count = rendered_items
        .iter()
        .filter(|(_, rendered, _, _)| rendered.len() > 1)
        .count();
    let spaced = multi_line_count * 2 > items.len();

    for (index, (marker, rendered, rendered_joins, rendered_chrome)) in
        rendered_items.into_iter().enumerate()
    {
        // All continuation lines align under the widest marker so wrapped
        // text lines up across the whole list.
        let continuation_indent = indent + max_marker_width;
        // Zip the item's rows with the joins their inner renderer
        // recorded: the two vectors stay in lockstep by construction,
        // so `joins` below needs no fallback.  The first row (which
        // carries the marker) is consumed with its join — each item
        // starts a fresh line, whatever the inner renderer said about
        // its first line is superseded.
        let mut zipped = rendered
            .into_iter()
            .zip(rendered_joins)
            .zip(rendered_chrome);
        if let Some(((first, _first_join), first_chrome)) = zipped.next() {
            // The marker (a content span, not chrome — list markers stay
            // copyable) plus the outer indent prepend
            // `indent + marker_width` columns, so the item's own chrome
            // shifts right by that much to stay in the emitted row's
            // column space.
            let marker_width = display_width(&marker);
            let mut spans = vec![Span::styled(
                format!("{}{}", " ".repeat(indent), marker),
                Style::default(),
            )];
            spans.extend(first.spans.clone());
            lines.push(Line::from(spans));
            joins.push(LineJoin::Break);
            let mut row_chrome = LineChrome::default();
            row_chrome.extend_shifted(&first_chrome, indent + marker_width);
            chrome.push(row_chrome);
        } else {
            lines.push(indented_line(indent, marker));
            joins.push(LineJoin::Break);
            chrome.push(LineChrome::default());
        }
        for ((line, join), inner_chrome) in zipped {
            let mut spans = vec![Span::styled(
                " ".repeat(continuation_indent),
                Style::default(),
            )];
            spans.extend(line.spans);
            lines.push(Line::from(spans));
            // Wrapped continuations inside the item rejoin with
            // Space/Join exactly as the inner renderer recorded
            // (their predecessor's text is the line above them).
            joins.push(join);
            // The continuation indent is layout chrome (outside the
            // content), so the row's own chrome shifts right by it.
            let mut row_chrome = LineChrome::default();
            row_chrome.extend_shifted(&inner_chrome, continuation_indent);
            chrome.push(row_chrome);
        }

        // Blank line between items only when the list is spaced out as
        // a whole (majority of items wrap).  Uses ensure_blank_line so
        // consecutive blanks collapse into one (e.g. when a spaced
        // item ends with a nested list that already produced a blank).
        if index + 1 < items.len() && spaced {
            ensure_blank_line_joined(&mut lines, &mut joins, &mut chrome);
        }
    }

    // A list is delimited: emit a collapsing blank line after its
    // items.  Blocks between lists get their separation from the
    // *next* block's before-margin, but a nested list's successor is
    // the next item marker of the enclosing list, which never
    // receives a before-margin — without this margin the marker would
    // run flush against the nested list's last line.  ensure_blank_line
    // collapses this margin with any following before-margin, and
    // markdown_lines strips it when the list is the document's last
    // block, so the rule is invisible everywhere except the boundary
    // that was previously missing it.
    if !items.is_empty() {
        ensure_blank_line_joined(&mut lines, &mut joins, &mut chrome);
    }

    (lines, joins, chrome)
}
