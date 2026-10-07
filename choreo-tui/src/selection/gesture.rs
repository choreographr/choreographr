//! The selection gesture: screen↔content mapping, gesture state, and the
//! mouse-event state machine.
//!
//! A selection is stored in *content* coordinates — a global content line in
//! `[0, total history height)` plus a viewport column — NOT screen coordinates:
//! mouse events arrive in viewport space, so each is mapped to the content it
//! covers the moment it is processed ([`screen_to_content`], the exact inverse
//! of the click hit-testing the TUI already does).  Storing content coordinates
//! is what lets the selection survive scrolling: the anchor stays pinned to the
//! text it was placed on, while the live drag head re-resolves to the content
//! under the cursor — on wheel events immediately, and on content-induced
//! scrolls (streaming growth, appended turns) at draw time via [`follow_cursor`].

use crate::state::App;
use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

use super::extract_selection_text;

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
/// The signed arithmetic runs in isize over u16-bounded rows and small
/// non-negative height/scroll counts (far below isize range), and the
/// result is clamped back into the valid non-negative range before the
/// conversion back.
/// This is also why the tests can call it directly (`use super::*`); the
/// gesture state machine and the extraction path are its only callers.
#[expect(clippy::cast_possible_wrap, clippy::cast_sign_loss)] // small non-negative values, see above
pub(crate) fn screen_to_content(app: &App, row: u16, column: u16) -> (usize, u16) {
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
/// Same small-range reasoning as [`screen_to_content`]: isize arithmetic
/// over u16-bounded rows and non-negative counts, guarded by the range
/// check before the u16 conversion.
#[expect(
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cast_possible_truncation
)] // see above
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
pub(crate) fn selection_bounds_for_line(
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
#[expect(clippy::trivially_copy_pass_by_ref)] // &MouseEvent reads naturally at call sites; the copy saving is trivial
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
