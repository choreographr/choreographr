use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Rect, Size};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tai_client_core::{
    ClientError, ClientHistory, DaemonMessageHandler, HistoryItem as SharedHistoryItem,
    MAX_HISTORY_ITEMS, broken_pipe,
};
use tai_proto::{
    AccountInfo, ClientMessage, ImageMetadata, OutputStream, SessionMessage, SessionStatus,
    SessionSummary, ThinkingEffort, TokenUsage,
};
use tai_tui::image_worker::{ImageId, ImageJob, ImageResult, next_job_id};
use tai_tui::{RenderedImage, StreamingText};
use unicode_segmentation::UnicodeSegmentation;

use crate::db::{self, CommandEntry};
use crate::diff_render::{diff_display_height, is_diff_text, parse_diff};
use crate::markdown_render::{lines_height, session_message_lines, streaming_text_lines};
use ratatui::text::Line;
use tai_client_core::FileDiff;
use tui_prompts::{SelectState, State, TextState};

/// If the text looks like a unified diff and can be parsed successfully,
/// return the structured diffs. Otherwise return `None`.
fn try_parse_as_diff(text: &str) -> Option<Vec<FileDiff>> {
    if is_diff_text(text) {
        let diffs = parse_diff(text);
        if !diffs.is_empty() {
            return Some(diffs);
        }
    }
    None
}

pub(crate) const INPUT_BAR_HEIGHT: u16 = 3;
pub(crate) const STATUS_BAR_HEIGHT: u16 = 2;
pub(crate) const PAGE_SCROLL_LINES: usize = 3;

/// A menu item on the Home page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HomeMenuItem {
    Sessions,
    AIProviders,
    Settings,
    Exit,
}

impl HomeMenuItem {
    pub(crate) fn label(&self) -> &'static str {
        match self {
            HomeMenuItem::Sessions => "Sessions",
            HomeMenuItem::AIProviders => "AI Provider Accounts",
            HomeMenuItem::Settings => "Settings",
            HomeMenuItem::Exit => "Exit",
        }
    }

    pub(crate) fn key_hint(&self) -> &'static str {
        match self {
            HomeMenuItem::Sessions => "(s)",
            HomeMenuItem::AIProviders => "(p)",
            HomeMenuItem::Settings => "(t)",
            HomeMenuItem::Exit => "(q)",
        }
    }
}

