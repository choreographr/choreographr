//! The TUI's full application state: the `App` struct, per-session display
//! state, input plumbing, and the page/overlay states.  The history pane's
//! render model (height prefix-sum, markers, scroll, viewport, render cache),
//! the provider catalog, text-input machinery, page states, picker geometry,
//! and displayed-image fetch plumbing live in sibling modules (`history`,
//! `providers`, `input`, `pages`, `layout`, `images`) and are re-exported here
//! so the rest of the crate keeps referring to `crate::state::*` unchanged.

use crate::RenderedImage;
use crate::image_worker::{ImageId, ImageJob};
use crate::selection::TextSelection;
use crate::terminal;
use choreo_client_core::dispatch::{SessionStateData, ToolCallEvent};
use choreo_client_core::{ClientError, PendingReplies, SessionView, TurnEventHandler};
use choreo_proto::{
    AccountInfo, ClientMessage, ClientMessageType, OutputStream, ReasoningCapability,
    SessionStatus, SessionSummary, TokenUsage, Turn, socket_path,
};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use std::borrow::Cow;
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
mod session_manager;
mod streaming;
mod terminal_status;

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

    /// Enter `session_id`: rebind the active session and reset only the
    /// transient *render* state that the next layout pass will rebuild.  The
    /// per-session reading position (`history_scroll`) and prompt draft are
    /// preserved so a session the user returns to looks the way they left it.
    pub(crate) fn reset_for_session_switch(&mut self, session_id: u64) {
        // Capture the outgoing session's reading position before rebinding the
        // active session: the history viewport height reflows when the help /
        // status bands change on attach (and background sessions keep
        // streaming), so a raw from-bottom offset cannot be restored verbatim.
        // The captured anchor is applied once on the target's first rebuild.
        if let Some(prev) = self.active_session_id {
            let vp = self.history_viewport;
            if let Some(prev_display) = self.session_displays.get_mut(&prev) {
                prev_display.capture_scroll_restore(&vp);
            }
        }
        self.active_session_id = Some(session_id);
        // A command line belongs to the session the user was editing; it must
        // never become the newly-attached session's draft.  Discarding it clears
        // the buffer before any draft hand-off happens elsewhere.
        self.discard_command_line();
        // A selection is keyed to the previous session's rendered content in
        // screen coordinates; it must not linger and highlight the next
        // session's history.
        self.text_selection = None;
        // A history-recalled entry belongs to the session being left; end
        // browsing so the new session can never show, or stash, the previous
        // session's recalled prompt.  `persist_input_draft` — which runs before
        // this on every real switch — already exits browsing, so this is the
        // defensive backstop that makes the field's "reset on session switch"
        // contract hold even if a future caller skips the input hand-off.
        self.history_index = None;
        let display = self.display_for(session_id);
        // Keep the session's live state: `view.turns` and `view.request_to_turn`
        // (accumulated via the all-activity subscription while the user was
        // viewing another session), the active-request set, live token
        // estimates, and per-turn reasoning preferences.  Destroying these on
        // switch was the root cause of "switching to a streaming session shows
        // nothing until the next turn": the accumulated content AND the
        // request→turn routing map were wiped exactly when they were needed,
        // and the attach snapshot only holds the empty in-flight placeholder.
        //
        // Only transient *render* state is reset here — it is rebuilt on the
        // next layout pass because `markers_dirty` forces a full rebuild from
        // the preserved `view.turns`.  The reading position is NOT reset: the
        // outgoing session's absolute anchor was captured above, and the
        // target's own `scroll_restore` (set when it was last left) is applied
        // on the rebuild below, so a session reopens showing the content the
        // user left it on rather than snapping to the bottom.  A session never
        // visited has no anchor and opens at scroll 0 (the bottom) because
        // `or_default()` builds a fresh display behind `display_for`.
        display.render_cache.clear();
        display.visible_turn_ids.clear();
        display.markers.clear();
        display.height_prefix.clear();
        display.turn_heights.clear();
        display.turn_layouts.clear();
        display.streaming_turn_index = None;
        display.streaming_response = None;
        display.streaming_dirty = false;
        display.markers_dirty = true;
        display.content_dirty = false;
        display.status = None;
        display.error = None;
        display.progress_dirty = true;
        self.fullscreen_image_target = None;
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

    // ── Legacy per-session daemon message handlers ─────────────────────

    pub(crate) fn handle_session_created(
        &mut self,
        session_id: u64,
        parent_session_id: Option<u64>,
        account_name: Option<String>,
        selected_model: Option<String>,
        reasoning_effort: Option<String>,
        client_tx: &crossbeam_channel::Sender<ClientMessage>,
    ) -> Result<(), ClientError> {
        // Agent-spawned sub-sessions (parent_session_id = Some) are transient
        // tool artifacts, not sessions the user opened.  Navigating to one
        // would hijack the Chat view away from the session the user is reading
        // and destroy its scroll position, so treat it like background noise.
        if let Some(parent_id) = parent_session_id {
            tracing::info!(
                session_id,
                parent_session_id = parent_id,
                "sub-session created — not navigating",
            );
            return Ok(());
        }

        // Direct reply to THIS client's create (`SessionCreatedForRequester`):
        // navigate the requester to the session it just made — attach and
        // switch to the Chat page.  This is the requester-side half of the
        // requester-vs-broadcast split; every OTHER client sees only the
        // `SessionCreated` broadcast (`note_session_created`) and refreshes its
        // list without moving.  All three create entry points funnel here —
        // `/new`, `/session new`, and `n` on the Session Manager — so the client
        // that asked for the session always lands on it (previously the Session
        // Manager branch returned early, leaving the creator on the list).
        //
        // Prime the display fields from the creation params BEFORE attaching:
        // the session summary (from the `ListSessions` below) may not have
        // arrived yet, and the status bar should read correctly the instant the
        // attach lands.  `attach_to_session` -> `reset_for_session_switch`
        // preserves these (it clears only transient render state).
        {
            let display = self.display_for(session_id);
            display.account_name = account_name;
            display.selected_model = selected_model;
            display.reasoning_effort = reasoning_effort;
        }
        // Fetch the session summary before attaching ONLY when leaving another
        // page (Chat): the reply populates `session_mgr.all`, so the attach's
        // status-bar priming and the daemon's `SessionAttached` gap-fill have
        // the data.  On the Session Manager page the fetch is deliberately
        // SKIPPED: the broadcast `SessionCreated` already refreshes an open list
        // (`note_session_created`), and the direct reply races that broadcast —
        // so fetching here too would send a redundant `ListSessions` for a
        // create the user is navigating away from (the page re-fetches on the
        // next `open_session_manager` anyway).  This is what restores the
        // original "one list refresh per create, not two" invariant now that a
        // create from ANY page funnels through this handler.
        if self.page != Page::SessionManager {
            self.pending
                .send(client_tx, ClientMessageType::ListSessions);
        }
        // Shared attach sequence (also used by the Session Manager's Enter):
        // it sends UnsubscribeSessionsSummary + AttachSession, hands the input
        // bar over, rebinds the active session, and switches to the Chat page.
        // A broken pipe leaves the view on the previous session instead of
        // stranding the user on an un-attached one.
        self.attach_to_session(session_id, client_tx)
    }

    /// Handle the BROADCAST notification that a session was created — by any
    /// client, this one included (a create arrives both as the direct
    /// `SessionCreatedForRequester` reply and as the `SessionCreated`
    /// notification).
    ///
    /// Unlike [`App::handle_session_created`] — the direct reply to THIS
    /// client's create, which auto-attaches — a notification must NEVER change
    /// the attached session. This is the fix for the phone-view-follows-laptop
    /// bug: before the split, a broadcast create was indistinguishable from
    /// the reply and every client attached to it.
    ///
    /// The only action is to keep the session list current *when the user is
    /// looking at it*: on the Session Manager page an unsolicited `ListSessions`
    /// renders a fresh list; on the Chat page it would rewrite the status line
    /// for an event the user did not initiate, so it is skipped — matching the
    /// sub-session-on-the-Chat-page rule in `handle_session_created`.
    pub(crate) fn note_session_created(
        &mut self,
        session_id: u64,
        parent_session_id: Option<u64>,
        client_tx: &crossbeam_channel::Sender<ClientMessage>,
    ) {
        tracing::debug!(
            session_id,
            parent_session_id,
            "session created elsewhere — refreshing list only, not attaching",
        );
        if self.page == Page::SessionManager {
            // Best-effort refresh: a broken channel means the whole connection
            // is tearing down, and the reply renders into the session list
            // (never the status line), so there is nothing to propagate.
            self.pending
                .send(client_tx, ClientMessageType::ListSessions);
        }
    }

    pub(crate) fn handle_session_attached(&mut self, session_id: u64) {
        self.active_session_id = Some(session_id);
        self.attached_session_id = Some(session_id);
        // Attaching (re)points the terminal records at this session.
        self.term_status_dirty = true;
        // Copy session summary fields before borrowing display.
        let (
            token_usage,
            context_window,
            last_prompt_tokens,
            account_name,
            selected_model,
            reasoning_effort,
            working_dir,
            status,
        ) = self
            .session_mgr
            .all
            .iter()
            .find(|s| s.session_id == session_id)
            .map_or((None, None, None, None, None, None, None, None), |s| {
                (
                    s.token_usage,
                    s.context_window,
                    s.last_prompt_tokens,
                    s.account_name.clone(),
                    s.selected_model.clone(),
                    s.reasoning_effort.clone(),
                    s.working_dir.clone(),
                    Some(s.status.clone()),
                )
            });
        {
            let display = self.display_for(session_id);
            // Fill gaps from the (potentially stale) session summary, but never
            // clobber values already accumulated via the all-activity
            // subscription while this session was in the background: the
            // summary is refreshed on ListSessions, whereas the display may
            // hold fresher per-turn token usage / live counts / model that
            // arrived mid-stream.  Overwriting here would regress the status
            // bar right after switching into a streaming session.
            if display.token_usage.is_none() {
                display.token_usage = token_usage;
            }
            if display.context_window.is_none() {
                display.context_window = context_window;
            }
            if display.last_prompt_tokens.is_none() {
                display.last_prompt_tokens = last_prompt_tokens;
            }
            if display.account_name.is_none() {
                display.account_name = account_name;
            }
            if display.selected_model.is_none() {
                display.selected_model = selected_model;
            }
            if display.reasoning_effort.is_none() {
                display.reasoning_effort = reasoning_effort;
            }
            if display.working_dir.is_none() {
                display.working_dir = working_dir;
            }
            if let Some(ref st) = status {
                display.status = Some(format!("{st:?}"));
            }
        }
        self.attached_status = status;
        self.refresh_attached_account_slug();
        self.show_help_overlay = true;
        if let Some(d) = self.active_display() {
            d.progress_dirty = true;
        }
    }

    /// The account slug shown in the status bar: the attached session's
    /// account name (the account name is the slug users enter when creating
    /// one). Previously this showed the inference provider slug resolved via
    /// the accounts list, but the account itself is the more useful identity.
    pub(crate) fn refresh_attached_account_slug(&mut self) {
        self.attached_account_slug = self
            .active_display_ref()
            .and_then(|d| d.account_name.clone());
    }

    pub(crate) fn attached_session_mut(&mut self) -> Option<&mut SessionSummary> {
        self.session_mgr
            .all
            .iter_mut()
            .find(|s| Some(s.session_id) == self.attached_session_id)
    }

    /// The summary of `session_id`, but only when it is the attached session.
    ///
    /// Per-session display updates mirror into the status bar's summary
    /// exclusively for the attached session — a background session's model,
    /// effort or account change must never rewrite the identity fields of the
    /// session on screen.
    fn mirror_to_attached_summary(&mut self, session_id: u64) -> Option<&mut SessionSummary> {
        if self.attached_session_id == Some(session_id) {
            self.attached_session_mut()
        } else {
            None
        }
    }

    /// A model was selected on the session `session_id`.  Only that session's
    /// display (and, when it is the attached session, the summary used by the
    /// status bar) is updated — a `ModelSelected` broadcast for a background
    /// session must never overwrite the display the user is currently viewing.
    pub(crate) fn handle_model_selected(
        &mut self,
        session_id: u64,
        model: &str,
        reasoning_capability: Option<ReasoningCapability>,
    ) {
        let display = self.display_for(session_id);
        display.selected_model = Some(model.to_owned());
        display.reasoning_capability = reasoning_capability;
        if let Some(s) = self.mirror_to_attached_summary(session_id) {
            s.selected_model = Some(model.to_owned());
        }
    }

    /// A reasoning-effort change was accepted on the session `session_id`.
    /// Routed to that session's own display only — see `handle_model_selected`.
    pub(crate) fn handle_reasoning_effort_set(&mut self, session_id: u64, effort: String) {
        let display = self.display_for(session_id);
        display.reasoning_effort = Some(effort.clone());
        if let Some(s) = self.mirror_to_attached_summary(session_id) {
            s.reasoning_effort = Some(effort);
        }
    }

    // Call sites in `connection/daemon.rs` pass `&Option<String>`; changing
    // the signature would touch files outside this one.
    #[expect(clippy::ref_option)]
    pub(crate) fn handle_session_working_dir_set(
        &mut self,
        session_id: u64,
        path: &Option<String>,
    ) {
        if self.attached_session_id == Some(session_id) {
            if let Some(d) = self.active_display() {
                d.working_dir.clone_from(path);
                d.progress_dirty = true;
            }
            if let Some(s) = self.attached_session_mut() {
                s.working_dir.clone_from(path);
            }
        }
    }

    pub(crate) fn handle_session_title_set(&mut self, session_id: u64, title: &str) {
        if self.attached_session_id == Some(session_id) {
            self.status = Some(format!("Session title changed to '{title}'"));
            if let Some(s) = self.attached_session_mut() {
                s.title = Some(title.to_owned());
            }
        }
        // The window title (OSC 2) and this session's program-status record
        // (OSC 7501 `title=`) both carry the title.
        self.term_status_dirty = true;
    }

    /// The account for the session `session_id` was set.  Only that session's
    /// display is updated; the status bar's provider slug and the session
    /// summary are refreshed only when the message belongs to the attached
    /// session (a background session's account change must not alter the
    /// attached session's identity fields).
    pub(crate) fn handle_session_account_set(&mut self, session_id: u64, account: &str) {
        let display = self.display_for(session_id);
        display.account_name = Some(account.to_owned());
        if let Some(s) = self.mirror_to_attached_summary(session_id) {
            s.account_name = Some(account.to_owned());
        }
        // Refresh the status-bar account slug only for the attached session
        // (it reads the display account name set above).
        if self.attached_session_id == Some(session_id) {
            self.refresh_attached_account_slug();
        }
    }

    pub(crate) fn handle_session_status_changed(
        &mut self,
        session_id: u64,
        status: &SessionStatus,
        last_modified: i64,
    ) {
        if let Some(session) = self
            .session_mgr
            .all
            .iter_mut()
            .find(|s| s.session_id == session_id)
        {
            session.status = status.clone();
            // last_modified is monotonic; guard against duplicate or
            // out-of-order deliveries (per-session + summary paths).
            session.last_modified = session.last_modified.max(last_modified);
        }
        // A status change bumps last_modified on the daemon, so the list may
        // reorder while the user is looking at it — re-sort but keep the
        // cursor on the same session.
        self.session_mgr.resort_after_status_change();
        if let Some(ref mut detail) = self.session_mgr.detail_data
            && detail.session_id == session_id
        {
            detail.status = status.clone();
        }
        if self.attached_session_id == Some(session_id) {
            self.attached_status = Some(status.clone());
        }
        // A new turn (or the session going to sleep) clears the previous
        // turn's done/error outcome; the trailing idle status of a
        // just-finished turn leaves it in place so the outcome survives the
        // prompt. Either way the published records may have changed.
        if !matches!(status, SessionStatus::Inactive) {
            self.term_status_override.remove(&session_id);
        }
        self.term_status_dirty = true;
    }

    /// Detect when the user is reading an agent-spawned sub-session on the
    /// Chat page and that sub-session just finished running.
    ///
    /// A sub-session "finishes" when its status transitions from an active
    /// state (inference / tool call / retrying) to an idle one (inactive /
    /// sleeping) — the daemon broadcasts exactly one such transition when the
    /// child's request completes.  The check reads the *pre-update* summary
    /// status (the caller invokes this before applying the new status), so
    /// duplicate idle→idle broadcasts — summary refreshes, or re-attaching to
    /// a child that finished earlier — never re-fire the switch.
    ///
    /// Returns the parent session id to switch back to, or `None` when the
    /// user is not viewing a finishing sub-session.  The parent id (and the
    /// titles for the notification) come from the summary list, so a missing
    /// summary — or a parent that no longer exists in it — is a graceful
    /// no-op rather than a misdirected switch.
    pub(crate) fn attached_subsession_finished(
        &self,
        session_id: u64,
        new_status: &SessionStatus,
    ) -> Option<u64> {
        // Only the Chat page: the Session Manager is a browsing view, and
        // auto-jumping away from it would fight the user's navigation.
        if self.page != Page::Chat || self.attached_session_id != Some(session_id) {
            return None;
        }
        // The finishing session must be an agent-spawned sub-session; its
        // parent is only known from the session summary list.
        let summary = self
            .session_mgr
            .all
            .iter()
            .find(|s| s.session_id == session_id)?;
        let parent_id = summary.parent_session_id?;
        // Only the active → idle transition counts as "finished".  Idle →
        // idle (e.g. a summary refresh after the child already finished)
        // must not yank the view away while the user is still reading.
        if summary.status.is_active()
            && !new_status.is_active()
            // The parent must still exist in the summary: if it was deleted
            // while the child ran, switching would attach to a dead session
            // id and strand the user on a session the daemon rejects.
            && self
                .session_mgr
                .all
                .iter()
                .any(|s| s.session_id == parent_id)
        {
            Some(parent_id)
        } else {
            None
        }
    }

    /// Attach the Chat view to `session_id` via the shared sequence every
    /// attach path follows (Session Manager list/detail Enter, and the
    /// sub-session finish switch-back).
    ///
    /// The daemon messages are sent *before* the local state is mutated, so a
    /// broken pipe leaves the view on the previous session instead of
    /// stranding the user on a session that was never attached.
    /// `UnsubscribeSessionsSummary` is idempotent on the daemon (removing a
    /// client that was never registered is a no-op), so it is safe to send
    /// unconditionally.  `reset_for_session_switch` runs before `set_page` so
    /// the subsequent `set_page` marks the target's display dirty — the one
    /// that will actually render next — and `attached_status` is refreshed
    /// immediately from the summary instead of waiting for the daemon's
    /// `SessionAttached` reply to arrive.
    #[expect(clippy::unnecessary_wraps)]
    pub(crate) fn attach_to_session(
        &mut self,
        session_id: u64,
        client_tx: &crossbeam_channel::Sender<ClientMessage>,
    ) -> Result<(), ClientError> {
        self.pending
            .send(client_tx, ClientMessageType::UnsubscribeSessionsSummary);
        self.pending
            .send(client_tx, ClientMessageType::AttachSession { session_id });
        // Discard a command line BEFORE the input hand-off below: it is not a
        // prompt, so it must be dropped (not stashed as the outgoing session's
        // draft) and the target session's draft loaded in its place.
        self.discard_command_line();
        // Hand the input bar over to the target session (stash the outgoing
        // session's input, load the target's draft) before `attached_session_id`
        // is rebound below — it still names the session the input bar's
        // current contents belong to.  See `persist_input_draft`.
        self.persist_input_draft(session_id);
        // reset_for_session_switch first so the subsequent set_page marks the
        // target's display dirty — the one that will actually render next.
        self.reset_for_session_switch(session_id);
        self.set_page(Page::Chat);
        self.attached_session_id = Some(session_id);
        // Refresh the status bar right away from the summary; the daemon's
        // SessionAttached reply re-applies the same (possibly newer) value.
        self.attached_status = self
            .session_mgr
            .all
            .iter()
            .find(|s| s.session_id == session_id)
            .map(|s| s.status.clone());
        // The attached session changed, so both the window title (OSC 2) and
        // the program-status records (OSC 7501) must be re-published.
        self.term_status_dirty = true;
        Ok(())
    }

    /// Switch the Chat view back to the parent session of a sub-session that
    /// just finished, and surface a status notification explaining the jump.
    ///
    /// Delegates to [`attach_to_session`] — the same sequence the Session
    /// Manager Enter handlers use — so the daemon messages are sent *before*
    /// the local state is mutated, and a broken pipe leaves the view on the
    /// finished sub-session instead of stranding the user on a session that
    /// was never attached.
    pub(crate) fn switch_back_to_parent(
        &mut self,
        finished_session_id: u64,
        parent_id: u64,
        client_tx: &crossbeam_channel::Sender<ClientMessage>,
    ) -> Result<(), ClientError> {
        // Titles come from the summary list — the same source that told us
        // the sub-session's parent — falling back to "untitled" exactly like
        // the session list renderer does.
        let title = |id: u64| {
            self.session_mgr
                .all
                .iter()
                .find(|s| s.session_id == id)
                .and_then(|s| s.title.clone())
                .unwrap_or_else(|| "untitled".to_string())
        };
        let subsession_title = title(finished_session_id);
        let parent_title = title(parent_id);

        self.attach_to_session(parent_id, client_tx)?;

        self.status = Some(format!(
            "Subsession \"{subsession_title}\" finished. Switched back to parent \"{parent_title}\"."
        ));
        Ok(())
    }

    pub(crate) fn handle_accounts(&mut self, accounts: &[AccountInfo]) {
        self.ai_providers.set_accounts(accounts.to_vec());
        self.refresh_attached_account_slug();
    }

    #[expect(clippy::unnecessary_wraps)]
    pub(crate) fn handle_sessions(
        &mut self,
        sessions: &[SessionSummary],
        client_tx: &crossbeam_channel::Sender<ClientMessage>,
    ) -> Result<(), ClientError> {
        self.session_mgr.set_sessions(sessions.to_vec());
        if self.page == Page::Chat {
            if sessions.is_empty() {
                self.status = Some("[daemon] no sessions".to_string());
            } else {
                self.status = Some(format!("[daemon] sessions ({})", sessions.len()));
                for session in sessions {
                    let prefix = if Some(session.session_id) == self.attached_session_id {
                        "*"
                    } else {
                        " "
                    };
                    let title = session.title.as_deref().unwrap_or("untitled");
                    let model = session.selected_model.as_deref().unwrap_or("-");
                    self.status = Some(format!(
                        "{} {}: \"{title}\" ({model}) — {} turns",
                        prefix, session.session_id, session.turn_count,
                    ));
                }
            }
            if self.attached_session_id.is_none() {
                // Auto-attach to a LIVE session only. Archived sessions are
                // deliberately hidden from the sessions list's live view, so
                // they must not hijack the Chat page either — a restart with a
                // pinned/archived session would otherwise silently open it.
                // Filtered into a local slice FIRST so the existing top-level
                // preference (and the sub-session fallback) keep their exact
                // semantics over the non-archived subset.
                //
                // Prefer the most recently modified *top-level* session.
                // Agent-spawned sub-sessions (parent_session_id = Some) are
                // transient tool artifacts whose last_modified is bumped as
                // they stream, so they'd otherwise top the list and silently
                // hijack the view to a session the user never opened — e.g.
                // its streaming token count would appear on the chat page.
                let live: Vec<&SessionSummary> = sessions
                    .iter()
                    .filter(|s| s.archived_at.is_none())
                    .collect();
                let target = live
                    .iter()
                    .copied()
                    .find(|s| s.parent_session_id.is_none())
                    .or_else(|| live.first().copied());
                if let Some(first) = target {
                    // Set attachment state immediately (mirroring the session
                    // manager Enter handler) so a second Sessions reply in the
                    // same tick cannot auto-attach again to a different
                    // session, and so the page renders the target session
                    // instead of a blank screen until SessionAttached arrives.
                    // Hand the input bar over like every other attach path;
                    // with nothing attached yet this only loads the target's
                    // draft (see `persist_input_draft`).
                    self.persist_input_draft(first.session_id);
                    self.reset_for_session_switch(first.session_id);
                    self.attached_session_id = Some(first.session_id);
                    // The auto-attach changed the attached session, so the
                    // window title and program-status records must refresh.
                    self.term_status_dirty = true;
                    self.pending.send(
                        client_tx,
                        ClientMessageType::AttachSession {
                            session_id: first.session_id,
                        },
                    );
                } else {
                    // Inherit account_name from the first available account,
                    // so the auto-created default session doesn't lose the
                    // account selection that was already configured.
                    let default_account =
                        self.ai_providers.accounts.first().map(|a| a.name.clone());
                    // Deliberately no working directory: the daemon may serve a
                    // remote client over TCP, so the TUI process's own cwd is
                    // meaningless on the daemon host and could set a nonexistent
                    // session working directory. The working directory is chosen
                    // later (e.g. via `set_working_dir` or when attaching a
                    // session that already has one).
                    self.pending.send(
                        client_tx,
                        ClientMessageType::CreateSession {
                            title: Some("default".to_string()),
                            parent_session_id: None,
                            working_dir: None,
                            context_config: None,
                            account_name: default_account,
                            selected_model: None,
                            reasoning_effort: None,
                        },
                    );
                }
            }
        }
        Ok(())
    }

    pub(crate) fn handle_session_deleted(&mut self, session_id: u64) {
        self.session_mgr.remove_session(session_id);
        self.session_displays.remove(&session_id);
        self.rendered_images.remove(&session_id);
        if self.attached_session_id == Some(session_id) {
            self.attached_session_id = None;
            self.active_session_id = None;
            self.attached_account_slug = None;
            // Drop the deleted session's cached status and tool groups too.
            // They describe the attachment that just went away; leaving them
            // set would both render a stale status bar (there is no attached
            // session to describe anymore) and mislead the submit-time
            // idle-guard in `connection/chat.rs`, which reads `attached_status`
            // and would wrongly reject a prompt as "session not idle" when
            // nothing is attached at all.
            //
            // This restores the invariant `attached_session_id == None`
            // implies `attached_status == None`, which the auto-attach path in
            // `handle_sessions` relies on: it re-binds `attached_session_id`
            // (and only that) before the daemon's `SessionAttached` reply
            // refreshes the status, so a stale `attached_status` would leak
            // across the switch.  Every path that clears `attached_session_id`
            // must clear these two alongside it.
            self.attached_status = None;
            self.attached_tool_groups.clear();
            // The deleted session's unsent prompt dies with it — the display
            // (and its draft) are gone above, so drop the input bar too
            // rather than leak the orphaned text into whichever session gets
            // attached next.
            let had_input = !self.input.text.is_empty();
            tracing::debug!(
                session_id,
                had_input,
                "deleted attached session: dropping its input draft",
            );
            self.input.clear();
            self.commit_to_history();
        }
        // Drop the deleted session's terminal records too, and re-publish:
        // its child record must be cleared (the publisher's sync clears an id
        // that vanished from the desired set) and, if it was attached, the
        // window title falls back to the plain program name.
        self.term_status_override.remove(&session_id);
        self.term_status_dirty = true;
    }

    pub(crate) fn handle_session_delete_failed(&mut self, session_id: u64, error: &str) {
        self.status = Some(format!("failed to delete session {session_id}: {error}"));
    }

    /// A per-session `pinned`/`archived_at` flag change was broadcast by the
    /// daemon — the success signal for a `SetSessionPinned`/`SetSessionArchived`
    /// request.  There is no targeted success reply, so the TUI deliberately
    /// does NOT mutate its own list on the keypress; this handler is what
    /// applies the change (a failure instead arrives as `SessionEvent::SessionFailed`).
    pub(crate) fn handle_session_flags_changed(
        &mut self,
        session_id: u64,
        pinned: bool,
        archived_at: Option<i64>,
    ) {
        self.session_mgr
            .apply_session_flags(session_id, pinned, archived_at);
    }

    pub(crate) fn display_token_usage(&self) -> Option<TokenUsage> {
        let display = self.active_display_ref()?;
        let usage = display.token_usage.as_ref()?;
        Some(TokenUsage {
            input_tokens: usage.input_tokens + display.live_input_estimate,
            output_tokens: usage.output_tokens + display.live_output_tokens,
            total_tokens: usage.total_tokens
                + display.live_input_estimate
                + display.live_output_tokens,
            // Cached tokens are only reported in settled usage, not in the live
            // per-chunk estimates, so carry the settled value through untouched.
            // The cache-write count is settled-only for the same reason.
            cached_tokens: usage.cached_tokens,
            cache_write_tokens: usage.cache_write_tokens,
        })
    }
}

