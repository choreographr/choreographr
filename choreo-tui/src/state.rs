use choreo_client_core::dispatch::{SessionStateData, ToolCallEvent};
use choreo_client_core::{ClientError, SessionView, TurnEventHandler, broken_pipe};
use choreo_proto::{
    AccountInfo, ClientMessage, OutputStream, ReasoningCapability, SessionStatus, SessionSummary,
    TokenUsage, Turn,
};
use choreo_tui::RenderedImage;
use choreo_tui::image_worker::{ImageId, ImageJob, ImageResult, next_job_id};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Rect, Size};
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::markdown_render::{
    RenderedTurnLines, compute_visual_offsets, lines_height, plain_text_lines,
    reasoning_expanded_default, render_turn_lines,
};
use ratatui::text::Line;
use tui_prompts::{SelectState, State, TextState};

pub(crate) const STATUS_BAR_HEIGHT: u16 = 1;
pub(crate) const MIN_INPUT_CONTENT_LINES: u16 = 1;
pub(crate) const MAX_INPUT_CONTENT_LINES: u16 = 10;
pub(crate) const PAGE_SCROLL_LINES: usize = 3;

pub(crate) const CTRL_HELP_LINE1: &str =
    "ctrl+h help  ctrl+q quit  ctrl+a accounts  ctrl+s sessions  ctrl+r reasoning";
pub(crate) const CTRL_HELP_LINE2: &str =
    "esc stop  alt+enter continue  ctrl+up undo  ctrl+down redo";

