//! Ordered and unordered list layout: the shared marker column, per-item
//! rendering, the per-pair item spacing, and the delimited-list margins.

use super::{
    DocRender, Line, LineChrome, LineJoin, ListItem, Span, Style, display_width,
    ensure_blank_line_joined, indented_line, ordered_marker, render_markdown_blocks,
};

/// One rendered list item: its padded marker, its rendered rows, and the
/// aligned per-row [`LineJoin`]/[`LineChrome`] metadata.  A named alias keeps
/// the four-element tuple out of the `rendered_items` declaration (past the
/// `clippy::type_complexity` bar) and gives the tuple one place to evolve.
type RenderedItem = (String, Vec<Line<'static>>, Vec<LineJoin>, Vec<LineChrome>);

/// Render a list block (a bullet list, or an ordered list whose item numbers
/// come from each item's own captured marker, numbering from `start` when an
/// item carries no marker) into fresh `(lines, joins, chrome)` buffers, which
/// the caller appends to its own.
pub(crate) fn render_list(
    ordered: bool,
    start: usize,
    items: &[ListItem],
    indent: usize,
    width: usize,
    opts: &DocRender<'_>,
) -> (Vec<Line<'static>>, Vec<LineJoin>, Vec<LineChrome>) {
    // Build the list into its own buffers: every item is rendered before any
    // output is emitted, so each item's row count is known when its pair is
    // laid out.
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut joins: Vec<LineJoin> = Vec::new();
    let mut chrome: Vec<LineChrome> = Vec::new();

    // The vertical rhythm is decided per item *pair*: a blank row
    // separates two items when either of them renders to more than one row
    // (its own text wrapping, or a nested block it carries).  Keying on the
    // pair makes the spacing a one-way gate under streaming — an item's row
    // count only grows as its text arrives, and the item before it is final
    // by the time the next marker appears — so a gap can appear once and
    // never disappear.  A list-wide rule cannot promise that: a mode chosen
    // from a ratio of multi-row to total items is re-decided by every newly
    // streamed item, so a long list visibly flips between spaced and tight as
    // the ratio crosses the threshold.  Pair keying also keeps the cost
    // proportional to the multi-row items themselves rather than blanking out
    // an entire long list because two of its items wrapped.
    //
    // The list shares a single indentation unit: the width of the
    // *widest* item number plus the fixed ". " suffix for an ordered list
    // (4 columns for a list reaching "10. "), or the bullet marker's
    // width for an unordered list.  Ordered markers are right-aligned
    // within that digit column — the number is left-padded with spaces
    // ("9" -> " 9") — so the ones digits stack vertically (item 9's "9"
    // sits above item 10's "0", not its "1") while the ". " suffix and
    // the content that follows stay at a fixed column.  Every
    // continuation line is indented to that same column, so first lines
    // and wrapped lines all line up as one block.
    //
    // The width is scanned from the items' *actual* numbers rather than
    // assumed to be `start + len - 1`: a source list may number its items
    // non-sequentially, and the digit column must still fit the widest one
    // actually rendered.
    let (max_number_width, max_marker_width) = if ordered {
        let number_width = items
            .iter()
            .enumerate()
            .map(|(index, item)| item.effective_number(start, index))
            .map(|number| display_width(&number.to_string()))
            .max()
            .unwrap_or(1);
        // ". " is two fixed columns after the number column.
        (number_width, number_width + 2)
    } else {
        (0, display_width("• "))
    };
    let mut rendered_items: Vec<RenderedItem> = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        let marker = if ordered {
            ordered_marker(item.effective_number(start, index), max_number_width)
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
            &item.blocks,
            &mut rendered,
            &mut rendered_joins,
            &mut rendered_chrome,
            0,
            width.saturating_sub(indent + max_marker_width),
            &opts.without_prefix(),
        );
        rendered_items.push((marker, rendered, rendered_joins, rendered_chrome));
    }

    // Whether the previously emitted item spanned more than one row; decides
    // the blank between it and the item about to be emitted.
    let mut previous_multi_line = false;
    for (index, (marker, rendered, rendered_joins, rendered_chrome)) in
        rendered_items.into_iter().enumerate()
    {
        // `rendered` is consumed below, so read the row count first.  An item
        // is multi-line whenever it renders to more than one row, which covers
        // both its own text wrapping and a nested list/fence/table inside it.
        let is_multi_line = rendered.len() > 1;
        // A blank row separates this pair when either side is multi-line.
        // `ensure_blank_line_joined` keeps consecutive blanks collapsed into
        // one, so a multi-row item that ends with a nested list (whose own
        // after-margin is already emitted) does not gain a second blank here.
        if index > 0 && (previous_multi_line || is_multi_line) {
            ensure_blank_line_joined(&mut lines, &mut joins, &mut chrome);
        }
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

        // The next pair's spacing decision starts from this item's row count.
        previous_multi_line = is_multi_line;
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
