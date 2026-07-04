use ratatui::layout::Rect;
use std::collections::HashSet;
use tai_client_core::{
    ClientError, ClientHistory, DaemonMessageHandler, HistoryItem as SharedHistoryItem,
    MAX_HISTORY_ITEMS,
};
use tai_proto::{ImageMetadata, OutputStream, SessionMessage, SessionStatus, SessionSummary};
use tai_tui::{RenderedImage, StreamingText, build_rendered_image};

use crate::markdown_render::{lines_height, session_message_lines, streaming_text_lines};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Page {
    Chat,
    SessionManager,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SessionManagerView {
    List,
    Detail,
}

pub(crate) struct SessionDetailData {
    pub(crate) session_id: u64,
    pub(crate) title: String,
    pub(crate) selected_model: String,
    pub(crate) parent_session_id: Option<u64>,
    pub(crate) cwd: String,
    pub(crate) created_at: i64,
    pub(crate) message_count: u32,
    pub(crate) max_turns: Option<u32>,
    pub(crate) status: SessionStatus,
}

pub(crate) struct SessionManagerState {
    pub(crate) sessions: Vec<SessionSummary>,
    pub(crate) view: SessionManagerView,
    pub(crate) selection: Option<usize>,
    pub(crate) scroll: usize,
    pub(crate) detail_data: Option<SessionDetailData>,
}

pub(crate) struct App {
    pub(crate) input: String,
    pub(crate) next_request_id: u32,
    pub(crate) active: HashSet<u32>,
    pub(crate) client: ClientHistory<Box<RenderedImage>>,
    pub(crate) history_scroll: HistoryScrollState,
    pub(crate) history_viewport: HistoryViewport,
    pub(crate) should_quit: bool,
    pub(crate) picker: Option<ratatui_image::picker::Picker>,
    pub(crate) attached_session_id: Option<u64>,
    pub(crate) page: Page,
    pub(crate) session_mgr: SessionManagerState,
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
            HistoryItem::Text(text) => history_text_height(text, self.width).max(1),
            HistoryItem::SessionMessage(message) => {
                let lines = session_message_lines(message, self.width);
                lines_height(&lines, self.width).max(1)
            }
            HistoryItem::Streaming(text) => {
                let lines = streaming_text_lines(text, self.width);
                lines_height(&lines, self.width).max(1)
            }
            HistoryItem::Image(_) => image_block_height(self.height as usize),
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
        }
    }

    pub(crate) fn set_sessions(&mut self, sessions: Vec<SessionSummary>) {
        let was_attached = self.selection.and_then(|i| self.sessions.get(i)).map(|s| s.session_id);
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
            let cwd = s.cwd.clone().unwrap_or_else(|| "-".to_string());
            let created_at = s.created_at;
            let message_count = s.message_count;
            let max_turns = s.max_turns;
            SessionDetailData {
                session_id,
                title,
                selected_model,
                parent_session_id,
                cwd,
                created_at,
                message_count,
                max_turns,
                status: s.status.clone(),
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
}

impl App {
    pub(crate) fn new(socket_path: String, picker_protocol: String) -> Self {
        Self {
            input: String::new(),
            next_request_id: 1,
            active: HashSet::new(),
            client: ClientHistory::new(vec![
                HistoryItem::Text(format!("Connected to tai-daemon at {socket_path}")),
                HistoryItem::Text(format!("image protocol: {picker_protocol}")),
            ]),
            history_scroll: HistoryScrollState::new(),
            history_viewport: HistoryViewport::new(),
            should_quit: false,
            picker: None,
            attached_session_id: None,
            page: Page::Chat,
            session_mgr: SessionManagerState::new(),
        }
    }

    pub(crate) fn total_history_height(&self) -> usize {
        self.client
            .history
            .iter()
            .map(|item| self.history_viewport.item_height(item))
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

    pub(crate) fn push_tool_text(&mut self, request_id: u32, text: impl Into<String>) {
        let text = text.into();
        let item = HistoryItem::Text(text.clone());
        let added_height = self.history_viewport.item_height(&item);
        let trimmed_height = self.trimmed_height_on_append();
        self.client.insert_text_before_stream(request_id, text);
        self.history_scroll
            .on_item_appended(added_height, self.max_scroll_offset());
        self.account_for_trimmed_height(trimmed_height);
        self.clamp_scroll_state();
    }

    pub(crate) fn push_session_message(&mut self, message: SessionMessage) {
        self.push_history_item(HistoryItem::SessionMessage(message));
    }

    pub(crate) fn push_image(&mut self, image: RenderedImage) {
        let item = HistoryItem::Image(Box::new(image));
        self.push_history_item(item);
    }

    pub(crate) fn push_history_item(&mut self, item: HistoryItem) {
        let added_height = self.history_viewport.item_height(&item);
        let trimmed_height = self.trimmed_height_on_append();
        self.client.push_history_item(item);
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
        self.client.in_progress.remove(&request_id);
        self.client.pending_images.drop_request(request_id);
    }

    fn handle_image_start(
        &mut self,
        request_id: u32,
        metadata: ImageMetadata,
    ) -> Result<(), ClientError> {
        self.client.start_image(request_id, metadata)
    }

    fn handle_image_chunk(
        &mut self,
        request_id: u32,
        image_id: u32,
        data: &[u8],
    ) -> Result<(), ClientError> {
        self.client.push_image_chunk(request_id, image_id, data)
    }

    fn handle_image_end(&mut self, request_id: u32, image_id: u32) -> Result<(), ClientError> {
        let (metadata, data) = self.client.finish_image(request_id, image_id)?;
        let picker = self.picker.as_ref().ok_or_else(|| {
            ClientError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "image picker not set",
            ))
        })?;
        let rendered = build_rendered_image(picker, metadata, data)?;
        self.push_image(rendered);
        Ok(())
    }
}

pub(crate) fn history_text_height(text: &str, width: u16) -> usize {
    lines_height(&crate::markdown_render::plain_text_lines(text), width)
}

pub(crate) fn image_block_height(available_height: usize) -> usize {
    available_height.min(12)
}
