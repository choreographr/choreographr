use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Rect, Size};
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tai_client_core::dispatch::{SessionStateData, ToolCallEvent};
use tai_client_core::{ClientError, SessionView, TurnEventHandler, broken_pipe};
use tai_proto::{
    AccountInfo, ClientMessage, OutputStream, SessionStatus, SessionSummary, ThinkingEffort,
    TokenUsage, Turn,
};
use tai_tui::RenderedImage;
use tai_tui::image_worker::{ImageId, ImageJob, ImageResult, next_job_id};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::markdown_render::{lines_height, plain_text_lines, render_turn_lines};
use ratatui::text::Line;
use tui_prompts::{SelectState, State, TextState};

pub(crate) const STATUS_BAR_HEIGHT: u16 = 2;
pub(crate) const MIN_INPUT_CONTENT_LINES: u16 = 1;
pub(crate) const MAX_INPUT_CONTENT_LINES: u16 = 10;
pub(crate) const PAGE_SCROLL_LINES: usize = 3;

pub(crate) const AI_PROVIDER_ITEM_LINES: usize = 4;

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

#[derive(Clone, Copy, Debug)]
pub(crate) struct ProviderInfo {
    pub(crate) slug: &'static str,
    pub(crate) display_name: &'static str,
}

pub(crate) const PROVIDER_OPTIONS: &[ProviderInfo] = &[
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
    ProviderInfo {
        slug: "minimax",
        display_name: "MiniMax",
    },
    ProviderInfo {
        slug: "custom-anthropic",
        display_name: "Custom Anthropic-Compatible",
    },
];

pub(crate) struct AIProvidersState {
    pub(crate) accounts: Vec<AccountInfo>,
    pub(crate) view: AIProvidersView,
    pub(crate) selection: Option<usize>,
    pub(crate) scroll: usize,
    pub(crate) confirm_remove: Option<String>,
    pub(crate) new_name_state: TextState<'static>,
    pub(crate) new_provider_state: SelectState,
    pub(crate) new_api_key_state: TextState<'static>,
    pub(crate) credential_target: Option<String>,
    pub(crate) credential_input: InputBuffer,
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

    pub(crate) fn scroll_up_page(&mut self) {
        self.scroll = self.scroll.saturating_sub(1);
    }

    pub(crate) fn scroll_down_page(&mut self) {
        if !self.accounts.is_empty() {
            let max_scroll = self.accounts.len().saturating_sub(1);
            self.scroll = (self.scroll + 1).min(max_scroll);
        }
    }

    pub(crate) fn remove_account(&mut self, name: &str) {
        let old_len = self.accounts.len();
        self.accounts.retain(|a| a.name != name);
        if self.accounts.len() == old_len {
            return;
        }
        if let Some(sel) = self.selection
            && sel >= self.accounts.len()
        {
            self.selection = if self.accounts.is_empty() {
                None
            } else {
                Some(self.accounts.len().saturating_sub(1))
            };
        }
        let max_scroll = self.accounts.len().saturating_sub(1);
        self.scroll = self.scroll.min(max_scroll);
        if self.confirm_remove.as_deref() == Some(name) {
            self.confirm_remove = None;
        }
    }

    pub(crate) fn enter_new_form(&mut self) {
        self.view = AIProvidersView::NewForm;
        self.new_name_state = TextState::default();
        self.new_provider_state = SelectState::default();
        self.new_api_key_state = TextState::default();
        self.add_error = None;
        self.new_name_state.focus();
    }

    pub(crate) fn enter_credential(&mut self, account_name: String) {
        self.credential_target = Some(account_name);
        self.credential_input = InputBuffer::new();
        self.add_error = None;
    }

    pub(crate) fn leave_credential(&mut self) {
        self.credential_target = None;
        self.credential_input = InputBuffer::new();
        self.add_error = None;
    }

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
    pub(crate) turn_count: u32,
    pub(crate) max_turns: Option<u32>,
    pub(crate) status: SessionStatus,
    pub(crate) active_tool_groups: Vec<String>,
    pub(crate) account_name: Option<String>,
    pub(crate) accumulated_usage: Option<TokenUsage>,
    pub(crate) context_window: Option<u32>,
    pub(crate) last_prompt_tokens: Option<u32>,
}

pub(crate) struct SessionManagerState {
    pub(crate) sessions: Vec<SessionSummary>,
    pub(crate) view: SessionManagerView,
    pub(crate) selection: Option<usize>,
    pub(crate) scroll: usize,
    pub(crate) detail_data: Option<SessionDetailData>,
    pub(crate) confirm_delete: Option<(u64, String)>,
    pub(crate) error: Option<String>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Marker {
    pub content_line: usize,
    pub virtual_slot: usize,
}

/// Per-turn image content-line ranges, computed alongside `height_prefix`.
/// Maps a content-line offset within the turn to the correct image index
/// in the click handler — no text-height recomputation needed.
#[derive(Debug)]
pub(crate) struct TurnImageLayout {
    /// (start, end) content-line ranges for each displayed image,
    /// relative to the turn's start.  Empty when the turn has no images.
    pub image_ranges: Vec<(usize, usize)>,
}

#[derive(Clone)]
pub(crate) struct RenderedCache {
    pub lines: Vec<Line<'static>>,
    pub width: u16,
}

pub(crate) struct App {
    pub(crate) input: InputBuffer,
    pub(crate) next_request_id: u32,
    pub(crate) active: HashSet<u32>,
    pub(crate) session_view: SessionView,
    pub(crate) rendered_images: HashMap<u32, HashMap<usize, RenderedImage>>,
    pub(crate) history_scroll: HistoryScrollState,
    pub(crate) history_viewport: HistoryViewport,
    pub(crate) should_quit: bool,
    pub(crate) image_job_tx: Option<crossbeam::channel::Sender<ImageJob>>,
    pub(crate) attached_session_id: Option<u64>,
    pub(crate) attached_account_name: Option<String>,
    pub(crate) attached_model: Option<String>,
    pub(crate) attached_reasoning_effort: Option<ThinkingEffort>,
    pub(crate) attached_provider_slug: Option<String>,
    pub(crate) attached_working_dir: Option<String>,
    pub(crate) attached_status: Option<SessionStatus>,
    pub(crate) page: Page,
    pub(crate) previous_page: Page,
    pub(crate) home_selection: usize,
    pub(crate) session_mgr: SessionManagerState,
    pub(crate) ai_providers: AIProvidersState,
    pub(crate) scroll_accumulator: isize,
    pub(crate) scrollbar_dragging: bool,
    pub(crate) markers: Vec<Marker>,
    pub(crate) height_prefix: Vec<usize>,
    pub(crate) markers_dirty: bool,
    pub(crate) last_terminal_size: Option<(u16, u16)>,
    pub(crate) terminal_resized: bool,
    pub(crate) history_index: Option<usize>,
    pub(crate) saved_draft: String,
    pub(crate) render_cache: Vec<Option<RenderedCache>>,
    pub(crate) fullscreen_image_target: Option<(u32, usize)>,
    pub(crate) attached_token_usage: Option<TokenUsage>,
    pub(crate) attached_context_window: Option<u32>,
    pub(crate) attached_last_prompt_tokens: Option<u32>,
    /// Estimated input tokens for the current streaming turn (set at request start).
    pub(crate) live_input_estimate: u32,
    /// Cumulative output-token estimate for the current turn, updated by
    /// `LiveOutputTokenCount` messages from the daemon (which tokenizes
    /// each stream chunk via tiktoken).
    pub(crate) live_output_tokens: u32,
    pub(crate) progress_dirty: bool,
    pub(crate) status: Option<String>,
    pub(crate) error: Option<String>,
    pub(crate) visible_turn_ids: Vec<u32>,
    /// Maps in-flight job IDs to their (turn_id, img_idx) for O(1) result dispatch.
    pub(crate) pending_job_idx: HashMap<ImageId, (u32, usize)>,
    /// Per-turn layout metadata (image content-line ranges), populated by
    /// `rebuild_height_prefix`.  Used by the click handler to determine
    /// which image was clicked without re-rendering text lines.
    pub(crate) turn_layouts: Vec<TurnImageLayout>,
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
}

pub(crate) enum UiEvent {
    Daemon(Box<tai_proto::DaemonMessage>),
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
}

impl HistoryScrollState {
    pub(crate) fn new() -> Self {
        Self {
            scroll: 0,
            scroll_compensation: 0,
        }
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
    }

