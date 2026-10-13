//! The streaming response auto-pin: holding an in-flight assistant response's
//! top row at the viewport top while it streams.
//!
//! While a response streams and the user is following the bottom, the history
//! pane follows the newest content from the bottom until the response block's
//! top row reaches the top of the viewport; there
//! [`SessionDisplayState::maybe_pin_response_top`] pins that row at the top and
//! the rest of the response grows below the fold.  The state machine
//! [`ResponsePin`] lives on the per-session display, so a response pins at most
//! once even across session switches.
//!
//! The decision is made in exactly one place — the tail of
//! [`SessionDisplayState::compute_total_height_and_markers`] — so every path
//! that can grow the response (the streaming fast path, a full rebuild after an
//! interleaved `Done`/`TurnAppended`, a resize, a session switch-in) is covered
//! by construction.  The fast path only keeps the anchor current; it never pins.
//!
//! A single agent run is a *tool loop*: the daemon appends several turns against
//! one stream id before it is done, so the pin's target moves as the run
//! advances.  `App::handle_started` re-targets an engaged pin to each new turn,
//! releasing the viewport back to the bottom, so a multi-turn run ends pinned on
//! its *final* turn rather than frozen on the first answer that happened to fill
//! the viewport.

use super::{HistoryViewport, SessionDisplayState};

/// Streaming response auto-pin state: whether the assistant response currently
/// being output should, or already did, pin its top row to the top of the
/// viewport.
///
/// The pin fires at most once per response, when the response's top row would
/// otherwise scroll above the viewport top; from then on the normal anchoring
/// holds that row in place.  State is per display and survives session switches,
/// so re-entering a session never re-pins a response that already pinned (or
/// lets one that pinned reposition).  It is re-armed by a *new* response (a
/// different turn id — `App::handle_started`, which re-targets the pin when a
/// multi-turn agent run advances to its next turn), dropped by a manual scroll,
/// and cleared on completion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResponsePin {
    /// No response is being output, or the user has taken over the view.
    Idle,
    /// A response is streaming and has not pinned yet; pin it when its top
    /// row reaches the viewport top.  Carries the turn id it belongs to.
    Armed(u32),
    /// This response already pinned once — it can never pin again.
    Pinned(u32),
}

impl ResponsePin {
    /// Drop the pin when the user scrolls manually.
    ///
    /// A manual scroll means the user has taken over the view, so the pin is
    /// dropped in every state — a not-yet-pinned response so returning to the
    /// bottom cannot yank the view up to the response top, and an already-pinned
    /// response so a later turn in a multi-turn run does not release the
    /// viewport back to the bottom over a user who chose to read elsewhere.
    /// The same response can never re-pin (no second arming); a later turn can
    /// still arm, but only ever engages from the bottom (see
    /// [`SessionDisplayState::maybe_pin_response_top`]).
    pub(crate) fn disarm_on_manual_scroll(&mut self) {
        *self = ResponsePin::Idle;
    }
}

// `HistoryViewport` is a 4-byte Copy struct; every call site already holds a
// reference (the compute entry point and tests), so taking it by value would
// churn signatures for no measurable gain.  Same rationale as the sibling
// `impl SessionDisplayState` blocks.
#[expect(clippy::trivially_copy_pass_by_ref)]
impl SessionDisplayState {
    /// Pin the streaming response's top row to the viewport top, at most once
    /// per response.
    ///
    /// Fires only while the response is [`ResponsePin::Armed`] and the user is
    /// following the bottom.  It sets the scroll so the response block's top
    /// row (`height_prefix[turn - 1] + layout.response_start_row`) sits at the
    /// top of the viewport, then records [`ResponsePin::Pinned`] so the response
    /// can never pin again — not on a later chunk, not after the user scrolls,
    /// and not after leaving and re-entering the session.  The normal
    /// from-bottom anchoring holds the pinned row in place on every later
    /// growth.
    pub(crate) fn maybe_pin_response_top(&mut self, viewport: &HistoryViewport) {
        let ResponsePin::Armed(turn_id) = self.response_pin else {
            return;
        };
        // Only ever engage from the following state: a user reading older
        // content must not be yanked, and the "response top reached the
        // viewport top" transition is defined relative to bottom-following.
        if self.effective_scroll(viewport) != 0 {
            return;
        }
        let Some(idx) = self.pinned_turn_index(turn_id) else {
            return;
        };
        let Some(row) = self
            .turn_layouts
            .get(idx)
            .and_then(|layout| layout.response_start_row)
        else {
            return;
        };
        let turn_start = self
            .height_prefix
            .get(idx.wrapping_sub(1))
            .copied()
            .unwrap_or(0);
        let response_top = turn_start.saturating_add(row);
        let max_scroll = self
            .total_history_height()
            .saturating_sub(viewport.height as usize);
        // Strict `>`: pin only once the response top would pass the viewport
        // top.  At `max_scroll == response_top` the top is already flush with
        // the viewport, and the next chunk would scroll it off — pinning here
        // (rather than at equality) is what holds it in place afterwards.
        if max_scroll > response_top {
            self.history_scroll.scroll = max_scroll - response_top;
            self.response_pin = ResponsePin::Pinned(turn_id);
            tracing::debug!(
                turn_id,
                response_top,
                scroll = self.history_scroll.scroll,
                "pinned streaming response top to viewport top",
            );
        }
    }

    /// Visible index of the armed response's turn, resolved in O(1) from the
    /// streaming turn index and falling back to a scan only when that cache is
    /// stale (e.g. a background response pinned on switch-in, before
    /// `streaming_turn_index` is re-resolved by the rebuild).
    fn pinned_turn_index(&self, turn_id: u32) -> Option<usize> {
        match self.streaming_turn_index {
            // `streaming_turn_index` is the visible index of the streaming turn;
            // the armed turn *is* the streaming turn, so this is the fast path.
            Some(i) if self.visible_turn_ids.get(i) == Some(&turn_id) => Some(i),
            _ => self.visible_turn_ids.iter().position(|&t| t == turn_id),
        }
    }
}