/// Merge a daemon-provided token usage into the display's accumulated value,
/// never regressing it.
///
/// Cumulative token usage only ever increases (the daemon accumulates per-turn
/// usage monotonically), so the merge is a per-field max via
/// [`TokenUsage::merge_max`].  This matters when switching into a session that
/// is mid-turn: the attach `SessionState` snapshot is built from the session
/// thread's config, which can lag the request worker's live accumulation (and,
/// in the worker→main sync window, the value already broadcast to this client).
/// A blind overwrite would regress the status bar's `↑in ↓out` readout until
/// the next `TokenUsageUpdate` — i.e. until the turn ends — while a `None`
/// snapshot must never wipe an accumulated total.
// Call sites in `connection/daemon.rs` pass `&Option<TokenUsage>`;
// changing the signature would touch files outside this one.
#[expect(clippy::ref_option)]
pub(crate) fn merge_token_usage(
    current: &Option<TokenUsage>,
    incoming: &Option<TokenUsage>,
) -> Option<TokenUsage> {
    match (current, incoming) {
        (Some(cur), Some(inc)) => {
            let mut merged = *cur;
            merged.merge_max(*inc);
            Some(merged)
        }
        (Some(cur), None) => Some(*cur),
        (None, Some(inc)) => Some(*inc),
        (None, None) => None,
    }
}

