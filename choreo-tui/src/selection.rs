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

use crate::markdown_render::LineJoin;
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
    /// The screen→content mapping fingerprint (total history height,
    /// effective scroll, viewport height) at the last time `head` was
    /// resolved against the cursor.  [`follow_cursor`] compares this against
    /// the current layout and skips when unchanged, so the every-frame draw-
    /// path sync costs one tuple compare on idle frames instead of a full
    /// re-resolution.
    pub head_sync: Option<(usize, usize, u16)>,
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
        // The head sits at the anchor, resolved against the current layout;
        // record that layout so the first draw-time sync sees no drift.
        head_sync: Some(follow_fingerprint(app)),
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
    let fingerprint = follow_fingerprint(app);
    if let Some(sel) = &mut app.text_selection {
        sel.head = head;
        sel.cursor = (row, column);
        // The head is now resolved against exactly this layout; the draw-time
        // sync must not re-resolve it until the layout moves again.
        sel.head_sync = Some(fingerprint);
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
///
/// The re-resolution is **fingerprint-gated**: the head is only touched when
/// one of the screen→content mapping inputs (total history height, effective
/// scroll, viewport height — [`follow_fingerprint`]) changed since the last
/// head resolution.  All movement that matters changes at least one of the
/// three (content streaming/append/undo change the total; wheel, keyboard,
/// and scrollbar scrolling change the scroll; a resize changes the viewport
/// — and also clears the gesture entirely), so an idle frame costs a single
/// tuple compare instead of a re-resolution.
pub(crate) fn follow_cursor(app: &mut App) {
    let Some(sel) = app.text_selection else {
        return;
    };
    if !sel.active {
        return;
    }
    let fingerprint = follow_fingerprint(app);
    if sel.head_sync == Some(fingerprint) {
        // Nothing moved since the head was last resolved against the cursor;
        // the stored head is already the content under it.
        return;
    }
    let head = screen_to_content(app, sel.cursor.0, sel.cursor.1);
    if let Some(sel) = &mut app.text_selection {
        sel.head = head;
        sel.head_sync = Some(fingerprint);
    }
}

/// The screen→content mapping fingerprint: the three inputs that decide
/// which content line a fixed cursor position covers (see
/// [`screen_to_content`]).  When all three are unchanged, [`follow_cursor`]
/// is a no-op.
fn follow_fingerprint(app: &App) -> (usize, usize, u16) {
    (
        app.total_history_height(),
        app.effective_scroll(),
        app.history_viewport.height,
    )
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
/// region is exactly the region the highlight covers.  Consecutive rows that
/// are wrapped continuations of one original line (marked by the renderer's
/// per-line [`LineJoin`] metadata) are glued back together, so copying a
/// selected paragraph yields the original unwrapped text, not the display's
/// line-wrapped rows.
fn extract_selection_text(app: &App) -> Option<String> {
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
    let mut slots: Vec<(String, LineJoin, usize, usize)> = Vec::new();
    // Iterate the selection's *content* lines directly — no screen mapping,
    // which is exactly why the selection survives scrolling (the endpoints
    // are content-anchored, so this is scroll-independent).
    for content_line in start_line..=end_line {
        let (col_lo, col_hi) = selection_bounds_for_line(anchor, head, content_line);
        if let Some((text, join, turn_idx, line_idx)) =
            text_and_join_for_content_line(display, vp_width, content_line, col_lo, col_hi)
        {
            slots.push((text, join, turn_idx, line_idx));
        }
    }

    // Assemble the slots.  Each slot's join metadata says how the text glued
    // to its immediate predecessor when the renderer wrapped the original
    // line — except when the two slots are rows of the *same* semantic line
    // split across viewport rows (never in practice: content is pre-wrapped
    // narrower than the viewport), which always concatenate directly.
    let mut out = String::new();
    for (i, (text, join, turn_idx, line_idx)) in slots.iter().enumerate() {
        if i == 0 {
            out.push_str(text);
            continue;
        }
        let (_, _, prev_turn, prev_line) = slots[i - 1];
        let join = if prev_turn == *turn_idx && prev_line == *line_idx {
            // Same semantic line across two viewport rows — contiguous text.
            LineJoin::Join
        } else {
            *join
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
            out.push_str(text.trim_start());
        } else {
            out.push_str(text);
        }
    }
    if out.is_empty() { None } else { Some(out) }
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
    let join = rendered
        .joins
        .get(line_idx)
        .copied()
        .unwrap_or(LineJoin::Break);
    Some((
        slice_line_columns(&rendered.lines[line_idx], lo, hi),
        join,
        turn_idx,
        line_idx,
    ))
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
/// the text).  A *blank* content row (an empty `(lo, lo)` range) resolves
/// to that empty range rather than `None`: it is genuine content with no
/// characters (the renderer's blank spacer between markdown blocks), so the
/// extraction path records it as an empty slot and the blank line survives
/// the copy, while chrome stays `None` and contributes nothing.
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
        // highlight path is unaffected: `style_line_selection` no-ops on an
        // empty column range.  (An empty overlap on a non-blank row falls
        // through the first arm to `None` above; only the row's own empty
        // content range reaches this arm.)
        Some((clo, chi)) => Some((line_idx, (clo, chi))),
        // Pure chrome (box separators, padding, image blocks, past-end): no
        // content, no slot.
        None => None,
    }
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
mod tests;
