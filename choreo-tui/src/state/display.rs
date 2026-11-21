//! The per-session display model: the view state `App` holds one of per
//! session (`SessionDisplayState`), the client-side note it derives
//! (`SessionNote`), and the bulk reasoning-visibility setting that belongs to it
//! (`ReasoningViewMode`).
//!
//! This is the state a *view* of a session accumulates — what is loaded, what
//! is scrolled where, which overrides the user clicked — as opposed to the
//! process-wide plumbing (`App` in [`super`]).  It lives in its own module
//! because it grows independently of `App`: every new per-session view concern
//! lands on this struct, and keeping it beside the `App` definition would make
//! that one file the bottleneck for unrelated edits.
//!
//! The per-turn visibility mutators (reasoning collapse/expand, tool-result
//! collapse/expand) live here rather than with the history model's other
//! `SessionDisplayState` methods: they are the only writers of
//! [`SessionDisplayState::reasoning_override`],
//! [`SessionDisplayState::reasoning_mode`] and
//! [`SessionDisplayState::tool_collapse_override`], so they sit beside the state
//! they mutate.  What they share with the history model is the *invalidation*:
//! each records the user's choice and then calls `mark_content_changed`, which
//! schedules the height-model rebuild the new visibility requires.
//!
//! Every item is re-exported from `state` (`pub(crate) use display::*`), so
//! `crate::state::SessionDisplayState` and friends keep resolving.

use super::{
    HistoryScrollState, Marker, RenderedCache, ResponsePin, ScrollRestore, StreamingResponseCache,
    TurnLayout,
};
use crate::markdown_render::{
    reasoning_expanded_default, tool_result_default_collapsed, turn_has_reasoning,
};
use choreo_client_core::SessionView;
use choreo_proto::{ReasoningCapability, TokenUsage, ToolResultRecord};
use std::collections::{HashMap, HashSet};

/// Bulk reasoning-visibility mode for one session — the two-state setting
/// `Alt+Shift+R` / `/reasoning-toggle` flips.
///
/// The mode is the *fallback* a turn's reasoning visibility derives from, never
/// an override: a per-turn [`SessionDisplayState::reasoning_override`] (set by
/// clicking a header) still wins in either mode, so a turn can be expanded while
/// the session is in [`Collapsed`](Self::Collapsed) and collapsed while it is in
/// [`Expanded`](Self::Expanded).
///
/// The two modes are not mirror images, by design:
///
/// - [`Collapsed`](Self::Collapsed) hides **every** reasoning body, including a
///   block that is still streaming (so reasoning is never watched live).
/// - [`Expanded`](Self::Expanded) re-applies the content-derived default
///   ([`crate::markdown_render::reasoning_expanded_default`]) rather than forcing
///   every body open, so a turn that already has a response stays collapsed
///   (its answer is the primary content) and a turn whose answer *starts* while
///   it is expanded collapses itself the moment the response arrives.
///
/// What the mode therefore changes is the **reasoning-only turns**: an answered
/// turn stays collapsed in [`Expanded`](Self::Expanded) too, so a transcript
/// whose every reasoning block already has its answer renders identically in
/// both modes and the chord is a no-op there. That is the intent — the mode
/// decides whether live thinking is watched as it streams, not whether finished
/// reasoning ever reopens — and it is why `Expanded` is the default: it needs no
/// recorded state, because it simply defers to the content default.
///
/// It is per-session view state with no daemon counterpart: it lives and dies
/// with the TUI process (choreo-tui keeps no state on disk), and it survives
/// switching away from the session because it hangs off the display state, not
/// the active view.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum ReasoningViewMode {
    /// Reasoning follows the content-derived default (expanded while it has no
    /// response, collapsed once a response exists).
    #[default]
    Expanded,
    /// Reasoning is collapsed everywhere.
    Collapsed,
}

impl ReasoningViewMode {
    /// Resolve the mode against a turn's content-derived default.
    pub(crate) fn default_expanded(self, content_default: bool) -> bool {
        match self {
            // Expanded deliberately defers to the content default: an answered
            // turn stays collapsed.
            Self::Expanded => content_default,
            Self::Collapsed => false,
        }
    }
}

/// A fact about a session this client knows that the daemon's `SessionStatus`
/// vocabulary cannot express.
///
/// The daemon reports what it is *doing* with a session; it cannot report that
/// this client has not heard from it yet, or that its last attach attempt
/// failed.  Those are properties of the client's view of the session, so they
/// are held per display ([`SessionDisplayState`]) and derived into a note by
/// [`App::session_note`](crate::state::App::session_note).
///
/// The note is a *display* concern only: it never feeds the OSC 7501
/// program-status reporting, which publishes the daemon's own view.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SessionNote {
    /// The session's snapshot has not arrived yet, so its transcript is
    /// unknown.  This is the state a freshly attached session sits in until
    /// the daemon's `SessionState` snapshot lands.
    Loading,
    /// The last attempt to attach this session failed.  Nothing is loading and
    /// nothing is running: the session's transcript is unavailable until the
    /// user re-enters it from the session list.
    Error,
}