/// Invalidate the render cache entry for `turn_id`.
fn invalidate_turn_cache(display: &mut SessionDisplayState, turn_id: u32) {
    if let Some(idx) = display
        .visible_turn_ids
        .iter()
        .position(|id| *id == turn_id)
        && let Some(slot) = display.render_cache.get_mut(idx)
    {
        *slot = None;
    }
}

/// Decide whether the locally-accumulated version of a turn should win over
/// the daemon snapshot's version when merging an attach snapshot.
///
/// The snapshot is authoritative for finished turns, but for the in-flight
/// turn it contains only the empty placeholder inserted by `start_turn` — the
/// accumulated version (fed by `OutputChunk`, `ToolCallStarted` and
/// `ToolResultChunk` via the all-activity subscription) holds the real
/// streaming content.  Keep the accumulated turn whenever it carries content
/// the snapshot version lacks; otherwise prefer the snapshot, which is the
/// daemon's canonical state.
fn turn_has_live_content(accumulated: &Turn, snapshot: &Turn) -> bool {
    (accumulated
        .assistant_text
        .as_deref()
        .is_some_and(|s| !s.is_empty())
        && snapshot.assistant_text.as_deref().is_none_or(str::is_empty))
        || (accumulated
            .assistant_reasoning
            .as_deref()
            .is_some_and(|s| !s.is_empty())
            && snapshot
                .assistant_reasoning
                .as_deref()
                .is_none_or(str::is_empty))
        || (!accumulated.tool_calls.is_empty() && snapshot.tool_calls.is_empty())
        || (!accumulated.tool_results.is_empty() && snapshot.tool_results.is_empty())
        || (!accumulated.displayed_images.is_empty() && snapshot.displayed_images.is_empty())
}