    pub(crate) fn effective_scroll(&self, max_scroll: usize) -> usize {
        self.unclamped_effective_scroll().min(max_scroll)
    }

    pub(crate) fn scroll_up(&mut self, amount: usize, max_scroll: usize) {
        self.scroll = self.scroll.saturating_add(amount);
        self.clamp(max_scroll);
    }

    pub(crate) fn scroll_down(&mut self, amount: usize, max_scroll: usize) {
        let compensation_reduction = self.scroll_compensation.min(amount);
        self.scroll_compensation -= compensation_reduction;
        let remaining = amount.saturating_sub(compensation_reduction);
        self.scroll = self.scroll.saturating_sub(remaining);
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

    pub(crate) fn scroll_up_page(&mut self) {
        self.scroll = self.scroll.saturating_sub(PAGE_SCROLL_LINES);
    }

    pub(crate) fn scroll_down_page(&mut self) {
        if !self.sessions.is_empty() {
            let max_scroll = self.sessions.len().saturating_sub(1);
            self.scroll = (self.scroll + PAGE_SCROLL_LINES).min(max_scroll);
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
            let turn_count = s.turn_count;
            let max_turns = s.max_turns;
            SessionDetailData {
                session_id,
                title,
                selected_model,
                reasoning_effort: s.reasoning_effort,
                parent_session_id,
                working_dir,
                created_at,
                turn_count,
                max_turns,
                status: s.status.clone(),
                active_tool_groups: s.active_tool_groups.clone(),
                account_name: s.account_name.clone(),
                accumulated_usage: s.token_usage,
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

    pub(crate) fn remove_session(&mut self, id: u64) {
        let old_len = self.sessions.len();
        self.sessions.retain(|s| s.session_id != id);
        let removed = old_len - self.sessions.len();
        if removed == 0 {
            return;
        }
        if let Some(sel) = self.selection
            && sel >= self.sessions.len()
        {
            self.selection = if self.sessions.is_empty() {
                None
            } else {
                Some(self.sessions.len().saturating_sub(1))
            };
        }
        let max_scroll = self.sessions.len().saturating_sub(1);
        self.scroll = self.scroll.min(max_scroll);
        if self
            .detail_data
            .as_ref()
            .is_some_and(|d| d.session_id == id)
        {
            self.view = SessionManagerView::List;
            self.detail_data = None;
        }
        if self.confirm_delete.as_ref().map(|(sid, _)| *sid) == Some(id) {
            self.confirm_delete = None;
        }
    }

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

    fn word_right_boundary(&self) -> usize {
        let s = &self.text[self.cursor..];
        if s.is_empty() {
            return self.cursor;
        }
        let mut chars = s.char_indices().peekable();
        if chars.peek().is_some_and(|&(_, c)| !c.is_whitespace()) {
            for (_, c) in chars.by_ref() {
                if c.is_whitespace() {
                    break;
                }
            }
        }
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
            KeyCode::Home if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.cursor_home();
                true
            }
            KeyCode::End if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.cursor_end();
                true
            }
            KeyCode::Home => {
                self.cursor_home_line();
                true
            }
            KeyCode::End => {
                self.cursor_end_line();
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
            KeyCode::Enter | KeyCode::Tab | KeyCode::Esc => false,
            _ => false,
        }
    }

    /// Move cursor to the start of the current logical line (after `\n` or at offset 0).
    pub(crate) fn cursor_home_line(&mut self) {
        let prefix = &self.text[..self.cursor];
        self.cursor = prefix.rfind('\n').map(|p| p + 1).unwrap_or(0);
    }

    /// Move cursor to the end of the current logical line (at the `\n` or at text end).
    pub(crate) fn cursor_end_line(&mut self) {
        let suffix = &self.text[self.cursor..];
        self.cursor += suffix.find('\n').unwrap_or(suffix.len());
    }

    /// Move cursor up one visual line (wrapping-aware).
    pub(crate) fn cursor_up(&mut self, max_width: usize) {
        if max_width < 1 {
            return;
        }
        let lines = compute_visual_lines(&self.text, max_width);
        let (current_line, col) = find_cursor_pos(&self.text, self.cursor, &lines);
        if current_line == 0 {
            return;
        }
        let target = &lines[current_line as usize - 1];
        let target_text = &self.text[target.start_byte..target.end_byte];
        let target_col = (col as usize).min(target.display_width);
        let byte_off = byte_offset_at_column(target_text, target_col);
        self.cursor = target.start_byte + byte_off;
    }

    /// Move cursor down one visual line (wrapping-aware).
    pub(crate) fn cursor_down(&mut self, max_width: usize) {
        if max_width < 1 {
            return;
        }
        let lines = compute_visual_lines(&self.text, max_width);
        let (current_line, col) = find_cursor_pos(&self.text, self.cursor, &lines);
        if current_line + 1 >= lines.len() as u16 {
            return;
        }
        let target = &lines[current_line as usize + 1];
        let target_text = &self.text[target.start_byte..target.end_byte];
        let target_col = (col as usize).min(target.display_width);
        let byte_off = byte_offset_at_column(target_text, target_col);
        self.cursor = target.start_byte + byte_off;
    }

    /// Return the (visual_row, visual_col) of the cursor within wrapped text.
    /// Both are 0-indexed.
    pub(crate) fn cursor_visual_pos(&self, max_width: usize) -> (u16, u16) {
        if max_width < 1 {
            return (0, 0);
        }
        let lines = compute_visual_lines(&self.text, max_width);
        find_cursor_pos(&self.text, self.cursor, &lines)
    }

    /// True when the cursor is on the first visual line of the input.
    pub(crate) fn is_on_first_visual_line(&self, max_width: usize) -> bool {
        self.cursor_visual_pos(max_width).0 == 0
    }

    /// True when the cursor is on the last visual line of the input.
    pub(crate) fn is_on_last_visual_line(&self, max_width: usize) -> bool {
        if max_width < 1 {
            return true;
        }
        let lines = compute_visual_lines(&self.text, max_width);
        let (row, _) = find_cursor_pos(&self.text, self.cursor, &lines);
        row + 1 >= lines.len() as u16
    }
}

/// A single visual (wrapped) line derived from the input text.
#[derive(Debug)]
pub(crate) struct VisualLineInfo {
    /// Byte offset of the start of this visual line within the full input text.
    pub(crate) start_byte: usize,
    /// Byte offset of the end (exclusive) of this visual line.
    pub(crate) end_byte: usize,
    /// Display width of the text on this visual line.
    pub(crate) display_width: usize,
}

/// Find the cursor's (visual_row, visual_col) within pre-computed visual lines.
fn find_cursor_pos(text: &str, cursor: usize, lines: &[VisualLineInfo]) -> (u16, u16) {
    for (i, vl) in lines.iter().enumerate() {
        if cursor >= vl.start_byte && cursor <= vl.end_byte {
            let line_text = &text[vl.start_byte..cursor.min(vl.end_byte)];
            let col = UnicodeWidthStr::width(line_text);
            return (i as u16, col as u16);
        }
    }
    // Cursor past the last visual line — place at end.
    let last = match lines.last() {
        Some(vl) => vl,
        None => return (0, 0),
    };
    let col = UnicodeWidthStr::width(&text[last.start_byte..last.end_byte]);
    (lines.len().saturating_sub(1) as u16, col as u16)
}

/// Word-wrap `text` into visual lines that each fit within `max_width`.
/// Explicit `\n` characters always create line breaks.  Words longer than
/// `max_width` are placed on their own line and overflow — they are never
/// character-broken.  Returns at least one entry (for empty text).
pub(crate) fn compute_visual_lines(text: &str, max_width: usize) -> Vec<VisualLineInfo> {
    if max_width == 0 {
        return vec![VisualLineInfo {
            start_byte: 0,
            end_byte: 0,
            display_width: 0,
        }];
    }

    let text_ptr = text.as_ptr() as usize;
    let mut lines: Vec<VisualLineInfo> = Vec::new();

    for logical in text.split('\n') {
        let logical_offset = logical.as_ptr() as usize - text_ptr;

        if logical.is_empty() {
            lines.push(VisualLineInfo {
                start_byte: logical_offset,
                end_byte: logical_offset,
                display_width: 0,
            });
            continue;
        }

        // Collect word positions (non-whitespace runs) within this logical line.
        let mut words: Vec<(usize, usize)> = Vec::new(); // (start, end) byte offsets within `logical`
        let mut pos = 0;
        while pos < logical.len() {
            while pos < logical.len() && logical.as_bytes()[pos].is_ascii_whitespace() {
                pos += 1;
            }
            if pos >= logical.len() {
                break;
            }
            let w_start = pos;
            while pos < logical.len() && !logical.as_bytes()[pos].is_ascii_whitespace() {
                pos += 1;
            }
            words.push((w_start, pos));
        }

        if words.is_empty() {
            // Logical line contained only whitespace.
            lines.push(VisualLineInfo {
                start_byte: logical_offset,
                end_byte: logical_offset + logical.len(),
                display_width: UnicodeWidthStr::width(logical),
            });
            continue;
        }

        // Greedy word-wrap: accumulate words onto visual lines.
        let mut line_start_byte = logical_offset; // byte offset in full text
        let mut line_width: usize = 0;
        let mut last_word_end_byte = logical_offset; // end of last word placed

        for (i, &(w_start, w_end)) in words.iter().enumerate() {
            let word = &logical[w_start..w_end];
            let word_width = UnicodeWidthStr::width(word);

            // Whitespace between the previous word (or start of logical line) and this word.
            let preceding_ws = if i == 0 {
                &logical[0..w_start]
            } else {
                &logical[words[i - 1].1..w_start]
            };
            let ws_width = UnicodeWidthStr::width(preceding_ws);

            let space_needed = ws_width + word_width;

            if line_width > 0 && line_width + space_needed > max_width {
                // Flush current line (everything up to the end of the previous word).
                lines.push(VisualLineInfo {
                    start_byte: line_start_byte,
                    end_byte: last_word_end_byte,
                    display_width: line_width,
                });
                // Start new visual line with this word (leading whitespace trimmed).
                line_start_byte = logical_offset + w_start;
                line_width = word_width;
                last_word_end_byte = logical_offset + w_end;
            } else {
                line_width += space_needed;
                last_word_end_byte = logical_offset + w_end;
            }
        }

        // Flush the last visual line of this logical line,
        // including any trailing whitespace after the last word.
        let trailing_ws = words
            .last()
            .map(|&(_, w_end)| &logical[w_end..])
            .unwrap_or(logical);
        let trailing_ws_width = UnicodeWidthStr::width(trailing_ws);
        lines.push(VisualLineInfo {
            start_byte: line_start_byte,
            end_byte: last_word_end_byte + trailing_ws.len(),
            display_width: line_width + trailing_ws_width,
        });
    }

    if lines.is_empty() {
        lines.push(VisualLineInfo {
            start_byte: 0,
            end_byte: 0,
            display_width: 0,
        });
    }

    lines
}

/// Find the byte offset within `s` for the given display-width column,
/// without exceeding `target_col`.  Returns `s.len()` if `target_col` is
/// larger than the string's display width.
pub(crate) fn byte_offset_at_column(s: &str, target_col: usize) -> usize {
    let mut col = 0;
    for (byte_i, ch) in s.char_indices() {
        let ch_w = UnicodeWidthStr::width(&s[byte_i..byte_i + ch.len_utf8()]);
        if col + ch_w > target_col {
            return byte_i;
        }
        col += ch_w;
    }
    s.len()
}

impl App {
    pub(crate) fn new(_socket_path: String) -> Self {
        Self {
            input: InputBuffer::new(),
            next_request_id: 1,
            active: HashSet::new(),
            session_view: SessionView::new(),
            rendered_images: HashMap::new(),
            render_cache: Vec::new(),
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
            attached_status: None,
            page: Page::Chat,
            previous_page: Page::Chat,
            home_selection: 0,
            session_mgr: SessionManagerState::new(),
            ai_providers: AIProvidersState::new(),
            scroll_accumulator: 0,
            scrollbar_dragging: false,
            markers: Vec::new(),
            history_index: None,
            saved_draft: String::new(),
            fullscreen_image_target: None,
            attached_token_usage: None,
            attached_context_window: None,
            attached_last_prompt_tokens: None,
            live_input_estimate: 0,
            live_output_tokens: 0,
            progress_dirty: false,
            status: None,
            error: None,
            height_prefix: Vec::new(),
            turn_layouts: Vec::new(),
            markers_dirty: true,
            last_terminal_size: None,
            terminal_resized: false,
            visible_turn_ids: Vec::new(),
        }
    }

    pub(crate) fn total_history_height(&self) -> usize {
        self.height_prefix.last().copied().unwrap_or(0)
    }

    /// Rebuild height_prefix, markers, visible_turn_ids, and populate render_cache from
    /// the current session_view.turns.  Called whenever turns change.
    pub(crate) fn rebuild_height_prefix(&mut self) {
        self.height_prefix.clear();
        self.visible_turn_ids.clear();
        self.markers.clear();
        self.turn_layouts.clear();
        let mut total = 0usize;
        let viewport_height = self.history_viewport.height as usize;
        let virtual_track = 2 * viewport_height;
        let fallback_img_height = self.image_block_height() as usize;
        // Collect computed lines so we can warm the render_cache below.
        let mut computed_lines: Vec<Option<(Vec<Line<'static>>, u16)>> = Vec::new();
        // Iterate turns in order (oldest first).
        for (&turn_id, turn) in self.session_view.turns.iter() {
            if turn.undone {
                continue;
            }
            // Compute height for this turn.
            let content_width = self.history_viewport.width.saturating_sub(9);
            let tool_content_width = self.history_viewport.width.saturating_sub(4);
            let text_lines = render_turn_lines(turn, content_width, tool_content_width);
            let text_height = lines_height(&text_lines, self.history_viewport.width).max(1);
            computed_lines.push(Some((text_lines, content_width)));
            // Image blocks always use `image_block_height()` — this must
            // match the render allocation in `render_turn_image` so that
            // click-to-fullscreen detection (via `image_ranges`) and scroll
            // positions (via `height_prefix`) stay in sync.
            let mut image_ranges: Vec<(usize, usize)> = Vec::new();
            let mut total_img_height: usize = 0;
            for _ in 0..turn.displayed_images.len() {
                let start = text_height + total_img_height;
                image_ranges.push((start, start + fallback_img_height));
                total_img_height += fallback_img_height;
            }
            self.turn_layouts.push(TurnImageLayout { image_ranges });
            let turn_height = text_height + total_img_height;
            // Marker for turns with user_text — points to the start of the turn.
            if turn.user_text.is_some() {
                let start_line = total;
                let slot = start_line * virtual_track / (total + turn_height).max(1);
                self.markers.push(Marker {
                    content_line: start_line,
                    virtual_slot: slot,
                });
            }
            total += turn_height;
            self.height_prefix.push(total);
            self.visible_turn_ids.push(turn_id);
        }
        // Ensure render_cache matches visible_turn_ids count.
        self.ensure_cache_synced();
        // Warm cache with the lines we just computed, avoiding a second
        // render_turn_lines call in render_history.
        for (slot, entry) in self.render_cache.iter_mut().zip(computed_lines.drain(..)) {
            if let Some((lines, w)) = entry {
                *slot = Some(RenderedCache { lines, width: w });
            }
        }
        self.markers_dirty = false;
    }

    pub(crate) fn compute_total_height_and_markers(&mut self) -> usize {
        if self.markers_dirty {
            self.rebuild_height_prefix();
        }
        self.total_history_height().max(1)
    }

    pub(crate) fn max_scroll_offset(&self) -> usize {
        let viewport_height = self.history_viewport.height as usize;
        let total_height = self.total_history_height();
        total_height.saturating_sub(viewport_height)
    }

    pub(crate) fn clamp_scroll_state(&mut self) {
        self.history_scroll.clamp(self.max_scroll_offset());
    }

    /// Number of lines needed for the status/error bar, based on the current
    /// message content and the available terminal width.  Returns 0 when there
    /// is no message to display.
    pub(crate) fn status_error_height(&self, width: u16) -> u16 {
        let text = if let Some(ref err) = self.error {
            err.as_str()
        } else if let Some(ref status) = self.status {
            status.as_str()
        } else {
            return 0;
        };
        let lines = plain_text_lines(text);
        lines_height(&lines, width).max(1) as u16
    }

    /// Number of visual content lines the input box currently occupies,
    /// computed from the text and terminal width.
    pub(crate) fn input_bar_content_lines(&self, term_width: u16) -> u16 {
        let inner = term_width.saturating_sub(2) as usize;
        if inner < 1 {
            return 1;
        }
        let visual = compute_visual_lines(&self.input.text, inner);
        (visual.len() as u16).clamp(MIN_INPUT_CONTENT_LINES, MAX_INPUT_CONTENT_LINES)
    }

    /// Total height of the input bar (content + borders).
    pub(crate) fn input_bar_height(&self, term_width: u16) -> u16 {
        self.input_bar_content_lines(term_width) + 2
    }

    pub(crate) fn update_viewport_from_terminal_size(&mut self) {
        // Resolve the terminal size first so we have a width to pass to
        // status_error_height for the wrapped-line calculation.
        let size = if self.terminal_resized || self.last_terminal_size.is_none() {
            if let Ok(size) = crossterm::terminal::size() {
                self.last_terminal_size = Some(size);
                self.terminal_resized = false;
                size
            } else {
                return;
            }
        } else {
            match self.last_terminal_size {
                Some(s) => s,
                None => return,
            }
        };
        let (width, height) = size;
        let bottom_height =
            self.input_bar_height(width) + STATUS_BAR_HEIGHT + self.status_error_height(width);
        if width > 1 && height > bottom_height {
            let old_width = self.history_viewport.width;
            let old_height = self.history_viewport.height;
            let new_height = height - bottom_height;
            self.history_viewport.update(Rect {
                x: 0,
                y: 0,
                width: width - 1,
                height: new_height,
            });
            if old_width != width || old_height != new_height {
                for cached in &mut self.render_cache {
                    *cached = None;
                }
                self.markers_dirty = true;
            }
        }
    }

    pub(crate) fn mark_terminal_resized(&mut self) {
        self.terminal_resized = true;
    }

    pub(crate) fn effective_scroll(&self) -> usize {
        self.history_scroll
            .effective_scroll(self.max_scroll_offset())
    }

    /// Populate `rendered_images` from a turn's `displayed_images`.
    ///
    /// Each `DisplayedImageRecord` is converted to a `RenderedImage::new_placeholder`
    /// so the inline/fullscreen render path can find it and submit encoding jobs
    /// to the background worker.  Called whenever a turn arrives.
    pub(crate) fn sync_turn_images(&mut self, turn_id: u32, turn: &Turn) {
        let images = self.rendered_images.entry(turn_id).or_default();
        for (idx, record) in turn.displayed_images.iter().enumerate() {
            // Only insert missing entries — preserve cached protocols across
            // re-syncs (e.g. TurnAppended → TurnFinalized with same images).
            images.entry(idx).or_insert_with(|| {
                RenderedImage::new_placeholder(
                    record.metadata.clone(),
                    Arc::from(record.data.clone()),
                )
            });
        }
        // Remove entries for images that no longer exist in the turn.
        images.retain(|&idx, _| idx < turn.displayed_images.len());
    }

    /// Stable image block height for layout and scroll calculations.
    ///
    /// Uses half the viewport height as a heuristic.  This value is stable
    /// within a given terminal size so scroll positions don't shift when
    /// encoding completes.
    pub(crate) fn image_block_height(&self) -> u16 {
        (self.history_viewport.height / 2).max(1)
    }

    pub(crate) fn ensure_cache_synced(&mut self) {
        let turns_len = self.visible_turn_ids.len();
        let cache_len = self.render_cache.len();
        if cache_len == turns_len {
            return;
        }
        if cache_len > turns_len {
            self.render_cache.drain(0..(cache_len - turns_len));
            return;
        }
        self.render_cache.resize(turns_len, None);
    }

    /// Apply a completed ImageResult to the corresponding rendered_images entry.
    pub(crate) fn apply_image_result(&mut self, result: ImageResult) {
        let (turn_id, img_idx) = match self.pending_job_idx.remove(&result.id) {
            Some(key) => key,
            None => return,
        };
        if let Some(images) = self.rendered_images.get_mut(&turn_id)
            && let Some(img) = images.get_mut(&img_idx)
            && img.pending_job == Some(result.id)
        {
            tracing::trace!(
                "[tai-tui] image job {} completed for turn {} img {}",
                result.id,
                turn_id,
                img_idx,
            );
            img.apply_result(result);
            // Image encoding completes without changing block sizes
            // (`image_block_height()` depends only on viewport height),
            // so height_prefix / turn_layouts remain valid.  No need to
            // trigger a rebuild.
        }
    }

    /// Submit an encoding job for a turn-displayed image.
    pub(crate) fn submit_image_job(
        &mut self,
        turn_id: u32,
        img_idx: usize,
        data: std::sync::Arc<[u8]>,
        metadata: tai_proto::ImageMetadata,
        cell_size: Size,
        resize: ratatui_image::Resize,
    ) -> Option<ImageId> {
        let tx = self.image_job_tx.as_ref()?;
        let id = next_job_id();

        tracing::trace!(
            "[tai-tui] submitting image job {} for turn {} img {} ({} {}x{} @ {}x{})",
            id,
            turn_id,
            img_idx,
            metadata.mime_type,
            metadata.width,
            metadata.height,
            cell_size.width,
            cell_size.height,
        );

        self.pending_job_idx.insert(id, (turn_id, img_idx));

        if let Some(images) = self.rendered_images.get_mut(&turn_id)
            && let Some(img) = images.get_mut(&img_idx)
        {
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
    pub(crate) fn reset_for_session_switch(&mut self) {
        self.session_view = SessionView::new();
        self.rendered_images.clear();
        self.render_cache.clear();
        self.visible_turn_ids.clear();
        self.history_scroll = HistoryScrollState::new();
        self.pending_job_idx.clear();
        self.active.clear();
        self.markers.clear();
        self.height_prefix.clear();
        self.turn_layouts.clear();
        self.markers_dirty = true;
        self.status = None;
        self.error = None;
        self.fullscreen_image_target = None;
    }

    pub(crate) fn scroll_up(&mut self, amount: usize) {
        self.history_scroll
            .scroll_up(amount, self.max_scroll_offset());
    }

    pub(crate) fn scroll_down(&mut self, amount: usize) {
        self.history_scroll
            .scroll_down(amount, self.max_scroll_offset());
    }

    pub(crate) fn scroll_to(&mut self, row: usize) {
        let max_scroll = self.max_scroll_offset();
        let amount = row.min(max_scroll);
        self.history_scroll.scroll = amount;
        self.history_scroll.scroll_compensation = 0;
    }

    pub(crate) fn scroll_to_track_row(&mut self, mouse_row: u16, track_height: u16) {
        let track_height = track_height as usize;
        if track_height > 1 {
            let row = (mouse_row as usize).min(track_height.saturating_sub(1));
            let max_scroll = self.max_scroll_offset();
            let ratio = row as f64 / track_height.saturating_sub(1) as f64;
            let target = (ratio * max_scroll as f64).round() as usize;
            self.scroll_to(max_scroll.saturating_sub(target.min(max_scroll)));
        }
    }

    pub(crate) fn scroll_to_content_line(&mut self, content_line: usize) {
        let total = self.total_history_height();
        let viewport = self.history_viewport.height as usize;
        let target = total.saturating_sub(content_line + viewport);
        self.scroll_to(target.min(self.max_scroll_offset()));
    }

    /// Scroll up by one scrollbar notch — the amount the viewport moves when
    /// the user clicks or scroll-wheels one "row" on the scrollbar track.
    /// Each notch is the smallest movement that visibly shifts the content:
    /// `ceil(max_scroll / track_height)`, at least 1.
    pub(crate) fn scrollbar_scroll_up(&mut self) {
        let notch = self.scrollbar_notch();
        self.scroll_up(notch);
    }

    /// Scroll down by one scrollbar notch.
    pub(crate) fn scrollbar_scroll_down(&mut self) {
        let notch = self.scrollbar_notch();
        self.scroll_down(notch);
    }

    /// Compute the scrollbar notch size — the amount of content lines that
    /// a single scrollbar-track row corresponds to.
    fn scrollbar_notch(&self) -> usize {
        let track = self.history_viewport.height as usize;
        let max_scroll = self.max_scroll_offset();
        if track > 0 {
            // Integer ceiling division: ceil(max_scroll / track).
            // checked_div is safe here because we've verified track > 0.
            max_scroll
                .saturating_add(track)
                .saturating_sub(1)
                .checked_div(track)
                .unwrap_or(0)
        } else {
            // Degenerate case: viewport has zero height.  Just use
            // max_scroll — the .max(1) below ensures we always move at
            // least one line.
            max_scroll
        }
        .max(1)
    }

    pub(crate) fn apply_scroll_delta(&mut self) {
        let delta = self.scroll_accumulator;
        self.scroll_accumulator = 0;
        if delta > 0 {
            self.scroll_up(delta as usize);
        } else if delta < 0 {
            self.scroll_down((-delta) as usize);
        }
    }

    /// Collect user_text contents from session_view.turns, newest first.
    pub(crate) fn user_texts(&self) -> Vec<String> {
        self.session_view
            .turns
            .iter()
            .rev()
            .filter_map(|(_, turn)| turn.user_text.clone())
            .collect()
    }

    pub(crate) fn navigate_history_up(&mut self) {
        let texts = self.user_texts();
        if texts.is_empty() {
            return;
        }
        if self.history_index.is_none() {
            self.saved_draft = self.input.text.clone();
            self.history_index = Some(0);
            self.input.text = texts[0].to_string();
        } else if let Some(idx) = self.history_index {
            let next = idx + 1;
            if next < texts.len() {
                self.history_index = Some(next);
                self.input.text = texts[next].to_string();
            }
        }
        self.input.cursor = self.input.text.len();
    }

    pub(crate) fn navigate_history_down(&mut self) {
        if let Some(idx) = self.history_index {
            let texts = self.user_texts();
            if idx > 0 {
                let prev = idx - 1;
                self.history_index = Some(prev);
                self.input.text = texts[prev].to_string();
            } else {
                self.history_index = None;
                self.input.text = self.saved_draft.clone();
                self.saved_draft.clear();
            }
            self.input.cursor = self.input.text.len();
        }
    }

    pub(crate) fn commit_to_history(&mut self) {
        self.history_index = None;
        self.saved_draft.clear();
    }

    pub(crate) fn set_page(&mut self, page: Page) {
        self.page = page;
        self.progress_dirty = true;
    }

    // ── Legacy per-session daemon message handlers ─────────────────────

    pub(crate) fn handle_session_created(
        &mut self,
        session_id: u64,
        client_tx: &std::sync::mpsc::Sender<ClientMessage>,
    ) -> Result<(), ClientError> {
        if self.page == Page::SessionManager {
            let _ = client_tx.send(ClientMessage::ListSessions);
        } else {
            self.reset_for_session_switch();
            self.attached_session_id = Some(session_id);
            client_tx
                .send(ClientMessage::AttachSession { session_id })
                .map_err(broken_pipe)?;
        }
        Ok(())
    }

    pub(crate) fn handle_session_attached(&mut self, session_id: u64) {
        self.attached_session_id = Some(session_id);
        if let Some(s) = self
            .session_mgr
            .sessions
            .iter()
            .find(|s| s.session_id == session_id)
        {
            self.attached_token_usage = s.token_usage;
            self.attached_context_window = s.context_window;
            self.attached_last_prompt_tokens = s.last_prompt_tokens;
            self.attached_account_name = s.account_name.clone();
            self.attached_model = s.selected_model.clone();
            self.attached_reasoning_effort = s.reasoning_effort;
            self.attached_working_dir = s.working_dir.clone();
            self.attached_status = Some(s.status.clone());
        }
        self.refresh_attached_provider_slug();
        self.progress_dirty = true;
    }

    pub(crate) fn refresh_attached_provider_slug(&mut self) {
        self.attached_provider_slug = self.attached_account_name.as_ref().and_then(|name| {
            self.ai_providers
                .accounts
                .iter()
                .find(|a| a.name == *name)
                .map(|a| a.provider.clone())
        });
    }

    fn attached_session_mut(&mut self) -> Option<&mut SessionSummary> {
        self.session_mgr
            .sessions
            .iter_mut()
            .find(|s| Some(s.session_id) == self.attached_session_id)
    }

    pub(crate) fn handle_model_selected(&mut self, model: &str) {
        if self.attached_session_id.is_some() {
            self.attached_model = Some(model.to_owned());
            if let Some(s) = self.attached_session_mut() {
                s.selected_model = Some(model.to_owned());
            }
        }
    }

    pub(crate) fn handle_reasoning_effort_set(&mut self, effort: ThinkingEffort) {
        if self.attached_session_id.is_some() {
            self.attached_reasoning_effort = Some(effort);
            if let Some(s) = self.attached_session_mut() {
                s.reasoning_effort = Some(effort);
            }
        }
    }

    pub(crate) fn handle_session_working_dir_set(
        &mut self,
        session_id: u64,
        path: &Option<String>,
    ) {
        if self.attached_session_id == Some(session_id) {
            self.attached_working_dir = path.clone();
            self.progress_dirty = true;
            if let Some(s) = self.attached_session_mut() {
                s.working_dir = path.clone();
            }
        }
    }

    pub(crate) fn handle_session_account_set(&mut self, account: &str) {
        if self.attached_session_id.is_some() {
            self.attached_account_name = Some(account.to_owned());
            self.refresh_attached_provider_slug();
            if let Some(s) = self.attached_session_mut() {
                s.account_name = Some(account.to_owned());
            }
        }
    }

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
        if self.attached_session_id == Some(session_id) {
            self.attached_status = Some(status.clone());
        }
    }

    pub(crate) fn handle_accounts(&mut self, accounts: &[AccountInfo]) {
        self.ai_providers.set_accounts(accounts.to_vec());
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
                            selected_model: None,
                            reasoning_effort: None,
                        })
                        .map_err(broken_pipe)?;
                }
            }
        }
        Ok(())
    }