pub(crate) const HOME_MENU_ITEMS: &[HomeMenuItem] = &[
    HomeMenuItem::Sessions,
    HomeMenuItem::AIProviders,
    HomeMenuItem::Settings,
    HomeMenuItem::Exit,
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Page {
    Chat,
    SessionManager,
    AIProviders,
    Settings,
    Home,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SessionManagerView {
    List,
    Detail,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AIProvidersView {
    List,
    NewForm,
}

/// A provider entry in the new-account form dropdown.
///
/// Note: This list must be kept in sync with `tai-daemon/src/providers/catalog.rs`.
/// There is no compile-time cross-crate sharing, so the daemon catalog is the
/// source of truth and this list should be updated whenever providers are added
/// or removed there.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ProviderInfo {
    pub(crate) slug: &'static str,
    pub(crate) display_name: &'static str,
}

pub(crate) const PROVIDER_OPTIONS: &[ProviderInfo] = &[
    // Core providers first
    ProviderInfo {
        slug: "openai",
        display_name: "OpenAI",
    },
    ProviderInfo {
        slug: "anthropic",
        display_name: "Anthropic",
    },
    ProviderInfo {
        slug: "google",
        display_name: "Google Gemini",
    },
    // OpenAI-compatible, roughly alphabetical
    ProviderInfo {
        slug: "cerebras",
        display_name: "Cerebras",
    },
    ProviderInfo {
        slug: "custom-openai",
        display_name: "Custom OpenAI-Compatible",
    },
    ProviderInfo {
        slug: "dashscope",
        display_name: "DashScope (Alibaba)",
    },
    ProviderInfo {
        slug: "deepseek",
        display_name: "DeepSeek",
    },
    ProviderInfo {
        slug: "fireworks",
        display_name: "Fireworks AI",
    },
    ProviderInfo {
        slug: "github",
        display_name: "GitHub Models",
    },
    ProviderInfo {
        slug: "groq",
        display_name: "Groq",
    },
    ProviderInfo {
        slug: "huggingface",
        display_name: "Hugging Face",
    },
    ProviderInfo {
        slug: "lmstudio",
        display_name: "LM Studio",
    },
    ProviderInfo {
        slug: "mistral",
        display_name: "Mistral",
    },
    ProviderInfo {
        slug: "moonshot",
        display_name: "Moonshot AI (Kimi)",
    },
    ProviderInfo {
        slug: "novita",
        display_name: "Novita AI",
    },
    ProviderInfo {
        slug: "nvidia",
        display_name: "NVIDIA NIM",
    },
    ProviderInfo {
        slug: "ollama",
        display_name: "Ollama (Local)",
    },
    ProviderInfo {
        slug: "ollama-cloud",
        display_name: "Ollama Cloud",
    },
    ProviderInfo {
        slug: "opencode",
        display_name: "OpenCode Zen",
    },
    ProviderInfo {
        slug: "opencode-go",
        display_name: "OpenCode Go",
    },
    ProviderInfo {
        slug: "openai_compatible",
        display_name: "OpenAI Compatible",
    },
    ProviderInfo {
        slug: "openrouter",
        display_name: "OpenRouter",
    },
    ProviderInfo {
        slug: "perplexity",
        display_name: "Perplexity",
    },
    ProviderInfo {
        slug: "together",
        display_name: "Together AI",
    },
    ProviderInfo {
        slug: "venice",
        display_name: "Venice AI",
    },
    ProviderInfo {
        slug: "xiaomi-mimo",
        display_name: "Xiaomi MiMo",
    },
    ProviderInfo {
        slug: "xai",
        display_name: "xAI Grok",
    },
    ProviderInfo {
        slug: "zai",
        display_name: "Z.ai (GLM)",
    },
    // Anthropic-compatible
    ProviderInfo {
        slug: "minimax",
        display_name: "MiniMax",
    },
    ProviderInfo {
        slug: "custom-anthropic",
        display_name: "Custom Anthropic-Compatible",
    },
];

/// State for the AI Provider Accounts page.
pub(crate) struct AIProvidersState {
    pub(crate) accounts: Vec<AccountInfo>,
    pub(crate) view: AIProvidersView,
    pub(crate) selection: Option<usize>,
    pub(crate) scroll: usize,
    pub(crate) confirm_remove: Option<String>,
    /// State for the account name text prompt.
    pub(crate) new_name_state: TextState<'static>,
    /// State for the provider select prompt.
    pub(crate) new_provider_state: SelectState,
    /// State for the API key password prompt.
    pub(crate) new_api_key_state: TextState<'static>,
    /// When set, the user is typing a credential (API key) for this account name.
    pub(crate) credential_target: Option<String>,
    /// Input buffer for typing a credential value.
    pub(crate) credential_input: InputBuffer,
    /// Whether the last add operation failed — stored here so the renderer
    /// can show it without interfering with the history.
    pub(crate) add_error: Option<String>,
}

impl AIProvidersState {
    pub(crate) fn new() -> Self {
        Self {
            accounts: Vec::new(),
            view: AIProvidersView::List,
            selection: None,
            scroll: 0,
            confirm_remove: None,
            new_name_state: TextState::default(),
            new_provider_state: SelectState::default(),
            new_api_key_state: TextState::default(),
            credential_target: None,
            credential_input: InputBuffer::new(),
            add_error: None,
        }
    }

    pub(crate) fn set_accounts(&mut self, accounts: Vec<AccountInfo>) {
        let was_selected = self
            .selection
            .and_then(|i| self.accounts.get(i))
            .map(|a| a.name.clone());
        self.accounts = accounts;
        self.selection = if self.accounts.is_empty() {
            None
        } else {
            let idx = was_selected
                .and_then(|name| self.accounts.iter().position(|a| a.name == name))
                .unwrap_or(0);
            Some(idx)
        };
        self.scroll = 0;
    }

    pub(crate) fn select_up(&mut self) {
        let sel = self.selection.unwrap_or(0);
        if sel > 0 {
            self.selection = Some(sel - 1);
            if sel - 1 < self.scroll {
                self.scroll = self.scroll.saturating_sub(1);
            }
        }
    }

    pub(crate) fn select_down(&mut self) {
        let max = self.accounts.len().saturating_sub(1);
        let sel = self.selection.unwrap_or(0);
        if sel < max {
            self.selection = Some(sel + 1);
        }
    }

    pub(crate) fn remove_account(&mut self, name: &str) {
        let old_len = self.accounts.len();
        self.accounts.retain(|a| a.name != name);
        if self.accounts.len() == old_len {
            return;
        }
        // Adjust selection
        if let Some(sel) = self.selection
            && sel >= self.accounts.len()
        {
            self.selection = if self.accounts.is_empty() {
                None
            } else {
                Some(self.accounts.len().saturating_sub(1))
            };
        }
        // Clamp scroll
        let max_scroll = self.accounts.len().saturating_sub(1);
        self.scroll = self.scroll.min(max_scroll);
        // Clear confirmation
        if self.confirm_remove.as_deref() == Some(name) {
            self.confirm_remove = None;
        }
    }

    /// Enter the new-account form and reset all form fields.
    pub(crate) fn enter_new_form(&mut self) {
        self.view = AIProvidersView::NewForm;
        self.new_name_state = TextState::default();
        self.new_provider_state = SelectState::default();
        self.new_api_key_state = TextState::default();
        self.add_error = None;
        // Focus the name field so key events are routed there.
        self.new_name_state.focus();
    }

    /// Enter credential-input mode for a specific account.
    pub(crate) fn enter_credential(&mut self, account_name: String) {
        self.credential_target = Some(account_name);
        self.credential_input = InputBuffer::new();
        self.add_error = None;
    }

    /// Leave credential-input mode.
    pub(crate) fn leave_credential(&mut self) {
        self.credential_target = None;
        self.credential_input = InputBuffer::new();
        self.add_error = None;
    }

    /// Leave the new-account form back to the list view.
    pub(crate) fn leave_new_form(&mut self) {
        self.view = AIProvidersView::List;
        self.new_name_state = TextState::default();
        self.new_provider_state = SelectState::default();
        self.new_api_key_state = TextState::default();
        self.add_error = None;
    }
}

pub(crate) struct SessionDetailData {
    pub(crate) session_id: u64,
    pub(crate) title: String,
    pub(crate) selected_model: String,
    pub(crate) reasoning_effort: Option<ThinkingEffort>,
    pub(crate) parent_session_id: Option<u64>,
    pub(crate) working_dir: String,
    pub(crate) created_at: i64,
    pub(crate) message_count: u32,
    pub(crate) max_turns: Option<u32>,
    pub(crate) status: SessionStatus,
    pub(crate) active_tool_groups: Vec<String>,
    /// The AI provider account associated with this session, if any.
    pub(crate) account_name: Option<String>,
    /// Accumulated token usage for this session, if tracked.
    pub(crate) accumulated_usage: Option<TokenUsage>,
    /// Model context window size for this session, if known.
    pub(crate) context_window: Option<u32>,
    /// The prompt_tokens from the most recent API response (the actual
    /// context size being sent to the model), if available.
    pub(crate) last_prompt_tokens: Option<u32>,
}

pub(crate) struct SessionManagerState {
    pub(crate) sessions: Vec<SessionSummary>,
    pub(crate) view: SessionManagerView,
    pub(crate) selection: Option<usize>,
    pub(crate) scroll: usize,
    pub(crate) detail_data: Option<SessionDetailData>,
    pub(crate) confirm_delete: Option<(u64, String)>,
    /// Error message to display on the session manager page (e.g.
    /// daemon-locked, create failure). Cleared on next successful
    /// session list refresh.
    pub(crate) error: Option<String>,
}

/// Cached rendering of a history item whose content does not change between
/// frames.  Stored alongside the item in `App::render_cache` to avoid
/// re-running markdown parsing and syntect highlighting on every render.
#[derive(Clone)]
pub(crate) struct RenderedCache {
    /// The rendered styled lines at `width`.
    pub lines: Vec<Line<'static>>,
    /// The number of wrapped terminal rows this item occupies at `width`.
    pub height: usize,
    /// Terminal width at which `lines` and `height` were computed.
    /// When the terminal is resized the next render will detect the mismatch
    /// and recompute.
    pub width: u16,
}

pub(crate) struct App {
    pub(crate) input: InputBuffer,
    pub(crate) next_request_id: u32,
    pub(crate) active: HashSet<u32>,
    pub(crate) client: ClientHistory<Box<RenderedImage>>,
    pub(crate) history_scroll: HistoryScrollState,
    pub(crate) history_viewport: HistoryViewport,
    pub(crate) should_quit: bool,
    pub(crate) image_job_tx: Option<std::sync::mpsc::Sender<ImageJob>>,
    /// Maps in-flight job IDs to their history index for O(1) result
    /// dispatch.  Entries are inserted when a job is submitted and removed
    /// when the result arrives or the image is trimmed from history.
    pub(crate) pending_job_idx: HashMap<ImageId, usize>,
    pub(crate) attached_session_id: Option<u64>,
    /// Cached account name for the attached session — populated by
    /// `handle_session_attached` so the status bar can render without
    /// iterating the session list each frame.
    pub(crate) attached_account_name: Option<String>,
    /// Cached model name for the attached session.
    pub(crate) attached_model: Option<String>,
    /// Cached reasoning effort for the attached session.
    pub(crate) attached_reasoning_effort: Option<ThinkingEffort>,
    /// Cached provider slug for the attached session, resolved from the
    /// account name against `ai_providers.accounts`.
    pub(crate) attached_provider_slug: Option<String>,
    /// Cached working directory for the attached session.
    pub(crate) attached_working_dir: Option<String>,
    pub(crate) page: Page,
    /// The page the user was on before opening the Home menu.  `Esc` on the
    /// Home page returns to this page.
    pub(crate) previous_page: Page,
    pub(crate) home_selection: usize, // index into HOME_MENU_ITEMS
    pub(crate) session_mgr: SessionManagerState,
    pub(crate) ai_providers: AIProvidersState,
    /// Accumulated scroll-wheel delta consumed each frame.
    ///
    /// Mouse scroll events increment/decrement this counter instead of
    /// adjusting the scroll position immediately.  Once per frame
    /// `apply_scroll_delta` reads it, resets it to zero, and applies
    /// the total delta in one batch.  This coalesces multiple events
    /// that arrive between frames into a single operation, making fast
    /// trackpad scrolling smooth while ensuring scrolling stops
    /// instantly when the finger lifts (no momentum carry-over).
    pub(crate) scroll_accumulator: isize,

    // ── Command history ─────────────────────────────────────────
    /// Command history entries, newest first.  Loaded from redb on startup
    /// and kept in memory so that Up/Down navigation is instant.
    pub(crate) command_history: Vec<String>,

    /// Current position when navigating history with Up/Down.
    /// `None` = not navigating.  `Some(0)` = most recent entry.
    pub(crate) history_index: Option<usize>,

    /// A copy of the input text taken the moment the user first presses Up.
    /// Restored when pressing Down past the newest entry.
    pub(crate) saved_draft: String,

    /// Optional handle to the redb database.  `None` if the database
    /// could not be opened (history is still usable in-memory during
    /// the session, it just won't persist).
    pub(crate) db: Option<redb::Database>,

    /// Per-item render cache, indexed in lockstep with `client.history`.
    ///
    /// Each entry caches the rendered `Vec<Line>` and height for history
    /// items whose content never changes (`SessionMessage`, `Text`, `Diff`).
    /// `None` means the item has not been rendered yet, is stale after a
    /// resize, or is a `Streaming` item that is never cached.
    ///
    /// The cache is rebuilt from scratch (all `None`s) whenever the history
    /// vector grows or shrinks — this is O(n) but only happens on mutation,
    /// never during scrolling.
    pub(crate) render_cache: Vec<Option<RenderedCache>>,

    /// Index into `client.history` of the image currently displayed
    /// fullscreen.  `None` means no fullscreen overlay is active.
    /// The existing `StatefulProtocol` is rendered directly (no re-decode).
    pub(crate) fullscreen_image_idx: Option<usize>,

    /// Token usage for the currently-attached session, updated from
    /// `SessionState` and `Done` messages.
    pub(crate) attached_token_usage: Option<TokenUsage>,
    /// Context window size for the currently-attached session's model.
    pub(crate) attached_context_window: Option<u32>,
    /// The prompt_tokens from the most recent API response (the actual
    /// context size being sent to the model) for the attached session.
    pub(crate) attached_last_prompt_tokens: Option<u32>,
    /// Set to `true` when the terminal-native progress bar data or current
    /// page changes.  The render loop checks and clears this once per frame
    /// instead of emitting the OSC sequence unconditionally.
    pub(crate) progress_dirty: bool,
}

#[derive(Clone, Copy)]
pub(crate) struct HistoryViewport {
    pub(crate) width: u16,
    pub(crate) height: u16,
}

#[derive(Clone, Copy)]
pub(crate) struct HistoryScrollState {
    pub(crate) scroll: usize,
    pub(crate) scroll_compensation: usize,
    pub(crate) follow_output: bool,
}

pub(crate) type HistoryItem = SharedHistoryItem<Box<RenderedImage>>;
pub(crate) type StreamingTextItem = StreamingText;

pub(crate) enum UiEvent {
    Daemon(tai_proto::DaemonMessage),
    ReaderClosed,
}

impl HistoryViewport {
    pub(crate) fn new() -> Self {
        Self {
            width: 80,
            height: 24,
        }
    }

    pub(crate) fn update(&mut self, area: Rect) {
        self.width = area.width.max(1);
        self.height = area.height;
    }

    pub(crate) fn item_height(&self, item: &HistoryItem) -> usize {
        match item {
            HistoryItem::Text(text) => {
                history_text_height(text, self.width.saturating_sub(2)).max(1) + 1
            }
            HistoryItem::SessionMessage(message) => {
                let content_width = if matches!(message, SessionMessage::AssistantText { .. }) {
                    self.width.saturating_sub(4)
                } else {
                    self.width.saturating_sub(2)
                };
                let lines = session_message_lines(message, content_width);
                lines_height(&lines, content_width).max(1) + 1
            }
            HistoryItem::Streaming(text) => {
                let content_width = self.width.saturating_sub(2);
                let lines = streaming_text_lines(text, content_width);
                lines_height(&lines, content_width).max(1) + 1
            }
            HistoryItem::Image(_) => {
                // Use a stable height for layout and click-target calculations
                // regardless of encoding state.  The actual rendered height
                // in render_item_image uses the protocol dimensions, but the
                // scroll / click logic always sees the same value so that
                // find_history_item_at_row does not shift after encoding.
                (self.height / 2).max(1) as usize
            }
            HistoryItem::Diff(diffs) => diff_display_height(diffs) + 2,
        }
    }
}

impl HistoryScrollState {
    pub(crate) fn new() -> Self {
        Self {
            scroll: 0,
            scroll_compensation: 0,
            follow_output: true,
        }
    }

    #[cfg(test)]
    pub(crate) fn scroll(&self) -> usize {
        self.scroll
    }

    #[cfg(test)]
    pub(crate) fn scroll_compensation(&self) -> usize {
        self.scroll_compensation
    }

    pub(crate) fn follow_output(&self) -> bool {
        self.follow_output
    }

    fn unclamped_effective_scroll(&self) -> usize {
        self.scroll.saturating_add(self.scroll_compensation)
    }

    pub(crate) fn clamp(&mut self, max_scroll: usize) {
        let effective = self.unclamped_effective_scroll();
        if effective <= max_scroll {
            return;
        }

        let overflow = effective - max_scroll;
        let compensation_reduction = self.scroll_compensation.min(overflow);
        self.scroll_compensation -= compensation_reduction;
        let remaining = overflow - compensation_reduction;
        self.scroll = self.scroll.saturating_sub(remaining);
        if self.scroll == 0 && self.scroll_compensation == 0 {
            self.follow_output = true;
        }
    }

    pub(crate) fn effective_scroll(&self, max_scroll: usize) -> usize {
        self.unclamped_effective_scroll().min(max_scroll)
    }

    pub(crate) fn preserve_for_growth(
        &mut self,
        old_height: usize,
        new_height: usize,
        max_scroll: usize,
    ) {
        if !self.follow_output && new_height > old_height {
            self.scroll_compensation = self
                .scroll_compensation
                .saturating_add(new_height - old_height);
            self.clamp(max_scroll);
        }
    }

    pub(crate) fn on_item_appended(&mut self, added_height: usize, max_scroll: usize) {
        if self.follow_output {
            self.scroll = 0;
            self.scroll_compensation = 0;
        } else {
            self.scroll_compensation = self.scroll_compensation.saturating_add(added_height);
        }
        self.clamp(max_scroll);
    }

    pub(crate) fn scroll_up(&mut self, amount: usize, max_scroll: usize) {
        self.scroll = self.scroll.saturating_add(amount);
        if self.scroll > 0 {
            self.follow_output = false;
        }
        self.clamp(max_scroll);
    }

    pub(crate) fn scroll_down(&mut self, amount: usize, max_scroll: usize) {
        let compensation_reduction = self.scroll_compensation.min(amount);
        self.scroll_compensation -= compensation_reduction;
        let remaining = amount.saturating_sub(compensation_reduction);
        self.scroll = self.scroll.saturating_sub(remaining);
        if self.scroll == 0 && self.scroll_compensation == 0 {
            self.follow_output = true;
        }
        self.clamp(max_scroll);
    }

    pub(crate) fn account_for_trimmed_height(&mut self, trimmed_height: usize, max_scroll: usize) {
        self.scroll_compensation = self.scroll_compensation.saturating_sub(trimmed_height);
        self.clamp(max_scroll);
    }
}

impl SessionManagerState {
    pub(crate) fn new() -> Self {
        Self {
            sessions: Vec::new(),
            view: SessionManagerView::List,
            selection: None,
            scroll: 0,
            detail_data: None,
            confirm_delete: None,
            error: None,
        }
    }

    pub(crate) fn set_sessions(&mut self, sessions: Vec<SessionSummary>) {
        self.error = None;
        let was_attached = self
            .selection
            .and_then(|i| self.sessions.get(i))
            .map(|s| s.session_id);
        self.sessions = sessions;
        self.selection = if self.sessions.is_empty() {
            None
        } else {
            let idx = was_attached
                .and_then(|id| self.sessions.iter().position(|s| s.session_id == id))
                .unwrap_or(0);
            Some(idx)
        };
        self.scroll = 0;
    }

    pub(crate) fn select_up(&mut self) {
        let sel = self.selection.unwrap_or(0);
        if sel > 0 {
            self.selection = Some(sel - 1);
            if sel - 1 < self.scroll {
                self.scroll = self.scroll.saturating_sub(1);
            }
        }
    }

    pub(crate) fn select_down(&mut self) {
        let max = self.sessions.len().saturating_sub(1);
        let sel = self.selection.unwrap_or(0);
        if sel < max {
            self.selection = Some(sel + 1);
        }
    }

    pub(crate) fn enter_detail(&mut self) {
        let sel = self.selection;
        let sum = sel.and_then(|i| self.sessions.get(i));
        self.detail_data = sum.map(|s| {
            let session_id = s.session_id;
            let title = s.title.clone().unwrap_or_else(|| "untitled".to_string());
            let selected_model = s.selected_model.clone().unwrap_or_else(|| "-".to_string());
            let parent_session_id = s.parent_session_id;
            let working_dir = s.working_dir.clone().unwrap_or_else(|| "-".to_string());
            let created_at = s.created_at;
            let message_count = s.message_count;
            let max_turns = s.max_turns;
            SessionDetailData {
                session_id,
                title,
                selected_model,
                reasoning_effort: s.reasoning_effort,
                parent_session_id,
                working_dir,
                created_at,
                message_count,
                max_turns,
                status: s.status.clone(),
                active_tool_groups: s.active_tool_groups.clone(),
                account_name: s.account_name.clone(),
                accumulated_usage: s.token_usage.clone(),
                context_window: s.context_window,
                last_prompt_tokens: s.last_prompt_tokens,
            }
        });
        if self.detail_data.is_some() {
            self.view = SessionManagerView::Detail;
        }
    }

    pub(crate) fn leave_detail(&mut self) {
        self.view = SessionManagerView::List;
        self.detail_data = None;
    }

    /// Remove a session by ID from the list, adjusting selection and scroll
    /// state.  Safe to call even when the session is not in the list.
    pub(crate) fn remove_session(&mut self, id: u64) {
        let old_len = self.sessions.len();
        self.sessions.retain(|s| s.session_id != id);
        let removed = old_len - self.sessions.len();
        if removed == 0 {
            return;
        }
        // Adjust selection
        if let Some(sel) = self.selection
            && sel >= self.sessions.len()
        {
            self.selection = if self.sessions.is_empty() {
                None
            } else {
                Some(self.sessions.len().saturating_sub(1))
            };
        }
        // Clamp scroll to valid range after removal
        let max_scroll = self.sessions.len().saturating_sub(1);
        self.scroll = self.scroll.min(max_scroll);
        // If detail view was showing this session, go back to list
        if self
            .detail_data
            .as_ref()
            .is_some_and(|d| d.session_id == id)
        {
            self.view = SessionManagerView::List;
            self.detail_data = None;
        }
        // Clear any pending confirmation
        if self.confirm_delete.as_ref().map(|(sid, _)| *sid) == Some(id) {
            self.confirm_delete = None;
        }
    }

    /// Set an error message to display on the session manager page.
    /// Cleared automatically on the next successful `set_sessions` call.
    pub(crate) fn set_error(&mut self, msg: impl Into<String>) {
        self.error = Some(msg.into());
    }
}

pub(crate) struct InputBuffer {
    pub(crate) text: String,
    pub(crate) cursor: usize,
}

impl InputBuffer {
    pub(crate) fn new() -> Self {
        Self {
            text: String::new(),
            cursor: 0,
        }
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub(crate) fn as_str(&self) -> &str {
        self.text.as_str()
    }

    pub(crate) fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
    }

    // ── Cursor movement ────────────────────────────────────────

    pub(crate) fn cursor_left(&mut self) {
        let prefix = &self.text[..self.cursor];
        if let Some((start, _)) = prefix.grapheme_indices(true).next_back() {
            self.cursor = start;
        }
    }

    pub(crate) fn cursor_right(&mut self) {
        let suffix = &self.text[self.cursor..];
        if suffix.is_empty() {
            return;
        }
        if let Some((offset, grapheme)) = suffix.grapheme_indices(true).next() {
            self.cursor += offset + grapheme.len();
        }
    }

    pub(crate) fn cursor_home(&mut self) {
        self.cursor = 0;
    }

    pub(crate) fn cursor_end(&mut self) {
        self.cursor = self.text.len();
    }

    // ── Word movement ──────────────────────────────────────────

    /// Returns the byte position of the word boundary before current cursor.
    fn word_left_boundary(&self) -> usize {
        let s = &self.text[..self.cursor];
        let trimmed = s.trim_end();
        if trimmed.is_empty() {
            return 0;
        }
        trimmed
            .rfind(|c: char| c.is_whitespace())
            .map(|i| i + 1)
            .unwrap_or(0)
    }

    /// Returns the byte position of the word boundary after current cursor.
    fn word_right_boundary(&self) -> usize {
        let s = &self.text[self.cursor..];
        if s.is_empty() {
            return self.cursor;
        }
        let mut chars = s.char_indices().peekable();

        // Skip past the current word if not at whitespace
        if chars.peek().is_some_and(|&(_, c)| !c.is_whitespace()) {
            for (_, c) in chars.by_ref() {
                if c.is_whitespace() {
                    break;
                }
            }
        }

        // Skip whitespace to find the start of the next word
        while chars.peek().is_some_and(|&(_, c)| c.is_whitespace()) {
            chars.next();
        }

        self.cursor + chars.next().map(|(pos, _)| pos).unwrap_or(s.len())
    }

    pub(crate) fn word_left(&mut self) {
        self.cursor = self.word_left_boundary();
    }

    pub(crate) fn word_right(&mut self) {
        self.cursor = self.word_right_boundary();
    }

    // ── Editing ────────────────────────────────────────────────

    pub(crate) fn insert_char_at_cursor(&mut self, c: char) {
        self.text.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    pub(crate) fn backspace_at_cursor(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let prefix = &self.text[..self.cursor];
        if let Some((start, _)) = prefix.grapheme_indices(true).next_back() {
            self.text.drain(start..self.cursor);
            self.cursor = start;
        }
    }

    pub(crate) fn delete_at_cursor(&mut self) {
        if self.cursor >= self.text.len() {
            return;
        }
        let suffix = &self.text[self.cursor..];
        if let Some((offset, grapheme)) = suffix.grapheme_indices(true).next() {
            self.text
                .drain(self.cursor + offset..self.cursor + offset + grapheme.len());
        }
    }

    pub(crate) fn delete_word_backward(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let boundary = self.word_left_boundary();
        self.text.drain(boundary..self.cursor);
        self.cursor = boundary;
    }

    pub(crate) fn delete_word_forward(&mut self) {
        if self.cursor >= self.text.len() {
            return;
        }
        let boundary = self.word_right_boundary();
        self.text.drain(self.cursor..boundary);
    }

    pub(crate) fn delete_to_start(&mut self) {
        self.text.drain(..self.cursor);
        self.cursor = 0;
    }

    /// Map a `KeyEvent` to an edit operation on the buffer.
    ///
    /// Returns `true` if the key was consumed by the buffer (character input,
    /// cursor movement, text editing).  Returns `false` for keys the caller
    /// should handle itself (Enter, Tab, Esc, and any unrecognised key).
    ///
    /// The key-event-kind check (`Press` only) is left to the caller so that
    /// repeat and release events can be filtered in one place.
    pub(crate) fn handle_key(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Backspace if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.delete_word_backward();
                true
            }
            KeyCode::Backspace => {
                self.backspace_at_cursor();
                true
            }
            KeyCode::Delete if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.delete_word_forward();
                true
            }
            KeyCode::Delete => {
                self.delete_at_cursor();
                true
            }
            KeyCode::Left if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.word_left();
                true
            }
            KeyCode::Left => {
                self.cursor_left();
                true
            }
            KeyCode::Right if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.word_right();
                true
            }
            KeyCode::Right => {
                self.cursor_right();
                true
            }
            KeyCode::Home => {
                self.cursor_home();
                true
            }
            KeyCode::End => {
                self.cursor_end();
                true
            }
            KeyCode::Char('w') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.delete_word_backward();
                true
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.delete_to_start();
                true
            }
            KeyCode::Char(c) => {
                self.insert_char_at_cursor(c);
                true
            }
            // Enter, Tab, and Esc are not editing operations — the caller
            // must handle them (submit, focus-change, quit, etc.).
            KeyCode::Enter | KeyCode::Tab | KeyCode::Esc => false,
            // Everything else is unrecognised.
            _ => false,
        }
    }
}

