use crate::error::ClientError;
use crate::{FileDiff, ImageAssembler};
use std::collections::HashMap;
use tai_proto::{ImageMetadata, OutputStream, SessionMessage};

pub const MAX_HISTORY_ITEMS: usize = 500;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamingText {
    pub request_id: u32,
    pub reasoning: String,
    pub answer: String,
}

impl StreamingText {
    pub fn new(request_id: u32) -> Self {
        Self {
            request_id,
            reasoning: String::new(),
            answer: String::new(),
        }
    }

    pub fn append(&mut self, stream: OutputStream, chunk: &str) {
        match stream {
            OutputStream::Answer => self.answer.push_str(chunk),
            OutputStream::Reasoning => self.reasoning.push_str(chunk),
            _ => {}
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistoryItem<TImage> {
    Text(String),
    SessionMessage(SessionMessage),
    Streaming(StreamingText),
    Image(TImage),
    Diff(Vec<FileDiff>),
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

    pub fn push_diff(&mut self, diffs: Vec<FileDiff>) {
        self.push_history_item(HistoryItem::Diff(diffs));
    }

    pub fn push_image(&mut self, image: TImage) {
        self.push_history_item(HistoryItem::Image(image));
    }

    pub fn push_history_item(&mut self, item: HistoryItem<TImage>) {
        self.history.push(item);
        self.trim_history();
    }

    pub fn insert_before_stream(&mut self, request_id: u32, item: HistoryItem<TImage>) {
        if let Some(&index) = self.in_progress.get(&request_id) {
            self.history.insert(index, item);
            for idx in self.in_progress.values_mut() {
                if *idx >= index {
                    *idx += 1;
                }
            }
            self.trim_history();
        } else {
            self.history.push(item);
            self.trim_history();
        }
    }

    pub fn insert_text_before_stream(&mut self, request_id: u32, text: impl Into<String>) {
        self.insert_before_stream(request_id, HistoryItem::Text(text.into()));
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

    pub fn start_image(
        &mut self,
        request_id: u32,
        metadata: ImageMetadata,
    ) -> Result<(), ClientError> {
        self.pending_images.start(request_id, metadata)
    }

    pub fn push_image_chunk(
        &mut self,
        request_id: u32,
        image_id: u32,
        data: &[u8],
    ) -> Result<(), ClientError> {
        self.pending_images.push_chunk(request_id, image_id, data)
    }

    pub fn finish_image(
        &mut self,
        request_id: u32,
        image_id: u32,
    ) -> Result<(ImageMetadata, Vec<u8>), ClientError> {
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
        self.in_progress
            .retain(|_, index| *index < self.history.len());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DiffHunk;

    fn sample_diffs() -> Vec<FileDiff> {
        vec![FileDiff {
            old_path: "old.txt".into(),
            new_path: "new.txt".into(),
            hunks: vec![DiffHunk {
                header: "@@ -1 +1 @@".into(),
                lines: vec![],
            }],
        }]
    }

    #[test]
    fn push_diff_appends_diff_item() {
        let mut hist: ClientHistory<()> = ClientHistory::new(vec![]);
        let diffs = sample_diffs();
        hist.push_diff(diffs.clone());
        assert_eq!(hist.history.len(), 1);
        match &hist.history[0] {
            HistoryItem::Diff(d) => assert_eq!(d.len(), 1),
            other => panic!("expected Diff, got {other:?}"),
        }
    }

    #[test]
    fn push_diff_respects_max_history() {
        let mut hist: ClientHistory<()> = ClientHistory::new(
            (0..MAX_HISTORY_ITEMS)
                .map(|i| HistoryItem::Text(format!("line {i}")))
                .collect(),
        );
        assert_eq!(hist.history.len(), MAX_HISTORY_ITEMS);
        hist.push_diff(sample_diffs());
        assert_eq!(hist.history.len(), MAX_HISTORY_ITEMS);
        match &hist.history[MAX_HISTORY_ITEMS - 1] {
            HistoryItem::Diff(_) => {} // newest item is the diff
            other => panic!("expected Diff at tail, got {other:?}"),
        }
    }

    #[test]
    fn insert_diff_before_stream_inserts_in_middle() {
        let mut hist: ClientHistory<()> =
            ClientHistory::new(vec![HistoryItem::Text("before".into())]);
        hist.begin_stream(7);
        assert_eq!(hist.history.len(), 2);
        assert_eq!(hist.in_progress.get(&7), Some(&1));

        hist.insert_before_stream(7, HistoryItem::Diff(sample_diffs()));

        assert_eq!(hist.history.len(), 3);
        match &hist.history[1] {
            HistoryItem::Diff(_) => {}
            other => panic!("expected Diff at index 1, got {other:?}"),
        }
        assert_eq!(hist.in_progress.get(&7), Some(&2));
    }

    #[test]
    fn insert_diff_before_stream_falls_back_to_push_when_no_stream() {
        let mut hist: ClientHistory<()> = ClientHistory::new(vec![]);
        hist.insert_before_stream(99, HistoryItem::Diff(sample_diffs()));
        assert_eq!(hist.history.len(), 1);
        match &hist.history[0] {
            HistoryItem::Diff(_) => {}
            other => panic!("expected Diff, got {other:?}"),
        }
    }
}
