//! Pointer-shape selection (OSC 22) for the Chat page's hover targets.
//!
//! The pointer communicates affordance: a hand over something clickable, an
//! I-beam over selectable text, the ordinary arrow everywhere else. The shape is
//! derived from the *last known pointer position* plus the current hit-test
//! state, never from "did a mouse-motion event just arrive" — so a target that
//! disappears (a scrollbar hidden because the history now fits the viewport)
//! resets to the default arrow with no further input at all.
//!
//! That position is sampled from **every** mouse event, not only from motion:
//! a terminal reports button-less motion as `Moved` only while its any-event
//! tracking is in force and no button is held, and reports the same movement as
//! a `Drag` otherwise (or not at all, if the mode lapsed). Since the shape is
//! emitted only when it changes, a hover refreshed from motion alone would
//! freeze on the last target for the rest of the session; see
//! `crate::connection::terminal_event`.
//!
//! The hit-tests are the same ones click handling uses (`mouse_in_scrollbar_column`,
//! `mouse_in_history_box`, `App::input_box_rect`, and the reasoning/tool-result
//! header ranges in `TurnLayout`), so what the pointer promises and what a click
//! does cannot drift apart. Emitting is the UI loop's job — it writes the OSC 22
//! sequence outside `terminal.draw` — while deduping the emitted shape is
//! [`App::sync_pointer`].

use crate::render::{mouse_in_history_box, mouse_in_scrollbar_column};
use crate::state::{App, Page, find_turn_at_row};
use crate::terminal::pointer::PointerShape;

impl App {
    /// The pointer shape for a hover at terminal position `(column, row)`.
    ///
    /// Order matters: the scrollbar column sits *outside* the history box, the
    /// clickable turn headers sit *inside* it (and win over its selectable
    /// text), and the composer box is below both.
    pub(crate) fn pointer_shape_at(&mut self, column: u16, row: u16) -> PointerShape {
        // Every hover target below belongs to the Chat page. An overlay that
        // covers the page (the fullscreen image, the OAuth login modal) is not
        // a hover target either, so the arrow is the honest answer there.
        if self.page != Page::Chat || self.fullscreen_image_target.is_some() || self.login.is_some()
        {
            return PointerShape::Default;
        }
        let (vp_width, vp_height) = (self.history_viewport.width, self.history_viewport.height);
        if self.scrollbar_visible() && mouse_in_scrollbar_column(column, row, vp_width, vp_height) {
            // The scrollbar is draggable, and its column is blank (not text)
            // whenever the scrollbar is hidden — hence the visibility gate
            // above, which keeps a click-free column from promising a hand.
            return PointerShape::Pointer;
        }
        if mouse_in_history_box(column, row, vp_width, vp_height) {
            // A turn's collapsible header (reasoning, tool result) is a button;
            // everything else in the pane is selectable text.
            return if self.hover_is_turn_header(row) {
                PointerShape::Pointer
            } else {
                PointerShape::Text
            };
        }
        if let Some((term_w, term_h)) = self.last_terminal_size {
            let box_rect = self.input_box_rect(term_w, term_h);
            if column >= box_rect.x
                && column < box_rect.right()
                && row >= box_rect.y
                && row < box_rect.bottom()
            {
                // The composer holds editable text, so the I-beam is right over
                // its borders and padding too — the whole box is the input.
                return PointerShape::Text;
            }
        }
        PointerShape::Default
    }

    /// Whether `row` (a screen row) lands on a clickable turn header — the
    /// reasoning header or one of the tool-result headers of the turn at that
    /// row.
    fn hover_is_turn_header(&self, row: u16) -> bool {
        let Some((turn_idx, offset)) = find_turn_at_row(self, row) else {
            return false;
        };
        let Some(layout) = self
            .active_display_ref()
            .and_then(|display| display.turn_layouts.get(turn_idx))
        else {
            return false;
        };
        let contains = |(start, end): (usize, usize)| offset >= start && offset < end;
        layout.reasoning_header_range.is_some_and(contains)
            || layout
                .tool_result_header_ranges
                .iter()
                .copied()
                .any(contains)
    }

    /// The pointer shape that should be showing now: the hover target's shape,
    /// or the default arrow while no pointer position is known.
    pub(crate) fn desired_pointer_shape(&mut self) -> PointerShape {
        self.pointer_hover
            .map_or(PointerShape::Default, |(column, row)| {
                self.pointer_shape_at(column, row)
            })
    }

    /// Recompute the pointer shape and report the one the caller must emit.
    ///
    /// Returns `None` when the shape is unchanged — the terminal keeps the
    /// pointer it already has — and `Some(shape)` when it changed, remembering
    /// it so an unmoved pointer over the same target emits nothing further.
    pub(crate) fn sync_pointer(&mut self) -> Option<PointerShape> {
        let desired = self.desired_pointer_shape();
        if self.term_pointer == Some(desired) {
            return None;
        }
        self.term_pointer = Some(desired);
        Some(desired)
    }

    /// Forget the pointer: the hover position, the shape last emitted, and any
    /// pending request to re-assert the mouse-tracking mode.
    ///
    /// The suspend and exit paths reset the terminal's pointer to the default
    /// arrow and call this, so nothing re-sends a stale shape afterwards — the
    /// ordinary change detection re-emits only once the pointer actually moves
    /// onto a target again. A pending re-assert is dropped for the same reason:
    /// both callers are handing the terminal back, so re-enabling mouse tracking
    /// afterwards would fight the teardown (and, on suspend, would run while the
    /// process is stopped).
    pub(crate) fn forget_pointer(&mut self) {
        self.pointer_hover = None;
        self.term_pointer = None;
        self.mouse_tracking_reassert = false;
    }
}