    pub(crate) fn handle_session_deleted(&mut self, session_id: u64) {
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

    pub(crate) fn handle_session_delete_failed(&mut self, session_id: u64, error: &str) {
        self.status = Some(format!(
            "failed to delete session {}: {}",
            session_id, error
        ));
    }

    pub(crate) fn display_token_usage(&self) -> Option<TokenUsage> {
        let auth = self.attached_token_usage.as_ref()?;
        Some(TokenUsage {
            input_tokens: auth.input_tokens + self.live_input_estimate,
            output_tokens: auth.output_tokens + self.live_output_tokens,
            total_tokens: auth.total_tokens + self.live_input_estimate + self.live_output_tokens,
        })
    }
}

// ── TurnEventHandler implementation ──────────────────────────────────

impl TurnEventHandler for App {
    fn handle_turn_appended(&mut self, turn_id: u32, turn: Turn) {
        tracing::trace!(%turn_id, "handle_turn_appended");
        self.sync_turn_images(turn_id, &turn);
        self.session_view.insert_or_replace(turn_id, turn);
        self.markers_dirty = true;
    }

    fn handle_turn_finalized(&mut self, turn_id: u32, turn: Turn) {
        tracing::trace!(%turn_id, "handle_turn_finalized");
        self.sync_turn_images(turn_id, &turn);
        self.session_view.insert_or_replace(turn_id, turn);
        self.markers_dirty = true;
    }