pub(crate) struct SessionDisplayState {
    pub(crate) view: SessionView,
    pub(crate) visible_turn_ids: Vec<u32>,
    pub(crate) turn_heights: Vec<usize>,
    pub(crate) height_prefix: Vec<usize>,
    /// Whether any visible turn currently reserves an image block.  Only
    /// image-bearing turns' heights depend on the viewport height
    /// (`image_block_height`), so a chrome-height change can skip the whole
    /// height rebuild when this is `false`.  Maintained by the height-model
    /// rebuild and the streaming fast path.
    pub(crate) any_image_turns: bool,
    pub(crate) markers: Vec<Marker>,
    pub(crate) markers_dirty: bool,
    /// Viewport-sized occupancy bitmap of the scrollbar marker slots, derived
    /// from [`Self::markers`] against the current total height and virtual
    /// track.  The renderer draws the scrollbar from this (O(vh)) instead of
    /// rebuilding a `Vec` + `HashSet` from every marker each frame (O(markers));
    /// it is rebuilt lazily when the marker set changes
    /// ([`Self::marker_slots_dirty`]) or the viewport height changes the virtual
    /// track (see `ensure_marker_slots`).  A rebuild also writes each marker's
    /// slot back into its `virtual_slot` field, so the stored slots and this
    /// bitmap always agree.
    pub(crate) marker_slots: Vec<bool>,
    /// Whether [`Self::marker_slots`] must be rebuilt from [`Self::markers`]
    /// (which also re-derives every marker's `virtual_slot` in the same pass).
    pub(crate) marker_slots_dirty: bool,
    pub(crate) streaming_turn_index: Option<usize>,
    pub(crate) streaming_dirty: bool,
    pub(crate) content_dirty: bool,
    pub(crate) history_scroll: HistoryScrollState,
    /// Reading position captured when this session was last left, applied once
    /// on the next visit (see [`ScrollRestore`]).  `None` while the session is
    /// active or has never been left.
    pub(crate) scroll_restore: Option<ScrollRestore>,
    pub(crate) turn_layouts: Vec<TurnLayout>,
    /// Per-turn explicit reasoning visibility (`turn_id` → expanded) set by
    /// clicking the reasoning header.  Absent entries fall back to
    /// [`reasoning_expanded_default`] (expanded while streaming, collapsed
    /// once a response exists), filtered through [`Self::reasoning_mode`].
    pub(crate) reasoning_override: HashMap<u32, bool>,
    /// Bulk reasoning-visibility mode for this session (`Alt+Shift+R` /
    /// `/reasoning-toggle`).  Defaults to [`ReasoningViewMode::Expanded`], which
    /// defers to the content-derived default; a display torn down and rebuilt
    /// (e.g. the session deleted and re-created) starts from that default again.
    pub(crate) reasoning_mode: ReasoningViewMode,
    /// Per-(turn, tool-call) explicit collapse state (`turn_id` → `call_id` →
    /// collapsed) set by clicking a tool result's header.  Absent entries
    /// fall back to [`tool_result_default_collapsed`] (quiet tools
    /// collapsed, everything else expanded).  Nested so the per-frame lookup
    /// can borrow the record's `call_id` instead of cloning it; keyed by
    /// `call_id` (not position) because a result's position is stable while
    /// its content streams in.
    pub(crate) tool_collapse_override: HashMap<u32, HashMap<String, bool>>,
    /// Monotonic per-turn content version, bumped by every event handler
    /// that mutates a turn's rendered content (streaming chunks, turn
    /// replacement, snapshot merges, undo/redo).  Included in the
    /// [`RenderCacheKey`] so a full rebuild can never reuse a cached
    /// rendering whose turn content changed behind the key's other fields.
    /// This is what makes the cache content-correct even when the streaming
    /// fast path was disarmed by an interleaved `mark_content_changed` (a
    /// `Done`/`TurnAppended`/`SessionState` from this or another session
    /// landing between chunks).
    pub(crate) turn_versions: HashMap<u32, u64>,
    pub(crate) render_cache: Vec<Option<RenderedCache>>,
    /// Incremental assistant-response render cache for the streaming fast path,
    /// keyed to [`Self::streaming_turn_index`] (see [`StreamingResponseCache`]).
    pub(crate) streaming_response: Option<StreamingResponseCache>,
    /// Whether the streaming assistant response has pinned (or is armed to
    /// pin) its top row at the viewport top.  Per display so it survives
    /// session switches — a response's single pin is never undone by leaving
    /// and re-entering the session.  See [`ResponsePin`] (in the `pin` module).
    pub(crate) response_pin: ResponsePin,
    pub(crate) active: HashSet<u64>,
    pub(crate) live_input_estimate: u32,
    pub(crate) live_output_tokens: u32,
    pub(crate) progress_dirty: bool,
    pub(crate) status: Option<String>,
    /// The last turn's failure text (`SessionEvent::Error`), shown with the
    /// turn.  Distinct from [`Self::attach_failed`], which is a client-side
    /// fact about a failed attach.
    pub(crate) error: Option<String>,
    /// Whether this client holds a transcript snapshot for the session — set
    /// when a `SessionState` snapshot is applied, which is the authoritative
    /// transcript.  It therefore means "a snapshot has arrived", not "an
    /// attach finished": the daemon sends `SessionAttached` (which this client
    /// acts on optimistically) *before* the snapshot, so the window between
    /// them is exactly the window in which the transcript is unknown.
    ///
    /// A display created fresh starts `false`, and a deleted session's display
    /// is dropped, so its replacement starts `false` again.
    pub(crate) history_loaded: bool,
    /// Whether the last attempt to attach this session failed.  Kept on the
    /// display so it travels with the session: switching away and back cannot
    /// lose it, and only a new attach (or a snapshot arriving anyway) clears
    /// it.  See [`SessionNote::Error`].
    pub(crate) attach_failed: bool,
    pub(crate) selected_model: Option<String>,
    pub(crate) reasoning_effort: Option<String>,
    pub(crate) reasoning_capability: Option<ReasoningCapability>,
    pub(crate) account_name: Option<String>,
    pub(crate) working_dir: Option<String>,
    pub(crate) token_usage: Option<TokenUsage>,
    pub(crate) context_window: Option<u32>,
    pub(crate) last_prompt_tokens: Option<u32>,
    /// The sole per-session prompt draft: the text (and cursor position) the
    /// user had typed but not yet submitted.  The input bar is a single
    /// shared buffer, so each session stashes its own draft here — captured
    /// on session switch and restored on the next visit — so an unsubmitted
    /// prompt never leaks into a different session.  History recall only
    /// begins from an empty draft, and editing a recalled entry turns it into
    /// the draft (see `App::detach_history_on_edit`); there is no separate
    /// global stash.  Cleared on submit and dropped when the session (and its
    /// display) is deleted.
    pub(crate) draft: String,
    pub(crate) draft_cursor: usize,
}