impl App {
    /// Tear down the per-session display state a terminal request outcome
    /// leaves behind.  A failure (`handle_failed`) and a cancel
    /// (`handle_cancelled`) both end an in-flight request, so they share this
    /// teardown: clear the tool-call description map for the closing turn,
    /// drop the request→turn mapping, and reset streaming state.
    fn finish_request(&mut self, session_id: u64, stream_id: u64) {
        let display = self.display_for(session_id);
        // A request that ends without re-broadcasting its turn never runs
        // `insert_or_replace`, so the description map is not cleaned
        // automatically — clear it here (before the request→turn mapping is
        // removed) to keep the map bounded by in-flight calls even on the
        // terminal path.
        if let Some(&turn_id) = display.view.request_to_turn.get(&stream_id) {
            display.view.clear_tool_call_descriptions(turn_id);
        }
        display.view.request_to_turn.remove(&stream_id);
        display.active.remove(&stream_id);
        display.streaming_turn_index = None;
        display.streaming_response = None;
        display.mark_content_changed();
    }
}

// ── TurnEventHandler implementation ──────────────────────────────────

impl TurnEventHandler for App {
    fn handle_image(
        &mut self,
        session_id: u64,
        turn_id: u32,
        key: choreo_proto::ImageKey,
        data: Option<Vec<u8>>,
    ) {
        tracing::trace!(%session_id, %turn_id, ?key, "handle_image");
        self.handle_image_reply(session_id, turn_id, key, data);
    }

