//! Mouse text selection over the chat history pane.
//!
//! Selecting text with the mouse in a raw-mode TUI is an *app-level* feature:
//! once `EnableMouseCapture` is on, the terminal forwards drag events to the
//! app instead of selecting natively, so the app must (a) track the drag,
//! (b) map the screen rectangle back to the text it covers, and (c) hand the
//! text to the clipboard itself.  This mirrors opencode's select-to-copy.
//!
//! Scope (v1): the chat *history pane* only — the input box and overlay
//! popups are out.  The selection is stored in *content* coordinates — a
//! global content line in `[0, total history height)` plus a viewport column
//! — NOT screen coordinates: mouse events arrive in viewport space, so each
//! is mapped to the content it covers the moment it is processed
//! ([`screen_to_content`], the exact inverse of the click hit-testing the
//! TUI already does).  Storing content coordinates is what lets the
//! selection survive scrolling: the anchor stays pinned to the text it was
//! placed on, while the live drag head re-resolves to the content under the
//! cursor — on wheel events immediately, and on content-induced scrolls
//! (streaming growth, appended turns) at draw time via [`follow_cursor`] —
//! and the draw-time highlight re-evaluates against the current scroll every
//! frame, so what is highlighted is exactly what gets copied.  Resolving
//! fresh at gesture end also means streaming that lands mid-drag cannot
//! corrupt the result: each row maps to whatever content is current then.
//!
//! Coordinates: the history pane's lines are pre-wrapped at `content_width`
//! (viewport width − 9) and drawn in a non-wrapping `Paragraph`, so every
//! semantic line occupies exactly one visual row; the code still walks the
//! cached `visual_offsets` so a hypothetical multi-row line maps correctly.

use crate::state::{App, RenderedTurn, SessionDisplayState, grapheme_offset_at_column};
use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::style::Color;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

/// Background color for the in-progress selection highlight.
///
/// A solid, dedicated color rather than `Modifier::REVERSED`: the history's
/// turns carry explicit `BG_SHADE` backgrounds, so reverse-video would
/// depend on the terminal's swap semantics (and can render dark-on-dark on
/// shaded cells).  A fixed background color reads as a selection on both the
/// shaded turns and the plain text between them, like a terminal's own
/// selection.
pub(crate) const SELECTION_BG: Color = Color::Rgb(0x2F, 0x5F, 0xAF);

/// An in-progress mouse text selection over the history pane.
///
/// Both endpoints are *content* coordinates — a global content line (row in
/// `[0, total history height)`, stable across scrolling) plus a viewport
/// display column — so the selection stays pinned to the text it was drawn
/// over when the user scrolls mid-gesture.  `active` flips to true once the
/// drag has actually moved — that is what distinguishes a selection gesture
/// from a plain click (a click still performs its existing toggle/cursor
/// actions; only a real drag copies text on release).
#[derive(Debug, Clone, Copy)]
pub(crate) struct TextSelection {
    /// Mouse-down position: (content line, viewport column).
    pub anchor: (usize, u16),
    /// Live drag head: (content line, viewport column); updated on every
    /// drag event.
    pub head: (usize, u16),
    /// The last mouse position (viewport row, column) the gesture saw.  When
    /// content moves under a stationary pointer (streaming growth, appended
    /// turns), [`follow_cursor`] re-resolves the head from this screen
    /// position so the selection's live end tracks the cursor.
    pub cursor: (u16, u16),
    /// True once `head != anchor` (a real selection, not a click).
    pub active: bool,
}

/// Map a viewport position to content space: the global content line under
/// that screen row, plus the viewport column unchanged.
///
/// The exact inverse of `find_turn_at_row`'s formula (content line = screen
/// row + total − scroll − vh), clamped into the valid content range: a row
/// in the blank band above short bottom-anchored content resolves to content
/// line 0, and a row at/below the last content line resolves to the last
/// line (a drag past the pane edge selects through the bottom).  The column
/// is left as-is — it is resolved against the line's content range at
/// highlight/extraction time.
fn screen_to_content(app: &App, row: u16, column: u16) -> (usize, u16) {
    let vh = app.history_viewport.height as isize;
    let total = app.total_history_height() as isize;
    let scroll = app.effective_scroll() as isize;
    let last_line = (total - 1).max(0);
    let content_line = (row as isize + total - scroll - vh).clamp(0, last_line);
    (content_line as usize, column)
}

/// Map a global content line to its screen row for the current scroll and
/// viewport — the exact inverse of [`screen_to_content`] (content row `c`
/// sits at screen row `c + scroll + vh − total`).  `None` when the line is
/// scrolled out of view.  Shared by the tests that locate content on screen
/// (`locate`, `first_content_row`) so the bottom-anchored formula lives in
/// one place.
#[cfg(test)]
pub(crate) fn content_to_screen_row(app: &App, content_line: usize) -> Option<u16> {
    let vh = app.history_viewport.height as isize;
    let total = app.total_history_height() as isize;
    let scroll = app.effective_scroll() as isize;
    let screen_row = content_line as isize + scroll + vh - total;
    if screen_row < 0 || screen_row >= vh {
        return None;
    }
    Some(screen_row as u16)
}

/// Arm a potential selection at the mouse-down position.  A selection only
/// becomes real (highlighted, copied) once the drag moves.
pub(crate) fn start_selection(app: &mut App, row: u16, column: u16) {
    let anchor = screen_to_content(app, row, column);
    app.text_selection = Some(TextSelection {
        anchor,
        head: anchor,
        cursor: (row, column),
        active: false,
    });
}