impl App {
    pub(crate) fn new(socket_path: String) -> Self {
        let (db, command_history) = match db::open_db() {
            Ok(database) => {
                let history = db::load_recent_commands(&database, 100)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|e| e.command)
                    .collect();
                (Some(database), history)
            }
            Err(_) => {
                #[cfg(not(test))]
                tracing::error!("[tai-tui] failed to open command history db");
                (None, Vec::new())
            }
        };

        let initial_items = vec![HistoryItem::Text(format!(
            "Connected to tai-daemon at {socket_path}"
        ))];
        let render_cache = vec![None; initial_items.len()];

        Self {
            input: InputBuffer::new(),
            next_request_id: 1,
            active: HashSet::new(),
            client: ClientHistory::new(initial_items),
            render_cache,
            history_scroll: HistoryScrollState::new(),
            history_viewport: HistoryViewport::new(),
            should_quit: false,
            image_job_tx: None,
            pending_job_idx: HashMap::new(),
            attached_session_id: None,
            attached_account_name: None,
            attached_model: None,
            attached_reasoning_effort: None,
            attached_provider_slug: None,
            attached_working_dir: None,
            page: Page::Chat,
            previous_page: Page::Chat,
            home_selection: 0,
            session_mgr: SessionManagerState::new(),
            ai_providers: AIProvidersState::new(),
            scroll_accumulator: 0,
            command_history,
            history_index: None,
            saved_draft: String::new(),
            db,
            fullscreen_image_idx: None,
            attached_token_usage: None,
            attached_context_window: None,
            attached_last_prompt_tokens: None,
            progress_dirty: false,
        }
    }

    pub(crate) fn total_history_height(&self) -> usize {
        // If the cache is out of sync (mutation happened before next render),
        // fall back to the uncached path.
        if self.render_cache.len() != self.client.history.len() {
            return self
                .client
                .history
                .iter()
                .map(|item| self.history_viewport.item_height(item))
                .sum();
        }

        self.client
            .history
            .iter()
            .zip(self.render_cache.iter())
            .map(|(item, cached)| {
                // Use the cached height if available and at the current width.
                if let Some(cached) = cached
                    && cached.width == self.history_viewport.width
                {
                    return cached.height;
                }
                // Fall back to uncached height computation.
                self.history_viewport.item_height(item)
            })
            .sum()
    }

    pub(crate) fn max_scroll_offset(&self) -> usize {
        let viewport_height = self.history_viewport.height as usize;
        let total_height = self.total_history_height();
        total_height.saturating_sub(viewport_height)
    }

    pub(crate) fn clamp_scroll_state(&mut self) {
        self.history_scroll.clamp(self.max_scroll_offset());
    }

    /// Query the terminal size and update the history viewport to match,
    /// reserving `INPUT_BAR_HEIGHT + STATUS_BAR_HEIGHT` rows for the input
    /// bar and the status bar.
    ///
    /// When the width changes, all cached renderings are stale — entries are
    /// invalidated so the next frame recomputes at the new width.
    pub(crate) fn update_viewport_from_terminal_size(&mut self) {
        let bottom_height = INPUT_BAR_HEIGHT + STATUS_BAR_HEIGHT;
        if let Ok((width, height)) = crossterm::terminal::size()
            && width > 0
            && height > bottom_height
        {
            let old_width = self.history_viewport.width;
            self.history_viewport.update(Rect {
                x: 0,
                y: 0,
                width,
                height: height - bottom_height,
            });
            // All cached entries were computed at the old width and are
            // now stale — invalidate every entry so the next render
            // recomputes at the current terminal width.
            if old_width != width {
                for cached in &mut self.render_cache {
                    *cached = None;
                }
            }
        }
    }

    pub(crate) fn effective_scroll(&self) -> usize {
        self.history_scroll
            .effective_scroll(self.max_scroll_offset())
    }

    pub(crate) fn preserve_scroll_for_growth(&mut self, old_height: usize, new_height: usize) {
        self.history_scroll
            .preserve_for_growth(old_height, new_height, self.max_scroll_offset());
    }

    pub(crate) fn push_text(&mut self, line: impl Into<String>) {
        self.push_history_item(HistoryItem::Text(line.into()));
    }

    /// Insert a `HistoryItem` before the active stream for `request_id`,
    /// updating the render cache and scroll state.
    fn insert_item_before_stream(&mut self, request_id: u32, item: HistoryItem) {
        let added_height = self.history_viewport.item_height(&item);
        let trimmed_height = self.trimmed_height_on_append();
        let old_hist_len = self.client.history.len();
        let insert_at = self.client.in_progress.get(&request_id).copied();
        self.client.insert_before_stream(request_id, item);
        let new_hist_len = self.client.history.len();
        let trimmed = (old_hist_len + 1).saturating_sub(new_hist_len);
        if trimmed > 0 {
            self.render_cache.drain(0..trimmed);
        }
        if let Some(index) = insert_at {
            let adjusted = index.saturating_sub(trimmed);
            self.render_cache.insert(adjusted, None);
        } else {
            self.render_cache.push(None);
        }
        if self.render_cache.len() != new_hist_len {
            self.render_cache.clear();
            self.render_cache.resize(new_hist_len, None);
        }
        self.history_scroll
            .on_item_appended(added_height, self.max_scroll_offset());
        self.account_for_trimmed_height(trimmed_height);
        self.clamp_scroll_state();
    }

    pub(crate) fn push_tool_text(&mut self, request_id: u32, text: impl Into<String>) {
        let text = text.into();
        let item: HistoryItem = match try_parse_as_diff(&text) {
            Some(diffs) => HistoryItem::Diff(diffs),
            None => HistoryItem::Text(text),
        };
        self.insert_item_before_stream(request_id, item);
    }

    /// Classify a `SessionMessage` into a `HistoryItem`, promoting non-error
    /// `ToolResult`s that look like unified diffs to `HistoryItem::Diff`.
    fn classify_session_message(message: SessionMessage) -> HistoryItem {
        if let SessionMessage::ToolResult {
            content, is_error, ..
        } = &message
            && !is_error
            && let Some(diffs) = try_parse_as_diff(content)
        {
            return HistoryItem::Diff(diffs);
        }
        HistoryItem::SessionMessage(message)
    }

    /// Feed a `SessionMessage` into history.
    ///
    /// `DisplayedImage` messages (persisted images) are decoded into
    /// `RenderedImage` objects and pushed as `HistoryItem::Image`, preserving
    /// them across session switches and daemon restarts.  All other message
    /// types go through the normal diff-classification path.
    pub(crate) fn push_session_message(&mut self, message: SessionMessage) {
        match message {
            SessionMessage::DisplayedImage(record) => {
                let img = RenderedImage::new_placeholder(record.metadata, Arc::from(record.data));
                self.push_image(img);
            }
            other => {
                self.push_history_item(Self::classify_session_message(other));
            }
        }
    }

    pub(crate) fn push_image(&mut self, image: RenderedImage) {
        let item = HistoryItem::Image(Box::new(image));
        self.push_history_item(item);
    }

    /// Apply a completed [`ImageResult`] to the corresponding placeholder in
    /// history using an O(1) job-id → history-index map.  Stale results
    /// (image already trimmed or replaced) are silently dropped.
    pub(crate) fn apply_image_result(&mut self, result: ImageResult) {
        let idx = match self.pending_job_idx.remove(&result.id) {
            Some(idx) if idx < self.client.history.len() => idx,
            _ => return,
        };
        if let Some(HistoryItem::Image(img)) = self.client.history.get_mut(idx)
            && img.pending_job == Some(result.id)
        {
            tracing::trace!(
                "[tai-tui] image job {} completed for history idx {} ({} {}x{})",
                result.id,
                idx,
                img.metadata.mime_type,
                img.metadata.width,
                img.metadata.height,
            );
            img.apply_result(result);
        }
    }

    /// Submit an encoding job for the image at `history_idx` and record the
    /// mapping so the result can be dispatched in O(1) time.  Returns the
    /// assigned job ID, or `None` if the worker sender is missing.
    pub(crate) fn submit_image_job(
        &mut self,
        history_idx: usize,
        data: Arc<[u8]>,
        metadata: ImageMetadata,
        cell_size: Size,
        resize: ratatui_image::Resize,
    ) -> Option<ImageId> {
        let tx = self.image_job_tx.as_ref()?;
        let id = next_job_id();

        tracing::trace!(
            "[tai-tui] submitting image job {} for history idx {} ({} {}x{} @ {}x{})",
            id,
            history_idx,
            metadata.mime_type,
            metadata.width,
            metadata.height,
            cell_size.width,
            cell_size.height,
        );

        // Record the mapping before the worker picks up the job so that
        // apply_image_result is always ready.
        self.pending_job_idx.insert(id, history_idx);

        // Update the RenderedImage's pending_job so duplicate submissions
        // are prevented.
        if let Some(HistoryItem::Image(img)) = self.client.history.get_mut(history_idx) {
            img.pending_job = Some(id);
        }

        let _ = tx.send(ImageJob {
            id,
            data,
            metadata,
            cell_size,
            resize,
        });
        Some(id)
    }

    /// Clear all per-session state when switching to a different session.
    ///
    /// Called from the Enter-key handlers in `connection.rs` before sending
    /// `AttachSession`, so that the incoming `SessionState` response populates
    /// a clean view of the target session.
    pub(crate) fn reset_for_session_switch(&mut self) {
        self.client.history.clear();
        self.render_cache.clear();
        self.history_scroll = HistoryScrollState::new();
        self.pending_job_idx.clear();
        self.active.clear();
        self.client.in_progress.clear();
    }

    /// Ensure the render cache is aligned with the history vector.
    ///
    /// Items appended to the back of the history get a `None` appended to
    /// the cache.  Items trimmed from the front are drained from the cache
    /// front.  Existing cache entries are preserved so that
    /// `total_history_height` can use cached heights instead of re-rendering
    /// all items after every mutation.
    pub(crate) fn ensure_cache_synced(&mut self) {
        let hist_len = self.client.history.len();
        let cache_len = self.render_cache.len();
        if cache_len == hist_len {
            return;
        }
        // Items trimmed from front (e.g. when history exceeds MAX_HISTORY_ITEMS).
        if cache_len > hist_len {
            self.render_cache.drain(0..(cache_len - hist_len));
            return;
        }
        // Items appended at the back.
        self.render_cache.resize(hist_len, None);
    }

    pub(crate) fn push_history_item(&mut self, item: HistoryItem) {
        // Capture the job ID before moving the item.
        let job_id = match &item {
            HistoryItem::Image(img) => img.pending_job,
            _ => None,
        };

        let old_len = self.client.history.len();
        let added_height = self.history_viewport.item_height(&item);
        let trimmed_height = self.trimmed_height_on_append();
        self.client.push_history_item(item);

        // Compute how many items were trimmed from the front.
        // `push_history_item` on `ClientHistory` trims down to
        // `MAX_HISTORY_ITEMS`, so the number removed is:
        //   (old_len + 1) - new_len
        let trimmed = old_len
            .saturating_add(1)
            .saturating_sub(self.client.history.len());

        // Shift pending_job_idx entries to account for items trimmed from
        // the front of the history ring buffer.
        if trimmed > 0 {
            self.pending_job_idx.retain(|_, idx| {
                if *idx < trimmed {
                    false // trimmed away
                } else {
                    *idx -= trimmed;
                    true
                }
            });
        }

        // Record the mapping for images that already carry a pending job.
        if let Some(id) = job_id {
            self.pending_job_idx
                .insert(id, self.client.history.len().saturating_sub(1));
        }

        self.ensure_cache_synced();
        self.history_scroll
            .on_item_appended(added_height, self.max_scroll_offset());
        self.account_for_trimmed_height(trimmed_height);
        self.clamp_scroll_state();
    }

    pub(crate) fn begin_stream(&mut self, request_id: u32) {
        if self.client.in_progress.contains_key(&request_id) {
            return;
        }
        let item = HistoryItem::Streaming(StreamingTextItem::new(request_id));
        let added_height = self.history_viewport.item_height(&item);
        let trimmed_height = self.trimmed_height_on_append();
        self.client.begin_stream(request_id);
        self.ensure_cache_synced();
        self.history_scroll
            .on_item_appended(added_height, self.max_scroll_offset());
        self.account_for_trimmed_height(trimmed_height);
        self.clamp_scroll_state();
    }

    pub(crate) fn append_stream_text(
        &mut self,
        request_id: u32,
        stream: OutputStream,
        chunk: &str,
    ) {
        if !self.client.in_progress.contains_key(&request_id) {
            self.begin_stream(request_id);
        }
        if let Some(&index) = self.client.in_progress.get(&request_id) {
            let old_height = self
                .client
                .history
                .get(index)
                .map(|item| self.history_viewport.item_height(item))
                .unwrap_or(0);
            self.client.append_stream(request_id, stream, chunk);
            // Streaming content changed — invalidate any stale cache entry
            if let Some(cached) = self.render_cache.get_mut(index) {
                *cached = None;
            }
            let new_height = self
                .client
                .history
                .get(index)
                .map(|item| self.history_viewport.item_height(item))
                .unwrap_or(old_height);
            self.preserve_scroll_for_growth(old_height, new_height);
        }
    }

    pub(crate) fn finalize_stream(&mut self, request_id: u32) {
        self.client.in_progress.remove(&request_id);
    }

    pub(crate) fn scroll_up(&mut self, amount: usize) {
        self.history_scroll
            .scroll_up(amount, self.max_scroll_offset());
    }

    pub(crate) fn scroll_down(&mut self, amount: usize) {
        self.history_scroll
            .scroll_down(amount, self.max_scroll_offset());
    }

    /// Consume the frame-accumulated scroll delta and apply it as a
    /// single scroll operation.
    ///
    /// Read-then-reset is atomic within the frame so that no delta
    /// carries forward to the next frame — this is what makes
    /// trackpad scrolling stop immediately on finger lift rather than
    /// coasting.
    pub(crate) fn apply_scroll_delta(&mut self) {
        let delta = self.scroll_accumulator;
        self.scroll_accumulator = 0;
        if delta > 0 {
            self.scroll_up(delta as usize);
        } else if delta < 0 {
            self.scroll_down((-delta) as usize);
        }
    }

    // ── Command history navigation ──────────────────────────────

    /// Navigate backward (older) in command history.
    ///
    /// On first invocation saves the current input as a draft.
    pub(crate) fn navigate_history_up(&mut self) {
        if self.command_history.is_empty() {
            return;
        }
        if self.history_index.is_none() {
            // First Up press: save the current input as draft.
            self.saved_draft = self.input.text.clone();
            self.history_index = Some(0);
            self.input.text = self.command_history[0].clone();
        } else if let Some(idx) = self.history_index {
            let next = idx + 1;
            if next < self.command_history.len() {
                self.history_index = Some(next);
                self.input.text = self.command_history[next].clone();
            }
        }
        self.input.cursor = self.input.text.len();
    }

    /// Navigate forward (newer) in command history.
    ///
    /// Restores the saved draft when moving past the newest entry.
    pub(crate) fn navigate_history_down(&mut self) {
        if let Some(idx) = self.history_index {
            if idx > 0 {
                let prev = idx - 1;
                self.history_index = Some(prev);
                self.input.text = self.command_history[prev].clone();
            } else {
                // Past the newest entry: restore draft.
                self.history_index = None;
                self.input.text = self.saved_draft.clone();
                self.saved_draft.clear();
            }
            self.input.cursor = self.input.text.len();
        }
    }

    /// Save a command to the history (DB + in-memory list).
    pub(crate) fn commit_to_history(&mut self, command: String) {
        if command.is_empty() {
            return;
        }
        // Avoid saving a duplicate of the most recent entry.
        if self.command_history.first() == Some(&command) {
            return;
        }

        // Prepend to in-memory list.
        self.command_history.insert(0, command.clone());

        // Persist to redb (best-effort).
        if let Some(ref database) = self.db {
            let entry = CommandEntry {
                command,
                timestamp: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0),
            };
            if let Err(e) = db::save_command(database, &entry) {
                tracing::error!("[tai-tui] failed to save command history: {e}");
            }
        }

        // Reset navigation state.
        self.history_index = None;
        self.saved_draft.clear();
    }

    fn trimmed_height_on_append(&self) -> usize {
        if self.client.history.len() < MAX_HISTORY_ITEMS || self.history_scroll.follow_output() {
            return 0;
        }
        self.client
            .history
            .first()
            .map(|item| self.history_viewport.item_height(item))
            .unwrap_or(0)
    }

    fn account_for_trimmed_height(&mut self, trimmed_height: usize) {
        self.history_scroll
            .account_for_trimmed_height(trimmed_height, self.max_scroll_offset());
    }

    // ── Navigation helpers ───────────────────────────────────────

    /// Navigate to a new page and mark the terminal-native progress bar
    /// dirty so it gets re-evaluated on the next frame.
    pub(crate) fn set_page(&mut self, page: Page) {
        self.page = page;
        self.progress_dirty = true;
    }

    // ── Daemon message handlers ──────────────────────────────────

    /// Handle `SessionCreated`: if on the session-manager page, request a
    /// list refresh so the new session appears; otherwise attach to the new
    /// session on the chat page so the user can send input immediately.
    pub(crate) fn handle_session_created(
        &mut self,
        session_id: u64,
        client_tx: &std::sync::mpsc::Sender<ClientMessage>,
    ) -> Result<(), ClientError> {
        if self.page == Page::SessionManager {
            // Stay on the session manager — the new session will appear
            // when the list refreshes.  The daemon no longer auto-attaches
            // on CreateSession, so the old session stays alive and the
            // user can accumulate multiple sessions.
            let _ = client_tx.send(ClientMessage::ListSessions);
        } else {
            // Chat page (auto-create flow): attach so the user can type
            // immediately.
            self.attached_session_id = Some(session_id);
            client_tx
                .send(ClientMessage::AttachSession { session_id })
                .map_err(broken_pipe)?;
        }
        Ok(())
    }

    /// Handle `SessionAttached`: record the attached session ID.
    pub(crate) fn handle_session_attached(&mut self, session_id: u64) {
        self.attached_session_id = Some(session_id);
        // Seed token usage / context window / status-bar data from the
        // session manager so UI has values even before SessionState arrives.
        if let Some(s) = self
            .session_mgr
            .sessions
            .iter()
            .find(|s| s.session_id == session_id)
        {
            self.attached_token_usage = s.token_usage.clone();
            self.attached_context_window = s.context_window;
            self.attached_last_prompt_tokens = s.last_prompt_tokens;
            self.attached_account_name = s.account_name.clone();
            self.attached_model = s.selected_model.clone();
            self.attached_reasoning_effort = s.reasoning_effort;
            self.attached_working_dir = s.working_dir.clone();
        }
        self.refresh_attached_provider_slug();
        self.progress_dirty = true;
    }

    /// Resolve the provider slug for the attached session from the accounts
    /// list and cache it in `attached_provider_slug`.
    pub(crate) fn refresh_attached_provider_slug(&mut self) {
        self.attached_provider_slug = self.attached_account_name.as_ref().and_then(|name| {
            self.ai_providers
                .accounts
                .iter()
                .find(|a| a.name == *name)
                .map(|a| a.provider.clone())
        });
    }

    /// Shortcut: return a mutable reference to the session-manager entry
    /// for the currently-attached session, if any.
    fn attached_session_mut(&mut self) -> Option<&mut SessionSummary> {
        self.session_mgr
            .sessions
            .iter_mut()
            .find(|s| Some(s.session_id) == self.attached_session_id)
    }

    /// Handle `ModelSelected`: cache the model name on `App` and sync it
    /// into the session-manager list entry so the status bar and Session
    /// Manager detail page are both up to date.
    pub(crate) fn handle_model_selected(&mut self, model: &str) {
        if self.attached_session_id.is_some() {
            self.attached_model = Some(model.to_owned());
            if let Some(s) = self.attached_session_mut() {
                s.selected_model = Some(model.to_owned());
            }
        }
    }

    /// Handle `ReasoningEffortSet`: cache the effort on `App` and sync it
    /// into the session-manager list entry.
    pub(crate) fn handle_reasoning_effort_set(&mut self, effort: ThinkingEffort) {
        if self.attached_session_id.is_some() {
            self.attached_reasoning_effort = Some(effort);
            if let Some(s) = self.attached_session_mut() {
                s.reasoning_effort = Some(effort);
            }
        }
    }

    /// Handle `SessionAccountSet`: cache the account name on `App`,
    /// re-resolve the provider slug, and sync into the session-manager
    /// list entry.
    pub(crate) fn handle_session_account_set(&mut self, account: &str) {
        if self.attached_session_id.is_some() {
            self.attached_account_name = Some(account.to_owned());
            self.refresh_attached_provider_slug();
            if let Some(s) = self.attached_session_mut() {
                s.account_name = Some(account.to_owned());
            }
        }
    }

    /// Handle `SessionStatusChanged`: propagate the new status into the
    /// session list and into the detail view if it's open for this session.
    pub(crate) fn handle_session_status_changed(
        &mut self,
        session_id: u64,
        status: &SessionStatus,
    ) {
        if let Some(session) = self
            .session_mgr
            .sessions
            .iter_mut()
            .find(|s| s.session_id == session_id)
        {
            session.status = status.clone();
        }
        if let Some(ref mut detail) = self.session_mgr.detail_data
            && detail.session_id == session_id
        {
            detail.status = status.clone();
        }
    }

    /// Handle `Sessions`: update the session list, show summaries on the
    /// chat page, and auto-attach or auto-create when no session is attached.
    pub(crate) fn handle_accounts(&mut self, accounts: &[AccountInfo]) {
        self.ai_providers.set_accounts(accounts.to_vec());
        // Re-resolve the provider slug with the now-current accounts list.
        self.refresh_attached_provider_slug();
    }

    pub(crate) fn handle_sessions(
        &mut self,
        sessions: &[SessionSummary],
        client_tx: &std::sync::mpsc::Sender<ClientMessage>,
    ) -> Result<(), ClientError> {
        self.session_mgr.set_sessions(sessions.to_vec());
        if self.page == Page::Chat {
            if sessions.is_empty() {
                self.push_text("[daemon] no sessions");
            } else {
                self.push_text(format!("[daemon] sessions ({})", sessions.len()));
                for session in sessions {
                    let prefix = if Some(session.session_id) == self.attached_session_id {
                        "*"
                    } else {
                        " "
                    };
                    let title = session.title.as_deref().unwrap_or("untitled");
                    let model = session.selected_model.as_deref().unwrap_or("-");
                    self.push_text(format!(
                        "{} {}: \"{title}\" ({model}) — {} messages",
                        prefix, session.session_id, session.message_count,
                    ));
                }
            }
            // Auto-attach/auto-create only on the chat page — the
            // session-manager page doesn't need an attached session,
            // and triggering one would bounce the user back to chat.
            if self.attached_session_id.is_none() {
                if let Some(first) = sessions.first() {
                    client_tx
                        .send(ClientMessage::AttachSession {
                            session_id: first.session_id,
                        })
                        .map_err(broken_pipe)?;
                } else {
                    client_tx
                        .send(ClientMessage::CreateSession {
                            title: Some("default".to_string()),
                            parent_session_id: None,
                            working_dir: None,
                            max_turns: None,
                            context_config: None,
                            account_name: None,
                        })
                        .map_err(broken_pipe)?;
                }
            }
        }
        Ok(())
    }

    /// Handle `SessionDeleted`: remove the session from the local list and
    /// clear the attachment if it was the attached session.
    pub(crate) fn handle_session_deleted(&mut self, session_id: u64) {
        // The session was removed on the daemon side.  Remove from the
        // local session list and clear the attachment if needed.
        self.session_mgr.remove_session(session_id);
        if self.attached_session_id == Some(session_id) {
            self.attached_session_id = None;
            self.attached_account_name = None;
            self.attached_model = None;
            self.attached_reasoning_effort = None;
            self.attached_provider_slug = None;
            self.attached_working_dir = None;
        }
    }

    /// Handle `SessionDeleteFailed`: report the failure in the history.
    pub(crate) fn handle_session_delete_failed(&mut self, session_id: u64, error: &str) {
        self.push_text(format!(
            "failed to delete session {}: {}",
            session_id, error,
        ));
    }
}

