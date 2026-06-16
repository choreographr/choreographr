use crate::{ImageAssembler, StreamingText};
use std::{collections::HashMap, io};
use tai_proto::{ImageMetadata, OutputStream, SessionMessage};

pub const MAX_HISTORY_ITEMS: usize = 500;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistoryItem<TImage> {
    Text(String),
    SessionMessage(SessionMessage),
    Streaming(StreamingText),
    Image(TImage),
}

#[derive(Debug, Clone)]
pub struct ClientHistory<TImage> {
    pub history: Vec<HistoryItem<TImage>>,
    pub in_progress: HashMap<u32, usize>,
    pub pending_images: ImageAssembler,
}

impl<TImage> ClientHistory<TImage> {
    pub fn new(history: Vec<HistoryItem<TImage>>) -> Self {
        Self {
            history,
            in_progress: HashMap::new(),
            pending_images: ImageAssembler::new(),
        }
    }

    pub fn push_text(&mut self, text: impl Into<String>) {
        self.push_history_item(HistoryItem::Text(text.into()));
    }

    pub fn push_session_message(&mut self, message: SessionMessage) {
        self.push_history_item(HistoryItem::SessionMessage(message));
    }

    pub fn push_image(&mut self, image: TImage) {
        self.push_history_item(HistoryItem::Image(image));
    }

    pub fn push_history_item(&mut self, item: HistoryItem<TImage>) {
        self.history.push(item);
        self.trim_history();
    }

    pub fn begin_stream(&mut self, request_id: u32) {
        if self.in_progress.contains_key(&request_id) {
            return;
        }
        let index = self.history.len();
        self.history
            .push(HistoryItem::Streaming(StreamingText::new(request_id)));
        self.in_progress.insert(request_id, index);
        self.trim_history();
    }

    pub fn append_stream(&mut self, request_id: u32, stream: OutputStream, chunk: &str) {
        if !self.in_progress.contains_key(&request_id) {
            self.begin_stream(request_id);
        }
        if let Some(&index) = self.in_progress.get(&request_id)
            && let Some(HistoryItem::Streaming(entry)) = self.history.get_mut(index)
        {
            entry.append(stream, chunk);
        }
    }

    pub fn finalize_stream(&mut self, request_id: u32) {
        self.in_progress.remove(&request_id);
        self.pending_images.drop_request(request_id);
    }

    pub fn start_image(&mut self, request_id: u32, metadata: ImageMetadata) -> io::Result<()> {
        self.pending_images.start(request_id, metadata)
    }

    pub fn push_image_chunk(
        &mut self,
        request_id: u32,
        image_id: u32,
        data: &[u8],
    ) -> io::Result<()> {
        self.pending_images.push_chunk(request_id, image_id, data)
    }

    pub fn finish_image(&mut self, request_id: u32, image_id: u32) -> io::Result<(ImageMetadata, Vec<u8>)> {
        self.pending_images.finish(request_id, image_id)
    }

    pub fn drop_request(&mut self, request_id: u32) {
        self.finalize_stream(request_id);
    }

    fn trim_history(&mut self) {
        if self.history.len() <= MAX_HISTORY_ITEMS {
            return;
        }
        let excess = self.history.len() - MAX_HISTORY_ITEMS;
        self.history.drain(0..excess);
        for index in self.in_progress.values_mut() {
            *index = index.saturating_sub(excess);
        }
        self.in_progress.retain(|_, index| *index < self.history.len());
    }
}