impl Default for SessionDisplayState {
    fn default() -> Self {
        Self {
            view: SessionView::new(),
            visible_turn_ids: Vec::new(),
            turn_heights: Vec::new(),
            height_prefix: Vec::new(),
            any_image_turns: false,
            markers: Vec::new(),
            markers_dirty: true,
            marker_slots: Vec::new(),
            marker_slots_dirty: true,
            streaming_turn_index: None,
            streaming_dirty: false,
            content_dirty: false,
            history_scroll: HistoryScrollState::new(),
            scroll_restore: None,
            turn_layouts: Vec::new(),
            reasoning_override: HashMap::new(),
            reasoning_mode: ReasoningViewMode::default(),
            tool_collapse_override: HashMap::new(),
            turn_versions: HashMap::new(),
            render_cache: Vec::new(),
            streaming_response: None,
            response_pin: ResponsePin::Idle,
            active: HashSet::new(),
            live_input_estimate: 0,
            live_output_tokens: 0,
            progress_dirty: false,
            status: None,
            error: None,
            history_loaded: false,
            attach_failed: false,
            selected_model: None,
            reasoning_effort: None,
            reasoning_capability: None,
            account_name: None,
            working_dir: None,
            token_usage: None,
            context_window: None,
            last_prompt_tokens: None,
            draft: String::new(),
            draft_cursor: 0,
        }
    }
}