    fn handle_turn_appended(&mut self, session_id: u64, turn_id: u32, turn: Turn) {
        tracing::trace!(%turn_id, "handle_turn_appended");
        self.sync_turn_images(session_id, turn_id, &turn);
        let display = self.display_for(session_id);
        invalidate_turn_cache(display, turn_id);
        display.view.insert_or_replace(turn_id, turn);
        // Replacement can change the rendered content even when the cache key's
        // other fields (widths, reasoning/collapse state) stay identical, so
        // bump the version to force a recompute on the next rebuild.
        display.bump_turn_version(turn_id);
        display.mark_content_changed();
    }

    fn handle_turns_undone(&mut self, session_id: u64, turn_ids: &[u32]) {
        tracing::trace!(?turn_ids, "handle_turns_undone");
        let display = self.display_for(session_id);
        for tid in turn_ids {
            invalidate_turn_cache(display, *tid);
            // Drop the content-version entry rather than bumping it: the
            // cache slot was invalidated above and undone turns are skipped
            // by rebuilds, so no cached rendering can survive for this turn;
            // `handle_turns_redone` re-invalidates the slot before
            // re-inserting, so a redone turn (even with byte-identical
            // content) always recomputes fresh.  Pruning keeps the version
            // map bounded by the live (non-undone) turn set instead of the
            // session's whole history.
            display.turn_versions.remove(tid);
            // Drop the user's reasoning-expansion preference for undone turns
            // so the map can't accumulate stale entries; a redo restores the
            // turn fresh with the derived default.
            display.reasoning_override.remove(tid);
            // Same for tool-result collapse preferences: a redo restores the
            // turn fresh, so stale (turn, call_id) overrides must not leak.
            display.tool_collapse_override.remove(tid);
            if let Some(turn) = display.view.turns.get_mut(tid) {
                turn.undone = true;
            }
        }
        display.mark_content_changed();
    }

