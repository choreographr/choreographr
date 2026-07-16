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

/// Accumulated streaming output for an in-progress tool call.
///
/// Created by `begin_tool_result_stream` and progressively populated by
/// `append_tool_result` as chunks arrive.  The entry is replaced in-place
/// by the canonical `HistoryItem::SessionMessage(ToolResult)` when the
/// daemon's post-request snapshot arrives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolResultStreamData {
    pub request_id: u32,
    pub call_id: String,
    pub tool_name: String,
    /// All tool output received so far, accumulated as plain text.
    pub accumulated_text: String,
    /// Whether the tool completed with an error.
    pub is_error: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistoryItem<TImage> {
    Text(String),
    SessionMessage(SessionMessage),
    Streaming(StreamingText),
    ToolResultStream(ToolResultStreamData),
    Image(TImage),
    Diff(Vec<FileDiff>),
}

#[derive(Debug, Clone)]
pub struct ClientHistory<TImage> {
    pub history: Vec<HistoryItem<TImage>>,
    pub in_progress: HashMap<u32, usize>,
    pub tool_streams: HashMap<u32, usize>,
}

impl<TImage> ClientHistory<TImage> {
    pub fn new(history: Vec<HistoryItem<TImage>>) -> Self {
        Self {
            history,
            in_progress: HashMap::new(),
            tool_streams: HashMap::new(),
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
            // Slide back tool_streams entries that pointed past the
            // removed streaming item — otherwise they become stale and
            // index into the wrong history entry.
            for other in self.tool_streams.values_mut() {
                if *other > index {
                    *other -= 1;
                }
            }
        }

        // Also drop any tool result stream for this request_id
        self.drop_tool_result_stream(request_id);
    }

    /// Begin tracking a tool result stream for this request.
    /// Creates a `ToolResultStream` history item and records its index.
    pub fn begin_tool_result_stream(
        &mut self,
        request_id: u32,
        call_id: String,
        tool_name: String,
    ) {
        if self.tool_streams.contains_key(&request_id) {
            return;
        }
        let item = HistoryItem::ToolResultStream(ToolResultStreamData {
            request_id,
            call_id,
            tool_name,
            accumulated_text: String::new(),
            is_error: false,
        });
        // Insert before the answer stream if one exists, otherwise append.
        if let Some(&stream_idx) = self.in_progress.get(&request_id) {
            self.history.insert(stream_idx, item);
            for idx in self.in_progress.values_mut() {
                if *idx >= stream_idx {
                    *idx += 1;
                }
            }
            // Record the tool stream at the index where it was just inserted.
            // The streaming text (in_progress) was pushed to stream_idx + 1.
            self.tool_streams.insert(request_id, stream_idx);
            for idx in self.tool_streams.values_mut() {
                if *idx > stream_idx {
                    *idx += 1;
                }
            }
        } else {
            let index = self.history.len();
            self.history.push(item);
            self.tool_streams.insert(request_id, index);
        }
        self.trim_history();
    }

    /// Append text to an active tool result stream.
    pub fn append_tool_result(&mut self, request_id: u32, data: &str) {
        if let Some(&index) = self.tool_streams.get(&request_id)
            && let Some(HistoryItem::ToolResultStream(entry)) = self.history.get_mut(index)
        {
            entry.accumulated_text.push_str(data);
        }
    }

    /// Mark a tool result stream as completed and record whether it errored.
    /// Returns the accumulated text and tool name, or None if not found.
    pub fn finalize_tool_result_stream(
        &mut self,
        request_id: u32,
        is_error: bool,
    ) -> Option<(String, String)> {
        let index = self.tool_streams.remove(&request_id);
        if let Some(index) = index
            && let Some(HistoryItem::ToolResultStream(entry)) = self.history.get_mut(index)
        {
            entry.is_error = is_error;
            Some((entry.accumulated_text.clone(), entry.tool_name.clone()))
        } else {
            None
        }
    }

