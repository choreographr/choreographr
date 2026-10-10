//! The TUI's full application state: the `App` struct, per-session display
//! state, input plumbing, and the page/overlay states.  The history pane's
//! render model (height prefix-sum, markers, scroll, viewport, render cache),
//! the provider catalog, text-input machinery, page states, picker geometry,
//! displayed-image fetch plumbing, the turn-event handlers, and the session
//! lifecycle live in sibling modules (`history`, `providers`, `input`, `pages`,
//! `layout`, `images`, `turn`, `session`) and are re-exported here so the rest
//! of the crate keeps referring to `crate::state::*` unchanged.

use crate::RenderedImage;
use crate::image_worker::{ImageId, ImageJob};
use crate::selection::TextSelection;
use crate::terminal;
use choreo_client_core::{PendingReplies, SessionView};
use choreo_proto::{ReasoningCapability, SessionStatus, TokenUsage, socket_path};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use std::collections::{HashMap, HashSet};

use crate::markdown_render::{lines_height, plain_text_lines};

mod command_palette;
mod draft;
mod history;
mod images;
mod input;
mod keymap;
mod layout;
mod pages;
mod providers;
mod session;
mod session_manager;
mod streaming;
mod terminal_status;
mod turn;

// Compatibility layer: every item moved into the sibling modules is
// re-exported here so `crate::state::X` references (in this crate and in
// `app_tests.rs`/`render_tests.rs`) keep resolving exactly as before.
pub(crate) use command_palette::*;
pub(crate) use history::*;
pub(crate) use images::{
    ImageJobRequest, ImageSlot, slot_source, turn_image_count, turn_image_slots,
};
pub(crate) use input::*;
pub(crate) use keymap::*;
pub(crate) use layout::*;
pub(crate) use pages::*;
pub(crate) use providers::*;
pub(crate) use session::*;
pub(crate) use session_manager::*;
pub(crate) use streaming::StreamingResponseCache;

pub(crate) const STATUS_BAR_HEIGHT: u16 = 1;
pub(crate) const MIN_INPUT_CONTENT_LINES: u16 = 1;
pub(crate) const MAX_INPUT_CONTENT_LINES: u16 = 10;
pub(crate) const PAGE_SCROLL_LINES: usize = 3;

/// Horizontal padding (columns) on each side of the command input box.
///
/// The box draws only top/bottom borders, so the content area loses `INPUT_PAD`
/// columns on each side and nothing more.  Every code path that wraps input
/// text (height estimation, cursor movement, rendering) must use
/// [`input_inner_width`] so they all agree on where word-wrap happens;
/// otherwise a wrapped line can be computed in one path but not another.
pub(crate) const INPUT_PAD: u16 = 2;

/// Inner content width (columns) of the command input box for a given terminal
/// width: terminal width minus the horizontal padding on both sides.
pub(crate) fn input_inner_width(term_width: u16) -> usize {
    term_width.saturating_sub(INPUT_PAD * 2) as usize
}

/// The two-line keyboard-shortcut help overlay (toggled with `Alt+H`).
///
/// Every app command lives on an `Alt+` chord (see `state/keymap.rs`); the
/// readline-style `Ctrl+<letter>` editing chords are advertised in the
/// `README` rather than here so the overlay stays two short lines.
pub(crate) const HELP_LINE1: &str =
    "alt+h help  alt+q quit  alt+a accounts  alt+s sessions  alt+m models";
pub(crate) const HELP_LINE2: &str =
    "esc stop  alt+enter continue  alt+up undo  alt+down redo  alt+r reasoning";

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
    /// once a response exists).
    pub(crate) reasoning_override: HashMap<u32, bool>,
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
    pub(crate) active: HashSet<u64>,
    pub(crate) live_input_estimate: u32,
    pub(crate) live_output_tokens: u32,
    pub(crate) progress_dirty: bool,
    pub(crate) status: Option<String>,
    pub(crate) error: Option<String>,
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
            tool_collapse_override: HashMap::new(),
            turn_versions: HashMap::new(),
            render_cache: Vec::new(),
            streaming_response: None,
            active: HashSet::new(),
            live_input_estimate: 0,
            live_output_tokens: 0,
            progress_dirty: false,
            status: None,
            error: None,
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