// The per-turn visibility overrides and the mode they fall back to.  Every reader
// resolves an override first and the content-derived default second, in that
// order, so a header click always wins over the session-wide mode; every writer
// records the user's explicit choice and then invalidates the render model,
// because a changed visibility changes the wrapped lines as much as new content
// does.
impl SessionDisplayState {
    /// Effective reasoning visibility for a turn: an explicit override from
    /// clicking the header wins; otherwise the session's bulk mode resolves the
    /// caller-provided derived default ([`ReasoningViewMode::default_expanded`] —
    /// the collapsed mode hides every body, the expanded mode defers to the
    /// content default).  Callers compute the default either from the turn
    /// content (`reasoning_expanded_default`) or from the precomputed
    /// `TurnLayout` when one is available (per-frame render path).
    pub(crate) fn effective_reasoning_expanded(&self, turn_id: u32, default: bool) -> bool {
        self.reasoning_override
            .get(&turn_id)
            .copied()
            .unwrap_or_else(|| self.reasoning_mode.default_expanded(default))
    }

    /// Toggle the reasoning section's visibility for a turn (clicking the
    /// header).  Records the explicit user preference in `reasoning_override`
    /// and invalidates the turn's render cache so the change takes effect on
    /// the next frame.
    pub(crate) fn toggle_reasoning(&mut self, turn_id: u32) {
        let Some(turn) = self.view.turns.get(&turn_id) else {
            return;
        };
        let current = self.effective_reasoning_expanded(turn_id, reasoning_expanded_default(turn));
        self.reasoning_override.insert(turn_id, !current);
        if let Some(idx) = self.visible_turn_ids.iter().position(|id| *id == turn_id)
            && let Some(slot) = self.render_cache.get_mut(idx)
        {
            *slot = None;
        }
        self.mark_content_changed();
    }

    /// Flip the session's bulk reasoning-visibility mode — the body of
    /// `Alt+Shift+R` and `/reasoning-toggle`.
    ///
    /// The decision is made from the *effective* state of every reasoning-bearing
    /// turn, so a turn the user manually expanded in the collapsed mode counts as
    /// not-collapsed and the next press collapses the transcript again:
    ///
    /// - any turn showing its reasoning ⇒ switch to collapsed;
    /// - every turn already collapsed (including a session with no reasoning at
    ///   all, vacuously) ⇒ switch to expanded.
    ///
    /// Switching drops every per-turn override: the mode is a bulk statement of
    /// intent, so a header-clicked preference from the previous mode must not
    /// survive it and leave one turn contradicting the mode.  Clicks made after
    /// the switch still override individual turns, in either mode.
    pub(crate) fn toggle_all_reasoning(&mut self) {
        let any_expanded = self.view.turns.iter().any(|(&turn_id, turn)| {
            !turn.undone
                && turn_has_reasoning(turn)
                && self.effective_reasoning_expanded(turn_id, reasoning_expanded_default(turn))
        });
        self.reasoning_mode = if any_expanded {
            ReasoningViewMode::Collapsed
        } else {
            ReasoningViewMode::Expanded
        };
        self.reasoning_override.clear();
        // Every cached rendering was built with the previous visibility, so drop
        // the whole cache rather than trusting the per-key comparison (the mode
        // is not part of `RenderCacheKey`; the key's `reasoning_expanded` flag is,
        // but a turn whose reasoning is hidden either way would compare equal).
        self.render_cache.fill(None);
        self.mark_content_changed();
    }

    /// Effective collapse state for a tool result: an explicit override from
    /// clicking the header wins; otherwise the derived default (quiet tools
    /// collapsed, everything else expanded — see
    /// [`tool_result_default_collapsed`]) is used.
    pub(crate) fn effective_tool_result_collapsed(
        &self,
        turn_id: u32,
        record: &ToolResultRecord,
    ) -> bool {
        // Nested (turn → call_id → state) lookup borrows the record's
        // call_id — this runs per result per frame, so avoiding a clone
        // here keeps the render path allocation-free for the common case.
        self.tool_collapse_override
            .get(&turn_id)
            .and_then(|by_call| by_call.get(&record.call_id))
            .copied()
            .unwrap_or_else(|| tool_result_default_collapsed(record))
    }

    /// Toggle a tool result's collapse state (clicking its header row).
    /// Records the explicit user preference in `tool_collapse_override` and
    /// invalidates the turn's render cache so the change takes effect on
    /// the next frame.
    pub(crate) fn toggle_tool_result(&mut self, turn_id: u32, call_id: &str) {
        let Some(turn) = self.view.turns.get(&turn_id) else {
            return;
        };
        let Some(record) = turn.tool_results.iter().find(|r| r.call_id == call_id) else {
            return;
        };
        let current = self.effective_tool_result_collapsed(turn_id, record);
        self.tool_collapse_override
            .entry(turn_id)
            .or_default()
            .insert(call_id.to_string(), !current);
        if let Some(idx) = self.visible_turn_ids.iter().position(|id| *id == turn_id)
            && let Some(slot) = self.render_cache.get_mut(idx)
        {
            *slot = None;
        }
        self.mark_content_changed();
    }
}