/// Whether a selection gesture is in progress (between mouse-down and
/// mouse-up in the history pane).
pub(crate) fn is_selecting(app: &App) -> bool {
    app.text_selection.is_some()
}

/// Extend the selection to the current drag position.
pub(crate) fn update_selection(app: &mut App, row: u16, column: u16) {
    // Resolve the head before the mutable borrow so `screen_to_content`
    // (which reads app state) and the `text_selection` write don't overlap.
    let head = screen_to_content(app, row, column);
    if let Some(sel) = &mut app.text_selection {
        sel.head = head;
        sel.cursor = (row, column);
        if sel.head != sel.anchor {
            sel.active = true;
        }
    }
}

/// Abandon the in-progress selection (right-click, page switch…).
pub(crate) fn cancel_selection(app: &mut App) {
    app.text_selection = None;
}

/// Re-resolve the selection's live head to the content now under the cursor.
///
/// Called from the draw path right after the height-prefix rebuild settles
/// content-induced viewport movement (streaming growth, appended turns,
/// undo/redo): the head is stored in content coordinates at the last mouse
/// event, but when the viewport's content moves under a stationary pointer
/// the text under the cursor is different — so the head must be re-derived
/// from the remembered screen position, or the highlight (and the copy on
/// release) would lag until the next drag.  Terminal-native drag-while-
/// scroll: the anchor stays pinned to the text it was placed on, the live
/// end follows the cursor.  A gesture that has not yet been activated by a
/// drag is never touched (a plain click + content scroll must not silently
/// start a selection).
pub(crate) fn follow_cursor(app: &mut App) {
    let Some(sel) = app.text_selection else {
        return;
    };
    if !sel.active {
        return;
    }
    let head = screen_to_content(app, sel.cursor.0, sel.cursor.1);
    if let Some(sel) = &mut app.text_selection {
        sel.head = head;
    }
}

/// The (anchor, head) endpoints of the active selection, or `None` when there
/// is no active selection (including a plain click that never dragged).
///
/// Returned as stored — deliberately NOT sorted into a start/end pair: the
/// column semantics are anchor-fixed (see [`selection_bounds_for_line`]), so
/// which endpoint owns which column depends on the row each sits on, not on
/// their order.  Lexicographically sorting here would swap the columns on a
/// reverse drag.
pub(crate) fn selection_range(app: &App) -> Option<((usize, u16), (usize, u16))> {
    let sel = app.text_selection?;
    if !sel.active {
        return None;
    }
    Some((sel.anchor, sel.head))
}

/// The display-column range `(lo, hi)` the selection covers on `line`, in
/// viewport columns.  `hi` of `usize::MAX` means "to the end of the line".
///
/// Terminal-native anchor semantics: the anchor row always extends from the
/// anchor column to end-of-line and the head row from start-of-line to the
/// head column — so a bottom-to-top drag that also moves horizontally
/// *mirrors* the columns instead of swapping them (dragging from bottom-right
/// to top-left selects `[0, head_col)` on the top row and `[anchor_col, EOL)`
/// on the bottom row, NOT the same rectangle as the forward drag, which is
/// what the old lexicographic normalization produced).  A drag that never
/// leaves its row is just the span between the two columns; middle rows are
/// full width.
fn selection_bounds_for_line(
    anchor: (usize, u16),
    head: (usize, u16),
    line: usize,
) -> (usize, usize) {
    if anchor.0 == head.0 {
        let (lo, hi) = (anchor.1.min(head.1), anchor.1.max(head.1));
        (lo as usize, hi as usize)
    } else if line == anchor.0 {
        (anchor.1 as usize, usize::MAX)
    } else if line == head.0 {
        (0, head.1 as usize)
    } else {
        (0, usize::MAX)
    }
}

/// Finish the selection: return the selected text (if the gesture was a real
/// drag over copyable rows) and always clear the selection state.  Returns
/// `None` for a plain click.
///
/// The release position is deliberately NOT consulted: the head already sits
/// where the last drag — or the draw-time [`follow_cursor`] sync — left it
/// in *content* coordinates, and re-resolving the release screen position
/// would point at whatever content now happens to sit under the cursor, which
/// after a mid-gesture scroll is NOT the text the user selected.  Only
/// explicit drag events and `follow_cursor` move the head, so the selection
/// stays pinned to the text even when the viewport moved under it.
pub(crate) fn finish_selection(app: &mut App) -> Option<String> {
    let text = if app.text_selection.is_some_and(|s| s.active) {
        extract_selection_text(app)
    } else {
        None
    };
    app.text_selection = None;
    text
}

/// Drive one mouse event through an in-progress selection gesture.
///
/// Returns the text to copy when a left-button release completed a real
/// selection (`None` otherwise — a plain click, a cancelled gesture, or any
/// drag/scroll event).  The caller performs the clipboard write and surfaces
/// the status; the entire gesture state machine lives here.
pub(crate) fn handle_selection_mouse(app: &mut App, mouse: &MouseEvent) -> Option<String> {
    match mouse.kind {
        MouseEventKind::Drag(MouseButton::Left) => {
            update_selection(app, mouse.row, mouse.column);
            None
        }
        MouseEventKind::Up(MouseButton::Left) => finish_selection(app),
        // A scroll wheel mid-gesture scrolls immediately AND keeps the
        // selection: the anchor stays pinned to the text it was placed on
        // (content coordinates), while the live drag head re-resolves to the
        // content now under the cursor — so the selection tracks the cursor
        // as the viewport moves, and the highlight updates on the wheel event
        // itself (terminal-native drag-while-scroll).  The scroll is applied
        // synchronously (not via the frame accumulator) so the head is
        // resolved against the post-scroll content immediately.
        MouseEventKind::ScrollUp => {
            app.scroll_up(1);
            update_selection(app, mouse.row, mouse.column);
            None
        }
        MouseEventKind::ScrollDown => {
            app.scroll_down(1);
            update_selection(app, mouse.row, mouse.column);
            None
        }
        _ => {
            // Any other mouse event (right-click, a second Down before the
            // first Up) cancels the gesture.
            cancel_selection(app);
            None
        }
    }
}

