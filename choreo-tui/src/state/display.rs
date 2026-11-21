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
//! Every item is re-exported from `state` (`pub(crate) use display::*`), so
//! `crate::state::SessionDisplayState` and friends keep resolving.

use super::{
    HistoryScrollState, Marker, RenderedCache, ResponsePin, ScrollRestore, StreamingResponseCache,
    TurnLayout,
};
use choreo_client_core::SessionView;
use choreo_proto::{ReasoningCapability, TokenUsage};
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
    pub(crate) markers: Vec<Marker>,
    pub(crate) markers_dirty: bool,
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
            markers: Vec::new(),
            markers_dirty: true,
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