    fn handle_turns_undone(&mut self, turn_ids: &[u32]) {
        tracing::trace!(?turn_ids, "handle_turns_undone");
        for tid in turn_ids {
            if let Some(turn) = self.session_view.turns.get_mut(tid) {
                turn.undone = true;
            }
        }
        self.markers_dirty = true;
    }

    fn handle_turns_redone(&mut self, turns: std::collections::BTreeMap<u32, Turn>) {
        tracing::trace!(?turns, "handle_turns_redone");
        for (tid, turn) in turns {
            self.sync_turn_images(tid, &turn);
            self.session_view.insert_or_replace(tid, turn);
        }
        self.markers_dirty = true;
    }

    fn handle_request_stream(&mut self, request_id: u32, stream: OutputStream, data: Cow<'_, str>) {
        self.session_view.stream_chunk(request_id, stream, &data);
        self.markers_dirty = true;
    }

    fn handle_started(&mut self, request_id: u32, turn_id: u32, estimated_prompt_tokens: u32) {
        tracing::trace!(%request_id, %turn_id, %estimated_prompt_tokens, "handle_started");
        self.session_view
            .request_to_turn
            .insert(request_id, turn_id);
        self.active.insert(request_id);
        self.live_input_estimate = estimated_prompt_tokens;
        self.live_output_tokens = 0;
    }