/// Extract the plain text covered by the active selection rectangle.
///
/// Content lines are resolved one at a time through the height prefix + the
/// render cache.  Lines that resolve to no copyable content — pure-chrome
/// rows, image blocks, lines past the end — are skipped, so the copyable
/// region is exactly the region the highlight covers.
fn extract_selection_text(app: &App) -> Option<String> {
    let (anchor, head) = selection_range(app)?;
    let display = app.active_display_ref()?;
    let vp_width = app.history_viewport.width as usize;
    let (start_line, end_line) = (anchor.0.min(head.0), anchor.0.max(head.0));
    let mut out = String::new();
    // Iterate the selection's *content* lines directly — no screen mapping,
    // which is exactly why the selection survives scrolling (the endpoints
    // are content-anchored, so this is scroll-independent).
    for content_line in start_line..=end_line {
        let (col_lo, col_hi) = selection_bounds_for_line(anchor, head, content_line);
        if let Some(text) = text_for_content_line(display, vp_width, content_line, col_lo, col_hi) {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&text);
        }
    }
    if out.is_empty() { None } else { Some(out) }
}

/// Resolve one content line of the selection to the text slice it covers.
fn text_for_content_line(
    display: &SessionDisplayState,
    vp_width: usize,
    content_line: usize,
    col_start: usize,
    col_end: usize,
) -> Option<String> {
    // Map the global content line to a visible turn and the turn-local
    // visual row — the inverse of `find_turn_at_row`'s screen mapping (the
    // height prefix is the same cumulative array that function binary
    // searches).
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
    let rendered = cached_rendered_turn(display, turn_idx, vp_width)?;
    let (line_idx, (lo, hi)) = content_range_for_row(
        &rendered.visual_offsets,
        &rendered.content_ranges,
        visual_row,
        col_start,
        col_end,
        vp_width,
    )?;
    Some(slice_line_columns(&rendered.lines[line_idx], lo, hi))
}

/// Resolve a turn-local visual row and viewport column range to the
/// content-clamped display-column range of the semantic line it covers.
///
/// The single row→line→column mapping shared by the extraction path
/// ([`text_for_content_line`]) and the draw-time highlight
/// ([`apply_selection_to_lines`]), so the two can never drift apart again —
/// they already diverged twice (the screen-row offset bug and the
/// within-line column bug, both fixed by pinning exactly this mapping).
/// `visual_offsets`/`content_ranges` are the turn's cached arrays, aligned
/// with its rendered lines (see the `debug_assert`s in
/// `cached_or_compute_lines`); `col_end == usize::MAX` means "to the end of
/// the line".  Returns `(line_idx, (lo, hi))` — the semantic line and its
/// selectable display-column range — or `None` when the row maps to no
/// selectable content (pure-chrome rows, image rows, rows past the end of
/// the text).
fn content_range_for_row(
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
    let (lo, hi) = match content {
        Some((lo, hi)) if lo < hi => (line_col_lo.max(lo), line_col_hi.min(hi)),
        _ => return None,
    };
    if lo >= hi {
        return None;
    }
    Some((line_idx, (lo, hi)))
}

