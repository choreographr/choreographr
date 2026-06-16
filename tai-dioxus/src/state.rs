use std::collections::HashMap;
use tai_client_core::{ImageAssembler, StreamingText};
use tai_proto::{ImageMetadata, OutputStream, SessionMessage};

pub(crate) type StreamingEntry = StreamingText;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DisplayImage {
    pub(crate) metadata: ImageMetadata,
    pub(crate) data_url: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HistoryItem {
    Text(String),
    SessionMessage(SessionMessage),
    Streaming(StreamingEntry),
    Image(DisplayImage),
}

#[derive(Debug, Clone)]
pub(crate) enum UiEvent {
    Daemon(tai_proto::DaemonMessage),
    ReaderClosed,
    ReaderFailed(String),
    WriterFailed(String),
}

#[derive(Debug)]
pub(crate) struct AppState {
    pub(crate) input: String,
    pub(crate) next_request_id: u32,
    pub(crate) history: Vec<HistoryItem>,
    pub(crate) in_progress: HashMap<u32, usize>,
    pub(crate) pending_images: ImageAssembler,
    pub(crate) pending_cancel: String,
}

impl AppState {
    pub(crate) fn new(socket_path: String) -> Self {
        Self {
            input: String::new(),
            next_request_id: 1,
            history: vec![HistoryItem::Text(format!(
                "Connected to tai-daemon at {socket_path}"
            ))],
            in_progress: HashMap::new(),
            pending_images: ImageAssembler::new(),
            pending_cancel: String::new(),
        }
    }

    pub(crate) fn push_text(&mut self, text: impl Into<String>) {
        self.push_history_item(HistoryItem::Text(text.into()));
    }

    pub(crate) fn push_session_message(&mut self, message: SessionMessage) {
        self.push_history_item(HistoryItem::SessionMessage(message));
    }

    pub(crate) fn push_history_item(&mut self, item: HistoryItem) {
        self.history.push(item);
        self.trim_history();
    }

    pub(crate) fn begin_stream(&mut self, request_id: u32) {
        if self.in_progress.contains_key(&request_id) {
            return;
        }
        let index = self.history.len();
        self.history
            .push(HistoryItem::Streaming(StreamingEntry::new(request_id)));
        self.in_progress.insert(request_id, index);
        self.trim_history();
    }

    pub(crate) fn append_stream(&mut self, request_id: u32, stream: OutputStream, chunk: &str) {
        if !self.in_progress.contains_key(&request_id) {
            self.begin_stream(request_id);
        }
        if let Some(&index) = self.in_progress.get(&request_id)
            && let Some(HistoryItem::Streaming(entry)) = self.history.get_mut(index)
        {
            entry.append(stream, chunk);
        }
    }

    pub(crate) fn finalize_stream(&mut self, request_id: u32) {
        self.in_progress.remove(&request_id);
        self.pending_images.drop_request(request_id);
    }

    pub(crate) fn push_image(&mut self, image: DisplayImage) {
        self.push_history_item(HistoryItem::Image(image));
    }

    fn trim_history(&mut self) {
        if self.history.len() <= 500 {
            return;
        }
        let excess = self.history.len() - 500;
        self.history.drain(0..excess);
        for index in self.in_progress.values_mut() {
            *index = index.saturating_sub(excess);
        }
        self.in_progress
            .retain(|_, index| *index < self.history.len());
    }
}