    fn handle_done(
        &mut self,
        request_id: u32,
        token_usage: Option<TokenUsage>,
        last_prompt_tokens: Option<u32>,
    ) {
        tracing::trace!(%request_id, "handle_done");
        self.session_view.request_to_turn.remove(&request_id);
        self.active.remove(&request_id);
        if let Some(usage) = token_usage {
            self.attached_token_usage = Some(usage);
        }
        if let Some(tokens) = last_prompt_tokens {
            self.attached_last_prompt_tokens = Some(tokens);
        }
        self.live_input_estimate = 0;
        self.live_output_tokens = 0;
        self.markers_dirty = true;
    }

    fn handle_failed(&mut self, request_id: u32, error: String) {
        tracing::trace!(%request_id, %error, "handle_failed");
        self.error = Some(error);
        self.session_view.request_to_turn.remove(&request_id);
        self.active.remove(&request_id);
        self.markers_dirty = true;
    }

    fn handle_tool_call_event(&mut self, request_id: u32, event: ToolCallEvent) {
        match event {
            ToolCallEvent::Started {
                call_id,
                tool_name,
                arguments_json,
            } => {
                self.session_view
                    .tool_call_started(request_id, call_id, tool_name, arguments_json);
                self.markers_dirty = true;
            }
            ToolCallEvent::Finished { .. } => {
                // Finished events are informational — tool results arrive via
                // TurnAppended/TurnFinalized.
            }
            ToolCallEvent::Failed { .. } => {
                // Failed events are informational — is_error will be set in
                // the Turn's tool_results via TurnAppended/TurnFinalized.
            }
        }
    }