    fn handle_turns_redone(
        &mut self,
        session_id: u64,
        turns: std::collections::BTreeMap<u32, Turn>,
    ) {
        // Never `?turns`: a `Turn` carries message content.
        tracing::trace!(count = turns.len(), "handle_turns_redone");
        // Sync images first, then get display to avoid borrow conflict.
        for (tid, turn) in &turns {
            self.sync_turn_images(session_id, *tid, turn);
        }
        let display = self.display_for(session_id);
        for (tid, turn) in turns {
            invalidate_turn_cache(display, tid);
            display.bump_turn_version(tid);
            display.view.insert_or_replace(tid, turn);
        }
        display.mark_content_changed();
    }

    fn handle_request_stream(
        &mut self,
        session_id: u64,
        stream_id: u64,
        stream: OutputStream,
        data: Cow<'_, str>,
    ) {
        let display = self.display_for(session_id);
        // Detect the first Answer chunk for this request: the turn has no
        // response text yet, so this chunk begins the response phase.
        let turn_id = display.view.request_to_turn.get(&stream_id).copied();
        let first_answer = matches!(stream, OutputStream::Answer)
            && turn_id
                .and_then(|id| display.view.turns.get(&id))
                .is_some_and(|t| t.assistant_text.is_none());

        display.view.stream_chunk(stream_id, &stream, &data);

        // The appended chunk changed the turn's rendered content: bump its
        // version so any rebuild (e.g. one triggered by an interleaved
        // `Done`/`TurnAppended` from another request or session) recomputes
        // this turn instead of serving the pre-chunk cached lines.
        if let Some(turn_id) = turn_id {
            display.bump_turn_version(turn_id);
        }

        // Auto-collapse reasoning when the response starts — drop any
        // explicit expansion override so the derived default (collapsed once
        // a response exists) takes over.  The user can re-expand it by
        // clicking the header.
        if first_answer && let Some(turn_id) = turn_id {
            display.reasoning_override.remove(&turn_id);
        }

        display.resolve_streaming_turn_index(stream_id);
        display.mark_streaming_changed();
    }

    fn handle_started(
        &mut self,
        session_id: u64,
        stream_id: u64,
        turn_id: u32,
        estimated_prompt_tokens: u32,
    ) {
        tracing::trace!(%stream_id, %turn_id, %estimated_prompt_tokens, "handle_started");
        let display = self.display_for(session_id);
        display.view.request_to_turn.insert(stream_id, turn_id);
        display.active.insert(stream_id);
        display.live_input_estimate = estimated_prompt_tokens;
        display.live_output_tokens = 0;
        display.streaming_turn_index = display
            .visible_turn_ids
            .iter()
            .position(|id| *id == turn_id);
    }

