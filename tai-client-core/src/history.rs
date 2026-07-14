use crate::FileDiff;
use std::collections::HashMap;
use tai_proto::{OutputStream, SessionMessage};
use tracing::{debug, info, trace};

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
}

impl<TImage> ClientHistory<TImage> {
    pub fn new(history: Vec<HistoryItem<TImage>>) -> Self {
        Self {
            history,
            in_progress: HashMap::new(),
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
        debug!("pushing history item");
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
        trace!("begin stream for request {request_id}");
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
        trace!("append stream for request {request_id}: {stream:?}");
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
        trace!("finalize stream for request {request_id}");
        self.in_progress.remove(&request_id);
    }

    pub fn drop_request(&mut self, request_id: u32) {
        // Save the index before calling finalize_stream so we can also remove
        // the streaming item from history — this prevents a stale duplicate
        // when the daemon later sends a SessionMessageAppended for the same
        // content (AssistantText, ToolResult, etc.) after Done.
        let index = self.in_progress.get(&request_id).copied();
        self.finalize_stream(request_id);
        if let Some(index) = index
            && index < self.history.len()
        {
            self.history.remove(index);
            // Slide back any other in-progress entries that pointed past
            // the removed streaming item.
            for other in self.in_progress.values_mut() {
                if *other > index {
                    *other -= 1;
                }
            }
        }
    }

    fn trim_history(&mut self) {
        if self.history.len() <= MAX_HISTORY_ITEMS {
            return;
        }
        let excess = self.history.len() - MAX_HISTORY_ITEMS;
        info!(
            "trimming history by {excess} items (current size: {})",
            self.history.len()
        );
        self.history.drain(0..excess);
        // Remove any in-progress entries whose streaming items were in the
        // drained range, and shift the rest down by `excess`.
        self.in_progress.retain(|_, index| {
            if *index < excess {
                // Streaming item was at an index that got drained — drop the
                // tracking entry so we don't point to a different item.
                return false;
            }
            *index -= excess;
            *index < self.history.len()
        });
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