/// Read-only render-cache lookup for a turn's rendered lines.
///
/// Mirrors the key the renderer computes (see `render_history`): only an
/// entry with matching turn id and widths is reused, so a stale entry (e.g.
/// from before a resize) is treated as a miss and the row is skipped rather
/// than extracting text from the wrong wrapping.
fn cached_rendered_turn(
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

/// Apply the selection highlight to the visible slice of one turn's lines.
///
/// Called from `render_history` for the visible semantic-line slice of each
/// turn.  For every line occupying any selected *content* line, the covered
/// column range (translated into the line's own column space) is restyled
/// with the selection background.  The render cache is never mutated: lines
/// are restyled at draw time only.
pub(crate) fn apply_selection_to_lines(
    app: &App,
    turn_start: usize,
    text_offsets: &[usize],
    content_ranges: &[Option<(usize, usize)>],
    line_start: usize,
    lines: &mut [Line<'static>],
) {
    let (anchor, head) = match selection_range(app) {
        Some(range) => range,
        None => return,
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
            let Some((_, (c_lo, c_hi))) = content_range_for_row(
                text_offsets,
                content_ranges,
                vr,
                col_lo,
                col_hi,
                vp.width as usize,
            ) else {
                continue;
            };
            *line = style_line_selection(line, c_lo, c_hi);
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
pub(crate) fn style_line_selection(
    line: &Line<'static>,
    col_lo: usize,
    col_hi: usize,
) -> Line<'static> {
    if col_lo >= col_hi {
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
        if span_hi <= col_lo || span_lo >= col_hi {
            // Entirely before or after the selection — keep as-is.
            out.push(span.clone());
            continue;
        }
        // Overlap: split this span at the selection boundaries, snapping both
        // cuts to grapheme boundaries.
        let before_w = col_lo.saturating_sub(span_lo);
        let sel_hi_col = col_hi.saturating_sub(span_lo).min(span_w);
        let mut sel_lo = grapheme_offset_at_column(span_text, before_w);
        let mut sel_hi = grapheme_offset_at_column(span_text, sel_hi_col);
        // Defensive monotonicity (columns are ordered; the snap can't invert
        // them, but a malformed range must never panic on the slice below).
        if sel_lo > sel_hi {
            std::mem::swap(&mut sel_lo, &mut sel_hi);
        }
        let before = &span_text[..sel_lo];
        let selected = &span_text[sel_lo..sel_hi];
        let after = &span_text[sel_hi..];
        if !before.is_empty() {
            out.push(Span::styled(before.to_owned(), span.style));
        }
        out.push(Span::styled(
            selected.to_owned(),
            span.style.bg(SELECTION_BG),
        ));
        if !after.is_empty() {
            out.push(Span::styled(after.to_owned(), span.style));
        }
    }
    Line::from(out)
}

/// Concatenate a rendered line's spans into its plain text.
fn line_text(line: &Line<'_>) -> String {
    let mut text = String::new();
    for span in &line.spans {
        text.push_str(span.content.as_ref());
    }
    text
}

/// Slice a rendered line's text by display columns, snapping to grapheme
/// boundaries (a selection can never split a ZWJ emoji or combining mark).
fn slice_line_columns(line: &Line<'_>, col_lo: usize, col_hi: usize) -> String {
    let text = line_text(line);
    let width = UnicodeWidthStr::width(text.as_str());
    let lo = grapheme_offset_at_column(&text, col_lo.min(width));
    let hi = grapheme_offset_at_column(&text, col_hi.min(width));
    text[lo.min(hi)..lo.max(hi)].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::find_turn_at_row;
    use crate::test_util::test_app;
    use choreo_proto::Turn;

    /// Build a turn with a single assistant text line (and no user text) so
    /// the rendered layout is exactly one line per turn — the easiest canvas
    /// for asserting exact extracted text.  `turns` is a `BTreeMap` keyed by
    /// turn id, so iteration order (and thus render order) is deterministic.
    fn turn(assistant: &str) -> Turn {
        Turn {
            created_at: choreo_proto::TimestampMs::now(),
            undone: false,
            error: None,
            user_text: None,
            assistant_text: Some(assistant.into()),
            assistant_reasoning: None,
            tool_calls: vec![],
            token_usage: None,
            tool_results: vec![],
            displayed_images: vec![],
            reasoning_artifact: None,
            reasoning_producer: None,
        }
    }

    /// Build an app with `turns` rendered, viewport wide enough that the
    /// whole history is visible (so no row is scrolled out), with the render
    /// cache populated.  `vh` must be >= total height (tests pass a large
    /// value); the selection coordinates below are derived from the actual
    /// layout via [`locate`], never assumed.
    fn app_with_turns(turns: &[(u32, &str)], vh: u16) -> App {
        let mut app = test_app();
        app.history_viewport.width = 80;
        app.history_viewport.height = vh;
        for (id, text) in turns {
            app.display_for(0).view.insert_or_replace(*id, turn(text));
        }
        app.rebuild_height_prefix();
        app
    }

    fn drag_and_finish(app: &mut App, from: (u16, u16), to: (u16, u16)) -> Option<String> {
        start_selection(app, from.0, from.1);
        update_selection(app, to.0, to.1);
        finish_selection(app)
    }

    /// Locate `needle` in the rendered history and return its exact selection
    /// rectangle in viewport coordinates, derived from the actual render
    /// cache + `height_prefix`.  Assistant turns render inside a boxed,
    /// shaded block (a `┃` gutter + separator rows), so the tests must not
    /// hardcode row/column offsets — this helper turns the layout upside
    /// down: it finds where the text *actually* is and feeds that back in,
    /// verifying the row→turn→line→column mapping end to end.
    fn locate(app: &App, needle: &str) -> ((u16, u16), (u16, u16)) {
        let display = app.active_display_ref().expect("active display");
        for (turn_idx, _turn_id) in display.visible_turn_ids.iter().enumerate() {
            let cached = display.render_cache[turn_idx]
                .as_ref()
                .expect("render cache populated by rebuild_height_prefix");
            let turn_start = display
                .height_prefix
                .get(turn_idx.wrapping_sub(1))
                .copied()
                .unwrap_or(0);
            for (line_idx, line) in cached.rendered.lines.iter().enumerate() {
                let text = line_text(line);
                if let Some(char_off) = text.find(needle) {
                    let col_start = UnicodeWidthStr::width(&text[..char_off]);
                    let col_end = col_start + UnicodeWidthStr::width(needle);
                    let row_lo = cached
                        .rendered
                        .visual_offsets
                        .get(line_idx.wrapping_sub(1))
                        .copied()
                        .unwrap_or(0);
                    // Every rendered line occupies one visual row in practice.
                    let screen_row = content_to_screen_row(app, turn_start + row_lo)
                        .expect("needle must be on screen");
                    return ((screen_row, col_start as u16), (screen_row, col_end as u16));
                }
            }
        }
        panic!("needle {needle:?} not found in rendered history");
    }

    // ── state machine ──

    #[test]
    fn click_without_drag_is_not_a_selection() {
        let mut app = app_with_turns(&[(0, "hello"), (1, "world")], 30);
        let ((r, c), _) = locate(&app, "hello");
        start_selection(&mut app, r, c);
        update_selection(&mut app, r, c); // no movement
        assert!(!app.text_selection.unwrap().active);
        assert_eq!(finish_selection(&mut app), None);
        assert!(
            app.text_selection.is_none(),
            "selection cleared after finish"
        );
    }

    #[test]
    fn drag_activates_and_finish_clears() {
        let mut app = app_with_turns(&[(0, "hello"), (1, "world")], 30);
        let (start, _) = locate(&app, "hello");
        let (_, end) = locate(&app, "world");
        start_selection(&mut app, start.0, start.1);
        update_selection(&mut app, end.0, end.1);
        assert!(app.text_selection.unwrap().active);
        assert!(finish_selection(&mut app).is_some());
        assert!(app.text_selection.is_none());
    }

    // ── extraction ──

    #[test]
    fn extract_single_row_mid_line() {
        let mut app = app_with_turns(&[(0, "hello world")], 30);
        let (start, end) = locate(&app, "hello");
        start_selection(&mut app, start.0, start.1);
        update_selection(&mut app, end.0, end.1);
        let text = finish_selection(&mut app).expect("selection should extract");
        assert_eq!(text, "hello");
    }

    #[test]
    fn extract_skips_box_chrome_across_turns() {
        // Selecting across boxed turn blocks must copy ONLY the content text:
        // the `┃` gutter, leading box padding, and trailing fill of every
        // selected row are excluded, and pure-chrome rows (separators,
        // padding) contribute nothing.  Regression for the box chrome being
        // dragged into the copied text.  (Also covers the multi-row case:
        // both turns' words arrive in render order joined by newlines.)
        let mut app = app_with_turns(&[(0, "first"), (1, "second")], 30);
        let (start, _) = locate(&app, "first");
        let (_, end) = locate(&app, "second");
        let text = drag_and_finish(&mut app, start, end).expect("selection should extract");
        assert!(
            !text.contains('┃'),
            "box gutter must not be copied: {text:?}"
        );
        let first_pos = text.find("first").expect("first word copied");
        let second_pos = text.find("second").expect("second word copied");
        assert!(first_pos < second_pos, "turns copied in render order");
        assert!(text.contains('\n'), "rows are joined with newlines");
        for line in text.lines() {
            assert_eq!(
                line.trim_end().len(),
                line.len(),
                "no trailing box fill spaces copied: {line:?}"
            );
        }
    }

    #[test]
    fn extract_mid_line_start_and_end() {
        let mut app = app_with_turns(&[(0, "abcdefghij")], 30);
        let (start, end) = locate(&app, "cdef");
        start_selection(&mut app, start.0, start.1);
        update_selection(&mut app, end.0, end.1);
        let text = finish_selection(&mut app).expect("selection should extract");
        assert_eq!(text, "cdef");
    }

    #[test]
    fn reverse_diagonal_drag_keeps_columns_anchored() {
        // Terminal-native anchor semantics: the anchor row extends from the
        // anchor column to end-of-line and the head row from start-of-line to
        // the head column — so a bottom-to-top drag that also moves
        // horizontally *mirrors* the columns instead of swapping them (the
        // old lexicographic normalize swapped them, extracting a mirror-image
        // rectangle: for this drag it would have yielded "one\nline ").
        let mut app = app_with_turns(&[(0, "line one"), (1, "line two")], 30);
        // Head at the end of "lin" on the top turn's content row; anchor at
        // the start of "wo" on the bottom turn's content row (a reverse
        // diagonal drag).
        let (_, head_end) = locate(&app, "lin");
        let (anchor_start, _) = locate(&app, "wo");
        let text =
            drag_and_finish(&mut app, anchor_start, head_end).expect("selection should extract");
        // Head row: [0, head_col) → "lin"; anchor row: [anchor_col, EOL) → "wo".
        assert_eq!(text, "lin\nwo");
    }

    #[test]
    fn selection_bounds_follow_anchor_semantics() {
        // Anchor row: [anchor_col, EOL); head row: [0, head_col); middle
        // rows: full width — regardless of which endpoint is on top.
        let anchor = (5, 30);
        let head = (2, 4);
        assert_eq!(selection_bounds_for_line(anchor, head, 2), (0, 4));
        assert_eq!(selection_bounds_for_line(anchor, head, 3), (0, usize::MAX));
        assert_eq!(selection_bounds_for_line(anchor, head, 5), (30, usize::MAX));
        // Same-row drags are just the span between the two columns.
        assert_eq!(selection_bounds_for_line((3, 8), (3, 2), 3), (2, 8));
    }

    #[test]
    fn extract_wide_chars_use_display_columns() {
        // 日本語 is 6 display columns wide; selecting the rectangle locate()
        // reports for 日本 must copy exactly those two CJK chars — if the
        // column math counted chars instead of display columns, the slice
        // would land inside the wide characters.
        let mut app = app_with_turns(&[(0, "日本語")], 30);
        let (start, end) = locate(&app, "日本");
        start_selection(&mut app, start.0, start.1);
        update_selection(&mut app, end.0, end.1);
        let text = finish_selection(&mut app).expect("selection should extract");
        assert_eq!(text, "日本");
    }

    #[test]
    fn extract_rows_outside_content_are_skipped() {
        // Viewport taller than content: rows above the content (the blank
        // band) resolve to no turn and must be skipped, not cancel the copy.
        let mut app = app_with_turns(&[(0, "hello")], 30);
        let (_, end) = locate(&app, "hello");
        let text = drag_and_finish(&mut app, (0, 0), end).expect("selection should extract");
        assert!(text.contains("hello"), "blank band skipped, content copied");
    }

    #[test]
    fn extract_via_render_cache_matches_highlight_source() {
        // Extraction reads the same cached lines the highlight styles, so the
        // copied text is exactly what the user sees highlighted.
        let mut app = app_with_turns(&[(0, "styled text")], 30);
        let (start, end) = locate(&app, "text");
        start_selection(&mut app, start.0, start.1);
        update_selection(&mut app, end.0, end.1);
        let text = finish_selection(&mut app).expect("selection should extract");
        assert_eq!(text, "text");
    }

    #[test]
    fn anchor_stays_pinned_to_text_while_head_tracks_the_cursor() {
        // Scrolling mid-gesture: the anchor stays on the text it was placed
        // on (content coordinates), while the live drag head re-resolves to
        // the content now under the (stationary) cursor — so the selection
        // tracks the cursor as the viewport moves, and the highlight reflects
        // it on the scroll event itself rather than waiting for the next
        // drag.
        let mut app = test_app();
        app.history_viewport.width = 80;
        app.history_viewport.height = 10;
        for i in 0..20u32 {
            app.display_for(0)
                .view
                .insert_or_replace(i, turn(&format!("turn {i}")));
        }
        app.rebuild_height_prefix();
        assert!(
            app.total_history_height() > app.history_viewport.height as usize,
            "history must overflow the viewport"
        );

        // Drag rows 2..7 at scroll 0: anchor = content line 92, head = line 97.
        start_selection(&mut app, 2, 3);
        update_selection(&mut app, 7, 80);
        assert!(app.text_selection.unwrap().active);
        let ((a0, _), (h0, _)) = selection_range(&app).unwrap();
        assert_eq!((a0, h0), (92, 97));

        // Simulate the selection arm's scroll handling: apply the scroll,
        // then re-resolve the head at the cursor (the wheel event position).
        app.scroll_accumulator = 3;
        app.apply_scroll_delta();
        update_selection(&mut app, 7, 80);
        let ((a1, _), (h1, _)) = selection_range(&app).unwrap();
        assert_eq!(
            a1, 92,
            "the anchor stays pinned to the text it was placed on"
        );
        assert_eq!(h1, 94, "the head tracks the content now under the cursor");
        assert!(
            app.text_selection.is_some_and(|s| s.active),
            "scrolling must not cancel the selection"
        );
    }

    #[test]
    fn release_uses_head_from_last_sync_without_rewinding() {
        // The release event never moves the head: after a mid-gesture scroll
        // the draw-time `follow_cursor` sync re-resolves it to the content
        // under the cursor, and releasing must preserve exactly that —
        // re-resolving the release *screen* position would point at different
        // content than the cursor sat on at the last draw (the viewport moved
        // under it).
        let mut app = test_app();
        app.history_viewport.width = 80;
        app.history_viewport.height = 10;
        for i in 0..20u32 {
            app.display_for(0)
                .view
                .insert_or_replace(i, turn(&format!("turn {i}")));
        }
        app.rebuild_height_prefix();

        // Drag rows 2..4 at scroll 0, then scroll up 3 and let the draw-time
        // sync re-resolve the head (a wheel mid-gesture does both of these).
        start_selection(&mut app, 2, 3);
        update_selection(&mut app, 4, 10);
        app.scroll_accumulator = 3;
        app.apply_scroll_delta();
        follow_cursor(&mut app);

        let expected = extract_selection_text(&app).expect("selection text after the sync");
        // Release: the copied text is whatever the last sync left — the
        // release position is never consulted, so it can neither extend nor
        // rewind the selection.
        let actual = finish_selection(&mut app).expect("selection text on release");
        assert_eq!(actual, expected, "release must not move the head");
    }

    #[test]
    fn follow_cursor_reanchors_head_after_content_scrolls() {
        // New content appended at the bottom while a selection is active (the
        // auto-following bottom of a streaming session): the content under
        // the stationary cursor changes, so the draw-time sync must re-resolve
        // the head to it — without any mouse movement.  The anchor stays
        // pinned to the text it was placed on.
        let mut app = test_app();
        app.history_viewport.width = 80;
        app.history_viewport.height = 10;
        for i in 0..20u32 {
            app.display_for(0)
                .view
                .insert_or_replace(i, turn(&format!("turn {i}")));
        }
        app.rebuild_height_prefix();

        // Drag rows 2..5 at scroll 0 (bottom-anchored: content lines
        // total−vh+2 ..= total−vh+5).
        start_selection(&mut app, 2, 3);
        update_selection(&mut app, 5, 40);
        let ((anchor0, _), (head0, _)) = selection_range(&app).unwrap();
        assert_eq!((anchor0, head0), (92, 95));

        // A new turn streams in below, growing the history exactly as a
        // daemon message would (the viewport is bottom-anchored, so the old
        // content shifts up and the content under the stationary cursor moves
        // down).
        app.display_for(0)
            .view
            .insert_or_replace(20, turn("turn 20"));
        app.rebuild_height_prefix();
        follow_cursor(&mut app);

        let ((anchor1, _), (head1, _)) = selection_range(&app).unwrap();
        assert_eq!(anchor1, 92, "the anchor stays pinned to its text");
        assert_eq!(head1, 100, "the head tracks the content under the cursor");
        assert!(
            app.text_selection.is_some_and(|s| s.active),
            "content scroll must keep the selection active"
        );
    }

    #[test]
    fn follow_cursor_does_not_activate_an_armed_click() {
        // A plain click (armed but never dragged) must not silently become a
        // selection when the content scrolls under it — only an explicit drag
        // activates the gesture.
        let mut app = app_with_turns(&[(0, "hello")], 30);
        let ((r, c), _) = locate(&app, "hello");
        start_selection(&mut app, r, c);
        follow_cursor(&mut app);
        assert!(
            !app.text_selection.unwrap().active,
            "a plain click + content scroll must not start a selection"
        );
        assert_eq!(finish_selection(&mut app), None, "nothing to copy");
    }

    // ── style_line_selection ──

    #[test]
    fn style_selection_splits_single_span() {
        let line = Line::from("hello world");
        let styled = style_line_selection(&line, 6, 11);
        // ["hello ", "world"(SELECTION_BG)] — the selected slice gets the
        // selection background.
        assert_eq!(styled.spans.len(), 2);
        assert_eq!(styled.spans[0].content, "hello ");
        assert_eq!(styled.spans[1].content, "world");
        assert_eq!(styled.spans[1].style.bg, Some(SELECTION_BG));
    }

    #[test]
    fn style_selection_middle_of_span() {
        let line = Line::from("abcdefgh");
        let styled = style_line_selection(&line, 2, 5);
        assert_eq!(styled.spans.len(), 3);
        assert_eq!(styled.spans[0].content, "ab");
        assert_eq!(styled.spans[1].content, "cde");
        assert_eq!(styled.spans[1].style.bg, Some(SELECTION_BG));
        assert_eq!(styled.spans[2].content, "fgh");
    }

    #[test]
    fn style_selection_full_line_keeps_spans() {
        let line = Line::from("hello");
        let styled = style_line_selection(&line, 0, usize::MAX);
        assert_eq!(styled.spans.len(), 1);
        assert_eq!(styled.spans[0].style.bg, Some(SELECTION_BG));
    }

    #[test]
    fn style_selection_no_overlap_returns_unchanged() {
        let line = Line::from("hello");
        let styled = style_line_selection(&line, 5, 10);
        assert_eq!(styled, line);
    }

    #[test]
    fn style_selection_does_not_split_zwj_emoji() {
        // The selection boundary at column 1 falls inside the first emoji
        // (2 columns wide); the cut must snap to a grapheme boundary, so the
        // emoji stays whole in one span and the selected slice starts after
        // it.  The property under test is *no grapheme is split*, not where
        // the boundary lands.
        let line = Line::from("😀abc");
        let styled = style_line_selection(&line, 1, 3);
        // Emoji (cols 0..2) stays whole and unselected; 'a' (col 2) is the
        // selected cell; 'bc' follows unselected.
        let texts: Vec<String> = styled.spans.iter().map(|s| s.content.to_string()).collect();
        assert_eq!(
            texts,
            vec!["😀".to_string(), "a".to_string(), "bc".to_string()]
        );
        assert_eq!(styled.spans[1].style.bg, Some(SELECTION_BG));
        assert_ne!(styled.spans[0].style.bg, Some(SELECTION_BG));
    }

    #[test]
    fn style_selection_preserves_span_style() {
        let styled = Line::from(Span::styled(
            "abcdef",
            ratatui::style::Style::default().fg(ratatui::style::Color::Cyan),
        ));
        let styled = style_line_selection(&styled, 1, 3);
        assert_eq!(styled.spans.len(), 3);
        // The unselected pieces keep Cyan; the selected piece is Cyan + the
        // selection background.
        assert_eq!(styled.spans[0].style.fg, Some(ratatui::style::Color::Cyan));
        assert_eq!(styled.spans[1].style.fg, Some(ratatui::style::Color::Cyan));
        assert_eq!(styled.spans[1].style.bg, Some(SELECTION_BG));
        assert_eq!(styled.spans[2].style.fg, Some(ratatui::style::Color::Cyan));
    }

    #[test]
    fn style_selection_overrides_shaded_background() {
        // History turns render on the dark-gray BG_SHADE background (see
        // `add_margin_lines`); the selection must replace it with the visible
        // selection background, not stack on top of it.
        let shaded = Line::from(Span::styled(
            "hello",
            ratatui::style::Style::default().bg(crate::render::BG_SHADE),
        ));
        let styled = style_line_selection(&shaded, 0, 5);
        assert_eq!(styled.spans.len(), 1);
        assert_eq!(styled.spans[0].style.bg, Some(SELECTION_BG));
    }

    // ── apply_selection_to_lines (screen-row mapping) ──

    /// Apply a full-width selection to `screen_row` and report whether that
    /// row's line ended up with the selection background.
    fn row_highlighted(app: &mut App, screen_row: u16) -> bool {
        let vp_width = app.history_viewport.width;
        // The selection is stored in content space, so the target screen row
        // is converted to its content line via the exact mapping
        // `start_selection` applies (`screen_to_content`), not a hand-rolled
        // copy of the formula.
        let (content_line, _) = screen_to_content(app, screen_row, 0);
        app.text_selection = Some(TextSelection {
            anchor: (content_line, 0),
            head: (content_line, vp_width),
            cursor: (screen_row, 0),
            active: true,
        });
        let (turn_idx, visual_row) = find_turn_at_row(app, screen_row).expect("row maps");
        let (cached_lines, offsets, content_ranges, turn_start) = {
            let display = app.active_display_ref().unwrap();
            let cached = display.render_cache[turn_idx].as_ref().unwrap();
            (
                cached.rendered.lines.clone(),
                cached.rendered.visual_offsets.clone(),
                cached.rendered.content_ranges.clone(),
                display
                    .height_prefix
                    .get(turn_idx.wrapping_sub(1))
                    .copied()
                    .unwrap_or(0),
            )
        };
        let mut lines = cached_lines.to_vec();
        apply_selection_to_lines(app, turn_start, &offsets, &content_ranges, 0, &mut lines);
        let line_idx = offsets
            .partition_point(|&o| o <= visual_row)
            .min(lines.len().saturating_sub(1));
        lines[line_idx]
            .spans
            .iter()
            .any(|s| s.style.bg == Some(SELECTION_BG))
    }

    #[test]
    fn apply_selection_to_lines_overflowing_history_styles_visible_rows() {
        // Regression (kept as a behavior pin): when the history overflows
        // the viewport (the common long-conversation case), the content is
        // bottom-anchored — a saturating unsigned screen-row offset once
        // clamped to 0 there, so no screen row ever matched and the
        // highlight was never drawn.  The content-space selection has no
        // screen-row math to get wrong, but this still pins that content
        // rows highlight while chrome rows never do in the overflowing
        // layout.
        let mut app = test_app();
        app.history_viewport.width = 80;
        app.history_viewport.height = 10;
        for i in 0..6 {
            app.display_for(0)
                .view
                .insert_or_replace(i, turn(&format!("turn {i}")));
        }
        app.rebuild_height_prefix();
        let vh = app.history_viewport.height as usize;
        let total = app.total_history_height();
        assert!(total > vh, "history must overflow the viewport");

        // Find one visible content row and one visible chrome row (the box
        // separator/padding rows carry no content and must stay
        // unhighlighted).
        let (content_row, chrome_row) = {
            let display = app.active_display_ref().unwrap();
            let mut content_row = None;
            let mut chrome_row = None;
            for screen_row in 0..vh as u16 {
                let (turn_idx, visual_row) = find_turn_at_row(&app, screen_row).expect("row maps");
                let cached = display.render_cache[turn_idx].as_ref().unwrap();
                let line_idx = cached
                    .rendered
                    .visual_offsets
                    .partition_point(|&o| o <= visual_row)
                    .min(cached.rendered.lines.len().saturating_sub(1));
                let has_content = cached
                    .rendered
                    .content_ranges
                    .get(line_idx)
                    .copied()
                    .flatten()
                    .is_some_and(|(lo, hi)| lo < hi);
                if has_content {
                    content_row.get_or_insert(screen_row);
                } else {
                    chrome_row.get_or_insert(screen_row);
                }
                if content_row.is_some() && chrome_row.is_some() {
                    break;
                }
            }
            (
                content_row.expect("a visible content row must exist"),
                chrome_row.expect("a visible chrome row must exist"),
            )
        };

        assert!(
            row_highlighted(&mut app, content_row),
            "a content row must be highlighted"
        );
        assert!(
            !row_highlighted(&mut app, chrome_row),
            "a chrome row (separator/padding) must not be highlighted"
        );
    }

    #[test]
    fn apply_selection_to_lines_short_history_styles_visible_rows() {
        // The short-history case (content fits the viewport, blank band on
        // top): content row c maps to screen row c + vh - total (positive).
        // The selection is built through the public API so the screen →
        // content conversion is exercised (`locate` returns viewport coords).
        let mut app = app_with_turns(&[(0, "hello"), (1, "world")], 30);
        let (start, _) = locate(&app, "hello");
        start_selection(&mut app, start.0, start.1);
        update_selection(&mut app, start.0, start.1.saturating_add(5));
        let (turn_idx, _) = find_turn_at_row(&app, start.0).expect("row maps");
        let (cached_lines, offsets, content_ranges, turn_start) = {
            let display = app.active_display_ref().unwrap();
            let cached = display.render_cache[turn_idx].as_ref().unwrap();
            (
                cached.rendered.lines.clone(),
                cached.rendered.visual_offsets.clone(),
                cached.rendered.content_ranges.clone(),
                display
                    .height_prefix
                    .get(turn_idx.wrapping_sub(1))
                    .copied()
                    .unwrap_or(0),
            )
        };
        let mut lines = cached_lines.to_vec();
        apply_selection_to_lines(&app, turn_start, &offsets, &content_ranges, 0, &mut lines);
        assert!(
            lines
                .iter()
                .any(|l| l.spans.iter().any(|s| s.style.bg == Some(SELECTION_BG))),
            "the selected row must be highlighted"
        );
    }

    // ── slice_line_columns ──

    #[test]
    fn slice_columns_basic() {
        let line = Line::from("hello world");
        assert_eq!(slice_line_columns(&line, 0, 5), "hello");
        assert_eq!(slice_line_columns(&line, 6, usize::MAX), "world");
    }

    #[test]
    fn slice_columns_clamps_past_end() {
        let line = Line::from("hi");
        assert_eq!(slice_line_columns(&line, 0, usize::MAX), "hi");
        assert_eq!(slice_line_columns(&line, 5, 10), "");
    }

    #[test]
    fn slice_columns_wide_chars() {
        let line = Line::from("日本語x");
        // Columns 0..3 → the first two CJK chars.
        assert_eq!(slice_line_columns(&line, 0, 3), "日本");
    }
}