    fn handle_done(
        &mut self,
        session_id: u64,
        stream_id: u64,
        token_usage: Option<TokenUsage>,
        last_prompt_tokens: Option<u32>,
    ) {
        tracing::trace!(%stream_id, "handle_done");
        // Done always arrives with `Some` (the session task knows its id), but
        // resolve defensively anyway so this choke point can never write to an
        // unintended display if a connection-level path is ever added.
        let Some(session_id) = self.resolve_daemon_session(Some(session_id)) else {
            return;
        };
        // A completed turn is a terminal outcome `SessionStatus` cannot
        // express; record it as `done` so it survives the trailing idle status
        // the daemon broadcasts when the request finishes.
        self.term_status_override.insert(session_id, "done");
        self.term_status_dirty = true;
        let display = self.display_for(session_id);
        // The final TurnAppended already cleaned description entries via
        // `insert_or_replace`, but if that broadcast was dropped under load
        // the map would keep them — clear for this turn so the map stays
        // bounded by in-flight calls even when the terminal broadcast is
        // lost.  (Looked up before `request_to_turn` is removed.)
        if let Some(&turn_id) = display.view.request_to_turn.get(&stream_id) {
            display.view.clear_tool_call_descriptions(turn_id);
        }
        display.view.request_to_turn.remove(&stream_id);
        display.active.remove(&stream_id);
        if let Some(usage) = token_usage {
            display.token_usage = Some(usage);
            if last_prompt_tokens.is_none() {
                display.last_prompt_tokens = Some(usage.input_tokens);
            }
        }
        if let Some(tokens) = last_prompt_tokens {
            display.last_prompt_tokens = Some(tokens);
        }
        display.live_input_estimate = 0;
        display.live_output_tokens = 0;
        display.streaming_turn_index = None;
        // Streaming is over: drop the incremental response cache so a later
        // turn can never reuse this response's committed markdown.
        display.streaming_response = None;
        display.mark_content_changed();
    }

    fn handle_failed(&mut self, session_id: Option<u64>, stream_id: u64, error: String) {
        // Never `%error`: a failure message can embed provider/request text.
        tracing::trace!(%stream_id, error_len = error.len(), "handle_failed");
        // A connection-level failure (e.g. "no session attached" from
        // RunInput/SetModel/SetReasoningEffort) arrives with `session_id:
        // None` — no origin session — meaning "the attached session".  Resolve
        // it so the failure lands in the session the user is actually attached
        // to rather than a phantom display.
        let is_connection_level = session_id.is_none();
        let Some(session_id) = self.resolve_daemon_session(session_id) else {
            tracing::debug!(%stream_id, %error, "dropping failure: no attached session to route the connection-level failure to");
            // No display to update, but a connection-level rejection (e.g.
            // "no session attached") is exactly what the user needs to see
            // on the status line.
            if is_connection_level {
                self.error = Some(error);
            }
            return;
        };
        // A request-level failure is a turn outcome `SessionStatus` cannot
        // express — record it so it survives the trailing idle status. A
        // connection-level rejection has no origin session and is not a
        // session turn outcome, so it is left to the status line.  A
        // cancellation never reaches here (it has its own `handle_cancelled`)
        // — this is a real failure, so it reports `error`.
        if !is_connection_level {
            self.term_status_override.insert(session_id, "error");
            self.term_status_dirty = true;
        }
        // A connection-level failure has no turn to render an error block in,
        // so the global status/error bar is its only surface.  A request-level
        // failure (a real session id) already renders the full error as the
        // turn's red block in the transcript — writing it here too would
        // print the same message twice on screen — so it is only recorded on
        // the per-session display.  (Written before the mutable display
        // borrow below so `self.error` is still reachable.)
        if is_connection_level {
            self.error = Some(error.clone());
        }
        // The per-session display records the failure for whichever session it
        // belongs to (rendered once the user views that session).
        self.display_for(session_id).error = Some(error);
        self.finish_request(session_id, stream_id);
    }

    fn handle_cancelled(&mut self, session_id: Option<u64>, stream_id: u64) {
        tracing::trace!(%stream_id, "handle_cancelled");
        // A connection-level cancel (no origin session) resolves to the
        // attached session, mirroring `handle_failed`, so it never lands in a
        // phantom display.  A cancel carries no text, so when there is no
        // session to route to there is nothing to show and we simply stop.
        let Some(session_id) = self.resolve_daemon_session(session_id) else {
            tracing::debug!(%stream_id, "dropping cancel: no attached session to route it to");
            return;
        };
        // A cancel is a terminal turn outcome `SessionStatus` cannot express,
        // but it is NOT a failure: report `idle` rather than `error` so the
        // cancelled turn clears its working state without a red error block.
        self.term_status_override.insert(session_id, "idle");
        self.term_status_dirty = true;
        // Deliberately NO error text (neither `self.error` nor `display.error`)
        // — a user cancel must not render as a failure.
        self.finish_request(session_id, stream_id);
    }

    fn handle_tool_call_event(&mut self, session_id: u64, stream_id: u64, event: ToolCallEvent) {
        let display = self.display_for(session_id);
        match event {
            ToolCallEvent::Started {
                call_id,
                tool_name,
                arguments_json,
                invocation_description,
            } => {
                // Look up the turn before mutating so the version bump below
                // can target the right turn (the start event may backfill the
                // stub's name/description — both visible in the rendered
                // header).
                let turn_id = display.view.request_to_turn.get(&stream_id).copied();
                display.view.tool_call_started(
                    stream_id,
                    call_id,
                    tool_name,
                    arguments_json,
                    &invocation_description,
                );
                if let Some(turn_id) = turn_id {
                    display.bump_turn_version(turn_id);
                }
                display.resolve_streaming_turn_index(stream_id);
                display.mark_streaming_changed();
            }
            ToolCallEvent::Finished { .. } => {}
            ToolCallEvent::Failed { .. } => {}
        }
    }

    fn handle_tool_result_chunk(
        &mut self,
        session_id: u64,
        stream_id: u64,
        call_id: String,
        data: Vec<u8>,
    ) {
        let text = String::from_utf8_lossy_owned(data);
        let display = self.display_for(session_id);
        // The chunk appends to `turn.tool_results[i].content` (rendered
        // live); bump the turn's content version so a rebuild between chunks
        // recomputes instead of reusing the pre-chunk cached lines — the
        // core fix for "scrollbar moves but results stay stuck".
        let turn_id = display.view.request_to_turn.get(&stream_id).copied();
        display.view.tool_result_chunk(stream_id, &call_id, &text);
        if let Some(turn_id) = turn_id {
            display.bump_turn_version(turn_id);
        }
        display.resolve_streaming_turn_index(stream_id);
        display.mark_streaming_changed();
    }