pub(crate) struct App {
    pub(crate) input: InputBuffer,
    /// The client's pending-request table: the single outbound path plus the
    /// reply-correlation side table (see [`PendingReplies`]). Every outbound
    /// `ClientMessageType` goes through [`PendingReplies::send`], and every
    /// inbound reply with `id: Some` resolves its slot here before the normal
    /// state dispatch runs.
    ///
    /// There is no client-side `stream_id` allocator: the daemon assigns a run's
    /// `stream_id` and reports it on `SessionEvent::Started`, from which the TUI
    /// records the live stream (see `handle_started`).
    pub(crate) pending: PendingReplies,
    pub(crate) rendered_images: HashMap<u64, HashMap<u32, HashMap<ImageSlot, RenderedImage>>>,
    pub(crate) pending_job_idx: HashMap<ImageId, (u64, u32, ImageSlot)>,
    /// Images the render path wants fetched on demand (both displayed images and
    /// tool-result vision images have their bytes stripped from turn snapshots).
    /// Queued here — deduped via the per-image `fetching` flag — because the
    /// render path has no client sender; the UI loop drains this each iteration
    /// and sends `ClientMessageType::GetImage`.
    pub(crate) pending_image_fetch: Vec<(u64, u32, ImageSlot)>,
    pub(crate) history_viewport: HistoryViewport,
    pub(crate) should_quit: bool,
    /// Why the TUI is exiting, when it is NOT a user-initiated quit (Alt+Q).
    /// Set when the daemon evicts this client, announces shutdown, or the
    /// connection drops; printed to the restored terminal after teardown so
    /// the user sees why the TUI left. `None` on a normal user quit.
    pub(crate) quit_message: Option<String>,
    pub(crate) image_job_tx: Option<crossbeam_channel::Sender<ImageJob>>,
    pub(crate) attached_session_id: Option<u64>,
    /// Account slug shown in the status bar — the attached session's account
    /// name (the account name is its slug). Replaces the inference provider
    /// slug that used to be shown here.
    pub(crate) attached_account_slug: Option<String>,
    pub(crate) attached_status: Option<SessionStatus>,
    pub(crate) attached_tool_groups: Vec<String>,
    /// OSC 7501 program-status publisher: the child records currently emitted
    /// to the terminal, cached so an unchanged record is not rewritten. Every
    /// report is a CHILD record keyed by session id; the OSC 9;4 root record
    /// belongs to the progress family (see `terminal::status`).
    pub(crate) term_status: terminal::status::Publisher,
    /// Set when something changed that affects the published program-status
    /// records or the window title; the UI loop recomputes and publishes them
    /// outside the render closure.
    pub(crate) term_status_dirty: bool,
    /// Terminal turn outcomes the session status cannot express: `done` after
    /// a completed turn, `error` after a failed one, `idle` after a
    /// cancellation. Keyed by session id; persists until a new turn begins (a
    /// fresh active `SessionStatusChanged`) so the outcome survives the
    /// trailing idle status of the finished turn.
    pub(crate) term_status_override: HashMap<u64, &'static str>,
    /// The last window title (OSC 2) emitted, so an unchanged title is not
    /// re-sent every frame.
    pub(crate) term_title: Option<String>,
    /// Persistent latch of whether the daemon's credential keystore is locked.
    /// Latched from the daemon's lock-state broadcasts (`Locked`/`Unlocked`)
    /// and the subscribe-time lock-state push in `handle_daemon_message`;
    /// drives the persistent lock banner and the submit-time prompt guard.
    /// Defaults to `true` (assume locked until told otherwise — the safest
    /// reading for a client that has not yet heard the daemon's state). Not
    /// cleared by the per-keypress transient status/error clear, so the lock
    /// indication survives every keystroke.
    pub(crate) keystore_locked: bool,
    /// Once-per-connection keystore auto-bind state machine (shared policy in
    /// `choreo_client_core`, so TUI and GUI cannot drift): the first
    /// `KeystoreUnbound` report mints+sends a bind, later ones surface an
    /// error instead of re-minting (bind-loop guard — see
    /// [`choreo_client_core::KeystoreAutoBind`]).
    pub(crate) keystore_auto_bind: choreo_client_core::KeystoreAutoBind,
    pub(crate) page: Page,
    pub(crate) show_help_overlay: bool,
    pub(crate) session_mgr: SessionManagerState,
    pub(crate) ai_providers: AIProvidersState,
    pub(crate) model_selector: ModelSelectorState,
    /// Transient selection state for the inline command palette (Chat page).
    /// The palette's highlight and scroll window; its visible/hidden condition
    /// is whether the input buffer starts with `/` (see `command_palette_active`)
    /// — there is no separate mode flag to keep in sync with the buffer.
    pub(crate) command_palette: CommandPaletteState,
    /// The live provider list for the new-account wizard's provider picker
    /// (S4). Initialized from the static `PROVIDER_OPTIONS` default and
    /// replaced wholesale whenever the daemon broadcasts `CatalogUpdated`, so
    /// the picker tracks the daemon's live catalog (cache + user overlay).
    pub(crate) providers: Vec<ProviderInfo>,
    pub(crate) scroll_accumulator: isize,
    pub(crate) scrollbar_dragging: bool,
    /// In-progress mouse text selection over the history pane (see
    /// `selection`).  `None` when no selection gesture is active; cleared on
    /// session switch and suspend so a stale rectangle never highlights a
    /// different session's content.
    pub(crate) text_selection: Option<TextSelection>,
    pub(crate) last_terminal_size: Option<(u16, u16)>,
    pub(crate) terminal_resized: bool,
    /// The index of the past prompt currently recalled into the input bar
    /// while browsing history (`Up`/`Down`), or `None` when not browsing.
    /// There is no separate stash of the user's draft: recall is only
    /// reachable from an empty draft, so exiting browsing simply returns to
    /// that empty draft (see `exit_history_browsing`).  Reset to `None` on
    /// session switch.
    pub(crate) history_index: Option<usize>,
    pub(crate) fullscreen_image_target: Option<(u64, u32, ImageSlot)>,
    pub(crate) status: Option<String>,
    /// Whether the current `status` came from a connection-task
    /// [`UiEvent::Status`] (a transient progress message like
    /// "daemon started") rather than from daemon traffic. A transient
    /// status is CLEARED the moment the first real daemon message arrives, so
    /// it cannot linger on the status line once the connection is live and
    /// the first turn happens to be quiet. Daemon-derived statuses never set
    /// this flag, so they are never cleared by the arrive-of-traffic rule.
    pub(crate) status_is_transient: bool,
    pub(crate) error: Option<String>,
    pub(crate) session_displays: HashMap<u64, SessionDisplayState>,
    pub(crate) active_session_id: Option<u64>,
    /// The address string this session talks to (dial addr for TCP, unix
    /// socket path otherwise). It keys the per-daemon unlock key in
    /// `known_servers`, so every `Unlock`/`AddCredential`/`record` must use it
    /// consistently. Set by `run_app` from the connection mode; defaults to
    /// the socket path so tests (which bypass `run_app`) still have a valid
    /// value.
    pub(crate) connection_addr: String,
}