    /// Remove a tool result stream without finalizing it (cleanup).
    fn drop_tool_result_stream(&mut self, request_id: u32) {
        if let Some(index) = self.tool_streams.remove(&request_id)
            && index < self.history.len()
        {
            self.history.remove(index);
            // Slide back any subsequent indices
            for idx in self.in_progress.values_mut() {
                if *idx > index {
                    *idx -= 1;
                }
            }
            for idx in self.tool_streams.values_mut() {
                if *idx > index {
                    *idx -= 1;
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
                return false;
            }
            *index -= excess;
            *index < self.history.len()
        });
        // Same for tool result stream indices.
        self.tool_streams.retain(|_, index| {
            if *index < excess {
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

    // ── tool result stream methods ──

    #[test]
    fn begin_tool_result_stream_creates_entry_and_tracks_index() {
        let mut hist: ClientHistory<()> =
            ClientHistory::new(vec![HistoryItem::Text("before".into())]);
        hist.begin_tool_result_stream(1, "call_1".into(), "read_file".into());

        assert!(
            hist.tool_streams.contains_key(&1),
            "should track request_id"
        );
        let index = hist.tool_streams[&1];
        assert_eq!(index, 1, "should be inserted after the existing text item");
        match &hist.history[index] {
            HistoryItem::ToolResultStream(data) => {
                assert_eq!(data.request_id, 1);
                assert_eq!(data.call_id, "call_1");
                assert_eq!(data.tool_name, "read_file");
                assert!(data.accumulated_text.is_empty());
                assert!(!data.is_error);
            }
            other => panic!("expected ToolResultStream, got {other:?}"),
        }
    }

    #[test]
    fn begin_tool_result_stream_is_idempotent() {
        let mut hist: ClientHistory<()> = ClientHistory::new(vec![]);
        hist.begin_tool_result_stream(1, "call_1".into(), "read_file".into());
        assert_eq!(hist.history.len(), 1);

        // Calling again with the same request_id should be a no-op.
        hist.begin_tool_result_stream(1, "call_2".into(), "write_file".into());
        assert_eq!(hist.history.len(), 1, "should not add a second entry");
        match &hist.history[0] {
            HistoryItem::ToolResultStream(data) => {
                assert_eq!(data.call_id, "call_1", "first call_id preserved");
                assert_eq!(data.tool_name, "read_file", "first tool_name preserved");
            }
            other => panic!("expected ToolResultStream, got {other:?}"),
        }
    }

    #[test]
    fn begin_tool_result_stream_inserts_before_streaming_item() {
        let mut hist: ClientHistory<()> = ClientHistory::new(vec![]);
        hist.begin_stream(1); // Streaming item at index 0
        hist.begin_tool_result_stream(1, "call_1".into(), "grep".into());

        // Tool result stream should be inserted BEFORE the streaming item.
        let stream_idx = hist.in_progress[&1];
        let tool_idx = hist.tool_streams[&1];
        assert_eq!(tool_idx, 0, "tool stream at index 0");
        assert_eq!(stream_idx, 1, "answer stream shifted to index 1");
        assert!(matches!(&hist.history[0], HistoryItem::ToolResultStream(_)));
        assert!(matches!(&hist.history[1], HistoryItem::Streaming(_)));
    }

    #[test]
    fn append_tool_result_updates_accumulated_text() {
        let mut hist: ClientHistory<()> = ClientHistory::new(vec![]);
        hist.begin_tool_result_stream(1, "call_1".into(), "grep".into());

        hist.append_tool_result(1, "hello\n");
        hist.append_tool_result(1, "world");

        let index = hist.tool_streams[&1];
        match &hist.history[index] {
            HistoryItem::ToolResultStream(data) => {
                assert_eq!(data.accumulated_text, "hello\nworld");
            }
            other => panic!("expected ToolResultStream, got {other:?}"),
        }
    }

    #[test]
    fn append_tool_result_unknown_request_is_noop() {
        let mut hist: ClientHistory<()> = ClientHistory::new(vec![]);
        // No stream started — should not panic.
        hist.append_tool_result(99, "data");
    }

    #[test]
    fn finalize_tool_result_stream_marks_error_and_returns_content() {
        let mut hist: ClientHistory<()> = ClientHistory::new(vec![]);
        hist.begin_tool_result_stream(1, "call_1".into(), "read_file".into());
        hist.append_tool_result(1, "error: not found");

        let result = hist.finalize_tool_result_stream(1, true);
        assert!(result.is_some(), "should return content");
        let (content, tool_name) = result.unwrap();
        assert_eq!(content, "error: not found");
        assert_eq!(tool_name, "read_file");

        // Stream entry should still exist (just marked as error).
        assert!(!hist.tool_streams.contains_key(&1), "tracking removed");
        // The history item is still there and marked as error.
        assert!(matches!(
            &hist.history[0],
            HistoryItem::ToolResultStream(data) if data.is_error
        ));
    }

    #[test]
    fn finalize_tool_result_stream_unknown_request_returns_none() {
        let mut hist: ClientHistory<()> = ClientHistory::new(vec![]);
        assert!(hist.finalize_tool_result_stream(99, false).is_none());
    }

    #[test]
    fn drop_request_removes_both_stream_and_tool_stream() {
        let mut hist: ClientHistory<()> =
            ClientHistory::new(vec![HistoryItem::Text("keep".into())]);
        hist.begin_stream(1); // index 1
        hist.begin_tool_result_stream(1, "call_1".into(), "ls".into()); // index 1 (before stream)

        assert!(hist.in_progress.contains_key(&1));
        assert!(hist.tool_streams.contains_key(&1));
        assert_eq!(hist.history.len(), 3);

        hist.drop_request(1);

        assert!(!hist.in_progress.contains_key(&1));
        assert!(!hist.tool_streams.contains_key(&1));
        // Both the streaming item and the tool result stream item should be removed.
        assert_eq!(hist.history.len(), 1, "only the original text item remains");
    }

    #[test]
    fn tool_stream_survives_history_trim() {
        let mut hist: ClientHistory<()> = ClientHistory::new(
            (0..MAX_HISTORY_ITEMS - 1)
                .map(|i| HistoryItem::Text(format!("line {i}")))
                .collect(),
        );
        // The tool stream is appended at the end.
        hist.begin_tool_result_stream(1, "call_1".into(), "grep".into());
        let before_idx = hist.tool_streams[&1];
        assert_eq!(before_idx, MAX_HISTORY_ITEMS - 1);

        // Push one more item to trigger trimming.
        hist.push_text("new item");

        // The tool stream should survive trimming (it was near the end).
        assert!(hist.tool_streams.contains_key(&1), "tool stream survived");
        let after_idx = hist.tool_streams[&1];
        assert!(after_idx < hist.history.len(), "index in bounds");
        // The history should still have the tool stream item.
        assert!(matches!(
            &hist.history[after_idx],
            HistoryItem::ToolResultStream(_)
        ));
    }

    #[test]
    fn tool_stream_is_removed_when_dropped_request() {
        let mut hist: ClientHistory<()> = ClientHistory::new(
            (0..5)
                .map(|i| HistoryItem::Text(format!("line {i}")))
                .collect(),
        );
        hist.begin_tool_result_stream(1, "c1".into(), "grep".into());
        assert!(hist.tool_streams.contains_key(&1));

        // drop_request removes the tool result stream.
        hist.drop_request(1);
        assert!(
            !hist.tool_streams.contains_key(&1),
            "tool stream should be removed by drop_request"
        );
        // The history item itself should also be gone (3 items: 5 text + 1 stream = 6, after
        // dropping the stream and the in_progress item that doesn't exist, we're back to 5).
        assert_eq!(hist.history.len(), 5, "stream item removed from history");
    }

    #[test]
    fn tool_stream_indices_adjusted_on_history_trim() {
        // Build a history where the tool stream is at a non-zero index and
        // verify that trimming from the front updates its position correctly.
        let mut hist: ClientHistory<()> = ClientHistory::new(vec![
            HistoryItem::Text("will be trimmed".into()),
            HistoryItem::Text("survivor".into()),
        ]);
        // Insert tool stream at index 2 (after two text items).
        hist.begin_tool_result_stream(1, "c1".into(), "grep".into());
        assert_eq!(hist.tool_streams[&1], 2);

        // Push enough items to trigger trimming of the first item (index 0).
        // History was len=3, MAX_HISTORY_ITEMS=500, so we need 498 more pushes.
        for i in 0..498 {
            hist.push_text(format!("fill {i}"));
        }
        // After all pushes and trims, the first text item ("will be trimmed") is gone,
        // and the tool stream index should have been adjusted.
        let idx = hist.tool_streams[&1];
        // The tool stream originally at index 2 had 1 item trimmed before it → now at index 1.
        assert_eq!(idx, 1, "tool stream index adjusted after trim");
        assert!(matches!(
            &hist.history[idx],
            HistoryItem::ToolResultStream(_)
        ));
    }

    #[test]
    fn tool_stream_trimmed_when_in_drained_range() {
        // Directly construct a history where the tool stream is at the very front,
        // then push items past MAX to verify it gets removed during trim.
        let mut items: Vec<HistoryItem<()>> = (0..MAX_HISTORY_ITEMS)
            .map(|i| HistoryItem::Text(format!("line {i}")))
            .collect();
        // Replace the first item with a ToolResultStream.
        items[0] = HistoryItem::ToolResultStream(ToolResultStreamData {
            request_id: 1,
            call_id: "c1".into(),
            tool_name: "grep".into(),
            accumulated_text: String::new(),
            is_error: false,
        });
        let mut hist = ClientHistory {
            history: items,
            in_progress: HashMap::new(),
            tool_streams: [(1, 0)].into(),
        };
        assert_eq!(hist.history.len(), MAX_HISTORY_ITEMS);

        // Push one more item to trigger trimming from front.
        hist.push_text("new item");
        assert_eq!(hist.history.len(), MAX_HISTORY_ITEMS);
        // The tool stream at index 0 should have been removed.
        assert!(
            !hist.tool_streams.contains_key(&1),
            "tool stream trimmed from front"
        );
        // The new item should be at the back.
        assert!(matches!(hist.history.last().unwrap(), HistoryItem::Text(t) if t == "new item"));
    }
}