    fn handle_session_state(&mut self, state: SessionStateData) {
        tracing::debug!(
            turn_count = %state.turns.len(),
            ?state.selected_model,
            ?state.status,
            "handle_session_state"
        );
        let session_id = state.session_id;
        // SessionState snapshots are per-session: the daemon sends one for
        // the attached session on attach, but also broadcasts them for
        // background sessions (e.g. load_tools/unload_tools on that session
        // reach activity subscribers like the TUI).  Route the snapshot to
        // the session it belongs to, and only let the *attached* session's
        // snapshot drive the view switch and the status-bar fields below —
        // otherwise a background session's token usage / status / turns
        // would clobber the display the user is currently looking at.
        let is_attached = self.attached_session_id == Some(session_id);
        if is_attached {
            self.active_session_id = Some(session_id);
        }

        let SessionStateData {
            turns,
            title: _,
            selected_model,
            active_tool_groups,
            token_usage,
            context_window,
            last_prompt_tokens,
            status,
            reasoning_effort,
            reasoning_capability,
            ..
        } = state;

        // Merge the daemon snapshot with turns already accumulated locally
        // via the all-activity subscription (while the user was viewing
        // another session).  The snapshot is authoritative for finished
        // turns, but for an in-flight turn it only holds the empty
        // placeholder inserted by `start_turn` — the worker owns the live
        // content and only syncs back on RequestFinished.  The accumulated
        // turn carries the real streamed content, so it must win; otherwise
        // switching into a streaming session would blank the turn until the
        // next chunk arrived.
        let accumulated = {
            let display = self.display_for(session_id);
            std::mem::take(&mut display.view.turns)
        };
        let mut merged = turns;
        for (turn_id, acc_turn) in &accumulated {
            match merged.get_mut(turn_id) {
                Some(snap_turn) if turn_has_live_content(acc_turn, snap_turn) => {
                    *snap_turn = acc_turn.clone();
                }
                // Turn only known to the client (e.g. a turn created just
                // before this snapshot) — keep the accumulated version.
                None => {
                    merged.insert(*turn_id, acc_turn.clone());
                }
                // Snapshot is at least as complete — keep it.
                Some(_) => {}
            }
        }

        // Sync images before getting display to avoid borrow conflict.
        self.rendered_images.remove(&session_id);
        for (tid, turn) in &merged {
            self.sync_turn_images(session_id, *tid, turn);
        }
        let display = self.display_for(session_id);
        display.view.turns = merged;
        // Content versions must never outlive the turns they fingerprint:
        // drop entries whose turn left the view.  The snapshot merge is a
        // union today (accumulated turns are re-inserted below), so this is
        // defensive — it pins the invariant against any future path that
        // removes turns (undo keeps turns, only marking them undone).
        let live_turn_ids: Vec<u32> = display.view.turns.keys().copied().collect();
        display
            .turn_versions
            .retain(|turn_id, _| live_turn_ids.contains(turn_id));
        // The merge can silently replace a turn's content (the snapshot wins
        // when it is at least as complete, or the accumulated version wins
        // for the in-flight turn) — either way the cached rendering, built
        // from the pre-merge content, may now be stale.  Bump every turn the
        // client already knew about so the next rebuild recomputes rather
        // than reusing those lines.  Turns only present in the snapshot have
        // no cache entry, so they need no bump.
        for turn_id in accumulated.keys() {
            display.bump_turn_version(*turn_id);
        }
        display.selected_model = selected_model;
        // Merge, never overwrite: the attach snapshot can lag the fresher
        // total accumulated via the all-activity subscription for a mid-turn
        // session (see [`merge_token_usage`]), so a blind assignment would
        // regress the status bar's token readout until the next update.
        display.token_usage = merge_token_usage(&display.token_usage, &token_usage);
        if let Some(cw) = context_window {
            display.context_window = Some(cw);
        }
        // Gap-fill, never overwrite: the snapshot's last_prompt_tokens can
        // lag the value already broadcast to this client via the
        // all-activity subscription (the same cross-channel race as
        // token_usage), and unlike cumulative usage it is not monotonic, so
        // a max-merge is wrong.  Never regress a fresher value; the next
        // TokenUsageUpdate / Done refreshes it anyway.
        if display.last_prompt_tokens.is_none()
            && let Some(tokens) = last_prompt_tokens
        {
            display.last_prompt_tokens = Some(tokens);
        }
        if let Some(effort) = reasoning_effort {
            display.reasoning_effort = Some(effort);
        }
        if let Some(cap) = reasoning_capability {
            display.reasoning_capability = Some(cap);
        }
        display.mark_content_changed();
        let _ = display;
        // Only the attached session's snapshot may update the status bar's
        // per-attachment state — a background session's snapshot must not
        // overwrite the status/tool-group display while the user is viewing
        // the attached session.
        if is_attached {
            self.attached_status = Some(status);
            self.attached_tool_groups = active_tool_groups;
        }
    }

    fn handle_token_usage_update(
        &mut self,
        session_id: u64,
        token_usage: TokenUsage,
        last_prompt_tokens: Option<u32>,
    ) {
        tracing::trace!(
            ?token_usage,
            ?last_prompt_tokens,
            "handle_token_usage_update"
        );
        let display = self.display_for(session_id);
        display.token_usage = Some(token_usage);
        if let Some(tokens) = last_prompt_tokens {
            display.last_prompt_tokens = Some(tokens);
        }
        display.live_input_estimate = 0;
        display.live_output_tokens = 0;
    }

    fn handle_status_text(&mut self, text: String) {
        self.status = Some(text);
    }

    fn handle_error(&mut self, error: String) {
        self.error = Some(error);
    }

    fn handle_session_attached(&mut self, session_id: u64) {
        self.active_session_id = Some(session_id);
        self.attached_session_id = Some(session_id);
        self.term_status_dirty = true;
    }

    fn handle_session_created(
        &mut self,
        _session_id: u64,
        _title: Option<String>,
        _working_dir: Option<String>,
        _account_name: Option<String>,
        _selected_model: Option<String>,
        _reasoning_effort: Option<String>,
    ) {
    }

    fn handle_session_status_changed(
        &mut self,
        session_id: u64,
        status: SessionStatus,
        last_modified: i64,
    ) {
        self.handle_session_status_changed(session_id, &status, last_modified);
    }
}

#[cfg(test)]
mod tests;