pub(crate) enum UiEvent {
    Daemon(Box<choreo_proto::DaemonMessage>),
    ReaderClosed,
    /// A transient status-line message from the connection task, used while
    /// the connection is not yet established (the daemon-autostart wait) —
    /// the reader thread cannot paint the UI itself, so progress feedback
    /// travels to the UI loop as an event. Unlike daemon messages this does
    /// NOT scroll or mutate any view: it just sets the status line. Marked
    /// transient ([`App::status_is_transient`]) so the first real daemon
    /// message clears it — the "daemon started" reassurance must not outlive
    /// the connection it describes.
    Status(String),
}

impl App {
    pub(crate) fn new() -> Self {
        Self {
            input: InputBuffer::new(),
            pending: PendingReplies::new(),
            rendered_images: HashMap::new(),
            pending_image_fetch: Vec::new(),
            history_viewport: HistoryViewport::new(),
            should_quit: false,
            quit_message: None,
            image_job_tx: None,
            pending_job_idx: HashMap::new(),
            attached_session_id: None,
            attached_account_slug: None,
            attached_status: None,
            attached_tool_groups: Vec::new(),
            term_status: terminal::status::Publisher::new(),
            term_status_dirty: false,
            term_status_override: HashMap::new(),
            term_title: None,
            // Assume locked until the daemon tells us otherwise (via the
            // subscribe-time lock-state push or a transition broadcast).
            keystore_locked: true,
            // No bind sent yet on this connection (see the field docs). The
            // latch lives for the whole `App` — `App::new()` runs once per
            // `run_app` (per process, i.e. per daemon connection), and the
            // UI loop does not rebuild `App` on reader errors, so
            // re-connecting means restarting the TUI with fresh state.
            keystore_auto_bind: choreo_client_core::KeystoreAutoBind::new(),
            page: Page::Chat,
            show_help_overlay: true,
            session_mgr: SessionManagerState::new(),
            ai_providers: AIProvidersState::new(),
            model_selector: ModelSelectorState::new(),
            command_palette: CommandPaletteState::new(),
            // Start from the static default; the daemon's CatalogUpdated
            // broadcast replaces it with the live list.  The picker must be
            // alphabetical, so sort the default here too (see `sort_providers`
            // and `set_providers`).
            providers: {
                let mut providers: Vec<ProviderInfo> = PROVIDER_OPTIONS
                    .iter()
                    .map(|(slug, display_name)| ProviderInfo {
                        slug: (*slug).to_string(),
                        display_name: (*display_name).to_string(),
                    })
                    .collect();
                sort_providers(&mut providers);
                providers
            },
            scroll_accumulator: 0,
            scrollbar_dragging: false,
            text_selection: None,
            history_index: None,
            fullscreen_image_target: None,
            status: None,
            status_is_transient: false,
            error: None,
            last_terminal_size: None,
            terminal_resized: false,
            session_displays: HashMap::new(),
            active_session_id: None,
            connection_addr: socket_path(),
        }
    }

