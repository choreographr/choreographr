//! The `App` struct itself and its accessors: construction, the status/error and
//! quit-message lines, the attached-session display lookup, the provider list,
//! and the client-side idle guards.
//!
//! This module owns the struct definition (every field, with the ingest and
//! ownership notes the fields need) and the methods that are about the app and
//! its per-session display plumbing.  Every other `impl App` block in the crate
//! lives beside the subject it serves — the history pane's scroll and viewport
//! in `state/history`, the input box geometry in `state/layout/chat`, the turn
//! event handlers in `state/turn`, the page states in `state/pages`, and so on —
//! so a reader of one subject never has to scroll past another's methods.
//!
//! The status/error/quit fields are private and reachable only through their
//! accessors below: ingest is the choke point where display-only text is made
//! inert (see [`normalized_display_text`](super::normalized_display_text)), so no
//! call site can store raw bytes
//! in a field whose reader is a widget — or, for the quit message, a plain
//! `println!`.
//!
//! Re-exported from `state` (`pub(crate) use app::*`), so `crate::state::App`
//! and the accessors keep resolving crate-wide.

use super::{
    AIProvidersState, CommandPaletteState, DisplayLine, HistoryViewport, ImageSlot, InputBuffer,
    LoginModal, ModelSelectorState, PROVIDER_OPTIONS, Page, ProviderInfo, ReasoningDialogState,
    SessionDisplayState, SessionManagerState, SessionNote, SetupFlow, SpawnedChildNav,
    TerminalNotice, alt_chord_label, host_report, sort_providers,
};
use crate::RenderedImage;
use crate::image_worker::{ImageId, ImageJob};
use crate::selection::TextSelection;
use crate::terminal;
use choreo_client_core::PendingReplies;
use choreo_proto::{SessionStatus, socket_path};
use std::collections::HashMap;

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
    /// the user sees why the TUI left.  No message is set on a normal user
    /// quit, so nothing prints.
    ///
    /// Held in [`DisplayLine`] rather than an `Option<String>` because this is
    /// the one message the TUI prints *outside* ratatui: `run_app` writes it
    /// with `println!` after the alternate screen is gone, so there is no cell
    /// buffer and no widget sink between the stored bytes and the terminal.  A
    /// [`DisplayLine`] makes the text inert where it is stored — the same
    /// ingest rule as `status`/`error` — so the print site needs no second
    /// normalizing pass and a future daemon-supplied reason cannot reach a
    /// normal-screen terminal as a live escape or OSC sequence.
    ///
    /// Private so every writer goes through [`App::set_quit_message`] (or the
    /// first-writer-wins [`App::set_quit_message_if_absent`]), which applies the
    /// same ingest rule as `status`/`error`: no call site can store raw bytes in
    /// the one field whose reader is a plain `println!`.
    quit_message: DisplayLine,
    pub(crate) image_job_tx: Option<crossbeam_channel::Sender<ImageJob>>,
    pub(crate) attached_session_id: Option<u64>,
    /// The session whose `AttachSession` is on the wire, if any — the only
    /// reliable way to attribute a failure back to the session that caused it.
    ///
    /// `SessionEvent::SessionFailed` carries no session id, and the two attach
    /// paths differ: [`App::attach_to_session`] rebinds
    /// [`Self::attached_session_id`] optimistically before the reply, while
    /// [`App::dispatch_pending_attach`] (the one-shot `--attach <SESSION_ID>`)
    /// deliberately waits for the daemon's `SessionAttached`. Recording the
    /// requested id here lets a failed attach be marked on the session it was
    /// about, instead of on whatever happens to be attached when the failure
    /// lands. Cleared by the confirmed attach it was waiting for.
    pub(crate) pending_attach_id: Option<u64>,
    /// A `--attach <SESSION_ID>` request named at startup that has not been
    /// dispatched yet. Set once from the CLI in `run_app` and taken exactly
    /// once by [`App::dispatch_pending_attach`], which runs right after the
    /// initial `Sessions` snapshot is applied — so the requested attach is
    /// never raced or clobbered by `handle_sessions`' auto-attach. `None` both
    /// before that take and when no flag was given.
    pub(crate) pending_attach: Option<u64>,
    /// Account slug shown in the status bar — the attached session's account
    /// name (the account name is its slug). Replaces the inference provider
    /// slug that used to be shown here.
    pub(crate) attached_account_slug: Option<String>,
    pub(crate) attached_status: Option<SessionStatus>,
    pub(crate) attached_tool_groups: Vec<String>,
    /// In-flight `spawn_subsession` tool calls, keyed by the session running
    /// them. The broadcast `SessionCreated` path auto-enters a spawned child
    /// only when exactly one such call is in flight for the attached session
    /// (see [`App::spawned_child_should_navigate`]). Maintained by
    /// `state/turn.rs`'s tool-call handler.
    pub(crate) spawn_subsession_inflight: HashMap<u64, usize>,
    /// The spawned-child round trip in progress, if any: the TUI auto-entered a
    /// child because the attached session ran a single `spawn_subsession` call,
    /// and switches back to `parent_id` when the child finishes (see
    /// [`SpawnedChildNav`]).
    pub(crate) spawned_child_nav: Option<SpawnedChildNav>,
    /// OSC 7501 program-status publisher: the child records currently emitted
    /// to the terminal, cached so an unchanged record is not rewritten. Every
    /// report is a CHILD record keyed by session id; the OSC 9;4 root record
    /// belongs to the progress family (see `terminal::status`).
    pub(crate) term_status: terminal::status::Publisher,
    /// Set when something changed that affects the published program-status
    /// records or the window title; the UI loop recomputes and publishes them
    /// outside the render closure.
    pub(crate) term_status_dirty: bool,
    /// Queue into the hosting workspace manager's reporter thread, when one is
    /// running (see [`host_report`]). `None` whenever this process is not
    /// inside a workspace-manager pane, so the whole feature is inert.
    pub(crate) host_report_sink: Option<host_report::HostReportSink>,
    /// Terminal turn outcomes the session status cannot express: `done` after
    /// a completed turn, `error` after a failed one, `idle` after a
    /// cancellation. Keyed by session id; persists until a new turn begins (a
    /// fresh active `SessionStatusChanged`) so the outcome survives the
    /// trailing idle status of the finished turn.
    pub(crate) term_status_override: HashMap<u64, &'static str>,
    /// The last window title (OSC 2) emitted, so an unchanged title is not
    /// re-sent every frame.
    pub(crate) term_title: Option<String>,
    /// The last pointer shape (OSC 22) emitted, so an unchanged shape is not
    /// re-sent on every mouse-motion event within the same target. `None` means
    /// nothing has been emitted yet (or the cache was forgotten on suspend), so
    /// the next computation re-emits.
    pub(crate) term_pointer: Option<crate::terminal::pointer::PointerShape>,
    /// The pointer's last reported position in terminal coordinates, recorded
    /// from every mouse event (motion, drag, click, wheel) — the event kinds
    /// that carry a cell and the shapes a terminal actually reports it with. The
    /// pointer shape is *derived* from it against the current hit-test state
    /// (see `state/pointer.rs`), so a hover target that disappears still resets
    /// the shape without any further input.
    pub(crate) pointer_hover: Option<(u16, u16)>,
    /// Set when the input path shows it has degraded, so the UI loop re-asserts
    /// the terminal's mouse-tracking mode outside `terminal.draw`.
    ///
    /// The mode is a set of DEC private modes enabled once at startup; a surface
    /// whose any-event tracking (`?1003`) lapses keeps delivering clicks, drags
    /// and wheel notches but never motion, which silently freezes the pointer
    /// shape for the life of the session (the shape is emitted only on change).
    /// Re-sending the mode set is idempotent and is the only recovery the client
    /// can drive itself, so it is requested from the input path (where the
    /// degradation is observed) and written by the loop (which owns terminal
    /// output).
    pub(crate) mouse_tracking_reassert: bool,
    /// The terminal this process launched in, identified once beside the image
    /// picker (`run_app`) and never re-derived: `TERM`-style variables can change
    /// under a shell inside the TUI, and a mid-session re-read would describe a
    /// terminal the user is no longer looking at. Defaults to the unidentified
    /// profile, so an `App` built by a test (which never runs `run_app`) still
    /// holds a valid value.
    pub(crate) terminal: crate::terminal::profile::TerminalProfile,
    /// The welcome panel's terminal advice, derived once at launch from
    /// [`Self::terminal`] beside it (see `state::terminal_notice::terminal_notice`).
    /// Stored rather than re-derived per frame because the profile it reads is
    /// immutable for the process lifetime; `None` means this terminal and its
    /// host cost the client nothing, so the panel stays silent.
    pub(crate) terminal_notice: Option<TerminalNotice>,
    /// Whether the read-only Terminal page is open over the Chat page. Toggled
    /// by `Alt+T` / `/terminal`; the page is a view over [`Self::terminal`], so
    /// nothing is copied or derived into this flag beyond `true`/`false`.
    pub(crate) show_terminal_page: bool,
    /// Scroll offset (in rows) of the Terminal page's body. The page can be
    /// taller than a short terminal's popup, so the overlay guard's
    /// `Up`/`Down`/`PgUp`/`PgDn` drive this and the renderer clamps it to the
    /// content (the same contract as the Help modal's scroll). Reset to 0 when
    /// the page opens so it always starts at the top.
    pub(crate) terminal_page_scroll: u16,
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
    /// The Help modal (`Alt+H`, or `/help`): a read-only overlay listing the
    /// keyboard shortcuts. A single flag, like `show_terminal_page`, toggled
    /// from the Chat page's shortcut table and closed by `Esc` or the same
    /// chord in the `handle_terminal_event` overlay guard. Defaults closed — a
    /// modal covering the transcript on every session attach would be noise.
    pub(crate) show_help_modal: bool,
    /// Scroll offset (in rows) of the Help modal's body. The page is taller than
    /// most terminals, so the overlay guard's `Up`/`Down`/`PgUp`/`PgDn` drive
    /// this and the renderer clamps it to the content. Reset to 0 when the modal
    /// opens so it always starts at the top.
    pub(crate) help_modal_scroll: u16,
    pub(crate) session_mgr: SessionManagerState,
    pub(crate) ai_providers: AIProvidersState,
    pub(crate) model_selector: ModelSelectorState,
    /// Transient selection state for the inline command palette (Chat page).
    /// The palette's highlight and scroll window; its visible/hidden condition
    /// is whether the input buffer is a command line (see
    /// `command_palette_active`) — there is no separate mode flag to keep in
    /// sync with the buffer.
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
    /// The transient status line drawn above the command box (green).
    ///
    /// Private so every writer goes through [`App::set_status`], which
    /// normalizes the text for display at ingest; see
    /// [`normalized_display_text`](super::normalized_display_text) for why that is
    /// the choke point.
    status: DisplayLine,
    /// Whether the current `status` came from a connection-task
    /// [`UiEvent::Status`](super::UiEvent::Status) (a transient progress message
    /// like "daemon started") rather than from daemon traffic. A transient
    /// status is CLEARED the moment the first real daemon message arrives, so
    /// it cannot linger on the status line once the connection is live and
    /// the first turn happens to be quiet. Daemon-derived statuses never set
    /// this flag, so they are never cleared by the arrive-of-traffic rule.
    pub(crate) status_is_transient: bool,
    /// The error line drawn above the command box (red), and the only surface
    /// for connection-level failures that have no turn to render into.
    ///
    /// Private so every writer goes through [`App::set_error`], which
    /// normalizes the text for display at ingest; see
    /// [`normalized_display_text`](super::normalized_display_text).
    error: DisplayLine,
    pub(crate) session_displays: HashMap<u64, SessionDisplayState>,
    pub(crate) active_session_id: Option<u64>,
    /// The address string this session talks to (dial addr for TCP, unix
    /// socket path otherwise). It keys the per-daemon unlock key in
    /// `known_servers`, so every `Unlock`/`AddCredential`/`record` must use it
    /// consistently. Set by `run_app` from the connection mode; defaults to
    /// the socket path so tests (which bypass `run_app`) still have a valid
    /// value.
    pub(crate) connection_addr: String,
    /// The staged account/model/reasoning choice in flight, if any. Nothing is
    /// committed to the session until its last step, so an abandoned flow
    /// leaves the session untouched (see [`SetupFlow`]). `None` outside it.
    pub(crate) setup_flow: Option<SetupFlow>,
    /// The reasoning-effort dialog the staged flow opens once a model is chosen
    /// (see [`ReasoningDialogState`]). Closed whenever no flow is staging an
    /// effort.
    pub(crate) reasoning_dialog: ReasoningDialogState,
    /// The active OAuth login modal, when a `/login` flow is running. Holds the
    /// worker session; dropped (which cancels) when the flow ends or the user
    /// cancels.
    pub(crate) login: Option<LoginModal>,
    /// The OAuth sign-in parameters served by the daemon, cached from the last
    /// `ListOAuthProviders` reply, keyed by provider slug in `provider`.
    pub(crate) oauth_providers: Vec<choreo_proto::OAuthProviderDescriptor>,
    /// Whether the descriptor list has been fetched once, so `/login` does not
    /// re-request it on every invocation.
    pub(crate) oauth_providers_fetched: bool,
    /// A `/login` awaiting the descriptor reply: `(account, provider, device)`.
    /// The account is resolved to its provider when the command arrives, so the
    /// reply need only start the flow.
    pub(crate) pending_login: Option<(String, String, bool)>,
    /// A `/mcp login <slug>` awaiting the fresh status reply that carries the
    /// server's `oauth` block, resource URL, and credential key.
    pub(crate) pending_mcp_login: Option<String>,
    /// A `/mcp logout <slug>` awaiting the fresh status reply that carries the
    /// server's credential key, so the right credential is removed.
    pub(crate) pending_mcp_logout: Option<String>,
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
            quit_message: DisplayLine::none(),
            image_job_tx: None,
            pending_job_idx: HashMap::new(),
            attached_session_id: None,
            pending_attach_id: None,
            pending_attach: None,
            attached_account_slug: None,
            attached_status: None,
            attached_tool_groups: Vec::new(),
            spawn_subsession_inflight: HashMap::new(),
            spawned_child_nav: None,
            term_status: terminal::status::Publisher::new(),
            term_status_dirty: false,
            host_report_sink: None,
            term_status_override: HashMap::new(),
            term_title: None,
            term_pointer: None,
            pointer_hover: None,
            mouse_tracking_reassert: false,
            terminal: crate::terminal::profile::TerminalProfile::default(),
            terminal_notice: None,
            show_terminal_page: false,
            terminal_page_scroll: 0,
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
            show_help_modal: false,
            help_modal_scroll: 0,
            session_mgr: SessionManagerState::new(),
            ai_providers: AIProvidersState::new(),
            model_selector: ModelSelectorState::new(),
            command_palette: CommandPaletteState::new(),
            // Start from the static default; the daemon's CatalogUpdated
            // broadcast replaces it with the live list.  The picker has a
            // fixed order (featured, then alphabetical), so sort the default
            // here too (see `sort_providers`
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
            status: DisplayLine::none(),
            status_is_transient: false,
            error: DisplayLine::none(),
            last_terminal_size: None,
            terminal_resized: false,
            session_displays: HashMap::new(),
            active_session_id: None,
            connection_addr: socket_path(),
            setup_flow: None,
            reasoning_dialog: ReasoningDialogState::new(),
            login: None,
            oauth_providers: Vec::new(),
            oauth_providers_fetched: false,
            pending_login: None,
            pending_mcp_login: None,
            pending_mcp_logout: None,
        }
    }

    /// Replace the status line, normalizing the text for display as it is
    /// stored.
    ///
    /// Ingest is the choke point: the field is private, so every daemon-authored
    /// message (a progress line, a list dump, a `[error] …` note) reaches the
    /// state through here and is made inert exactly once.  Every later reader —
    /// the status/error bar, the height measurement, a futures consumer with no
    /// ratatui sink — then sees display-safe bytes, and the widget sink's own
    /// pass over them borrows instead of rewriting.
    pub(crate) fn set_status(&mut self, text: impl Into<String>) {
        self.status.set(text);
    }

    /// Clear the status line (see [`Self::set_status`] for the ingest rule).
    pub(crate) fn clear_status(&mut self) {
        self.status.clear();
    }

    /// The status line to draw, already normalized (see [`Self::set_status`]).
    pub(crate) fn status(&self) -> Option<&str> {
        self.status.get()
    }

    /// Replace the error line, normalizing the text for display as it is stored
    /// (see [`Self::set_status`]: the same ingest rule and the same reasoning).
    pub(crate) fn set_error(&mut self, text: impl Into<String>) {
        self.error.set(text);
    }

    /// Clear the error line, restoring the status line (if any) as what draws.
    pub(crate) fn clear_error(&mut self) {
        self.error.clear();
    }

    /// The error line to draw, already normalized (see [`Self::set_status`]).
    pub(crate) fn error(&self) -> Option<&str> {
        self.error.get()
    }

    /// Replace the quit message — the reason printed on the restored terminal
    /// after the TUI tears down (see the field docs) — normalizing the text for
    /// display as it is stored (see [`Self::set_status`] for the ingest rule).
    pub(crate) fn set_quit_message(&mut self, text: impl Into<String>) {
        self.quit_message.set(text);
    }

    /// Replace the quit message only when none is stored yet, normalizing it as
    /// it is stored (see [`Self::set_quit_message`]).
    ///
    /// A reason already stored is strictly more specific than the generic
    /// fallback a later layer would install — a shutdown notice or an eviction
    /// cause beats "the connection was lost" — so the first writer wins.
    pub(crate) fn set_quit_message_if_absent(&mut self, text: impl Into<String>) {
        self.quit_message.set_if_absent(text);
    }

    /// The quit message to print, already normalized, or `None` on a normal
    /// user-initiated quit (see [`Self::set_quit_message`]).
    pub(crate) fn quit_message(&self) -> Option<&str> {
        self.quit_message.get()
    }

    /// The platform every user-facing chord is spelled for: this client's
    /// **detected** one (the profile's), not the compile target
    /// `Platform::current` reports.
    ///
    /// The two differ on exactly one axis that the user can see: a Termux
    /// userland reports Android, so the profile is the compile target refined by
    /// what this client actually launched in.  Reading the spelling source from
    /// the profile means a chord, a hint and the model-selector name all agree
    /// with the platform the Terminal page prints, and leaves one runtime source
    /// for every spelling ([`alt_chord_label`]).
    pub(crate) fn platform(&self) -> crate::terminal::profile::Platform {
        self.terminal.platform
    }

    /// The chord that opens the model selector, spelled for this platform —
    /// `Alt+M` on a keyboard that carries an Alt key, `⌥M` on a Mac, where the
    /// key is Option.  Used by hint and status strings, so it delegates to
    /// [`alt_chord_label`] rather than carrying a spelling of its own.
    pub(crate) fn model_selector_label(&self) -> String {
        alt_chord_label(self.platform(), "M")
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
        // The wizard's picker has a fixed order (featured providers, then
        // alphabetical); the daemon sends
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
    ///
    /// See [`Self::attached_is_busy`] for the idle signal every client-side
    /// guard shares.
    pub(crate) fn new_turn_rejection(&self) -> Option<&'static str> {
        // Idle guard: a new turn can only begin from an idle (`Inactive`)
        // session.  See `SessionStatus::is_idle` for why `Sleeping` counts as
        // busy too.  `None` (no status known yet) fails open.
        if self.attached_is_busy() {
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

    /// The client-side note for the ATTACHED session, if any.
    ///
    /// `Error` wins over `Loading`: a failed attach leaves the transcript
    /// unknown *and* explains why, and naming the failure is the more
    /// actionable of the two.  It is read by the status bar's status slot and
    /// by the idle guards; the welcome-panel decision reads
    /// `history_loaded` directly (see `show_welcome`).
    pub(crate) fn session_note(&self) -> Option<SessionNote> {
        let session_id = self.attached_session_id?;
        let display = self.session_displays.get(&session_id)?;
        if display.attach_failed {
            Some(SessionNote::Error)
        } else if !display.history_loaded {
            Some(SessionNote::Loading)
        } else {
            None
        }
    }

    /// Whether the attached session is known to be busy (not idle).  `None`
    /// (no status heard yet) fails open, so a fresh client still reaches the
    /// daemon — the daemon is the authority; this is only a local pre-empt.
    ///
    /// `Sleeping` counts as busy too (see `SessionStatus::is_idle`): its thread
    /// has terminated, so it cannot accept a new turn or a history mutation.
    ///
    /// A session in the client-side [`SessionNote::Error`] state is NOT busy,
    /// whatever the daemon last said about it.  A failed attach left no
    /// running work behind, and the failure may have landed while the summary's
    /// last known status was `Sleeping` — which `SessionStatus::is_idle` counts
    /// as busy.  Without this the idle guard would refuse every prompt with
    /// "Session is not idle, please wait before prompting." forever, because
    /// nothing in that state can ever clear the status.  `Loading` is
    /// deliberately *not* treated as idle: a loading session is genuinely not
    /// ready, and "please wait" is the right answer for the one frame it lasts.
    fn attached_is_busy(&self) -> bool {
        if matches!(self.session_note(), Some(SessionNote::Error)) {
            return false;
        }
        self.attached_status
            .as_ref()
            .is_some_and(|status| !status.is_idle())
    }

    /// Client-side submit-time guard for actions that need an idle session but
    /// do NOT begin an inference turn — undo and redo.  Unlike
    /// [`Self::new_turn_rejection`], the keystore-lock guard does not apply
    /// (undo/redo need no credentials), so only the idle check runs.  Returns
    /// `Some(message)` with the status line to show — and the caller must NOT
    /// send — when the session is busy, or `None` when it may proceed.
    pub(crate) fn idle_rejection(&self) -> Option<&'static str> {
        if self.attached_is_busy() {
            return Some("Session is not idle, please wait before undoing or redoing.");
        }
        None
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