    fn handle_tool_result_chunk(&mut self, request_id: u32, call_id: String, data: Vec<u8>) {
        let text = String::from_utf8_lossy(&data).into_owned();
        self.session_view
            .tool_result_chunk(request_id, &call_id, &text);
        self.markers_dirty = true;
    }

    fn handle_session_state(&mut self, state: SessionStateData) {
        tracing::debug!(
            turn_count = %state.turns.len(),
            ?state.title,
            ?state.selected_model,
            ?state.status,
            "handle_session_state"
        );
        let SessionStateData {
            turns,
            title: _,
            selected_model,
            token_usage,
            context_window,
            last_prompt_tokens,
            status,
            ..
        } = state;
        // Sync images from the incoming turns before assigning to self,
        // avoiding a borrow conflict between &mut self and &self.session_view.
        self.rendered_images.clear();
        for (tid, turn) in &turns {
            self.sync_turn_images(*tid, turn);
        }
        self.session_view.turns = turns;
        self.attached_model = selected_model;
        self.attached_token_usage = token_usage;
        self.attached_context_window = context_window;
        self.attached_last_prompt_tokens = last_prompt_tokens;
        self.attached_status = Some(status);
        self.markers_dirty = true;
    }

    fn handle_token_usage_update(
        &mut self,
        token_usage: TokenUsage,
        last_prompt_tokens: Option<u32>,
    ) {
        tracing::trace!(
            ?token_usage,
            ?last_prompt_tokens,
            "handle_token_usage_update"
        );
        self.attached_token_usage = Some(token_usage);
        if let Some(tokens) = last_prompt_tokens {
            self.attached_last_prompt_tokens = Some(tokens);
        }
        self.live_input_estimate = 0;
        self.live_output_tokens = 0;
        self.progress_dirty = true;
    }

    fn handle_status_text(&mut self, text: String) {
        self.status = Some(text);
    }

    fn handle_error(&mut self, error: String) {
        self.error = Some(error);
    }

    fn handle_session_attached(&mut self, session_id: u64) {
        self.attached_session_id = Some(session_id);
    }

    fn handle_session_created(
        &mut self,
        _session_id: u64,
        _title: Option<String>,
        _working_dir: Option<String>,
        _max_turns: Option<u32>,
    ) {
    }