pub(crate) const AI_PROVIDER_ITEM_LINES: usize = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Page {
    Chat,
    SessionManager,
    AIProviders,
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
    pub(crate) reasoning_effort: Option<String>,
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

/// Per-turn content-line ranges used for click hit-testing, computed
/// alongside `height_prefix`.  Maps a content-line offset within the turn to
/// the reasoning header or the correct image index — no text-height
/// recomputation needed in the click handler.
#[derive(Debug)]
pub(crate) struct TurnLayout {
    /// (start, end) content-line range of the reasoning header row(s),
    /// relative to the turn's start.  None when the turn has no reasoning.
    pub reasoning_header_range: Option<(usize, usize)>,
    /// Whether this turn's reasoning section is expanded by default, derived
    /// from turn content at layout time (an explicit header-click override in
    /// `reasoning_override` takes precedence at render time).  Stored here so
    /// the per-frame render path can compute the effective state in O(1)
    /// without re-scanning turn strings.
    pub reasoning_default_expanded: bool,
    /// (start, end) content-line ranges for each displayed image,
    /// relative to the turn's start.  Empty when the turn has no images.
    pub image_ranges: Vec<(usize, usize)>,
}

#[derive(Clone)]
pub(crate) struct RenderedCache {
    /// Turn ID this cache entry belongs to, used to detect stale entries
    /// after turns are removed/reordered.
    pub turn_id: u32,
    /// Reasoning visibility the cached lines were rendered with.  Part of
    /// the cache key: if the effective state changes without a cache
    /// invalidation, the stale entry is treated as a miss instead of being
    /// served.
    pub reasoning_expanded: bool,
    /// Semantic-line index of the reasoning header within `lines` (see
    /// [`RenderedTurnLines`]), so click hit-testing never re-scans the
    /// rendered output.
    pub reasoning_header_idx: Option<usize>,
    pub lines: Arc<[Line<'static>]>,
    pub width: u16,
    /// Full viewport width when this cache entry was computed.
    /// Stored alongside `width` (content width) so the cache key guards
    /// against skew in `lines_height` and `compute_visual_offsets`
    /// computations, which depend on viewport width.
    pub viewport_width: u16,
    pub height: usize,
    /// Cumulative visual-row offset for each semantic line.
    /// `visual_offsets[i]` = total visual rows covered by lines[0..=i].
    /// Used with `partition_point` to map a visual row → semantic line index
    /// in O(log n).
    pub visual_offsets: Arc<[usize]>,
}

/// Cached render output for a turn: the lines plus the precomputed height,
/// cumulative visual offsets, and reasoning-header semantic index.  Returned
/// from [`cached_or_compute_lines`] so callers can render and hit-test without
/// re-walking the lines.
pub(crate) type RenderedTurnCache = (Arc<[Line<'static>]>, usize, Arc<[usize]>, Option<usize>);

/// Check `render_cache[index]` for a valid entry matching `turn_id`, `width`,
/// `viewport_width`, and `reasoning_expanded`.  On hit, return the cached
/// [`RenderedTurnCache`].  On miss, call `compute`, store the result in
/// `render_cache[index]`, and return it.
///
/// When `index` is out of bounds (in-band or because the cache is shorter than
/// expected), the result is still returned but not cached.
pub(crate) fn cached_or_compute_lines(
    cache: &mut [Option<RenderedCache>],
    index: usize,
    turn_id: u32,
    width: u16,
    viewport_width: u16,
    reasoning_expanded: bool,
    compute: impl FnOnce() -> RenderedTurnLines,
) -> RenderedTurnCache {
    if let Some(Some(cached)) = cache.get(index)
        && cached.turn_id == turn_id
        && cached.width == width
        && cached.viewport_width == viewport_width
        && cached.reasoning_expanded == reasoning_expanded
    {
        return (
            Arc::clone(&cached.lines),
            cached.height,
            Arc::clone(&cached.visual_offsets),
            cached.reasoning_header_idx,
        );
    }

    let rendered = compute();
    let lines = Arc::from(rendered.lines);
    let height = lines_height(&lines, viewport_width).max(1);
    let visual_offsets = compute_visual_offsets(&lines, viewport_width);
    if let Some(slot) = cache.get_mut(index) {
        *slot = Some(RenderedCache {
            turn_id,
            reasoning_expanded,
            reasoning_header_idx: rendered.reasoning_header_idx,
            height,
            lines: Arc::clone(&lines),
            width,
            viewport_width,
            visual_offsets: Arc::clone(&visual_offsets),
        });
    }
    (lines, height, visual_offsets, rendered.reasoning_header_idx)
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
    pub(crate) turn_layouts: Vec<TurnLayout>,
    /// Per-turn explicit reasoning visibility (turn_id → expanded) set by
    /// clicking the reasoning header.  Absent entries fall back to
    /// [`reasoning_expanded_default`] (expanded while streaming, collapsed
    /// once a response exists).
    pub(crate) reasoning_override: HashMap<u32, bool>,
    pub(crate) render_cache: Vec<Option<RenderedCache>>,
    pub(crate) active: HashSet<u32>,
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
            turn_layouts: Vec::new(),
            reasoning_override: HashMap::new(),
            render_cache: Vec::new(),
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
        }
    }
}

pub(crate) struct App {
    pub(crate) input: InputBuffer,
    pub(crate) next_request_id: u32,
    pub(crate) rendered_images: HashMap<u64, HashMap<u32, HashMap<usize, RenderedImage>>>,
    pub(crate) pending_job_idx: HashMap<ImageId, (u64, u32, usize)>,
    pub(crate) history_viewport: HistoryViewport,
    pub(crate) should_quit: bool,
    pub(crate) image_job_tx: Option<crossbeam::channel::Sender<ImageJob>>,
    pub(crate) attached_session_id: Option<u64>,
    pub(crate) attached_provider_slug: Option<String>,
    pub(crate) attached_status: Option<SessionStatus>,
    pub(crate) attached_tool_groups: Vec<String>,
    pub(crate) page: Page,
    pub(crate) show_ctrl_help: bool,
    pub(crate) session_mgr: SessionManagerState,
    pub(crate) ai_providers: AIProvidersState,
    pub(crate) scroll_accumulator: isize,
    pub(crate) scrollbar_dragging: bool,
    pub(crate) last_terminal_size: Option<(u16, u16)>,
    pub(crate) terminal_resized: bool,
    pub(crate) history_index: Option<usize>,
    pub(crate) saved_draft: String,
    pub(crate) fullscreen_image_target: Option<(u64, u32, usize)>,
    pub(crate) status: Option<String>,
    pub(crate) error: Option<String>,
    pub(crate) session_displays: HashMap<u64, SessionDisplayState>,
    pub(crate) active_session_id: Option<u64>,
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
    Daemon(Box<choreo_proto::DaemonMessage>),
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
                reasoning_effort: s.reasoning_effort.clone(),
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
    /// Index of the first visual line shown in the visible window.
    /// Adjusted by `ensure_cursor_visible` after each mutation to keep
    /// the cursor in view.
    pub(crate) scroll_offset: usize,
    /// Monotonically increasing counter bumped on every text mutation.
    /// Used by `cached_visual_lines` to detect stale cache entries.
    pub(crate) generation: u64,
    /// Lazily computed visual lines, keyed by `(generation, max_width)`.
    pub(crate) lines_cache: Option<(u64, usize, Vec<VisualLineInfo>)>,
}

impl InputBuffer {
    pub(crate) fn new() -> Self {
        Self {
            text: String::new(),
            cursor: 0,
            scroll_offset: 0,
            generation: 0,
            lines_cache: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub(crate) fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
        self.scroll_offset = 0;
        self.generation += 1;
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
        self.generation += 1;
    }

    /// Insert a string at the cursor position.
    ///
    /// Used for paste events where a block of text (potentially
    /// containing newlines) is inserted all at once rather than
    /// character-by-character.
    pub(crate) fn insert_str_at_cursor(&mut self, s: &str) {
        if s.is_empty() {
            return;
        }
        self.text.insert_str(self.cursor, s);
        self.cursor += s.len();
        self.generation += 1;
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
        self.generation += 1;
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
        self.generation += 1;
    }

    pub(crate) fn delete_word_backward(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let boundary = self.word_left_boundary();
        self.text.drain(boundary..self.cursor);
        self.cursor = boundary;
        self.generation += 1;
    }

    pub(crate) fn delete_word_forward(&mut self) {
        if self.cursor >= self.text.len() {
            return;
        }
        let boundary = self.word_right_boundary();
        self.text.drain(self.cursor..boundary);
        self.generation += 1;
    }

    pub(crate) fn delete_to_start(&mut self) {
        self.text.drain(..self.cursor);
        self.cursor = 0;
        self.generation += 1;
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
            KeyCode::Char(c)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
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
        let lines = cached_visual_lines(
            &self.text,
            max_width,
            self.generation,
            &mut self.lines_cache,
        );
        let (current_line, col) = find_cursor_pos(&self.text, self.cursor, lines);
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
        let lines = cached_visual_lines(
            &self.text,
            max_width,
            self.generation,
            &mut self.lines_cache,
        );
        let (current_line, col) = find_cursor_pos(&self.text, self.cursor, lines);
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
    pub(crate) fn cursor_visual_pos(&mut self, max_width: usize) -> (u16, u16) {
        if max_width < 1 {
            return (0, 0);
        }
        let lines = cached_visual_lines(
            &self.text,
            max_width,
            self.generation,
            &mut self.lines_cache,
        );
        find_cursor_pos(&self.text, self.cursor, lines)
    }

    /// True when the cursor is on the first visual line of the input.
    pub(crate) fn is_on_first_visual_line(&mut self, max_width: usize) -> bool {
        self.cursor_visual_pos(max_width).0 == 0
    }

    /// True when the cursor is on the last visual line of the input.
    pub(crate) fn is_on_last_visual_line(&mut self, max_width: usize) -> bool {
        if max_width < 1 {
            return true;
        }
        let lines = cached_visual_lines(
            &self.text,
            max_width,
            self.generation,
            &mut self.lines_cache,
        );
        let (row, _) = find_cursor_pos(&self.text, self.cursor, lines);
        row + 1 >= lines.len() as u16
    }

    /// Adjust `scroll_offset` so the cursor's visual line is within the visible window.
    ///
    /// `max_width` is the inner width of the input box (terminal width minus borders).
    /// `visible_height` is the number of content rows available.
    pub(crate) fn ensure_cursor_visible(&mut self, max_width: usize, visible_height: usize) {
        if max_width < 1 || visible_height == 0 {
            self.scroll_offset = 0;
            return;
        }
        let lines = cached_visual_lines(
            &self.text,
            max_width,
            self.generation,
            &mut self.lines_cache,
        );
        if lines.len() <= visible_height {
            self.scroll_offset = 0;
            return;
        }
        let max_scroll = lines.len() - visible_height;
        let (cursor_row, _) = find_cursor_pos(&self.text, self.cursor, lines);
        let cursor_row = cursor_row as usize;

        // If cursor is above the visible area, scroll up.
        if cursor_row < self.scroll_offset {
            self.scroll_offset = cursor_row;
        }
        // If cursor is below the visible area, scroll down.
        if self.scroll_offset + visible_height <= cursor_row {
            self.scroll_offset = cursor_row + 1 - visible_height;
        }

        self.scroll_offset = self.scroll_offset.min(max_scroll);
    }
}

/// Return cached visual lines for `max_width`, recomputing only when
/// `max_width` or `text` has changed since the last call.
///
/// `generation` is a monotonically increasing counter from the owning
/// `InputBuffer` that is bumped on every text mutation.  The cache is
/// invalidated when either `generation` or `max_width` differs from
/// the values stored at the last computation.
///
/// Takes separate references to `text` and `cache` so callers can pass
/// field-level borrows and avoid borrow-checker conflicts with other
/// fields (e.g. `cursor`).
pub(crate) fn cached_visual_lines<'a>(
    text: &str,
    max_width: usize,
    generation: u64,
    cache: &'a mut Option<(u64, usize, Vec<VisualLineInfo>)>,
) -> &'a [VisualLineInfo] {
    let entry =
        cache.get_or_insert_with(|| (generation, max_width, compute_visual_lines(text, max_width)));
    if entry.0 != generation || entry.1 != max_width {
        entry.0 = generation;
        entry.1 = max_width;
        entry.2 = compute_visual_lines(text, max_width);
    }
    &entry.2
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
pub(crate) fn find_cursor_pos(text: &str, cursor: usize, lines: &[VisualLineInfo]) -> (u16, u16) {
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

/// Integer ceiling division: `ceil(a / b)`.
/// Returns 0 when `b == 0`.
fn ceil_div(a: usize, b: usize) -> usize {
    if b == 0 {
        return 0;
    }
    a.saturating_add(b).saturating_sub(1) / b
}

impl App {
    pub(crate) fn new() -> Self {
        Self {
            input: InputBuffer::new(),
            next_request_id: 1,
            rendered_images: HashMap::new(),
            history_viewport: HistoryViewport::new(),
            should_quit: false,
            image_job_tx: None,
            pending_job_idx: HashMap::new(),
            attached_session_id: None,
            attached_provider_slug: None,
            attached_status: None,
            attached_tool_groups: Vec::new(),
            page: Page::Chat,
            show_ctrl_help: true,
            session_mgr: SessionManagerState::new(),
            ai_providers: AIProvidersState::new(),
            scroll_accumulator: 0,
            scrollbar_dragging: false,
            history_index: None,
            saved_draft: String::new(),
            fullscreen_image_target: None,
            status: None,
            error: None,
            last_terminal_size: None,
            terminal_resized: false,
            session_displays: HashMap::new(),
            active_session_id: None,
        }
    }

    pub(crate) fn display_for(&mut self, session_id: u64) -> &mut SessionDisplayState {
        self.session_displays.entry(session_id).or_default()
    }
    pub(crate) fn active_display(&mut self) -> Option<&mut SessionDisplayState> {
        self.session_displays.get_mut(&self.active_session_id?)
    }
    pub(crate) fn active_display_ref(&self) -> Option<&SessionDisplayState> {
        self.session_displays.get(&self.active_session_id?)
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
    pub(crate) fn input_bar_content_lines(&mut self, term_width: u16) -> u16 {
        let inner = term_width.saturating_sub(2) as usize;
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

    pub(crate) fn update_viewport_from_terminal_size(&mut self) {
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
        let help_height: u16 = if self.show_ctrl_help { 2 } else { 0 };
        let bottom_height = self.input_bar_height(width)
            + STATUS_BAR_HEIGHT
            + self.status_error_height(width)
            + help_height;
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
            if old_width != width.saturating_sub(1) || old_height != self.history_viewport.height {
                for display in self.session_displays.values_mut() {
                    for cached in &mut display.render_cache {
                        *cached = None;
                    }
                    display.markers_dirty = true;
                    if old_width != width.saturating_sub(1) {
                        let new_vp_width = width.saturating_sub(1);
                        tracing::debug!(
                            "width changed ({} → {}), clearing content_dirty",
                            old_width,
                            new_vp_width,
                        );
                        display.content_dirty = false;
                    }
                }
            }
        }
    }

    pub(crate) fn mark_terminal_resized(&mut self) {
        self.terminal_resized = true;
    }

    pub(crate) fn total_history_height(&self) -> usize {
        self.active_display_ref()
            .map(|d| d.total_history_height())
            .unwrap_or(0)
    }

    #[cfg(test)]
    pub(crate) fn rebuild_height_prefix(&mut self) {
        let vp = self.history_viewport;
        if let Some(d) = self.active_display() {
            d.rebuild_height_prefix(&vp);
        }
    }

    pub(crate) fn compute_total_height_and_markers(&mut self) -> usize {
        let vp = self.history_viewport;
        self.active_display()
            .map(|d| d.compute_total_height_and_markers(&vp))
            .unwrap_or(1)
    }

    #[cfg(test)]
    pub(crate) fn mark_streaming_changed(&mut self) {
        if let Some(d) = self.active_display() {
            d.mark_streaming_changed();
        }
    }

    pub(crate) fn max_scroll_offset(&self) -> usize {
        self.active_display_ref()
            .map(|d| d.max_scroll_offset(&self.history_viewport))
            .unwrap_or(0)
    }

    pub(crate) fn clamp_scroll_state(&mut self) {
        let vp = self.history_viewport;
        if let Some(d) = self.active_display() {
            d.clamp_scroll_state(&vp);
        }
    }

    pub(crate) fn image_block_height(&self) -> u16 {
        self.active_display_ref()
            .map(|d| d.image_block_height(&self.history_viewport))
            .unwrap_or(1)
    }

    pub(crate) fn ensure_cache_synced(&mut self) {
        if let Some(d) = self.active_display() {
            d.ensure_cache_synced();
        }
    }

    pub(crate) fn sync_turn_images(&mut self, session_id: u64, turn_id: u32, turn: &Turn) {
        let images = self
            .rendered_images
            .entry(session_id)
            .or_default()
            .entry(turn_id)
            .or_default();
        for (idx, record) in turn.displayed_images.iter().enumerate() {
            images.entry(idx).or_insert_with(|| {
                RenderedImage::new_placeholder(
                    record.metadata.clone(),
                    Arc::from(record.data.clone()),
                )
            });
        }
        images.retain(|&idx, _| idx < turn.displayed_images.len());
    }

    pub(crate) fn apply_image_result(&mut self, result: ImageResult) {
        let (session_id, turn_id, img_idx) = match self.pending_job_idx.remove(&result.id) {
            Some(key) => key,
            None => return,
        };
        if let Some(session_images) = self.rendered_images.get_mut(&session_id)
            && let Some(images) = session_images.get_mut(&turn_id)
            && let Some(img) = images.get_mut(&img_idx)
            && img.pending_job == Some(result.id)
        {
            tracing::trace!(
                "[choreo-tui] image job {} completed for session {} turn {} img {}",
                result.id,
                session_id,
                turn_id,
                img_idx,
            );
            img.apply_result(result);
        }
    }

    // All eight parameters are already owned by the caller (an image-ready
    // event handler); grouping them would only add a wrapper struct without
    // reducing the information flow.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn submit_image_job(
        &mut self,
        session_id: u64,
        turn_id: u32,
        img_idx: usize,
        data: std::sync::Arc<[u8]>,
        metadata: choreo_proto::ImageMetadata,
        cell_size: Size,
        resize: ratatui_image::Resize,
    ) -> Option<ImageId> {
        let tx = self.image_job_tx.as_ref()?;
        let id = next_job_id();

        tracing::trace!(
            "[choreo-tui] submitting image job {} for session {} turn {} img {} ({} {}x{} @ {}x{})",
            id,
            session_id,
            turn_id,
            img_idx,
            metadata.mime_type,
            metadata.width,
            metadata.height,
            cell_size.width,
            cell_size.height,
        );

        self.pending_job_idx
            .insert(id, (session_id, turn_id, img_idx));

        if let Some(session_images) = self.rendered_images.get_mut(&session_id)
            && let Some(images) = session_images.get_mut(&turn_id)
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

    pub(crate) fn reset_for_session_switch(&mut self, session_id: u64) {
        self.active_session_id = Some(session_id);
        self.rendered_images.remove(&session_id);
        let display = self.display_for(session_id);
        display.view = SessionView::new();
        display.render_cache.clear();
        display.visible_turn_ids.clear();
        display.history_scroll = HistoryScrollState::new();
        display.active.clear();
        display.markers.clear();
        display.height_prefix.clear();
        display.turn_heights.clear();
        display.turn_layouts.clear();
        display.reasoning_override.clear();
        display.streaming_turn_index = None;
        display.streaming_dirty = false;
        display.markers_dirty = true;
        display.content_dirty = false;
        display.status = None;
        display.error = None;
        display.progress_dirty = true;
        self.fullscreen_image_target = None;
    }

    pub(crate) fn effective_scroll(&self) -> usize {
        self.active_display_ref()
            .map(|d| d.effective_scroll(&self.history_viewport))
            .unwrap_or(0)
    }

    #[cfg(test)]
    pub(crate) fn scrollbar_notch(&self) -> usize {
        self.active_display_ref()
            .map(|d| d.scrollbar_notch(&self.history_viewport))
            .unwrap_or(1)
    }

    pub(crate) fn scroll_up(&mut self, amount: usize) {
        let vp = self.history_viewport;
        if let Some(d) = self.active_display() {
            d.scroll_up(amount, &vp);
        }
    }

    pub(crate) fn scroll_down(&mut self, amount: usize) {
        let vp = self.history_viewport;
        if let Some(d) = self.active_display() {
            d.scroll_down(amount, &vp);
        }
    }

    pub(crate) fn scroll_to(&mut self, row: usize) {
        let vp = self.history_viewport;
        if let Some(d) = self.active_display() {
            d.scroll_to(row, &vp);
        }
    }

    pub(crate) fn scroll_to_track_row(&mut self, mouse_row: u16, track_height: u16) {
        let vp = self.history_viewport;
        if let Some(d) = self.active_display() {
            d.scroll_to_track_row(mouse_row, track_height, &vp);
        }
    }

    pub(crate) fn scroll_to_content_line(&mut self, content_line: usize) {
        let vp = self.history_viewport;
        if let Some(d) = self.active_display() {
            d.scroll_to_content_line(content_line, &vp);
        }
    }

    pub(crate) fn scrollbar_scroll_up(&mut self) {
        let vp = self.history_viewport;
        if let Some(d) = self.active_display() {
            d.scrollbar_scroll_up(&vp);
        }
    }

    pub(crate) fn scrollbar_scroll_down(&mut self) {
        let vp = self.history_viewport;
        if let Some(d) = self.active_display() {
            d.scrollbar_scroll_down(&vp);
        }
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

    pub(crate) fn user_texts(&self) -> Vec<String> {
        self.active_display_ref()
            .map(|d| {
                d.view
                    .turns
                    .iter()
                    .rev()
                    .filter_map(|(_, turn)| turn.user_text.clone())
                    .collect()
            })
            .unwrap_or_default()
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
            self.input.generation += 1;
            self.input.cursor = self.input.text.len();
            self.ensure_input_cursor_visible();
        } else if let Some(idx) = self.history_index {
            let next = idx + 1;
            if next < texts.len() {
                self.history_index = Some(next);
                self.input.text = texts[next].to_string();
                self.input.generation += 1;
                self.input.cursor = self.input.text.len();
                self.ensure_input_cursor_visible();
            }
        }
    }

    pub(crate) fn navigate_history_down(&mut self) {
        if let Some(idx) = self.history_index {
            let texts = self.user_texts();
            if idx > 0 {
                let prev = idx - 1;
                self.history_index = Some(prev);
                self.input.text = texts[prev].to_string();
                self.input.generation += 1;
                self.input.cursor = self.input.text.len();
                self.ensure_input_cursor_visible();
            } else {
                self.history_index = None;
                self.input.text = self.saved_draft.clone();
                self.input.generation += 1;
                self.saved_draft.clear();
                self.input.cursor = self.input.text.len();
                self.ensure_input_cursor_visible();
            }
        }
    }

    pub(crate) fn commit_to_history(&mut self) {
        self.history_index = None;
        self.saved_draft.clear();
    }

    pub(crate) fn ensure_input_cursor_visible(&mut self) {
        if let Some((term_w, _)) = self.last_terminal_size {
            let inner = term_w.saturating_sub(2) as usize;
            let visible_height = self.input_bar_content_lines(term_w) as usize;
            self.input.ensure_cursor_visible(inner, visible_height);
        }
    }

    pub(crate) fn set_page(&mut self, page: Page) {
        self.page = page;
        if let Some(d) = self.active_display() {
            d.progress_dirty = true;
        }
    }

    // ── Legacy per-session daemon message handlers ─────────────────────

    pub(crate) fn handle_session_created(
        &mut self,
        session_id: u64,
        account_name: Option<String>,
        selected_model: Option<String>,
        reasoning_effort: Option<String>,
        client_tx: &std::sync::mpsc::Sender<ClientMessage>,
    ) -> Result<(), ClientError> {
        if self.page == Page::SessionManager {
            let _ = client_tx.send(ClientMessage::ListSessions);
        } else {
            // When creating from the Chat page, send ListSessions before
            // AttachSession so the session summary list is populated before
            // SessionAttached triggers handle_session_attached.
            client_tx
                .send(ClientMessage::ListSessions)
                .map_err(broken_pipe)?;
            self.reset_for_session_switch(session_id);
            self.attached_session_id = Some(session_id);
            // Set display fields immediately so they're available when
            // SessionAttached arrives — check the session summary first,
            // then fall back to the creation parameters.
            {
                let display = self.display_for(session_id);
                display.account_name = account_name;
                display.selected_model = selected_model;
                display.reasoning_effort = reasoning_effort;
            }
            client_tx
                .send(ClientMessage::AttachSession { session_id })
                .map_err(broken_pipe)?;
        }
        Ok(())
    }

    pub(crate) fn handle_session_attached(&mut self, session_id: u64) {
        self.active_session_id = Some(session_id);
        self.attached_session_id = Some(session_id);
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
            .sessions
            .iter()
            .find(|s| s.session_id == session_id)
            .map(|s| {
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
            })
            .unwrap_or((None, None, None, None, None, None, None, None));
        {
            let display = self.display_for(session_id);
            display.token_usage = token_usage;
            display.context_window = context_window;
            display.last_prompt_tokens = last_prompt_tokens;
            display.account_name = account_name;
            display.selected_model = selected_model;
            display.reasoning_effort = reasoning_effort;
            display.working_dir = working_dir;
            if let Some(ref st) = status {
                display.status = Some(format!("{:?}", st));
            }
        }
        self.attached_status = status;
        self.refresh_attached_provider_slug();
        self.show_ctrl_help = true;
        if let Some(d) = self.active_display() {
            d.progress_dirty = true;
        }
    }

    pub(crate) fn refresh_attached_provider_slug(&mut self) {
        self.attached_provider_slug = self.active_display_ref().and_then(|d| {
            d.account_name.as_ref().and_then(|name| {
                self.ai_providers
                    .accounts
                    .iter()
                    .find(|a| a.name == *name)
                    .map(|a| a.provider.clone())
            })
        });
    }

    pub(crate) fn attached_session_mut(&mut self) -> Option<&mut SessionSummary> {
        self.session_mgr
            .sessions
            .iter_mut()
            .find(|s| Some(s.session_id) == self.attached_session_id)
    }

    pub(crate) fn handle_model_selected(
        &mut self,
        model: &str,
        reasoning_capability: Option<ReasoningCapability>,
    ) {
        if let Some(d) = self.active_display() {
            d.selected_model = Some(model.to_owned());
            d.reasoning_capability = reasoning_capability;
        }
        if self.attached_session_id.is_some()
            && let Some(s) = self.attached_session_mut()
        {
            s.selected_model = Some(model.to_owned());
        }
    }

    pub(crate) fn handle_reasoning_effort_set(&mut self, effort: String) {
        if let Some(d) = self.active_display() {
            d.reasoning_effort = Some(effort.clone());
        }
        if self.attached_session_id.is_some()
            && let Some(s) = self.attached_session_mut()
        {
            s.reasoning_effort = Some(effort);
        }
    }

    pub(crate) fn handle_session_working_dir_set(
        &mut self,
        session_id: u64,
        path: &Option<String>,
    ) {
        if self.attached_session_id == Some(session_id) {
            if let Some(d) = self.active_display() {
                d.working_dir = path.clone();
                d.progress_dirty = true;
            }
            if let Some(s) = self.attached_session_mut() {
                s.working_dir = path.clone();
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
    }

    pub(crate) fn handle_session_account_set(&mut self, account: &str) {
        if let Some(d) = self.active_display() {
            d.account_name = Some(account.to_owned());
        }
        if self.attached_session_id.is_some() {
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
                    // Inherit account_name from the first available account,
                    // so the auto-created default session doesn't lose the
                    // account selection that was already configured.
                    let default_account =
                        self.ai_providers.accounts.first().map(|a| a.name.clone());
                    client_tx
                        .send(ClientMessage::CreateSession {
                            title: Some("default".to_string()),
                            parent_session_id: None,
                            working_dir: None,
                            max_turns: None,
                            context_config: None,
                            account_name: default_account,
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
        self.session_displays.remove(&session_id);
        self.rendered_images.remove(&session_id);
        if self.attached_session_id == Some(session_id) {
            self.attached_session_id = None;
            self.active_session_id = None;
            self.attached_provider_slug = None;
        }
    }

    pub(crate) fn handle_session_delete_failed(&mut self, session_id: u64, error: &str) {
        self.status = Some(format!(
            "failed to delete session {}: {}",
            session_id, error
        ));
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
        })
    }
}

// ── SessionDisplayState methods ─────────────────────────────────────

impl SessionDisplayState {
    pub(crate) fn total_history_height(&self) -> usize {
        self.height_prefix.last().copied().unwrap_or(0)
    }

    /// Rebuild height_prefix, markers, visible_turn_ids, and populate render_cache.
    pub(crate) fn rebuild_height_prefix(&mut self, viewport: &HistoryViewport) {
        self.height_prefix.clear();
        self.visible_turn_ids.clear();
        self.markers.clear();
        self.turn_layouts.clear();
        self.turn_heights.clear();
        let mut total = 0usize;
        let virtual_track = self.virtual_track_slots(viewport);
        let fallback_img_height = self.image_block_height(viewport) as usize;
        let turn_count = self.view.turns.len();
        tracing::trace!(turn_count, "rebuild_height_prefix");

        let visible_count = self.view.turns.iter().filter(|(_, t)| !t.undone).count();
        self.render_cache.resize(visible_count, None);

        let mut user_text_start_lines: Vec<usize> = Vec::with_capacity(turn_count);
        let mut visible_idx = 0usize;
        for (&turn_id, turn) in self.view.turns.iter() {
            if turn.undone {
                continue;
            }
            let content_width = viewport.width.saturating_sub(9);
            let tool_content_width = viewport.width.saturating_sub(4);

            // Effective reasoning visibility for this turn: the per-turn
            // user override (from clicking the header), falling back to the
            // streaming-derived default.  The derived default is also stored
            // in the turn layout so the per-frame render path can reuse it
            // in O(1) without re-scanning turn strings.
            let reasoning_default_expanded = reasoning_expanded_default(turn);
            let reasoning_expanded =
                self.effective_reasoning_expanded(turn_id, reasoning_default_expanded);

            let (_text_lines, text_height, text_offsets, reasoning_header_idx) =
                cached_or_compute_lines(
                    &mut self.render_cache,
                    visible_idx,
                    turn_id,
                    content_width,
                    viewport.width,
                    reasoning_expanded,
                    || {
                        render_turn_lines(
                            turn,
                            content_width,
                            tool_content_width,
                            reasoning_expanded,
                        )
                    },
                );

            // The reasoning header's visual-row range for click hit-testing.
            // The renderer reports the header's semantic-line index directly
            // (no output scanning); the cached offsets convert it to a
            // visual-row range — O(1) in the click handler, same approach
            // as image ranges.
            let reasoning_header_range = reasoning_header_idx.map(|idx| {
                let start = if idx == 0 { 0 } else { text_offsets[idx - 1] };
                let end = text_offsets[idx];
                (start, end)
            });

            let mut image_ranges: Vec<(usize, usize)> = Vec::new();
            let mut total_img_height: usize = 0;
            for _ in 0..turn.displayed_images.len() {
                let start = text_height + total_img_height;
                image_ranges.push((start, start + fallback_img_height));
                total_img_height += fallback_img_height;
            }
            self.turn_layouts.push(TurnLayout {
                reasoning_header_range,
                reasoning_default_expanded,
                image_ranges,
            });
            let turn_height = text_height + total_img_height;
            self.turn_heights.push(turn_height);
            if turn.user_text.is_some() {
                user_text_start_lines.push(total);
            }
            total += turn_height;
            self.height_prefix.push(total);
            self.visible_turn_ids.push(turn_id);
            visible_idx += 1;
        }
        let final_total = total.max(1);
        tracing::trace!(
            marker_count = user_text_start_lines.len(),
            final_total,
            "computed markers"
        );
        self.markers.reserve(user_text_start_lines.len());
        for &start_line in &user_text_start_lines {
            let slot = start_line * virtual_track / final_total;
            self.markers.push(Marker {
                content_line: start_line,
                virtual_slot: slot,
            });
        }
        self.markers_dirty = false;
    }

    pub(crate) fn mark_streaming_changed(&mut self) {
        self.streaming_dirty = true;
        self.content_dirty = true;
    }

    pub(crate) fn mark_content_changed(&mut self) {
        self.markers_dirty = true;
        self.content_dirty = true;
        self.streaming_turn_index = None;
        self.streaming_dirty = false;
    }

    /// Effective reasoning visibility for a turn: an explicit override from
    /// clicking the header wins; otherwise the caller-provided derived
    /// default is used.  Callers compute the default either from the turn
    /// content (`reasoning_expanded_default`) or from the precomputed
    /// `TurnLayout` when one is available (per-frame render path).
    pub(crate) fn effective_reasoning_expanded(&self, turn_id: u32, default: bool) -> bool {
        self.reasoning_override
            .get(&turn_id)
            .copied()
            .unwrap_or(default)
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

    pub(crate) fn resolve_streaming_turn_index(&mut self, request_id: u32) {
        if self.streaming_turn_index.is_none()
            && let Some(&turn_id) = self.view.request_to_turn.get(&request_id)
        {
            self.streaming_turn_index = self.visible_turn_ids.iter().position(|id| *id == turn_id);
        }
    }

    pub(crate) fn compute_total_height_and_markers(&mut self, viewport: &HistoryViewport) -> usize {
        if self.streaming_dirty && !self.markers_dirty {
            self.apply_streaming_update(viewport);
        } else if self.markers_dirty {
            let at_bottom = self.effective_scroll(viewport) == 0;
            let preserve_scroll = self.content_dirty && !at_bottom;
            let old_total = if preserve_scroll {
                self.total_history_height()
            } else {
                0
            };

            self.rebuild_height_prefix(viewport);

            if preserve_scroll {
                let new_total = self.total_history_height();
                if new_total > old_total {
                    self.history_scroll.scroll = self
                        .history_scroll
                        .scroll
                        .saturating_add(new_total - old_total);
                } else if old_total > new_total {
                    // Content shrank (e.g. collapsing a reasoning section or
                    // undoing turns).  Pull the scroll offset up by the
                    // removed height so the same content rows stay anchored
                    // in the viewport instead of jumping to the bottom.
                    self.history_scroll.scroll = self
                        .history_scroll
                        .scroll
                        .saturating_sub(old_total - new_total);
                }
            }

            self.content_dirty = false;
            self.streaming_dirty = false;
        }

        self.total_history_height().max(1)
    }

    fn apply_streaming_update(&mut self, viewport: &HistoryViewport) {
        let Some(turn_idx) = self.streaming_turn_index else {
            return self.rebuild_height_prefix_preserving_scroll(viewport);
        };
        if turn_idx >= self.visible_turn_ids.len() {
            return self.rebuild_height_prefix_preserving_scroll(viewport);
        }

        let turn_id = self.visible_turn_ids[turn_idx];
        let Some(turn) = self.view.turns.get(&turn_id) else {
            return self.rebuild_height_prefix_preserving_scroll(viewport);
        };

        let content_width = viewport.width.saturating_sub(9);
        let tool_content_width = viewport.width.saturating_sub(4);

        // Re-render with the effective reasoning visibility so the streaming
        // fast path stays consistent with the collapsed/expanded state.  The
        // derived default is stored back into the turn layout, keeping the
        // per-frame render path O(1).
        let reasoning_default_expanded = reasoning_expanded_default(turn);
        let reasoning_expanded =
            self.effective_reasoning_expanded(turn_id, reasoning_default_expanded);

        if let Some(Some(cached)) = self.render_cache.get_mut(turn_idx)
            && cached.turn_id == turn_id
            && cached.width == content_width
            && cached.viewport_width == viewport.width
        {
            let rendered =
                render_turn_lines(turn, content_width, tool_content_width, reasoning_expanded);
            let text_lines = rendered.lines;
            let text_height = lines_height(&text_lines, viewport.width).max(1);
            let visual_offsets = compute_visual_offsets(&text_lines, viewport.width);

            // Keep the reasoning header's click-hit range and the precomputed
            // default in sync as the response streams — the header sits below
            // the growing response, so its position shifts on every chunk.
            // Rebuilds (via `rebuild_height_prefix`) recompute from scratch.
            if let Some(layout) = self.turn_layouts.get_mut(turn_idx) {
                layout.reasoning_header_range = rendered.reasoning_header_idx.map(|idx| {
                    let start = if idx == 0 { 0 } else { visual_offsets[idx - 1] };
                    let end = visual_offsets[idx];
                    (start, end)
                });
                layout.reasoning_default_expanded = reasoning_default_expanded;
            }

            // The cache entry now reflects the current reasoning state; the
            // next frame's lookup will treat this as a valid hit.
            cached.reasoning_expanded = reasoning_expanded;
            cached.reasoning_header_idx = rendered.reasoning_header_idx;
            cached.lines = Arc::from(text_lines);
            cached.height = text_height;
            cached.visual_offsets = visual_offsets;

            let full_img_height = self.image_block_height(viewport) as usize;
            let img_count = turn.displayed_images.len();
            let turn_height = text_height + img_count * full_img_height;

            let old_height = self.turn_heights[turn_idx];

            if turn_height > old_height {
                let delta = turn_height - old_height;
                self.turn_heights[turn_idx] = turn_height;
                for i in turn_idx..self.height_prefix.len() {
                    self.height_prefix[i] = self.height_prefix[i].saturating_add(delta);
                }
                let at_bottom = self.effective_scroll(viewport) == 0;
                if !at_bottom {
                    self.history_scroll.scroll = self.history_scroll.scroll.saturating_add(delta);
                }
                self.rebuild_markers(viewport);
            } else if old_height > turn_height {
                return self.rebuild_height_prefix_preserving_scroll(viewport);
            }
        } else {
            return self.rebuild_height_prefix_preserving_scroll(viewport);
        }

        self.streaming_dirty = false;
        self.content_dirty = false;
    }

    fn rebuild_height_prefix_preserving_scroll(&mut self, viewport: &HistoryViewport) {
        let at_bottom = self.effective_scroll(viewport) == 0;
        let preserve_scroll = self.content_dirty && !at_bottom;
        let old_total = if preserve_scroll {
            self.total_history_height()
        } else {
            0
        };

        self.rebuild_height_prefix(viewport);

        if preserve_scroll {
            let new_total = self.total_history_height();
            if new_total > old_total {
                self.history_scroll.scroll = self
                    .history_scroll
                    .scroll
                    .saturating_add(new_total - old_total);
            } else if old_total > new_total {
                // Mirror the anchor-preserving adjustment in
                // `compute_total_height_and_markers`: pull the scroll offset
                // up by the removed height rather than jumping to the bottom.
                self.history_scroll.scroll = self
                    .history_scroll
                    .scroll
                    .saturating_sub(old_total - new_total);
            }
        }

        self.streaming_dirty = false;
        self.content_dirty = false;
    }

    fn rebuild_markers(&mut self, viewport: &HistoryViewport) {
        self.markers.clear();
        let total = self.total_history_height().max(1);
        let virtual_track = self.virtual_track_slots(viewport);
        let mut accum = 0usize;
        for (i, &turn_id) in self.visible_turn_ids.iter().enumerate() {
            let turn_height = self.turn_heights[i];
            if let Some(turn) = self.view.turns.get(&turn_id)
                && turn.user_text.is_some()
            {
                let slot = accum * virtual_track / total;
                self.markers.push(Marker {
                    content_line: accum,
                    virtual_slot: slot,
                });
            }
            accum += turn_height;
        }
    }

    pub(crate) fn max_scroll_offset(&self, viewport: &HistoryViewport) -> usize {
        let viewport_height = viewport.height as usize;
        let total_height = self.total_history_height();
        total_height.saturating_sub(viewport_height)
    }

    pub(crate) fn virtual_track_slots(&self, viewport: &HistoryViewport) -> usize {
        2 * viewport.height as usize
    }

    pub(crate) fn clamp_scroll_state(&mut self, viewport: &HistoryViewport) {
        self.history_scroll.clamp(self.max_scroll_offset(viewport));
    }

    pub(crate) fn effective_scroll(&self, viewport: &HistoryViewport) -> usize {
        self.history_scroll
            .effective_scroll(self.max_scroll_offset(viewport))
    }

    pub(crate) fn image_block_height(&self, viewport: &HistoryViewport) -> u16 {
        (viewport.height / 2).max(1)
    }

    pub(crate) fn ensure_cache_synced(&mut self) {
        let turns_len = self.visible_turn_ids.len();
        let cache_len = self.render_cache.len();
        if cache_len == turns_len {
            return;
        }
        if cache_len > turns_len {
            self.render_cache.truncate(turns_len);
            return;
        }
        self.render_cache.resize(turns_len, None);
    }

    pub(crate) fn scrollbar_notch(&self, viewport: &HistoryViewport) -> usize {
        let max_scroll = self.max_scroll_offset(viewport);
        let virtual_track = self.virtual_track_slots(viewport);
        if virtual_track > 0 {
            ceil_div(max_scroll, virtual_track)
        } else {
            max_scroll
        }
        .max(1)
    }

    pub(crate) fn scroll_up(&mut self, amount: usize, viewport: &HistoryViewport) {
        self.history_scroll
            .scroll_up(amount, self.max_scroll_offset(viewport));
    }

    pub(crate) fn scroll_down(&mut self, amount: usize, viewport: &HistoryViewport) {
        self.history_scroll
            .scroll_down(amount, self.max_scroll_offset(viewport));
    }

    pub(crate) fn scroll_to(&mut self, row: usize, viewport: &HistoryViewport) {
        let max_scroll = self.max_scroll_offset(viewport);
        let amount = row.min(max_scroll);
        self.history_scroll.scroll = amount;
        self.history_scroll.scroll_compensation = 0;
    }

    pub(crate) fn scroll_to_track_row(
        &mut self,
        mouse_row: u16,
        track_height: u16,
        viewport: &HistoryViewport,
    ) {
        let track_height = track_height as usize;
        if track_height > 1 {
            let row = (mouse_row as usize).min(track_height.saturating_sub(1));
            let max_scroll = self.max_scroll_offset(viewport);
            let denom = track_height.saturating_sub(1);
            let target = row.saturating_mul(max_scroll).saturating_add(denom / 2) / denom;
            self.scroll_to(max_scroll.saturating_sub(target.min(max_scroll)), viewport);
        }
    }

    pub(crate) fn scroll_to_content_line(
        &mut self,
        content_line: usize,
        viewport: &HistoryViewport,
    ) {
        let total = self.total_history_height();
        let vh = viewport.height as usize;
        let target = total.saturating_sub(content_line + vh);
        self.scroll_to(target.min(self.max_scroll_offset(viewport)), viewport);
    }

    pub(crate) fn scrollbar_scroll_up(&mut self, viewport: &HistoryViewport) {
        let notch = self.scrollbar_notch(viewport);
        self.scroll_up(notch, viewport);
    }

    pub(crate) fn scrollbar_scroll_down(&mut self, viewport: &HistoryViewport) {
        let notch = self.scrollbar_notch(viewport);
        self.scroll_down(notch, viewport);
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

// ── TurnEventHandler implementation ──────────────────────────────────

impl TurnEventHandler for App {
    fn handle_turn_appended(&mut self, session_id: u64, turn_id: u32, turn: Turn) {
        tracing::trace!(%turn_id, "handle_turn_appended");
        self.sync_turn_images(session_id, turn_id, &turn);
        let display = self.display_for(session_id);
        invalidate_turn_cache(display, turn_id);
        display.view.insert_or_replace(turn_id, turn);
        display.mark_content_changed();
    }

    fn handle_turn_finalized(&mut self, session_id: u64, turn_id: u32, turn: Turn) {
        tracing::trace!(%turn_id, "handle_turn_finalized");
        self.sync_turn_images(session_id, turn_id, &turn);
        let display = self.display_for(session_id);
        invalidate_turn_cache(display, turn_id);
        display.view.insert_or_replace(turn_id, turn);
        display.mark_content_changed();
    }

    fn handle_turns_undone(&mut self, session_id: u64, turn_ids: &[u32]) {
        tracing::trace!(?turn_ids, "handle_turns_undone");
        let display = self.display_for(session_id);
        for tid in turn_ids {
            invalidate_turn_cache(display, *tid);
            // Drop the user's reasoning-expansion preference for undone turns
            // so the map can't accumulate stale entries; a redo restores the
            // turn fresh with the derived default.
            display.reasoning_override.remove(tid);
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
        tracing::trace!(?turns, "handle_turns_redone");
        // Sync images first, then get display to avoid borrow conflict.
        for (tid, turn) in &turns {
            self.sync_turn_images(session_id, *tid, turn);
        }
        let display = self.display_for(session_id);
        for (tid, turn) in turns {
            invalidate_turn_cache(display, tid);
            display.view.insert_or_replace(tid, turn);
        }
        display.mark_content_changed();
    }

    fn handle_request_stream(
        &mut self,
        session_id: u64,
        request_id: u32,
        stream: OutputStream,
        data: Cow<'_, str>,
    ) {
        let display = self.display_for(session_id);
        // Detect the first Answer chunk for this request: the turn has no
        // response text yet, so this chunk begins the response phase.
        let turn_id = display.view.request_to_turn.get(&request_id).copied();
        let first_answer = matches!(stream, OutputStream::Answer)
            && turn_id
                .and_then(|id| display.view.turns.get(&id))
                .is_some_and(|t| t.assistant_text.is_none());

        display.view.stream_chunk(request_id, stream, &data);

        // Auto-collapse reasoning when the response starts — drop any
        // explicit expansion override so the derived default (collapsed once
        // a response exists) takes over.  The user can re-expand it by
        // clicking the header.
        if first_answer && let Some(turn_id) = turn_id {
            display.reasoning_override.remove(&turn_id);
        }

        display.resolve_streaming_turn_index(request_id);
        display.mark_streaming_changed();
    }

    fn handle_started(
        &mut self,
        session_id: u64,
        request_id: u32,
        turn_id: u32,
        estimated_prompt_tokens: u32,
    ) {
        tracing::trace!(%request_id, %turn_id, %estimated_prompt_tokens, "handle_started");
        let display = self.display_for(session_id);
        display.view.request_to_turn.insert(request_id, turn_id);
        display.active.insert(request_id);
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
        request_id: u32,
        token_usage: Option<TokenUsage>,
        last_prompt_tokens: Option<u32>,
    ) {
        tracing::trace!(%request_id, "handle_done");
        let display = self.display_for(session_id);
        display.view.request_to_turn.remove(&request_id);
        display.active.remove(&request_id);
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
        display.mark_content_changed();
    }

    fn handle_failed(&mut self, session_id: u64, request_id: u32, error: String) {
        tracing::trace!(%request_id, %error, "handle_failed");
        let display = self.display_for(session_id);
        display.error = Some(error);
        display.view.request_to_turn.remove(&request_id);
        display.active.remove(&request_id);
        display.streaming_turn_index = None;
        display.mark_content_changed();
    }

    fn handle_tool_call_event(&mut self, session_id: u64, request_id: u32, event: ToolCallEvent) {
        let display = self.display_for(session_id);
        match event {
            ToolCallEvent::Started {
                call_id,
                tool_name,
                arguments_json,
            } => {
                display
                    .view
                    .tool_call_started(request_id, call_id, tool_name, arguments_json);
                display.resolve_streaming_turn_index(request_id);
                display.mark_streaming_changed();
            }
            ToolCallEvent::Finished { .. } => {}
            ToolCallEvent::Failed { .. } => {}
        }
    }

    fn handle_tool_result_chunk(
        &mut self,
        session_id: u64,
        request_id: u32,
        call_id: String,
        data: Vec<u8>,
    ) {
        let text = String::from_utf8_lossy(&data).into_owned();
        let display = self.display_for(session_id);
        display.view.tool_result_chunk(request_id, &call_id, &text);
        display.resolve_streaming_turn_index(request_id);
        display.mark_streaming_changed();
    }

    fn handle_session_state(&mut self, state: SessionStateData) {
        tracing::debug!(
            turn_count = %state.turns.len(),
            ?state.selected_model,
            ?state.status,
            "handle_session_state"
        );
        let session_id = self.attached_session_id.unwrap_or(0);
        self.active_session_id = Some(session_id);

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
        // Sync images before getting display to avoid borrow conflict.
        self.rendered_images.remove(&session_id);
        for (tid, turn) in &turns {
            self.sync_turn_images(session_id, *tid, turn);
        }
        let display = self.display_for(session_id);
        display.view.turns = turns;
        display.selected_model = selected_model;
        if let Some(usage) = token_usage {
            display.token_usage = Some(usage);
        }
        if let Some(cw) = context_window {
            display.context_window = Some(cw);
        }
        if let Some(tokens) = last_prompt_tokens {
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
        self.attached_status = Some(status);
        self.attached_tool_groups = active_tool_groups;
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
    }

    fn handle_session_created(
        &mut self,
        _session_id: u64,
        _title: Option<String>,
        _working_dir: Option<String>,
        _max_turns: Option<u32>,
        _account_name: Option<String>,
        _selected_model: Option<String>,
        _reasoning_effort: Option<String>,
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
    let display = app.active_display_ref()?;
    let vh = app.history_viewport.height;
    if screen_row >= vh {
        return None;
    }

    let effective_scroll = display.effective_scroll(&app.history_viewport);
    let total_height = display.total_history_height();

    let content_line = total_height
        .saturating_sub(effective_scroll + vh as usize)
        .saturating_add(screen_row as usize);

    if content_line >= total_height {
        return None;
    }

    let i = display
        .height_prefix
        .partition_point(|&p| p <= content_line);
    if i < display.height_prefix.len() {
        let turn_start = i
            .checked_sub(1)
            .and_then(|prev| display.height_prefix.get(prev))
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
        let mut app = test_app();
        app.history_viewport.width = 80;
        app.history_viewport.height = 5;

        for i in 0..5u32 {
            let turn = Turn {
                created_at: choreo_proto::TimestampMs::now(),
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
            let display = app.active_display().unwrap();
            display.view.insert_or_replace(i, turn);
        }
        app.rebuild_height_prefix();

        app.scroll_to_content_line(0);
        assert_eq!(app.effective_scroll(), app.max_scroll_offset());
    }

    // ── find_turn_at_row ──

    #[test]
    fn find_turn_at_row_returns_none_out_of_bounds() {
        let app = test_app();
        assert!(find_turn_at_row(&app, 999).is_none());
    }

    #[test]
    fn find_turn_at_row_returns_turn_idx_and_offset() {
        let mut app = test_app();
        app.history_viewport.width = 80;
        app.history_viewport.height = 200;
        let turn = Turn {
            created_at: choreo_proto::TimestampMs::now(),
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
        let display = app.active_display().unwrap();
        display.view.insert_or_replace(1, turn);
        app.rebuild_height_prefix();

        let (turn_idx, offset) = find_turn_at_row(&app, 0).unwrap();
        assert_eq!(turn_idx, 0);
        assert_eq!(offset, 0);
    }

    // ── scrollbar_notch ──

    #[test]
    fn scrollbar_notch_no_content() {
        let mut app = test_app();
        app.history_viewport.width = 80;
        app.history_viewport.height = 20;
        assert_eq!(app.scrollbar_notch(), 1);
    }

    #[test]
    fn scrollbar_notch_track_one() {
        let mut app = test_app();
        app.history_viewport.width = 80;
        app.history_viewport.height = 1;
        let display = app.active_display().unwrap();
        display.height_prefix.push(50);
        // max_scroll = 50 - 1 = 49, virtual_track = 2, notch = ceil(49 / 2) = 25
        assert_eq!(app.scrollbar_notch(), 25);
    }

    #[test]
    fn scrollbar_notch_ceiling_division() {
        let mut app = test_app();
        app.history_viewport.width = 80;
        app.history_viewport.height = 50;
        let display = app.active_display().unwrap();
        display.height_prefix.push(150);
        // max_scroll = 150 - 50 = 100, virtual_track = 100, notch = ceil(100 / 100) = 1
        assert_eq!(app.scrollbar_notch(), 1);
    }

    #[test]
    fn scrollbar_notch_rounds_up() {
        let mut app = test_app();
        app.history_viewport.width = 80;
        app.history_viewport.height = 30;
        let display = app.active_display().unwrap();
        display.height_prefix.push(105);
        // max_scroll = 105 - 30 = 75, virtual_track = 60, notch = ceil(75 / 60) = 2
        assert_eq!(app.scrollbar_notch(), 2);
    }

    // ── scrollbar_scroll_up / scrollbar_scroll_down ──

    #[test]
    fn scrollbar_scroll_up_increases_scroll_by_notch() {
        let mut app = test_app();
        app.history_viewport.width = 80;
        app.history_viewport.height = 10;
        let display = app.active_display().unwrap();
        display.height_prefix.push(110);
        // max_scroll = 100, virtual_track = 20, notch = 5
        display.history_scroll.scroll = 0;
        let before = app.effective_scroll();

        app.scrollbar_scroll_up();

        assert_eq!(app.effective_scroll(), before + 5);
    }

    #[test]
    fn scrollbar_scroll_up_clamps_at_max_scroll() {
        let mut app = test_app();
        app.history_viewport.width = 80;
        app.history_viewport.height = 10;
        let display = app.active_display().unwrap();
        display.height_prefix.push(110);
        // max_scroll = 100, virtual_track = 20, notch = 5
        display.history_scroll.scroll = 100;

        app.scrollbar_scroll_up();

        assert_eq!(app.effective_scroll(), 100);
    }

    #[test]
    fn scrollbar_scroll_down_decreases_scroll_by_notch() {
        let mut app = test_app();
        app.history_viewport.width = 80;
        app.history_viewport.height = 10;
        let display = app.active_display().unwrap();
        display.height_prefix.push(110);
        // max_scroll = 100, virtual_track = 20, notch = 5
        display.history_scroll.scroll = 100;
        let before = app.effective_scroll();

        app.scrollbar_scroll_down();

        assert_eq!(app.effective_scroll(), before - 5);
    }

    #[test]
    fn scrollbar_scroll_down_clamps_at_zero() {
        let mut app = test_app();
        app.history_viewport.width = 80;
        app.history_viewport.height = 10;
        let display = app.active_display().unwrap();
        display.height_prefix.push(110);
        // max_scroll = 100, virtual_track = 20, notch = 5
        display.history_scroll.scroll = 5;

        app.scrollbar_scroll_down();

        assert_eq!(app.effective_scroll(), 0);
    }

    // ── scroll_to_track_row ──

    #[test]
    fn scroll_to_track_row_at_bottom() {
        let mut app = test_app();
        app.history_viewport.width = 80;
        app.history_viewport.height = 20;
        let display = app.active_display().unwrap();
        display.height_prefix.push(110);
        // max_scroll = 90, denom = 19
        display.history_scroll.scroll = 90;

        app.scroll_to_track_row(0, 20);

        assert_eq!(app.effective_scroll(), 90);
    }

    #[test]
    fn scroll_to_track_row_at_top() {
        let mut app = test_app();
        app.history_viewport.width = 80;
        app.history_viewport.height = 20;
        let display = app.active_display().unwrap();
        display.height_prefix.push(110);
        // max_scroll = 90, denom = 19
        display.history_scroll.scroll = 0;

        app.scroll_to_track_row(19, 20);

        assert_eq!(app.effective_scroll(), 0);
    }

    #[test]
    fn scroll_to_track_row_midpoint() {
        let mut app = test_app();
        app.history_viewport.width = 80;
        app.history_viewport.height = 10;
        let display = app.active_display().unwrap();
        display.height_prefix.push(110);
        // max_scroll = 100, denom = 9

        app.scroll_to_track_row(4, 10);

        assert_eq!(app.effective_scroll(), 56);
    }

    #[test]
    fn scroll_to_track_row_zero_viewport() {
        let mut app = test_app();
        app.history_viewport.width = 80;
        app.history_viewport.height = 0;
        let display = app.active_display().unwrap();
        display.height_prefix.push(110);
        display.history_scroll.scroll = 42;

        app.scroll_to_track_row(0, 0);

        assert_eq!(app.effective_scroll(), 42);
    }

    #[test]
    fn scroll_to_track_row_track_one() {
        let mut app = test_app();
        app.history_viewport.width = 80;
        app.history_viewport.height = 10;
        let display = app.active_display().unwrap();
        display.height_prefix.push(110);
        display.history_scroll.scroll = 42;

        app.scroll_to_track_row(0, 1);

        assert_eq!(app.effective_scroll(), 42);
    }

    #[test]
    fn scroll_to_track_row_mouse_row_clamped() {
        let mut app = test_app();
        app.history_viewport.width = 80;
        app.history_viewport.height = 20;
        let display = app.active_display().unwrap();
        display.height_prefix.push(110);
        // max_scroll = 90, denom = 19
        display.history_scroll.scroll = 0;

        app.scroll_to_track_row(30, 20);

        assert_eq!(app.effective_scroll(), 0);
    }

    // ── scroll_to_content_line ──

    #[test]
    fn scroll_to_content_line_idempotent_when_already_visible() {
        let mut app = test_app();
        app.history_viewport.width = 80;
        app.history_viewport.height = 200;

        let turn = Turn {
            created_at: choreo_proto::TimestampMs::now(),
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
        let display = app.active_display().unwrap();
        display.view.insert_or_replace(0, turn);
        app.rebuild_height_prefix();

        let before = app.effective_scroll();
        app.scroll_to_content_line(0);
        assert_eq!(app.effective_scroll(), before);
    }

    #[test]
    fn scroll_to_content_line_large_content_line_saturates() {
        let mut app = test_app();
        app.history_viewport.width = 80;
        app.history_viewport.height = 5;

        for i in 0..5u32 {
            let turn = Turn {
                created_at: choreo_proto::TimestampMs::now(),
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
            let display = app.active_display().unwrap();
            display.view.insert_or_replace(i, turn);
        }
        app.rebuild_height_prefix();

        app.scroll_to_content_line(9999);
        assert_eq!(app.effective_scroll(), 0);
    }

    // ── status_error_height ──

    #[test]
    fn status_error_height_neither_set_returns_zero() {
        let app = test_app();
        assert_eq!(app.status_error_height(80), 0);
    }

    #[test]
    fn status_error_height_short_error_returns_one() {
        let mut app = test_app();
        app.error = Some("oops".into());
        assert_eq!(app.status_error_height(80), 1);
    }

    #[test]
    fn status_error_height_short_status_returns_one() {
        let mut app = test_app();
        app.status = Some("all good".into());
        assert_eq!(app.status_error_height(80), 1);
    }

    #[test]
    fn status_error_height_error_preferred_over_status() {
        let mut app = test_app();
        app.error = Some("error".into());
        app.status = Some("status".into());
        // Should use error text, not status text
        assert_eq!(app.status_error_height(80), 1);
    }

    #[test]
    fn status_error_height_wrapping() {
        let mut app = test_app();
        // A 10-char line at width 5 wraps to 2 lines
        app.error = Some("12345 7890".into());
        assert_eq!(app.status_error_height(5), 2);
    }

    #[test]
    fn status_error_height_multi_line() {
        let mut app = test_app();
        // Three explicit lines via \n
        app.status = Some("line a\nline b\nline c".into());
        // Each line fits in width 80, so total = 3
        assert_eq!(app.status_error_height(80), 3);
    }

    #[test]
    fn status_error_height_multi_line_with_wrapping() {
        let mut app = test_app();
        // Two lines, second wraps
        app.error = Some("hello\n12345 7890".into());
        // line 1: "hello" → 1 line
        // line 2: "12345 7890" → wraps to 2 lines at width 5
        // total = 3
        assert_eq!(app.status_error_height(5), 3);
    }

    #[test]
    fn status_error_height_empty_after_clearing() {
        let mut app = test_app();
        app.error = Some("error".into());
        app.error = None;
        assert_eq!(app.status_error_height(80), 0);
    }

    #[test]
    fn status_error_height_status_takes_over_when_error_cleared() {
        let mut app = test_app();
        app.status = Some("status".into());
        assert_eq!(app.status_error_height(80), 1);
    }

    #[test]
    fn sync_turn_images_populates_rendered_images() {
        let mut app = test_app();
        let metadata = choreo_proto::ImageMetadata {
            mime_type: "image/svg+xml".to_string(),
            width: 100,
            height: 200,
            byte_len: 50,
            alt: None,
        };
        let turn = Turn {
            created_at: choreo_proto::TimestampMs::now(),
            undone: false,
            error: None,
            user_text: None,
            assistant_text: None,
            assistant_reasoning: None,
            tool_calls: vec![],
            token_usage: None,
            tool_results: vec![],
            displayed_images: vec![
                choreo_proto::DisplayedImageRecord {
                    metadata: metadata.clone(),
                    data: b"svg-data".to_vec(),
                    tool_call_id: Some("call-1".into()),
                },
                choreo_proto::DisplayedImageRecord {
                    metadata: metadata.clone(),
                    data: b"more-svg".to_vec(),
                    tool_call_id: None,
                },
            ],
        };
        app.sync_turn_images(0, 42, &turn);

        let images = app.rendered_images.get(&0).unwrap().get(&42).unwrap();
        assert_eq!(images.len(), 2);
        assert_eq!(images[&0].data.as_ref(), b"svg-data");
        assert_eq!(images[&1].data.as_ref(), b"more-svg");
        // Second call is idempotent — preserves existing entries
        app.sync_turn_images(0, 42, &turn);
        assert_eq!(
            app.rendered_images.get(&0).unwrap().get(&42).unwrap().len(),
            2
        );
    }

    // ── TurnImageLayout image_ranges ──

    #[test]
    fn turn_layout_empty_when_no_images() {
        let mut app = test_app();
        app.history_viewport.width = 80;
        app.history_viewport.height = 20;
        let turn = Turn {
            created_at: choreo_proto::TimestampMs::now(),
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
        app.active_display()
            .unwrap()
            .view
            .insert_or_replace(1, turn);
        app.rebuild_height_prefix();

        assert_eq!(app.active_display().unwrap().turn_layouts.len(), 1);
        assert!(
            app.active_display().unwrap().turn_layouts[0]
                .image_ranges
                .is_empty()
        );
    }

    #[test]
    fn turn_layout_populates_image_ranges_with_fallback_height() {
        let mut app = test_app();
        app.history_viewport.width = 80;
        app.history_viewport.height = 20;
        let metadata = choreo_proto::ImageMetadata {
            mime_type: "image/png".to_string(),
            width: 100,
            height: 100,
            byte_len: 500,
            alt: None,
        };
        let turn = Turn {
            created_at: choreo_proto::TimestampMs::now(),
            undone: false,
            error: None,
            user_text: None,
            assistant_text: Some("short".into()),
            assistant_reasoning: None,
            tool_calls: vec![],
            token_usage: None,
            tool_results: vec![],
            displayed_images: vec![
                choreo_proto::DisplayedImageRecord {
                    metadata: metadata.clone(),
                    data: vec![0u8; 10],
                    tool_call_id: None,
                },
                choreo_proto::DisplayedImageRecord {
                    metadata: metadata.clone(),
                    data: vec![1u8; 10],
                    tool_call_id: None,
                },
            ],
        };
        let turn_clone = turn.clone();
        app.active_display()
            .unwrap()
            .view
            .insert_or_replace(2, turn);
        app.sync_turn_images(0, 2, &turn_clone);
        app.rebuild_height_prefix();

        assert_eq!(app.active_display().unwrap().turn_layouts.len(), 1);
        // Mutable borrow dropped.

        // Capture needed values before taking another mutable borrow for layout.
        let fallback_h = app.image_block_height() as usize;
        let vp_width = app.history_viewport.width;
        let text_h = {
            let display = app.active_display().unwrap();
            let turn = &display.view.turns[&2];
            lines_height(
                &render_turn_lines(turn, 71, vp_width, false).lines,
                vp_width,
            )
            .max(1)
        };

        let layout = &app.active_display().unwrap().turn_layouts[0];
        assert_eq!(layout.image_ranges.len(), 2);

        let (s0, e0) = layout.image_ranges[0];
        assert_eq!(s0, text_h);
        assert_eq!(e0, text_h + fallback_h);

        let (s1, e1) = layout.image_ranges[1];
        assert_eq!(s1, text_h + fallback_h);
        assert_eq!(e1, text_h + 2 * fallback_h);
    }

    // ── TurnLayout reasoning_header_range ──

    #[test]
    fn turn_layout_reasoning_header_range_present() {
        let mut app = test_app();
        app.history_viewport.width = 80;
        app.history_viewport.height = 20;
        let turn = Turn {
            created_at: choreo_proto::TimestampMs::now(),
            undone: false,
            error: None,
            user_text: Some("hello".into()),
            assistant_text: Some("world".into()),
            assistant_reasoning: Some("think".into()),
            tool_calls: vec![],
            token_usage: None,
            tool_results: vec![],
            displayed_images: vec![],
        };
        app.active_display()
            .unwrap()
            .view
            .insert_or_replace(1, turn);
        app.rebuild_height_prefix();

        let layout = &app.active_display().unwrap().turn_layouts[0];
        let Some((start, end)) = layout.reasoning_header_range else {
            panic!("reasoning header range should be present");
        };
        assert!(
            start < end,
            "header range must be non-empty ({start}..{end})"
        );
        // No images on this turn, so the full turn height is its text block;
        // the header must lie inside it.
        let turn_h = app.active_display().unwrap().turn_heights[0];
        assert!(end <= turn_h, "header must lie within the turn text");
    }

    #[test]
    fn turn_layout_reasoning_default_expanded_reflects_turn_content() {
        let mut app = test_app();
        app.history_viewport.width = 80;
        app.history_viewport.height = 20;

        // Response present → default collapsed.
        let responded = Turn {
            created_at: choreo_proto::TimestampMs::now(),
            undone: false,
            error: None,
            user_text: None,
            assistant_text: Some("world".into()),
            assistant_reasoning: Some("think".into()),
            tool_calls: vec![],
            token_usage: None,
            tool_results: vec![],
            displayed_images: vec![],
        };
        app.active_display()
            .unwrap()
            .view
            .insert_or_replace(1, responded);

        // Streaming (no response yet) → default expanded.
        let streaming = Turn {
            created_at: choreo_proto::TimestampMs::now(),
            undone: false,
            error: None,
            user_text: None,
            assistant_text: None,
            assistant_reasoning: Some("thinking".into()),
            tool_calls: vec![],
            token_usage: None,
            tool_results: vec![],
            displayed_images: vec![],
        };
        app.active_display()
            .unwrap()
            .view
            .insert_or_replace(2, streaming);

        app.rebuild_height_prefix();

        let display = app.active_display().unwrap();
        assert!(
            !display.turn_layouts[0].reasoning_default_expanded,
            "response present → collapsed default"
        );
        assert!(
            display.turn_layouts[1].reasoning_default_expanded,
            "no response yet → expanded default"
        );
    }

    #[test]
    fn turn_layout_reasoning_header_range_none_without_reasoning() {
        let mut app = test_app();
        app.history_viewport.width = 80;
        app.history_viewport.height = 20;
        let turn = Turn {
            created_at: choreo_proto::TimestampMs::now(),
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
        app.active_display()
            .unwrap()
            .view
            .insert_or_replace(1, turn);
        app.rebuild_height_prefix();

        let layout = &app.active_display().unwrap().turn_layouts[0];
        assert!(
            layout.reasoning_header_range.is_none(),
            "no reasoning → no header range"
        );
    }

    // ── toggle_reasoning ──

    #[test]
    fn toggle_reasoning_flips_override_and_invalidates_cache() {
        let mut app = test_app();
        let turn = Turn {
            created_at: choreo_proto::TimestampMs::now(),
            undone: false,
            error: None,
            user_text: None,
            assistant_text: Some("response".into()),
            assistant_reasoning: Some("thinking".into()),
            tool_calls: vec![],
            token_usage: None,
            tool_results: vec![],
            displayed_images: vec![],
        };
        let display = app.active_display().unwrap();
        display.view.insert_or_replace(1, turn);
        display.visible_turn_ids.push(1);
        display.render_cache = vec![Some(RenderedCache {
            turn_id: 1,
            reasoning_expanded: false, // response present → collapsed default
            reasoning_header_idx: None,
            lines: Arc::from(vec![Line::from("stale")]),
            width: 71,
            viewport_width: 80,
            height: 1,
            visual_offsets: Arc::from([1]),
        })];

        // Default is collapsed (response present) → first click expands.
        display.toggle_reasoning(1);
        assert_eq!(
            display.reasoning_override.get(&1),
            Some(&true),
            "first click should expand"
        );
        assert!(
            display.render_cache[0].is_none(),
            "toggle must invalidate the render cache"
        );

        // Second click collapses again.
        display.toggle_reasoning(1);
        assert_eq!(
            display.reasoning_override.get(&1),
            Some(&false),
            "second click should collapse"
        );
    }

    #[test]
    fn toggle_reasoning_missing_turn_is_noop() {
        let mut app = test_app();
        let display = app.active_display().unwrap();
        display.toggle_reasoning(999);
        assert!(
            display.reasoning_override.is_empty(),
            "unknown turn should not record an override"
        );
    }

    #[test]
    fn toggle_reasoning_default_expanded_without_response() {
        let mut app = test_app();
        let turn = Turn {
            created_at: choreo_proto::TimestampMs::now(),
            undone: false,
            error: None,
            user_text: None,
            assistant_text: None,
            assistant_reasoning: Some("thinking".into()),
            tool_calls: vec![],
            token_usage: None,
            tool_results: vec![],
            displayed_images: vec![],
        };
        let display = app.active_display().unwrap();
        display.view.insert_or_replace(1, turn);
        // No response yet → default expanded → first click collapses.
        display.toggle_reasoning(1);
        assert_eq!(
            display.reasoning_override.get(&1),
            Some(&false),
            "first click on streaming reasoning should collapse"
        );
    }

    // ── effective_reasoning_expanded ──

    #[test]
    fn effective_reasoning_expanded_prefers_override() {
        let mut app = test_app();
        let display = app.active_display().unwrap();
        // No override → the derived default wins.
        assert!(!display.effective_reasoning_expanded(1, false));
        assert!(display.effective_reasoning_expanded(1, true));
        // An explicit override wins over the derived default.
        display.reasoning_override.insert(1, true);
        assert!(
            display.effective_reasoning_expanded(1, false),
            "override should beat a collapsed default"
        );
        display.reasoning_override.insert(1, false);
        assert!(
            !display.effective_reasoning_expanded(1, true),
            "override should beat an expanded default"
        );
    }

    // ── reasoning_override pruning on undo ──

    #[test]
    fn turns_undone_prunes_reasoning_override() {
        let mut app = test_app();
        let turn = Turn {
            created_at: choreo_proto::TimestampMs::now(),
            undone: false,
            error: None,
            user_text: None,
            assistant_text: Some("response".into()),
            assistant_reasoning: Some("thinking".into()),
            tool_calls: vec![],
            token_usage: None,
            tool_results: vec![],
            displayed_images: vec![],
        };
        {
            let display = app.active_display().unwrap();
            display.view.insert_or_replace(1, turn);
            // Simulate the user having expanded the reasoning section.
            display.reasoning_override.insert(1, true);
        }

        app.handle_turns_undone(0, &[1]);

        let display = app.active_display_ref().unwrap();
        assert!(
            !display.reasoning_override.contains_key(&1),
            "undo should prune the reasoning override"
        );
        assert!(
            display.view.turns[&1].undone,
            "the turn should be marked undone"
        );
    }

    // ── auto-collapse on first answer chunk ──

    #[test]
    fn first_answer_chunk_auto_collapses_reasoning() {
        let mut app = test_app();
        let turn = Turn {
            created_at: choreo_proto::TimestampMs::now(),
            undone: false,
            error: None,
            user_text: None,
            assistant_text: None,
            assistant_reasoning: Some("thinking".into()),
            tool_calls: vec![],
            token_usage: None,
            tool_results: vec![],
            displayed_images: vec![],
        };
        let display = app.active_display().unwrap();
        display.view.insert_or_replace(1, turn);
        display.view.request_to_turn.insert(7, 1);
        // The user expanded reasoning during streaming.
        display.reasoning_override.insert(1, true);

        app.handle_request_stream(0, 7, OutputStream::Answer, Cow::Borrowed("Hi"));

        let display = app.active_display().unwrap();
        assert!(
            !display.reasoning_override.contains_key(&1),
            "first answer chunk should auto-collapse reasoning"
        );
        assert_eq!(display.view.turns[&1].assistant_text.as_deref(), Some("Hi"));
        assert!(
            display.view.turns[&1].assistant_reasoning.is_some(),
            "reasoning content must be retained after the response streams"
        );
    }

    #[test]
    fn reasoning_chunk_keeps_expansion_override() {
        let mut app = test_app();
        let turn = Turn {
            created_at: choreo_proto::TimestampMs::now(),
            undone: false,
            error: None,
            user_text: None,
            assistant_text: None,
            assistant_reasoning: Some("thinking".into()),
            tool_calls: vec![],
            token_usage: None,
            tool_results: vec![],
            displayed_images: vec![],
        };
        let display = app.active_display().unwrap();
        display.view.insert_or_replace(1, turn);
        display.view.request_to_turn.insert(7, 1);
        display.reasoning_override.insert(1, true);

        app.handle_request_stream(0, 7, OutputStream::Reasoning, Cow::Borrowed(" more"));

        let display = app.active_display().unwrap();
        assert_eq!(
            display.reasoning_override.get(&1),
            Some(&true),
            "reasoning chunks must not collapse the section"
        );
        assert_eq!(
            display.view.turns[&1].assistant_reasoning.as_deref(),
            Some("thinking more"),
            "reasoning chunk should append to the reasoning text"
        );
    }

    // ── apply_image_result ──

    #[test]
    fn apply_image_result_clears_pending_job_and_records_failure() {
        use choreo_tui::image_worker::next_job_id;

        let mut app = test_app();
        app.history_viewport.width = 80;
        app.history_viewport.height = 20;
        let metadata = choreo_proto::ImageMetadata {
            mime_type: "image/png".to_string(),
            width: 100,
            height: 100,
            byte_len: 500,
            alt: None,
        };
        let turn = Turn {
            created_at: choreo_proto::TimestampMs::now(),
            undone: false,
            error: None,
            user_text: None,
            assistant_text: None,
            assistant_reasoning: None,
            tool_calls: vec![],
            token_usage: None,
            tool_results: vec![],
            displayed_images: vec![choreo_proto::DisplayedImageRecord {
                metadata: metadata.clone(),
                data: vec![3u8; 30],
                tool_call_id: None,
            }],
        };
        let turn_clone = turn.clone();
        app.active_display()
            .unwrap()
            .view
            .insert_or_replace(4, turn);
        app.sync_turn_images(0, 4, &turn_clone);

        let img_id = next_job_id();
        app.pending_job_idx.insert(img_id, (0, 4, 0));
        let img = app
            .rendered_images
            .get_mut(&0)
            .unwrap()
            .get_mut(&4)
            .unwrap()
            .get_mut(&0)
            .unwrap();
        img.pending_job = Some(img_id);

        let inline_size = Size::new(app.history_viewport.width, app.image_block_height());
        let result = choreo_tui::image_worker::ImageResult {
            id: img_id,
            protocol: None,
            cell_size: inline_size,
        };
        app.apply_image_result(result);

        let img = app
            .rendered_images
            .get(&0)
            .unwrap()
            .get(&4)
            .unwrap()
            .get(&0)
            .unwrap();
        assert!(img.failed_sizes.contains(&inline_size));
        assert!(img.pending_job.is_none());
    }

    #[test]
    fn apply_image_result_records_failure_at_any_size() {
        use choreo_tui::image_worker::next_job_id;

        let mut app = test_app();
        app.history_viewport.width = 80;
        app.history_viewport.height = 20;
        let metadata = choreo_proto::ImageMetadata {
            mime_type: "image/png".to_string(),
            width: 100,
            height: 100,
            byte_len: 500,
            alt: None,
        };
        let turn = Turn {
            created_at: choreo_proto::TimestampMs::now(),
            undone: false,
            error: None,
            user_text: None,
            assistant_text: None,
            assistant_reasoning: None,
            tool_calls: vec![],
            token_usage: None,
            tool_results: vec![],
            displayed_images: vec![choreo_proto::DisplayedImageRecord {
                metadata: metadata.clone(),
                data: vec![4u8; 40],
                tool_call_id: None,
            }],
        };
        let turn_clone = turn.clone();
        app.active_display()
            .unwrap()
            .view
            .insert_or_replace(5, turn);
        app.sync_turn_images(0, 5, &turn_clone);

        let img_id = next_job_id();
        app.pending_job_idx.insert(img_id, (0, 5, 0));
        let img = app
            .rendered_images
            .get_mut(&0)
            .unwrap()
            .get_mut(&5)
            .unwrap()
            .get_mut(&0)
            .unwrap();
        img.pending_job = Some(img_id);

        // Use a cell_size that is NOT the inline size.
        let non_inline_size = Size::new(80, app.image_block_height() + 1);
        let result = choreo_tui::image_worker::ImageResult {
            id: img_id,
            protocol: None,
            cell_size: non_inline_size,
        };
        app.apply_image_result(result);

        let img = app
            .rendered_images
            .get(&0)
            .unwrap()
            .get(&5)
            .unwrap()
            .get(&0)
            .unwrap();
        assert!(img.failed_sizes.contains(&non_inline_size));
    }

    // ── compute_total_height_and_markers scroll preservation ──

    /// Helper: insert a minimal turn into `app`.
    fn insert_turn(app: &mut App, id: u32, user_text: &str, assistant_text: &str) {
        let turn = Turn {
            created_at: choreo_proto::TimestampMs::now(),
            undone: false,
            error: None,
            user_text: Some(user_text.into()),
            assistant_text: Some(assistant_text.into()),
            assistant_reasoning: None,
            tool_calls: vec![],
            token_usage: None,
            tool_results: vec![],
            displayed_images: vec![],
        };
        app.display_for(0).view.insert_or_replace(id, turn);
    }

    #[test]
    fn scroll_preserved_when_scrolled_up_and_content_changes() {
        let mut app = test_app();
        app.history_viewport.width = 80;
        app.history_viewport.height = 5;

        insert_turn(&mut app, 0, "a", "a");
        insert_turn(&mut app, 1, "b", "b");
        app.rebuild_height_prefix();

        // Capture viewport height before taking a mutable borrow.
        let viewport_height = app.history_viewport.height;
        {
            let display = app.active_display().unwrap();
            let initial_total = display.total_history_height();

            display.history_scroll.scroll =
                initial_total.saturating_sub(viewport_height as usize) / 2;
        }
        assert!(app.effective_scroll() > 0, "should be scrolled up");

        insert_turn(&mut app, 2, "new content", "new content");
        let old_total = app.total_history_height();
        let old_scroll;
        {
            let display = app.active_display().unwrap();
            old_scroll = display.history_scroll.scroll;

            display.mark_content_changed();
        }

        app.compute_total_height_and_markers();

        let display = app.active_display_ref().unwrap();
        let new_total = display.total_history_height();
        let delta = new_total.saturating_sub(old_total);
        assert!(
            delta > 0,
            "total height should increase after adding content"
        );
        assert_eq!(
            display.history_scroll.scroll,
            old_scroll + delta,
            "scroll should be adjusted by the content delta"
        );
        assert!(
            !display.content_dirty,
            "content_dirty should be cleared after computation"
        );
    }

    #[test]
    fn scroll_not_preserved_when_at_bottom_and_content_changes() {
        let mut app = test_app();
        app.history_viewport.width = 80;
        app.history_viewport.height = 5;

        insert_turn(&mut app, 0, "a", "a");
        insert_turn(&mut app, 1, "b", "b");
        app.rebuild_height_prefix();

        {
            let display = app.active_display().unwrap();
            display.history_scroll.scroll = 0;
        }
        assert_eq!(app.effective_scroll(), 0, "should be at bottom");

        insert_turn(&mut app, 2, "more", "more");
        let old_scroll;
        {
            let display = app.active_display().unwrap();
            old_scroll = display.history_scroll.scroll;
            display.mark_content_changed();
        }

        app.compute_total_height_and_markers();

        let display = app.active_display_ref().unwrap();
        assert_eq!(
            display.history_scroll.scroll, old_scroll,
            "scroll should stay at 0 when user is at bottom"
        );
    }

    // ── marker computation ──

    #[test]
    fn markers_empty_when_no_user_text_turns() {
        let mut app = test_app();
        app.history_viewport.width = 80;
        app.history_viewport.height = 20;

        let turn = Turn {
            created_at: choreo_proto::TimestampMs::now(),
            undone: false,
            error: None,
            user_text: None,
            assistant_text: Some("hello".into()),
            assistant_reasoning: None,
            tool_calls: vec![],
            token_usage: None,
            tool_results: vec![],
            displayed_images: vec![],
        };
        {
            let display = app.active_display().unwrap();
            display.view.insert_or_replace(0, turn);
        }
        app.rebuild_height_prefix();

        let display = app.active_display_ref().unwrap();
        assert!(
            display.markers.is_empty(),
            "no markers should be created when no turn has user_text"
        );
    }

    #[test]
    fn markers_created_for_each_user_text_turn() {
        let mut app = test_app();
        app.history_viewport.width = 80;
        app.history_viewport.height = 20;

        insert_turn(&mut app, 0, "user a", "assistant a");
        let turn_no_user = Turn {
            created_at: choreo_proto::TimestampMs::now(),
            undone: false,
            error: None,
            user_text: None,
            assistant_text: Some("assistant only".into()),
            assistant_reasoning: None,
            tool_calls: vec![],
            token_usage: None,
            tool_results: vec![],
            displayed_images: vec![],
        };
        {
            let display = app.active_display().unwrap();
            display.view.insert_or_replace(1, turn_no_user);
        }
        insert_turn(&mut app, 2, "user c", "assistant c");

        app.rebuild_height_prefix();

        let display = app.active_display_ref().unwrap();
        assert_eq!(
            display.markers.len(),
            2,
            "expected 2 markers for 2 user-text turns"
        );
        assert!(
            display.markers[0].content_line < display.markers[1].content_line,
            "first user-text turn should appear before the second"
        );

        let total = display.total_history_height();
        for marker in &display.markers {
            assert!(
                marker.content_line < total,
                "marker content_line {0} should be < total history {total}",
                marker.content_line
            );
        }
    }

    #[test]
    fn marker_virtual_slot_uses_final_total_height() {
        let mut app = test_app();
        app.history_viewport.width = 80;
        app.history_viewport.height = 20;
        let virtual_track = 2 * app.history_viewport.height as usize;

        insert_turn(&mut app, 0, "x", "y");
        insert_turn(&mut app, 1, "x", "y");
        app.rebuild_height_prefix();

        let display = app.active_display_ref().unwrap();
        let total = display.total_history_height();
        assert!(total > 0, "total history should be positive");

        let mut prev_end = 0usize;
        for (i, marker) in display.markers.iter().enumerate() {
            assert_eq!(
                marker.content_line, prev_end,
                "marker {i} content_line should equal the start of the turn"
            );
            if let Some(&end) = display.height_prefix.get(i) {
                prev_end = end;
            }

            let expected_slot = marker.content_line * virtual_track / total;
            assert_eq!(
                marker.virtual_slot, expected_slot,
                "marker {i} virtual_slot should use final total={total} as denominator"
            );
        }
    }

    #[test]
    fn marker_virtual_slot_proportional_to_position() {
        let mut app = test_app();
        app.history_viewport.width = 80;
        app.history_viewport.height = 20;
        let virtual_track = 2 * app.history_viewport.height as usize;

        insert_turn(&mut app, 0, "a", "a");
        insert_turn(&mut app, 1, "b", "b");
        insert_turn(&mut app, 2, "c", "c");
        app.rebuild_height_prefix();

        let display = app.active_display_ref().unwrap();
        assert!(
            display.markers[0].virtual_slot <= display.markers[1].virtual_slot,
            "second marker slot should be >= first marker slot"
        );
        assert!(
            display.markers[1].virtual_slot <= display.markers[2].virtual_slot,
            "third marker slot should be >= second marker slot"
        );
        assert!(
            display.markers[2].virtual_slot < virtual_track,
            "last marker slot should be less than virtual_track={virtual_track}"
        );
    }

    #[test]
    fn scroll_not_preserved_when_content_dirty_is_false() {
        let mut app = test_app();
        app.history_viewport.width = 80;
        app.history_viewport.height = 5;

        insert_turn(&mut app, 0, "a", "a");
        app.rebuild_height_prefix();

        let old_scroll;
        {
            let display = app.active_display().unwrap();
            display.history_scroll.scroll = 10;
            old_scroll = display.history_scroll.scroll;

            display.markers_dirty = true;
            assert!(!display.content_dirty, "content should not be dirty");
        }
        app.compute_total_height_and_markers();

        let display = app.active_display_ref().unwrap();
        assert_eq!(
            display.history_scroll.scroll, old_scroll,
            "scroll should not change when content_dirty is false"
        );
    }

    // ── update_viewport_from_terminal_size ──

    #[test]
    fn help_overlay_reduces_viewport_height() {
        let mut app = test_app();
        app.history_viewport.width = 80;
        app.history_viewport.height = 26;

        app.last_terminal_size = Some((80, 30));
        app.terminal_resized = false;

        app.show_ctrl_help = false;
        app.update_viewport_from_terminal_size();
        let height_without_help = app.history_viewport.height;

        app.last_terminal_size = Some((80, 30));
        app.terminal_resized = false;
        app.show_ctrl_help = true;
        app.update_viewport_from_terminal_size();
        let height_with_help = app.history_viewport.height;

        assert_eq!(height_without_help - height_with_help, 2,);

        let total = app.total_history_height();
        let max_scroll = app.max_scroll_offset();
        if total > height_with_help as usize {
            assert_eq!(max_scroll + height_with_help as usize, total,);
        }
    }

    #[test]
    fn width_change_clears_content_dirty() {
        let mut app = test_app();

        app.history_viewport.width = 80;
        app.history_viewport.height = 26;

        app.last_terminal_size = Some((100, 30));
        app.terminal_resized = false;

        {
            let display = app.active_display().unwrap();
            display.content_dirty = true;
            display.markers_dirty = true;
            display.render_cache = vec![Some(RenderedCache {
                turn_id: 0,
                reasoning_expanded: false,
                reasoning_header_idx: None,
                lines: Arc::from(Vec::<Line<'static>>::new()),
                width: 0,
                viewport_width: 0,
                height: 0,
                visual_offsets: Arc::from([]),
            })];
        }

        app.update_viewport_from_terminal_size();

        let display = app.active_display_ref().unwrap();
        assert!(
            !display.content_dirty,
            "content_dirty should be cleared on width change"
        );
        assert!(display.markers_dirty, "markers_dirty should remain true");
        assert!(
            display.render_cache.iter().all(|c| c.is_none()),
            "render_cache should be cleared"
        );
        assert_eq!(app.history_viewport.width, 99);
    }

    #[test]
    fn height_only_change_does_not_clear_content_dirty() {
        let mut app = test_app();

        app.history_viewport.width = 79;
        app.history_viewport.height = 20;

        app.last_terminal_size = Some((80, 30));
        app.terminal_resized = false;

        {
            let display = app.active_display().unwrap();
            display.content_dirty = true;
            display.markers_dirty = true;
            display.render_cache = vec![Some(RenderedCache {
                turn_id: 0,
                reasoning_expanded: false,
                reasoning_header_idx: None,
                lines: Arc::from(Vec::<Line<'static>>::new()),
                width: 0,
                viewport_width: 0,
                height: 0,
                visual_offsets: Arc::from([]),
            })];
        }

        app.update_viewport_from_terminal_size();

        let display = app.active_display_ref().unwrap();
        assert!(
            display.content_dirty,
            "content_dirty should NOT be cleared on height-only change"
        );
        assert!(display.markers_dirty, "markers_dirty should remain true");
        assert!(
            display.render_cache.iter().all(|c| c.is_none()),
            "render_cache should be cleared"
        );
    }

    // ── compute_total_height_and_markers: anchor preservation on content removal ──

    #[test]
    fn content_removed_preserves_scroll_anchor() {
        let mut app = test_app();
        app.history_viewport.width = 80;
        app.history_viewport.height = 5;

        insert_turn(&mut app, 0, "user text", "assistant text");
        insert_turn(&mut app, 1, "more user", "more assistant");
        app.rebuild_height_prefix();

        let old_total = app.total_history_height();
        assert!(old_total > 0, "should have content");

        let viewport_height = app.history_viewport.height;
        let old_scroll;
        {
            let display = app.active_display().unwrap();
            // Scroll to the top of the history so the removed turn (the
            // last one) lies entirely below the viewport — the scenario
            // where anchor preservation keeps the viewport still.
            display.history_scroll.scroll = old_total.saturating_sub(viewport_height as usize);
            old_scroll = display.history_scroll.scroll;
        }
        assert!(app.effective_scroll() > 0, "should be scrolled up");

        {
            let display = app.active_display().unwrap();
            display.view.turns.remove(&1);
            assert_eq!(display.view.turns.len(), 1);

            display.mark_content_changed();
        }

        app.compute_total_height_and_markers();

        let display = app.active_display_ref().unwrap();
        let new_total = display.total_history_height();
        let new_scroll = display.history_scroll.scroll;
        assert!(
            new_total < old_total,
            "removing a turn should shrink the total height"
        );
        // The content row at the viewport's bottom edge stays anchored
        // instead of the viewport jumping to the bottom.
        assert_eq!(
            new_total.saturating_sub(new_scroll),
            old_total.saturating_sub(old_scroll),
            "the anchored content row should not move"
        );
    }

    #[test]
    fn content_added_shifts_scroll_down() {
        let mut app = test_app();
        app.history_viewport.width = 80;
        app.history_viewport.height = 5;

        insert_turn(&mut app, 0, "a", "b");
        app.rebuild_height_prefix();

        let viewport_height = app.history_viewport.height;
        let old_total;
        let old_scroll;
        {
            let display = app.active_display().unwrap();
            old_total = display.total_history_height();
            display.history_scroll.scroll = old_total.saturating_sub(viewport_height as usize) / 2;
            old_scroll = display.history_scroll.scroll;
        }

        insert_turn(&mut app, 1, "c", "d");
        {
            let display = app.active_display().unwrap();
            display.mark_content_changed();
        }

        app.compute_total_height_and_markers();

        let display = app.active_display_ref().unwrap();
        let new_total = display.total_history_height();
        let delta = new_total.saturating_sub(old_total);
        assert!(delta > 0, "total height should increase");
        assert_eq!(
            display.history_scroll.scroll,
            old_scroll + delta,
            "scroll should be shifted down by the content delta"
        );
    }

    // ── streaming (incremental update) ──

    #[test]
    fn mark_streaming_changed_sets_flags() {
        let mut app = test_app();
        {
            let display = app.active_display_ref().unwrap();
            assert!(!display.streaming_dirty);
            assert!(!display.content_dirty);
        }

        app.mark_streaming_changed();

        let display = app.active_display_ref().unwrap();
        assert!(display.streaming_dirty, "streaming_dirty should be set");
        assert!(display.content_dirty, "content_dirty should be set");
    }

    #[test]
    fn mark_content_changed_resets_streaming_turn_index() {
        let mut app = test_app();
        let display = app.active_display().unwrap();
        display.markers_dirty = false;
        display.streaming_turn_index = Some(0);

        display.mark_content_changed();

        assert!(display.markers_dirty, "markers_dirty should be set");
        assert!(display.content_dirty, "content_dirty should be set");
        assert!(
            display.streaming_turn_index.is_none(),
            "streaming_turn_index should be cleared"
        );
    }

    #[test]
    fn streaming_update_without_turn_index_falls_back() {
        let mut app = test_app();
        insert_turn(&mut app, 0, "hello", "world");
        app.rebuild_height_prefix();

        let display = app.active_display_ref().unwrap();
        let old_total = display.total_history_height();
        assert!(old_total > 0);

        // Simulate streaming without a streaming_turn_index.
        // Capture viewport before mutable borrow.
        let viewport = app.history_viewport;
        let display = app.active_display().unwrap();
        display.streaming_turn_index = None;
        display.streaming_dirty = true;
        display.content_dirty = true;

        let total = display.compute_total_height_and_markers(&viewport);

        assert!(!display.streaming_dirty, "streaming_dirty cleared");
        assert!(!display.content_dirty, "content_dirty cleared");
        assert_eq!(total, old_total, "full rebuild produces same total");
    }

    #[test]
    fn streaming_update_recalculates_turn_height() {
        let mut app = test_app();
        app.history_viewport.width = 80;
        app.history_viewport.height = 200;

        insert_turn(&mut app, 0, "hello", "world");
        app.rebuild_height_prefix();

        let display = app.active_display_ref().unwrap();
        let before_height = display.turn_heights[0];
        let before_total = display.total_history_height();

        // Simulate streaming: append to assistant_text.
        let viewport = app.history_viewport;
        let display = app.active_display().unwrap();
        let turn = display.view.turns.get_mut(&0).unwrap();
        turn.assistant_text
            .as_mut()
            .unwrap()
            .push_str("\n\nnew streaming content");
        display.streaming_turn_index = Some(0);
        display.streaming_dirty = true;
        display.content_dirty = true;

        let total = display.compute_total_height_and_markers(&viewport);

        assert!(
            display.turn_heights[0] > before_height,
            "turn height should increase after content added"
        );
        assert!(
            total >= before_total,
            "total height should increase or stay same"
        );
        assert!(!display.streaming_dirty, "streaming_dirty cleared");
        assert!(!display.content_dirty, "content_dirty cleared");
    }

    #[test]
    fn streaming_answer_moves_reasoning_header_range() {
        let mut app = test_app();
        app.history_viewport.width = 80;
        app.history_viewport.height = 200;

        // A turn with reasoning only (no response yet), actively streaming.
        let turn = Turn {
            created_at: choreo_proto::TimestampMs::now(),
            undone: false,
            error: None,
            user_text: None,
            assistant_text: None,
            assistant_reasoning: Some("thinking".into()),
            tool_calls: vec![],
            token_usage: None,
            tool_results: vec![],
            displayed_images: vec![],
        };
        {
            let display = app.active_display().unwrap();
            display.view.insert_or_replace(1, turn);
            display.view.request_to_turn.insert(7, 1);
        }
        app.rebuild_height_prefix();

        // Before the answer: reasoning is the only content, so the header
        // sits at the top of the assistant block.
        let initial_start = app.active_display_ref().unwrap().turn_layouts[0]
            .reasoning_header_range
            .expect("header range should exist")
            .0;

        // First Answer chunk auto-collapses the reasoning and places the
        // response above the header.
        app.handle_request_stream(0, 7, OutputStream::Answer, Cow::Borrowed("Response text."));
        app.compute_total_height_and_markers();

        let (start, end) = app.active_display_ref().unwrap().turn_layouts[0]
            .reasoning_header_range
            .expect("header range should remain after auto-collapse");
        assert!(
            start > initial_start,
            "header should move below the streaming response ({initial_start} -> {start})"
        );
        assert!(start < end, "header range must be non-empty");
    }

    #[test]
    fn streaming_update_preserves_height_prefix_invariant() {
        let mut app = test_app();
        app.history_viewport.width = 80;
        app.history_viewport.height = 200;

        insert_turn(&mut app, 0, "a", "b");
        insert_turn(&mut app, 1, "c", "d");
        insert_turn(&mut app, 2, "e", "f");
        app.rebuild_height_prefix();

        let viewport = app.history_viewport;
        let display = app.active_display().unwrap();
        let old_prefix = display.height_prefix.clone();
        let old_heights = display.turn_heights.clone();

        // Stream content into turn 1.
        let turn = display.view.turns.get_mut(&1).unwrap();
        turn.assistant_text
            .as_mut()
            .unwrap()
            .push_str("\n\nlots of new content that should increase height");
        display.streaming_turn_index = Some(1);
        display.streaming_dirty = true;
        display.content_dirty = true;

        display.compute_total_height_and_markers(&viewport);

        // Verify invariant: height_prefix[i] == sum(turn_heights[0..=i]).
        let mut accum = 0usize;
        for i in 0..display.turn_heights.len() {
            accum += display.turn_heights[i];
            assert_eq!(
                display.height_prefix[i], accum,
                "invariant failed at index {i}: height_prefix[i] should equal cumulative turn_heights"
            );
        }

        // Turn 0 height unchanged.
        assert_eq!(
            display.turn_heights[0], old_heights[0],
            "turn 0 height should not change"
        );
        assert_eq!(
            display.height_prefix[0], old_prefix[0],
            "height_prefix[0] should not change"
        );
        // Markers must also be correct after the streaming update.
        assert_eq!(
            display.markers[0].content_line, 0,
            "marker[0] content_line should be 0"
        );
        assert_eq!(
            display.markers[1].content_line, display.turn_heights[0],
            "marker[1] content_line should equal turn 0 height"
        );
        assert_eq!(
            display.markers[2].content_line,
            display.turn_heights[0] + display.turn_heights[1],
            "marker[2] content_line should reflect updated turn 1 height"
        );
    }

    #[test]
    fn handle_started_sets_streaming_turn_index() {
        let mut app = test_app();
        app.history_viewport.width = 80;
        app.history_viewport.height = 200;

        // Pre-populate turns so visible_turn_ids exist.
        insert_turn(&mut app, 10, "user", "assistant");
        insert_turn(&mut app, 20, "another user", "another assistant");
        app.rebuild_height_prefix();

        {
            let display = app.active_display_ref().unwrap();
            assert_eq!(display.visible_turn_ids.len(), 2);
            assert_eq!(display.visible_turn_ids[0], 10);
            assert_eq!(display.visible_turn_ids[1], 20);
            assert!(display.streaming_turn_index.is_none());
        }

        // handle_started now requires session_id
        app.handle_started(0, 1, 10, 100);

        let display = app.active_display_ref().unwrap();
        assert_eq!(
            display.streaming_turn_index,
            Some(0),
            "should find turn 10 at index 0"
        );
    }

    #[test]
    fn handle_done_fires_full_rebuild() {
        let mut app = test_app();
        app.history_viewport.width = 80;
        app.history_viewport.height = 200;

        insert_turn(&mut app, 10, "user", "assistant");
        app.rebuild_height_prefix();
        {
            let display = app.active_display().unwrap();
            display.markers_dirty = false;
            display.streaming_turn_index = Some(0);
            display.streaming_dirty = false;
            display.content_dirty = false;
        }

        app.handle_done(0, 1, None, None);

        let display = app.active_display_ref().unwrap();
        assert!(
            display.streaming_turn_index.is_none(),
            "streaming_turn_index should be cleared"
        );
        assert!(
            display.markers_dirty,
            "markers_dirty should be set (full rebuild)"
        );
        assert!(display.content_dirty, "content_dirty should be set");
    }

    #[test]
    fn handle_failed_clears_streaming() {
        let mut app = test_app();
        {
            let display = app.active_display().unwrap();
            display.streaming_turn_index = Some(0);
            display.streaming_dirty = false;
            display.content_dirty = false;
            display.markers_dirty = false;
        }

        app.handle_failed(0, 1, "oops".into());

        let display = app.active_display_ref().unwrap();
        assert!(display.streaming_turn_index.is_none());
        assert!(display.error.is_some());
        assert!(display.markers_dirty, "markers_dirty should be set");
        assert!(display.content_dirty, "content_dirty should be set");
    }
}