    /// The key that opens the model selector — `Alt+M` on every terminal now
    /// that commands live on `Alt+` chords.  Used by hint and status strings.
    pub(crate) fn model_selector_label() -> &'static str {
        "Alt+M"
    }

    pub(crate) fn display_for(&mut self, session_id: u64) -> &mut SessionDisplayState {
        self.session_displays.entry(session_id).or_default()
    }

    /// Replace the live provider list from a daemon `CatalogUpdated`
    /// broadcast. Clamps the wizard's provider selection when the list
    /// shrank, so a catalog refresh that drops providers can never leave the
    /// selection pointing past the end of the list. Returns whether the list
    /// actually changed (identical payloads — e.g. the send-on-subscribe
    /// welcome — do not churn the status line).
    pub(crate) fn set_providers(&mut self, mut providers: Vec<ProviderInfo>) -> bool {
        // The wizard's picker is a flat alphabetical list; the daemon sends
        // the catalog in provenance order (see `sort_providers`), so re-sort
        // every incoming list before comparing/storing. Sorting first also
        // means a provider reorder alone never registers as a "change".
        sort_providers(&mut providers);
        if self.providers == providers {
            return false;
        }
        self.providers = providers;
        // Clamp the wizard's picker highlight when the list changed, so a
        // catalog refresh that drops providers can never leave the highlight
        // (or the scroll offset) pointing past the end of the narrowed list.
        self.ai_providers.wizard.clamp_focus(&self.providers);
        true
    }
    pub(crate) fn active_display(&mut self) -> Option<&mut SessionDisplayState> {
        self.session_displays.get_mut(&self.active_session_id?)
    }
    pub(crate) fn active_display_ref(&self) -> Option<&SessionDisplayState> {
        self.session_displays.get(&self.active_session_id?)
    }

    /// Whether a daemon message carrying the given wire session id is
    /// background noise that must not write the global status/error line.
    ///
    /// A connection-level reply (`None` — no origin session, e.g. a "no
    /// session attached" failure) resolves to the attached session, so it —
    /// like the attached session itself — keeps its fall-through feedback.
    /// Only a message about a real session that is not the attached session
    /// is suppressed.
    pub(crate) fn is_background_session_message(&self, reported_session_id: Option<u64>) -> bool {
        matches!(reported_session_id, Some(id) if self.attached_session_id != Some(id))
    }

    /// Resolve a daemon-reported session id to the session whose display it
    /// should update.  A connection-level reply carries `None` (no origin
    /// session — e.g. a "no session attached" failure, or a bare
    /// `GetReasoningEffort` reply without an attachment) and resolves to the
    /// attached session, so it never lands in a phantom display and defeats
    /// the attached-session routing in `is_background_session_message`.
    /// Returns `None` when there is no session to update — a `None` envelope
    /// with nothing attached.
    pub(crate) fn resolve_daemon_session(&self, session_id: Option<u64>) -> Option<u64> {
        match session_id {
            None => self.attached_session_id,
            Some(id) => Some(id),
        }
    }

    /// Number of lines needed for the status/error bar, based on the current
    /// message content and the available terminal width.  Returns 0 when there
    /// is no message to display.
    // Line counts are bounded by the u16 terminal width, so the usize→u16
    // cast cannot truncate in practice.
    #[expect(clippy::cast_possible_truncation)]
    pub(crate) fn status_error_height(&self, width: u16) -> u16 {
        let text = if let Some(ref err) = self.error {
            err.as_str()
        } else if let Some(ref status) = self.status {
            status.as_str()
        } else {
            return 0;
        };
        // The status/error Paragraph is drawn inset by one column on each side
        // (render.rs `notify_area`), so it wraps at `width - 2` columns.
        // Measure at that same inner width, or a long message's reserved
        // height can fall short of the rows ratatui actually draws and the
        // tail gets clipped by the layout.
        let inner = width.saturating_sub(2);
        let lines = plain_text_lines(text, inner);
        lines_height(&lines, inner).max(1) as u16
    }

    /// Number of visual content lines the input box currently occupies,
    /// computed from the text and terminal width.
    // Wrapped-line counts are bounded by the u16 terminal width, so the
    // usize→u16 cast cannot truncate in practice.
    #[expect(clippy::cast_possible_truncation)]
    pub(crate) fn input_bar_content_lines(&mut self, term_width: u16) -> u16 {
        // Must use the same inner width as the renderer (term_width minus the
        // INPUT_PAD padding on each side), or the box height can disagree with
        // the number of wrapped lines actually drawn — e.g. a wrap that the
        // renderer sees at the true inner width would not yet grow the box.
        let inner = input_inner_width(term_width);
        if inner < 1 {
            return 1;
        }
        let visual = cached_visual_lines(
            &self.input.text,
            inner,
            self.input.generation,
            &mut self.input.lines_cache,
        );
        (visual.len() as u16).clamp(MIN_INPUT_CONTENT_LINES, MAX_INPUT_CONTENT_LINES)
    }

    /// Total height of the input bar (content + borders).
    pub(crate) fn input_bar_height(&mut self, term_width: u16) -> u16 {
        self.input_bar_content_lines(term_width) + 2
    }

    /// The five vertical chunks of the Chat page: history, status/error, help,
    /// command input box, status bar.
    ///
    /// Single source of truth for the Chat page's vertical layout —
    /// `render_chat` draws into these chunks, `input_box_rect` hit-tests clicks
    /// against chunk 3, and `update_viewport_from_terminal_size` sizes the
    /// history viewport from chunk 0.  Every consumer runs the *identical*
    /// `Layout::split`, so they can never drift apart — including on terminals
    /// too small for the fixed chrome to fit, where the solver shrinks and
    /// relocates chunks rather than honouring every `Length`.
    pub(crate) fn chat_page_layout(&mut self, term_width: u16, term_height: u16) -> [Rect; 5] {
        let status_error_height = self.status_error_height(term_width);
        let help_height = if self.show_help_overlay { 2u16 } else { 0u16 };
        let input_height = self.input_bar_height(term_width);
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(1),
                Constraint::Length(status_error_height),
                Constraint::Length(help_height),
                Constraint::Length(input_height),
                Constraint::Length(STATUS_BAR_HEIGHT),
            ])
            .split(Rect {
                x: 0,
                y: 0,
                width: term_width,
                height: term_height,
            });
        // Layout::vertical([Min(1), Length(_), Length(_), Length(_),
        // Length(_)]) always yields exactly 5 chunks; a zero-area fallback
        // makes mouse hit-tests no-ops (nothing to click on).
        [
            chunks.first().copied().unwrap_or_default(),
            chunks.get(1).copied().unwrap_or_default(),
            chunks.get(2).copied().unwrap_or_default(),
            chunks.get(3).copied().unwrap_or_default(),
            chunks.get(4).copied().unwrap_or_default(),
        ]
    }

    /// Rectangle (terminal coordinates) occupied by the command input box on
    /// the Chat page, including its top/bottom borders.
    ///
    /// Delegates to [`Self::chat_page_layout`] so mouse hit-testing (clicking
    /// to position the cursor) always agrees with what `render_chat` draws —
    /// even on tiny terminals where the layout solver shrinks the box rather
    /// than placing it at a fixed distance above the status bar.
    pub(crate) fn input_box_rect(&mut self, term_width: u16, term_height: u16) -> Rect {
        self.chat_page_layout(term_width, term_height)[3]
    }

    /// Client-side submit-time guard shared by every action that begins a NEW
    /// inference turn: a plain prompt (`RunInput`) and the `ContinueGeneration`
    /// that the daemon turns into one.  The daemon is authoritative and would
    /// reject such a submission while it is busy (`session already has an
    /// active request`) or while the keystore is locked (no credentials in
    /// memory), but doing so costs a round-trip and surfaces a transient
    /// failure the user has to retype around — so the TUI pre-empts it here.
    ///
    /// Returns `Some(message)` with the status line to show — and the caller
    /// must NOT send — when the new turn is refused locally, or `None` when it
    /// may proceed.  A `None`/unknown `attached_status` fails open, so the
    /// daemon stays the authority for a fresh client that has not heard a
    /// status yet.
    ///
    /// The idle and locked cases are intentionally ordered idle-first: the
    /// busy case is the one the user hits during normal use, and it is the more
    /// actionable message.
    pub(crate) fn new_turn_rejection(&self) -> Option<&'static str> {
        // Idle guard: a new turn can only begin from an idle (`Inactive`)
        // session.  See `SessionStatus::is_idle` for why `Sleeping` counts as
        // busy too.  `None` (no status known yet) fails open.
        if let Some(status) = self.attached_status.as_ref()
            && !status.is_idle()
        {
            return Some("Session is not idle, please wait before prompting.");
        }
        // Locked guard: with no credentials decrypted in memory the daemon
        // cannot run inference, so a sent message would only come back as a
        // transient "no credential stored" failure that a keypress clears —
        // the persistent lock banner is the always-visible guidance.
        if self.keystore_locked {
            return Some(
                "daemon keystore is locked — unlock with /unlock, or /unlock \
                 <base64 unlock-key> (a fresh daemon binds automatically)",
            );
        }
        None
    }

    pub(crate) fn ensure_input_cursor_visible(&mut self) {
        if let Some((term_w, _)) = self.last_terminal_size {
            // inner width must match the renderer's drawing width so the
            // scroll window matches what is actually displayed.
            let inner = input_inner_width(term_w);
            let visible_height = self.input_bar_content_lines(term_w) as usize;
            self.input.ensure_cursor_visible(inner, visible_height);
        }
    }

    pub(crate) fn set_page(&mut self, page: Page) {
        self.page = page;
        // A command line is scoped to the Chat page: a page change (e.g.
        // Alt+S opening the session manager) must never leave one in the
        // buffer underneath.  Guarded so a real prompt draft survives a page
        // change (see `discard_command_line`).
        self.discard_command_line();
        // A selection is stored in screen coordinates keyed to the Chat
        // page's rendered history; leaving the page (or re-entering via an
        // attach flow, which changes the underlying session) invalidates
        // that context, so drop the gesture rather than highlight stale rows
        // or swallow the first click on return.
        self.text_selection = None;
        if let Some(d) = self.active_display() {
            d.progress_dirty = true;
        }
    }
}

#[cfg(test)]
mod tests;