    fn handle_session_status_changed(&mut self, session_id: u64, status: SessionStatus) {
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
        if self.attached_session_id == Some(session_id) {
            self.attached_status = Some(status);
        }
    }
}

#[cfg(test)]
pub(crate) fn history_text_height(text: &str, width: u16) -> usize {
    lines_height(&crate::markdown_render::plain_text_lines(text), width)
}

/// Find the visible turn index and the content-line offset within that
/// turn for a given screen row.  Binary search on `height_prefix`.
/// Returns `(turn_idx, offset_within_turn)`.
pub(crate) fn find_turn_at_row(app: &App, screen_row: u16) -> Option<(usize, usize)> {
    let vh = app.history_viewport.height;
    if screen_row >= vh {
        return None;
    }

    let effective_scroll = app.effective_scroll();
    let total_height = app.total_history_height();

    let content_line = total_height
        .saturating_sub(effective_scroll + vh as usize)
        .saturating_add(screen_row as usize);

    if content_line >= total_height {
        return None;
    }

    // Binary search on the prefix-sum array.
    let i = app.height_prefix.partition_point(|&p| p <= content_line);
    if i < app.height_prefix.len() {
        let turn_start = i
            .checked_sub(1)
            .and_then(|prev| app.height_prefix.get(prev))
            .copied()
            .unwrap_or(0);
        let offset = content_line.saturating_sub(turn_start);
        Some((i, offset))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::test_app;

    fn make_session(id: u64, title: &str) -> SessionSummary {
        SessionSummary {
            session_id: id,
            title: Some(title.into()),
            selected_model: None,
            reasoning_effort: None,
            parent_session_id: None,
            working_dir: None,
            created_at: 1000,
            turn_count: 0,
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
            turn_count: 0,
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
        mgr.selection = Some(1);
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
        mgr.remove_session(1);
        assert_eq!(mgr.scroll, 1);
        assert_eq!(mgr.sessions.len(), 2);
    }

    // ── scroll_to_content_line ──

    #[test]
    fn scroll_to_content_line_scrolls_to_content_line() {
        let mut app = test_app("/tmp/tai.sock");
        app.history_viewport.width = 80;
        app.history_viewport.height = 5;

        // Fill with enough turn content.
        for i in 0..5u32 {
            let turn = Turn {
                created_at: tai_proto::TimestampMs::now(),
                undone: false,
                error: None,
                user_text: Some(format!("user text {i}")),
                assistant_text: Some(format!("assistant text {i}")),
                assistant_reasoning: None,
                tool_calls: vec![],
                token_usage: None,
                tool_results: vec![],
                displayed_images: vec![],
            };
            app.session_view.insert_or_replace(i, turn);
        }
        app.rebuild_height_prefix();

        // content_line=0 → first visible turn
        app.scroll_to_content_line(0);
        // Should scroll to top (max scroll)
        assert_eq!(app.effective_scroll(), app.max_scroll_offset());
    }

    // ── find_turn_at_row ──

    #[test]
    fn find_turn_at_row_returns_none_out_of_bounds() {
        let app = test_app("/tmp/tai.sock");
        assert!(find_turn_at_row(&app, 999).is_none());
    }

    #[test]
    fn find_turn_at_row_returns_turn_idx_and_offset() {
        let mut app = test_app("/tmp/tai.sock");
        app.history_viewport.width = 80;
        app.history_viewport.height = 200;
        let turn = Turn {
            created_at: tai_proto::TimestampMs::now(),
            undone: false,
            error: None,
            user_text: Some("hello".into()),
            assistant_text: Some("world".into()),
            assistant_reasoning: None,
            tool_calls: vec![],
            token_usage: None,
            tool_results: vec![],
            displayed_images: vec![],
        };
        app.session_view.insert_or_replace(1, turn);
        app.rebuild_height_prefix();

        // Screen row 0 should map to turn_idx 0 and offset 0.
        let (turn_idx, offset) = find_turn_at_row(&app, 0).unwrap();
        assert_eq!(turn_idx, 0);
        assert_eq!(offset, 0);
    }

    // ── scrollbar_notch ──

    #[test]
    fn scrollbar_notch_no_content() {
        let mut app = test_app("/tmp/tai.sock");
        app.history_viewport.width = 80;
        app.history_viewport.height = 20;
        // No content → total_height = 0 → max_scroll = 0 → notch clamps to 1
        assert_eq!(app.scrollbar_notch(), 1);
    }

    #[test]
    fn scrollbar_notch_track_one() {
        let mut app = test_app("/tmp/tai.sock");
        app.history_viewport.width = 80;
        app.history_viewport.height = 1;
        app.height_prefix.push(50);
        // max_scroll = 50 - 1 = 49, notch = ceil(49 / 1) = 49
        assert_eq!(app.scrollbar_notch(), 49);
    }

    #[test]
    fn scrollbar_notch_ceiling_division() {
        let mut app = test_app("/tmp/tai.sock");
        app.history_viewport.width = 80;
        app.history_viewport.height = 50;
        app.height_prefix.push(150);
        // max_scroll = 150 - 50 = 100, notch = ceil(100 / 50) = 2
        assert_eq!(app.scrollbar_notch(), 2);
    }

    #[test]
    fn scrollbar_notch_rounds_up() {
        let mut app = test_app("/tmp/tai.sock");
        app.history_viewport.width = 80;
        app.history_viewport.height = 30;
        app.height_prefix.push(105);
        // max_scroll = 105 - 30 = 75, notch = ceil(75 / 30) = 3
        assert_eq!(app.scrollbar_notch(), 3);
    }

    // ── scrollbar_scroll_up / scrollbar_scroll_down ──

    #[test]
    fn scrollbar_scroll_up_increases_scroll_by_notch() {
        let mut app = test_app("/tmp/tai.sock");
        app.history_viewport.width = 80;
        app.history_viewport.height = 10;
        app.height_prefix.push(110);
        // max_scroll = 100, notch = ceil(100 / 10) = 10

        // Start at the bottom (scroll = 0).
        app.history_scroll.scroll = 0;
        let before = app.effective_scroll();

        app.scrollbar_scroll_up();

        assert_eq!(app.effective_scroll(), before + 10);
    }

    #[test]
    fn scrollbar_scroll_up_clamps_at_max_scroll() {
        let mut app = test_app("/tmp/tai.sock");
        app.history_viewport.width = 80;
        app.history_viewport.height = 10;
        app.height_prefix.push(110);
        // max_scroll = 100, notch = 10

        // Start at the top (scroll = 100) — already at max.
        app.history_scroll.scroll = 100;

        app.scrollbar_scroll_up();

        // Should not exceed max_scroll.
        assert_eq!(app.effective_scroll(), 100);
    }

    #[test]
    fn scrollbar_scroll_down_decreases_scroll_by_notch() {
        let mut app = test_app("/tmp/tai.sock");
        app.history_viewport.width = 80;
        app.history_viewport.height = 10;
        app.height_prefix.push(110);
        // max_scroll = 100, notch = 10

        // Start at the top (scroll = 100).
        app.history_scroll.scroll = 100;
        let before = app.effective_scroll();

        app.scrollbar_scroll_down();

        assert_eq!(app.effective_scroll(), before - 10);
    }

    #[test]
    fn scrollbar_scroll_down_clamps_at_zero() {
        let mut app = test_app("/tmp/tai.sock");
        app.history_viewport.width = 80;
        app.history_viewport.height = 10;
        app.height_prefix.push(110);
        // max_scroll = 100, notch = 10

        // Start at scroll = 5 — less than one notch from bottom.
        app.history_scroll.scroll = 5;

        app.scrollbar_scroll_down();

        // Should clamp to 0 (not underflow).
        assert_eq!(app.effective_scroll(), 0);
    }

    // ── scroll_to_content_line ──

    #[test]
    fn scroll_to_content_line_idempotent_when_already_visible() {
        let mut app = test_app("/tmp/tai.sock");
        app.history_viewport.width = 80;
        app.history_viewport.height = 200;

        // Single short turn that fits entirely in the viewport.
        let turn = Turn {
            created_at: tai_proto::TimestampMs::now(),
            undone: false,
            error: None,
            user_text: Some("hello".into()),
            assistant_text: Some("world".into()),
            assistant_reasoning: None,
            tool_calls: vec![],
            token_usage: None,
            tool_results: vec![],
            displayed_images: vec![],
        };
        app.session_view.insert_or_replace(0, turn);
        app.rebuild_height_prefix();
        // total_height <= viewport_height → max_scroll = 0 → already at top.

        let before = app.effective_scroll();
        app.scroll_to_content_line(0);
        assert_eq!(app.effective_scroll(), before);
    }

    #[test]
    fn scroll_to_content_line_large_content_line_saturates() {
        let mut app = test_app("/tmp/tai.sock");
        app.history_viewport.width = 80;
        app.history_viewport.height = 5;

        for i in 0..5u32 {
            let turn = Turn {
                created_at: tai_proto::TimestampMs::now(),
                undone: false,
                error: None,
                user_text: Some(format!("user text {i}")),
                assistant_text: Some(format!("assistant text {i}")),
                assistant_reasoning: None,
                tool_calls: vec![],
                token_usage: None,
                tool_results: vec![],
                displayed_images: vec![],
            };
            app.session_view.insert_or_replace(i, turn);
        }
        app.rebuild_height_prefix();

        // content_line larger than total height → should saturate to scroll=0.
        app.scroll_to_content_line(9999);
        assert_eq!(app.effective_scroll(), 0);
    }

    // ── status_error_height ──

    #[test]
    fn status_error_height_neither_set_returns_zero() {
        let app = test_app("/tmp/tai.sock");
        assert_eq!(app.status_error_height(80), 0);
    }

    #[test]
    fn status_error_height_short_error_returns_one() {
        let mut app = test_app("/tmp/tai.sock");
        app.error = Some("oops".into());
        assert_eq!(app.status_error_height(80), 1);
    }

    #[test]
    fn status_error_height_short_status_returns_one() {
        let mut app = test_app("/tmp/tai.sock");
        app.status = Some("all good".into());
        assert_eq!(app.status_error_height(80), 1);
    }

    #[test]
    fn status_error_height_error_preferred_over_status() {
        let mut app = test_app("/tmp/tai.sock");
        app.error = Some("error".into());
        app.status = Some("status".into());
        // Should use error text, not status text
        assert_eq!(app.status_error_height(80), 1);
    }

    #[test]
    fn status_error_height_wrapping() {
        let mut app = test_app("/tmp/tai.sock");
        // A 10-char line at width 5 wraps to 2 lines
        app.error = Some("12345 7890".into());
        assert_eq!(app.status_error_height(5), 2);
    }

    #[test]
    fn status_error_height_multi_line() {
        let mut app = test_app("/tmp/tai.sock");
        // Three explicit lines via \n
        app.status = Some("line a\nline b\nline c".into());
        // Each line fits in width 80, so total = 3
        assert_eq!(app.status_error_height(80), 3);
    }

    #[test]
    fn status_error_height_multi_line_with_wrapping() {
        let mut app = test_app("/tmp/tai.sock");
        // Two lines, second wraps
        app.error = Some("hello\n12345 7890".into());
        // line 1: "hello" → 1 line
        // line 2: "12345 7890" → wraps to 2 lines at width 5
        // total = 3
        assert_eq!(app.status_error_height(5), 3);
    }

    #[test]
    fn status_error_height_empty_after_clearing() {
        let mut app = test_app("/tmp/tai.sock");
        app.error = Some("error".into());
        // Clear it
        app.error = None;
        assert_eq!(app.status_error_height(80), 0);
    }

    #[test]
    fn status_error_height_status_takes_over_when_error_cleared() {
        let mut app = test_app("/tmp/tai.sock");
        app.status = Some("status".into());
        assert_eq!(app.status_error_height(80), 1);
    }

    #[test]
    fn sync_turn_images_populates_rendered_images() {
        let mut app = test_app("/tmp/tai.sock");
        let metadata = tai_proto::ImageMetadata {
            mime_type: "image/svg+xml".to_string(),
            width: 100,
            height: 200,
            byte_len: 50,
            alt: None,
        };
        let turn = Turn {
            created_at: tai_proto::TimestampMs::now(),
            undone: false,
            error: None,
            user_text: None,
            assistant_text: None,
            assistant_reasoning: None,
            tool_calls: vec![],
            token_usage: None,
            tool_results: vec![],
            displayed_images: vec![
                tai_proto::DisplayedImageRecord {
                    metadata: metadata.clone(),
                    data: b"svg-data".to_vec(),
                    tool_call_id: Some("call-1".into()),
                },
                tai_proto::DisplayedImageRecord {
                    metadata: metadata.clone(),
                    data: b"more-svg".to_vec(),
                    tool_call_id: None,
                },
            ],
        };
        app.sync_turn_images(42, &turn);

        let images = app.rendered_images.get(&42).unwrap();
        assert_eq!(images.len(), 2);
        assert_eq!(images[&0].data.as_ref(), b"svg-data");
        assert_eq!(images[&1].data.as_ref(), b"more-svg");
        // Second call is idempotent — preserves existing entries
        app.sync_turn_images(42, &turn);
        assert_eq!(app.rendered_images.get(&42).unwrap().len(), 2);
    }

    // ── TurnImageLayout image_ranges ──

    #[test]
    fn turn_layout_empty_when_no_images() {
        let mut app = test_app("/tmp/tai.sock");
        app.history_viewport.width = 80;
        app.history_viewport.height = 20;
        let turn = Turn {
            created_at: tai_proto::TimestampMs::now(),
            undone: false,
            error: None,
            user_text: Some("hello".into()),
            assistant_text: Some("world".into()),
            assistant_reasoning: None,
            tool_calls: vec![],
            token_usage: None,
            tool_results: vec![],
            displayed_images: vec![],
        };
        app.session_view.insert_or_replace(1, turn);
        app.rebuild_height_prefix();

        assert_eq!(app.turn_layouts.len(), 1);
        assert!(app.turn_layouts[0].image_ranges.is_empty());
    }

    #[test]
    fn turn_layout_populates_image_ranges_with_fallback_height() {
        let mut app = test_app("/tmp/tai.sock");
        app.history_viewport.width = 80;
        app.history_viewport.height = 20;
        let metadata = tai_proto::ImageMetadata {
            mime_type: "image/png".to_string(),
            width: 100,
            height: 100,
            byte_len: 500,
            alt: None,
        };
        let turn = Turn {
            created_at: tai_proto::TimestampMs::now(),
            undone: false,
            error: None,
            user_text: None,
            assistant_text: Some("short".into()),
            assistant_reasoning: None,
            tool_calls: vec![],
            token_usage: None,
            tool_results: vec![],
            displayed_images: vec![
                tai_proto::DisplayedImageRecord {
                    metadata: metadata.clone(),
                    data: vec![0u8; 10],
                    tool_call_id: None,
                },
                tai_proto::DisplayedImageRecord {
                    metadata: metadata.clone(),
                    data: vec![1u8; 10],
                    tool_call_id: None,
                },
            ],
        };
        let turn_clone = turn.clone();
        app.session_view.insert_or_replace(2, turn);
        app.sync_turn_images(2, &turn_clone);
        app.rebuild_height_prefix();

        assert_eq!(app.turn_layouts.len(), 1);
        let layout = &app.turn_layouts[0];
        assert_eq!(layout.image_ranges.len(), 2);

        // Fallback height: viewport_height / 2 = 10.
        let fallback_h = app.image_block_height() as usize;
        // Text "short" renders at least 1 line.
        let text_h = lines_height(
            &render_turn_lines(&app.session_view.turns[&2], 71, 76),
            app.history_viewport.width,
        )
        .max(1);

        let (s0, e0) = layout.image_ranges[0];
        assert_eq!(s0, text_h);
        assert_eq!(e0, text_h + fallback_h);

        let (s1, e1) = layout.image_ranges[1];
        assert_eq!(s1, text_h + fallback_h);
        assert_eq!(e1, text_h + 2 * fallback_h);
    }

    // ── apply_image_result ──

    #[test]
    fn apply_image_result_clears_pending_job_and_records_failure() {
        use tai_tui::image_worker::next_job_id;

        let mut app = test_app("/tmp/tai.sock");
        app.history_viewport.width = 80;
        app.history_viewport.height = 20;
        let metadata = tai_proto::ImageMetadata {
            mime_type: "image/png".to_string(),
            width: 100,
            height: 100,
            byte_len: 500,
            alt: None,
        };
        let turn = Turn {
            created_at: tai_proto::TimestampMs::now(),
            undone: false,
            error: None,
            user_text: None,
            assistant_text: None,
            assistant_reasoning: None,
            tool_calls: vec![],
            token_usage: None,
            tool_results: vec![],
            displayed_images: vec![tai_proto::DisplayedImageRecord {
                metadata: metadata.clone(),
                data: vec![3u8; 30],
                tool_call_id: None,
            }],
        };
        let turn_clone = turn.clone();
        app.session_view.insert_or_replace(4, turn);
        app.sync_turn_images(4, &turn_clone);

        let img_id = next_job_id();
        app.pending_job_idx.insert(img_id, (4, 0));
        let img = app
            .rendered_images
            .get_mut(&4)
            .unwrap()
            .get_mut(&0)
            .unwrap();
        img.pending_job = Some(img_id);

        let inline_size = Size::new(app.history_viewport.width, app.image_block_height());
        let result = tai_tui::image_worker::ImageResult {
            id: img_id,
            protocol: None,
            cell_size: inline_size,
        };
        app.apply_image_result(result);

        let img = app.rendered_images.get(&4).unwrap().get(&0).unwrap();
        assert!(img.failed_sizes.contains(&inline_size));
        assert!(img.pending_job.is_none());
    }

    #[test]
    fn apply_image_result_records_failure_at_any_size() {
        use tai_tui::image_worker::next_job_id;

        let mut app = test_app("/tmp/tai.sock");
        app.history_viewport.width = 80;
        app.history_viewport.height = 20;
        let metadata = tai_proto::ImageMetadata {
            mime_type: "image/png".to_string(),
            width: 100,
            height: 100,
            byte_len: 500,
            alt: None,
        };
        let turn = Turn {
            created_at: tai_proto::TimestampMs::now(),
            undone: false,
            error: None,
            user_text: None,
            assistant_text: None,
            assistant_reasoning: None,
            tool_calls: vec![],
            token_usage: None,
            tool_results: vec![],
            displayed_images: vec![tai_proto::DisplayedImageRecord {
                metadata: metadata.clone(),
                data: vec![4u8; 40],
                tool_call_id: None,
            }],
        };
        let turn_clone = turn.clone();
        app.session_view.insert_or_replace(5, turn);
        app.sync_turn_images(5, &turn_clone);

        let img_id = next_job_id();
        app.pending_job_idx.insert(img_id, (5, 0));
        let img = app
            .rendered_images
            .get_mut(&5)
            .unwrap()
            .get_mut(&0)
            .unwrap();
        img.pending_job = Some(img_id);

        // Use a cell_size that is NOT the inline size.
        let non_inline_size = Size::new(80, app.image_block_height() + 1);
        let result = tai_tui::image_worker::ImageResult {
            id: img_id,
            protocol: None,
            cell_size: non_inline_size,
        };
        app.apply_image_result(result);

        let img = app.rendered_images.get(&5).unwrap().get(&0).unwrap();
        assert!(img.failed_sizes.contains(&non_inline_size));
    }
}