impl DaemonMessageHandler for App {
    fn push_text(&mut self, text: String) {
        self.push_text(text);
    }

    fn push_tool_text(&mut self, request_id: u32, text: String) {
        self.push_tool_text(request_id, text);
    }

    fn push_session_message(&mut self, message: SessionMessage) {
        self.push_session_message(message);
    }

    fn insert_session_message_before_stream(&mut self, request_id: u32, message: SessionMessage) {
        self.insert_item_before_stream(request_id, App::classify_session_message(message));
    }

    fn begin_stream(&mut self, request_id: u32) {
        self.begin_stream(request_id);
    }

    fn append_stream(&mut self, request_id: u32, stream: OutputStream, chunk: &str) {
        self.append_stream_text(request_id, stream, chunk);
    }

    fn finalize_stream(&mut self, request_id: u32) {
        self.finalize_stream(request_id);
    }

    fn drop_request(&mut self, request_id: u32) {
        self.active.remove(&request_id);
        self.client.drop_request(request_id);
    }
}

pub(crate) fn history_text_height(text: &str, width: u16) -> usize {
    lines_height(&crate::markdown_render::plain_text_lines(text), width)
}

/// Walk the history bottom-up (same direction as `render_history`) and find
/// the index of the item whose vertical span contains `screen_row` (0 = top
/// of the history viewport), accounting for the current scroll offset.
///
/// Returns `None` when `screen_row` is out of bounds or the history is empty.
pub(crate) fn find_history_item_at_row(app: &App, screen_row: u16) -> Option<usize> {
    let vh = app.history_viewport.height;
    if screen_row >= vh {
        return None;
    }

    // Convert top-down screen row to 0-based distance from the bottom of
    // the viewport, then add the scroll offset to get the logical position
    // within the full history content.
    let from_bottom: usize = (vh - 1 - screen_row).into();
    let logical_row = from_bottom + app.effective_scroll();

    // Walk items from last (bottom) to first (top), accumulating heights.
    let mut accumulated: usize = 0;
    for i in (0..app.client.history.len()).rev() {
        let height = app.history_viewport.item_height(&app.client.history[i]);
        if logical_row < accumulated + height {
            return Some(i);
        }
        accumulated += height;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::test_app;
    use tai_proto::DisplayedImageRecord;

    fn make_session(id: u64, title: &str) -> SessionSummary {
        SessionSummary {
            session_id: id,
            title: Some(title.into()),
            selected_model: None,
            reasoning_effort: None,
            parent_session_id: None,
            working_dir: None,
            created_at: 1000,
            message_count: 0,
            max_turns: None,
            status: SessionStatus::Inactive,
            active_tool_groups: vec!["core".into()],
            account_name: None,
            token_usage: None,
            context_window: None,
            last_prompt_tokens: None,
        }
    }

    fn make_detail_data(session_id: u64) -> SessionDetailData {
        SessionDetailData {
            session_id,
            title: String::new(),
            selected_model: String::new(),
            reasoning_effort: None,
            parent_session_id: None,
            working_dir: String::new(),
            created_at: 0,
            message_count: 0,
            max_turns: None,
            status: SessionStatus::Inactive,
            active_tool_groups: vec![],
            account_name: None,
            accumulated_usage: None,
            context_window: None,
            last_prompt_tokens: None,
        }
    }

    // ── remove_session ──

    #[test]
    fn remove_session_removes_from_list() {
        let mut mgr = SessionManagerState::new();
        mgr.sessions = vec![make_session(1, "a"), make_session(2, "b")];
        mgr.selection = Some(0);
        mgr.remove_session(1);
        assert_eq!(mgr.sessions.len(), 1);
        assert_eq!(mgr.sessions[0].session_id, 2);
    }

    #[test]
    fn remove_session_nonexistent_is_noop() {
        let mut mgr = SessionManagerState::new();
        mgr.sessions = vec![make_session(1, "a")];
        mgr.selection = Some(0);
        mgr.remove_session(999);
        assert_eq!(mgr.sessions.len(), 1);
        assert_eq!(mgr.selection, Some(0));
    }

    #[test]
    fn remove_session_last_item_clears_selection() {
        let mut mgr = SessionManagerState::new();
        mgr.sessions = vec![make_session(1, "a")];
        mgr.selection = Some(0);
        mgr.remove_session(1);
        assert!(mgr.sessions.is_empty());
        assert_eq!(mgr.selection, None);
    }

    #[test]
    fn remove_session_clamps_selection_to_new_len() {
        let mut mgr = SessionManagerState::new();
        mgr.sessions = vec![make_session(1, "a"), make_session(2, "b")];
        mgr.selection = Some(1); // pointing at session 2
        mgr.remove_session(2);
        assert_eq!(mgr.sessions.len(), 1);
        assert_eq!(mgr.selection, Some(0));
    }

    #[test]
    fn remove_session_clears_detail_view_for_deleted_session() {
        let mut mgr = SessionManagerState::new();
        mgr.sessions = vec![make_session(1, "a"), make_session(2, "b")];
        mgr.view = SessionManagerView::Detail;
        mgr.detail_data = Some(make_detail_data(1));
        mgr.remove_session(1);
        assert_eq!(mgr.view, SessionManagerView::List);
        assert!(mgr.detail_data.is_none());
    }

    #[test]
    fn remove_session_clears_confirmation_for_deleted_session() {
        let mut mgr = SessionManagerState::new();
        mgr.sessions = vec![make_session(1, "a")];
        mgr.confirm_delete = Some((1, "a".into()));
        mgr.remove_session(1);
        assert!(mgr.confirm_delete.is_none());
    }

    #[test]
    fn remove_session_preserves_scroll_within_bounds() {
        let mut mgr = SessionManagerState::new();
        mgr.sessions = vec![
            make_session(1, "a"),
            make_session(2, "b"),
            make_session(3, "c"),
        ];
        mgr.selection = Some(2);
        mgr.scroll = 1;
        // Remove the first item; scroll of 1 should stay valid
        mgr.remove_session(1);
        assert_eq!(mgr.scroll, 1);
        assert_eq!(mgr.sessions.len(), 2);
    }

    // ── try_parse_as_diff ──

    #[test]
    fn try_parse_returns_some_for_diff_text() {
        let text = "diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1 +1 @@\n-a\n+b\n";
        assert!(try_parse_as_diff(text).is_some());
    }

    #[test]
    fn try_parse_returns_some_for_diff_with_metadata_prefix() {
        let text = "repository: /repo\nmode: working tree\n\ndiff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1 +1 @@\n-a\n+b\n";
        assert!(try_parse_as_diff(text).is_some());
    }

    #[test]
    fn try_parse_returns_none_for_plain_text() {
        assert!(try_parse_as_diff("hello").is_none());
    }

    #[test]
    fn try_parse_returns_none_for_empty_string() {
        assert!(try_parse_as_diff("").is_none());
    }

    // ── push_tool_text ──

    #[test]
    fn push_tool_text_with_diff_creates_diff_item() {
        let mut app = test_app("/tmp/tai.sock");
        app.history_viewport.width = 80;
        app.history_viewport.height = 10;

        let text = "diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1 +1 @@\n-old\n+new\n";
        app.push_tool_text(1, text);

        assert!(matches!(
            app.client.history.last().unwrap(),
            HistoryItem::Diff(_)
        ));
    }

    #[test]
    fn push_tool_text_with_plain_text_creates_text_item() {
        let mut app = test_app("/tmp/tai.sock");
        app.history_viewport.width = 80;
        app.history_viewport.height = 10;

        app.push_tool_text(1, "just some output");

        match &app.client.history.last().unwrap() {
            HistoryItem::Text(t) => assert!(t.contains("just some output")),
            _ => panic!("expected Text"),
        }
    }

    // ── push_session_message ──

    #[test]
    fn push_session_message_tool_result_with_diff_creates_diff_item() {
        let mut app = test_app("/tmp/tai.sock");
        app.history_viewport.width = 80;
        app.history_viewport.height = 10;

        let msg = SessionMessage::ToolResult {
            call_id: String::new(),
            name: "edit_file".into(),
            content: "edit_file: f (1 replacement, +3 chars)\n\ndiff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1 +1 @@\n-old\n+new\n".into(),
            is_error: false,
        };
        app.push_session_message(msg);

        assert!(matches!(
            app.client.history.last().unwrap(),
            HistoryItem::Diff(_)
        ));
    }

    #[test]
    fn push_session_message_tool_result_with_error_stays_session_message() {
        let mut app = test_app("/tmp/tai.sock");
        app.history_viewport.width = 80;
        app.history_viewport.height = 10;

        let msg = SessionMessage::ToolResult {
            call_id: String::new(),
            name: "edit_file".into(),
            content: "diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1 +1 @@\n-old\n+new\n".into(),
            is_error: true,
        };
        app.push_session_message(msg);

        assert!(matches!(
            app.client.history.last().unwrap(),
            HistoryItem::SessionMessage(_)
        ));
    }

    #[test]
    fn push_session_message_plain_text_stays_session_message() {
        let mut app = test_app("/tmp/tai.sock");
        app.history_viewport.width = 80;
        app.history_viewport.height = 10;

        let msg = SessionMessage::ToolResult {
            call_id: String::new(),
            name: "read_file".into(),
            content: "hello world".into(),
            is_error: false,
        };
        app.push_session_message(msg);

        assert!(matches!(
            app.client.history.last().unwrap(),
            HistoryItem::SessionMessage(_)
        ));
    }

    // ── push_session_message with DisplayedImage ──

    #[test]
    fn push_session_message_displayed_image_creates_placeholder() {
        let mut app = test_app("/tmp/tai.sock");
        app.history_viewport.width = 80;
        app.history_viewport.height = 10;

        let msg = SessionMessage::DisplayedImage(DisplayedImageRecord {
            metadata: ImageMetadata {
                image_id: 0,
                mime_type: "image/png".into(),
                width: 1,
                height: 1,
                byte_len: 0,
                alt: None,
            },
            data: vec![],
        });
        app.push_session_message(msg);

        let last = app.client.history.last().unwrap();
        assert!(
            matches!(last, HistoryItem::Image(_)),
            "DisplayedImage should create a placeholder Image"
        );
    }

    #[test]
    fn push_session_message_displayed_image_svg_creates_placeholder() {
        let mut app = test_app("/tmp/tai.sock");
        app.history_viewport.width = 80;
        app.history_viewport.height = 10;

        let svg = br#"<svg xmlns='http://www.w3.org/2000/svg' width='4' height='3'><rect width='4' height='3' fill='red'/></svg>"#;
        let msg = SessionMessage::DisplayedImage(DisplayedImageRecord {
            metadata: ImageMetadata {
                image_id: 0,
                mime_type: "image/svg+xml".into(),
                width: 4,
                height: 3,
                byte_len: svg.len() as u64,
                alt: Some("red rect".into()),
            },
            data: svg.to_vec(),
        });
        app.push_session_message(msg);

        let last = app.client.history.last().unwrap();
        assert!(
            matches!(last, HistoryItem::Image(_)),
            "expected Image placeholder"
        );
    }

    // ── reset_for_session_switch ──

    #[test]
    fn reset_for_session_switch_clears_state() {
        let mut app = test_app("/tmp/tai.sock");
        app.history_viewport.width = 80;
        app.history_viewport.height = 10;

        // Populate state as if we were in an active session.
        app.client.history.push(HistoryItem::Text("old".into()));
        app.render_cache.push(None);
        app.active.insert(1);
        app.client.in_progress.insert(1, 0);

        app.reset_for_session_switch();

        assert!(app.client.history.is_empty(), "history not cleared");
        assert!(app.render_cache.is_empty(), "render_cache not cleared");
        assert_eq!(app.history_scroll.scroll, 0);
        assert_eq!(app.history_scroll.scroll_compensation, 0);
        assert!(app.history_scroll.follow_output);
        assert!(app.active.is_empty(), "active not cleared");
        assert!(app.client.in_progress.is_empty(), "in_progress not cleared");
    }

    // ── image job lifecycle ──

    #[test]
    fn submit_image_job_returns_none_when_no_tx() {
        let mut app = test_app("/tmp/tai.sock");
        app.image_job_tx = None;

        let id = app.submit_image_job(
            0,
            Arc::<[u8]>::from(vec![]),
            ImageMetadata {
                image_id: 1,
                mime_type: "image/png".into(),
                width: 1,
                height: 1,
                byte_len: 0,
                alt: None,
            },
            Size::new(20, 16),
            tai_tui::IMAGE_RESIZE,
        );
        assert!(id.is_none(), "no job should be submitted when tx is None");
    }

    #[test]
    fn submit_image_job_sends_job_and_records_mapping() {
        let picker = ratatui_image::picker::Picker::halfblocks();
        let worker = tai_tui::image_worker::ImageWorker::spawn(picker);

        let mut app = test_app("/tmp/tai.sock");
        app.image_job_tx = Some(worker.job_tx);

        // The app starts with 1 text item at index 0.  Push an image
        // placeholder so there is an image at index 1.
        let img = tai_tui::RenderedImage::new_placeholder(
            ImageMetadata {
                image_id: 1,
                mime_type: "image/png".into(),
                width: 4,
                height: 3,
                byte_len: 0,
                alt: None,
            },
            Arc::<[u8]>::from(vec![]),
        );
        app.push_image(img);

        let id = app
            .submit_image_job(
                1,
                Arc::<[u8]>::from(vec![]),
                ImageMetadata {
                    image_id: 1,
                    mime_type: "image/png".into(),
                    width: 4,
                    height: 3,
                    byte_len: 0,
                    alt: None,
                },
                Size::new(20, 16),
                tai_tui::IMAGE_RESIZE,
            )
            .expect("job should be submitted");

        // pending_job_idx should contain the mapping.
        assert_eq!(
            app.pending_job_idx.get(&id),
            Some(&1),
            "pending_job_idx should map job id to history index"
        );

        // The RenderedImage's pending_job should be set.
        let HistoryItem::Image(image) = &app.client.history[1] else {
            panic!("expected Image history item");
        };
        assert_eq!(
            image.pending_job,
            Some(id),
            "pending_job should be set on the image"
        );

        // Drop the sender so the worker thread can shut down before join.
        app.image_job_tx = None;
        let _ = worker.handle.join();
    }

    #[test]
    fn apply_image_result_updates_image_protocol() {
        let mut app = test_app("/tmp/tai.sock");

        // The app starts with 1 text item at index 0.  Push an image
        // placeholder so there is an image at index 1.
        let img = tai_tui::RenderedImage::new_placeholder(
            ImageMetadata {
                image_id: 1,
                mime_type: "image/png".into(),
                width: 4,
                height: 3,
                byte_len: 0,
                alt: None,
            },
            Arc::<[u8]>::from(vec![]),
        );
        app.push_image(img);

        // Simulate an in-flight job at index 1.
        let job_id = 42usize;
        app.pending_job_idx.insert(job_id, 1);
        let HistoryItem::Image(image) = &mut app.client.history[1] else {
            panic!("expected Image");
        };
        image.pending_job = Some(job_id);

        // Apply a successful result.
        let cell_size = Size::new(20, 16);
        app.apply_image_result(tai_tui::image_worker::ImageResult {
            id: job_id,
            protocol: None, // protocol encoding not needed for this test
            cell_size,
        });

        // The image should have pending_job cleared and marked as failed
        // (since protocol was None, the worker signalled failure).
        let HistoryItem::Image(image) = &app.client.history[1] else {
            panic!("expected Image");
        };
        assert!(
            image.failed_sizes.contains(&cell_size),
            "image should have failed_sizes when protocol is None"
        );
        assert!(image.pending_job.is_none(), "pending_job should be cleared");
        assert!(
            app.pending_job_idx.get(&job_id).is_none(),
            "job id should be removed from pending_job_idx"
        );
    }

    #[test]
    fn apply_image_result_drops_stale_job() {
        let mut app = test_app("/tmp/tai.sock");

        // Job ID 99 is not in pending_job_idx — result should be silently dropped.
        app.apply_image_result(tai_tui::image_worker::ImageResult {
            id: 99,
            protocol: None,
            cell_size: Size::new(20, 16),
        });
        // No panic = pass.
    }

    #[test]
    fn apply_image_result_drops_result_for_trimmed_index() {
        let mut app = test_app("/tmp/tai.sock");

        // Register a job that maps to an out-of-bounds index.
        app.pending_job_idx.insert(7, 999);

        // Should not panic — the result is silently dropped.
        app.apply_image_result(tai_tui::image_worker::ImageResult {
            id: 7,
            protocol: None,
            cell_size: Size::new(20, 16),
        });
    }

    #[test]
    fn push_history_item_shifts_pending_job_idx_on_trim() {
        use tai_client_core::MAX_HISTORY_ITEMS;

        let mut app = test_app("/tmp/tai.sock");
        app.image_job_tx = None;

        // Fill history so the next push triggers trimming.  The app starts
        // with 1 text item ("Connected to …"), so we fill MAX - 1 items
        // to reach a total of 1 + (MAX - 1) = MAX.
        for i in 0..MAX_HISTORY_ITEMS - 1 {
            app.client
                .history
                .push(HistoryItem::Text(format!("line {i}")));
        }
        // Register a pending job for the initial text at index 0.
        let job_id = next_job_id();
        app.pending_job_idx.insert(job_id, 0);

        // Push one more item.  The initial text at old index 0 will be trimmed.
        app.push_history_item(HistoryItem::Text("new item".into()));

        // The stale mapping should be removed.
        assert!(
            app.pending_job_idx.get(&job_id).is_none(),
            "stale job mapping should be removed on trim"
        );
    }

    #[test]
    fn push_history_item_adjusts_pending_job_idx_for_surviving_items() {
        use tai_client_core::MAX_HISTORY_ITEMS;

        let mut app = test_app("/tmp/tai.sock");

        // App starts with 1 text item at index 0.  Fill so total exceeds MAX.
        // Fill MAX items + 2 extra survivors: total = 1 + MAX + 2 = MAX + 3.
        for i in 0..MAX_HISTORY_ITEMS + 2 {
            app.client
                .history
                .push(HistoryItem::Text(format!("line {i}")));
        }

        // After fill: index 0 = initial text, index 1 = "line 0", …
        // index 500 = "line 499", index 501 = "line 500", index 502 = "line 501"
        // total = 1 + 503 = 504 items.
        //
        // After push: old_len = 503, push → 504, trim → 500.
        // trimmed = 503 + 1 - 500 = 4 items removed from front.

        // A job at old index 0 will be trimmed (0 < 4).
        let trimmed_job = next_job_id();
        // A job at old index 6 survives and shifts to new index 2 (6 - 4 = 2).
        let surviving_job = next_job_id();
        app.pending_job_idx.insert(trimmed_job, 0);
        app.pending_job_idx.insert(surviving_job, 6);

        app.push_history_item(HistoryItem::Text("new item".into()));

        assert_eq!(
            app.pending_job_idx.get(&surviving_job),
            Some(&2),
            "surviving job should be shifted by trimmed count (6 - 4 = 2)"
        );
        assert!(
            app.pending_job_idx.get(&trimmed_job).is_none(),
            "trimmed job should be removed"
        );
    }

    #[test]
    fn reset_for_session_switch_clears_pending_job_idx() {
        let mut app = test_app("/tmp/tai.sock");

        app.pending_job_idx.insert(1, 0);
        app.pending_job_idx.insert(2, 1);
        assert!(!app.pending_job_idx.is_empty());

        app.reset_for_session_switch();

        assert!(
            app.pending_job_idx.is_empty(),
            "pending_job_idx should be cleared on session switch"
        );
    }

    // ── set_page ──

    #[test]
    fn set_page_sets_page_and_dirty_flag() {
        let mut app = test_app("/tmp/tai.sock");
        app.progress_dirty = false;
        app.set_page(Page::Settings);
        assert_eq!(app.page, Page::Settings);
        assert!(app.progress_dirty);
    }

    // ── handle_session_attached progress seeding ──

    #[test]
    fn handle_session_attached_seeds_progress_from_session_manager() {
        let mut app = test_app("/tmp/tai.sock");
        let mut s = make_session(42, "test");
        s.token_usage = Some(TokenUsage {
            input_tokens: 10,
            output_tokens: 20,
            total_tokens: 30,
        });
        s.context_window = Some(4096);
        s.account_name = Some("my-account".into());
        s.selected_model = Some("gpt-4o".into());
        s.reasoning_effort = Some(ThinkingEffort::Medium);
        app.session_mgr.sessions.push(s);

        app.handle_session_attached(42);

        assert_eq!(app.attached_session_id, Some(42));
        assert_eq!(
            app.attached_token_usage,
            Some(TokenUsage {
                input_tokens: 10,
                output_tokens: 20,
                total_tokens: 30,
            })
        );
        assert_eq!(app.attached_context_window, Some(4096));
        assert_eq!(app.attached_account_name.as_deref(), Some("my-account"),);
        assert_eq!(app.attached_model.as_deref(), Some("gpt-4o"));
        assert_eq!(app.attached_reasoning_effort, Some(ThinkingEffort::Medium),);
        assert!(app.progress_dirty);
    }

    #[test]
    fn handle_session_attached_ignores_unknown_session() {
        let mut app = test_app("/tmp/tai.sock");
        app.handle_session_attached(99);
        assert_eq!(app.attached_session_id, Some(99));
        assert!(app.attached_token_usage.is_none());
        assert!(app.attached_context_window.is_none());
        assert!(app.attached_account_name.is_none());
        assert!(app.attached_model.is_none());
        assert!(app.attached_reasoning_effort.is_none());
        assert!(app.progress_dirty);
    }

    // ── handle_model_selected ──

    #[test]
    fn handle_model_selected_updates_cached_value_and_session_entry() {
        let mut app = test_app("/tmp/tai.sock");
        app.session_mgr.sessions.push(make_session(1, "s1"));
        app.handle_session_attached(1);

        app.handle_model_selected("gpt-4o");

        assert_eq!(
            app.attached_model.as_deref(),
            Some("gpt-4o"),
            "cached model should be set",
        );
        assert_eq!(
            app.session_mgr.sessions[0].selected_model.as_deref(),
            Some("gpt-4o"),
            "session-manager entry should be updated",
        );
    }

    #[test]
    fn handle_model_selected_skipped_when_no_attached_session() {
        let mut app = test_app("/tmp/tai.sock");
        let saved = app.attached_model.clone();
        app.handle_model_selected("gpt-4o");
        assert_eq!(app.attached_model, saved, "should be a no-op");
    }

    // ── handle_reasoning_effort_set ──

    #[test]
    fn handle_reasoning_effort_set_updates_cached_value_and_session_entry() {
        let mut app = test_app("/tmp/tai.sock");
        app.session_mgr.sessions.push(make_session(1, "s1"));
        app.handle_session_attached(1);

        app.handle_reasoning_effort_set(ThinkingEffort::High);

        assert_eq!(app.attached_reasoning_effort, Some(ThinkingEffort::High),);
        assert_eq!(
            app.session_mgr.sessions[0].reasoning_effort,
            Some(ThinkingEffort::High),
        );
    }

    #[test]
    fn handle_reasoning_effort_set_skipped_when_no_attached_session() {
        let mut app = test_app("/tmp/tai.sock");
        app.handle_reasoning_effort_set(ThinkingEffort::Low);
        assert!(app.attached_reasoning_effort.is_none());
    }

    // ── handle_session_account_set ──

    #[test]
    fn handle_session_account_set_updates_cached_value_and_session_entry() {
        let mut app = test_app("/tmp/tai.sock");
        app.ai_providers.set_accounts(vec![tai_proto::AccountInfo {
            name: "my-acc".into(),
            provider: "openai".into(),
            has_credential: true,
        }]);
        app.session_mgr.sessions.push(make_session(1, "s1"));
        app.handle_session_attached(1);

        app.handle_session_account_set("my-acc");

        assert_eq!(app.attached_account_name.as_deref(), Some("my-acc"));
        assert_eq!(app.attached_provider_slug.as_deref(), Some("openai"));
        assert_eq!(
            app.session_mgr.sessions[0].account_name.as_deref(),
            Some("my-acc"),
        );
    }

    #[test]
    fn handle_session_account_set_skipped_when_no_attached_session() {
        let mut app = test_app("/tmp/tai.sock");
        app.handle_session_account_set("my-acc");
        assert!(app.attached_account_name.is_none());
        assert!(app.attached_provider_slug.is_none());
    }

    // ── attached_session_mut ──

    #[test]
    fn attached_session_mut_returns_none_when_not_attached() {
        let mut app = test_app("/tmp/tai.sock");
        app.session_mgr.sessions.push(make_session(1, "s1"));
        assert!(app.attached_session_mut().is_none());
    }

    #[test]
    fn attached_session_mut_returns_the_attached_session() {
        let mut app = test_app("/tmp/tai.sock");
        app.session_mgr.sessions.push(make_session(1, "s1"));
        app.handle_session_attached(1);

        let entry = app.attached_session_mut();
        assert!(entry.is_some());
        assert_eq!(entry.unwrap().session_id, 1);
    }

    #[test]
    fn attached_session_mut_allows_mutation() {
        let mut app = test_app("/tmp/tai.sock");
        app.session_mgr.sessions.push(make_session(1, "s1"));
        app.handle_session_attached(1);

        if let Some(s) = app.attached_session_mut() {
            s.title = Some("mutated".into());
        }
        assert_eq!(
            app.session_mgr.sessions[0].title.as_deref(),
            Some("mutated"),
        );
    }
}
