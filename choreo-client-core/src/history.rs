use choreo_proto::{AssistantToolCallRecord, OutputStream, ToolResultRecord, Turn};
use choreo_sanitize::{MAX_TOOL_OUTPUT_BYTES, TRUNCATION_SUFFIX};
use std::collections::{BTreeMap, HashMap};

/// Cap on the live accumulated content of a streaming tool result. The
/// daemon's final (authoritative) content is capped at
/// [`choreo_sanitize::MAX_TOOL_OUTPUT_BYTES`] (128 KiB) and replaces this
/// accumulation when the turn completes; the cap here bounds the *in-flight*
/// view so a streaming tool that out-produces the budget (shell/VM/find)
/// cannot balloon client memory before the final record lands. Deliberately
/// mirrors the daemon's cap so the live view and the recorded result agree.
const MAX_STREAMED_TOOL_CONTENT_BYTES: usize = MAX_TOOL_OUTPUT_BYTES;

/// Append `data` to `content`, stopping at [`MAX_STREAMED_TOOL_CONTENT_BYTES`]
/// with the shared `...[truncated]` byte-cap marker once the cap is crossed;
/// later chunks are dropped. Cuts on a char boundary so a multi-byte char is
/// never split. Shared by the append and stub-creation paths of
/// `SessionView::tool_result_chunk`. The marker text is the shared
/// [`choreo_sanitize::TRUNCATION_SUFFIX`], so the live view reads exactly
/// like the daemon's final capped result.
fn push_capped(content: &mut String, data: &str) {
    if content.len() >= MAX_STREAMED_TOOL_CONTENT_BYTES {
        return;
    }
    let remaining = MAX_STREAMED_TOOL_CONTENT_BYTES - content.len();
    if data.len() <= remaining {
        content.push_str(data);
    } else {
        // Same byte-cap marker the daemon's `truncate_tool_output` appends,
        // so the live view reads exactly like the final capped result.
        let cut = data.floor_char_boundary(remaining);
        content.push_str(&data[..cut]);
        content.push_str(TRUNCATION_SUFFIX);
    }
}

/// Client-side view of a session's turn history.
///
/// Maps `turn_id → Turn` (ordered) and `request_id → turn_id` for
/// routing streaming chunks during an active agent loop.
#[derive(Debug, Clone)]
pub struct SessionView {
    /// turn_id → Turn. Ordered by key (monotonically assigned by daemon).
    pub turns: BTreeMap<u32, Turn>,
    /// request_id → turn_id for streaming chunk routing.
    /// Inserted on `Started`, removed on `Done`/`Failed`/`Cancelled`.
    pub request_to_turn: HashMap<u32, u32>,
}

impl SessionView {
    pub fn new() -> Self {
        Self {
            turns: BTreeMap::new(),
            request_to_turn: HashMap::new(),
        }
    }

    pub fn insert_or_replace(&mut self, turn_id: u32, turn: Turn) {
        self.turns.insert(turn_id, turn);
    }

    pub fn get(&self, turn_id: u32) -> Option<&Turn> {
        self.turns.get(&turn_id)
    }

    pub fn get_mut(&mut self, turn_id: u32) -> Option<&mut Turn> {
        self.turns.get_mut(&turn_id)
    }

    pub fn request_turn(&self, request_id: u32) -> Option<&Turn> {
        let turn_id = self.request_to_turn.get(&request_id)?;
        self.turns.get(turn_id)
    }

    pub fn request_turn_mut(&mut self, request_id: u32) -> Option<&mut Turn> {
        let turn_id = self.request_to_turn.get(&request_id)?;
        self.turns.get_mut(turn_id)
    }

    /// Route streaming output to the current turn for this request.
    pub fn stream_chunk(&mut self, request_id: u32, stream: OutputStream, data: &str) {
        let Some(&turn_id) = self.request_to_turn.get(&request_id) else {
            tracing::warn!(%request_id, "stream_chunk: unknown request");
            return;
        };
        let Some(turn) = self.turns.get_mut(&turn_id) else {
            tracing::warn!(%turn_id, "stream_chunk: unknown turn");
            return;
        };
        match stream {
            OutputStream::Reasoning => {
                if let Some(ref mut text) = turn.assistant_reasoning {
                    text.push_str(data);
                } else {
                    turn.assistant_reasoning = Some(data.to_string());
                }
            }
            OutputStream::Answer => {
                // Reasoning content is retained when the response starts —
                // the TUI shows it behind a collapsible header, so clearing
                // it here would destroy the ability to re-expand it.
                if let Some(ref mut text) = turn.assistant_text {
                    text.push_str(data);
                } else {
                    turn.assistant_text = Some(data.to_string());
                }
            }
            _ => {
                tracing::warn!(?stream, "stream_chunk: unknown stream type");
            }
        }
    }

    /// Route a tool call start notification.
    pub fn tool_call_started(
        &mut self,
        request_id: u32,
        call_id: String,
        name: String,
        args: String,
    ) {
        let Some(&turn_id) = self.request_to_turn.get(&request_id) else {
            tracing::warn!(%request_id, "tool_call_started: unknown request");
            return;
        };
        let Some(turn) = self.turns.get_mut(&turn_id) else {
            tracing::warn!(%turn_id, "tool_call_started: unknown turn");
            return;
        };
        // Backfill the tool name onto a stub tool result created out of
        // order (a chunk that arrived before this event).  The stub's name
        // is unresolvable at chunk time if the tool_call hasn't landed yet;
        // filling it in here keeps the TUI's quiet/default-collapse
        // decision correct without waiting for the full turn to replace
        // the record.  Only fills empty names — a seeded placeholder or an
        // earlier backfill already carries the right value.
        if let Some(result) = turn.tool_results.iter_mut().find(|r| r.call_id == call_id)
            && result.name.is_empty()
        {
            result.name = name.clone();
        }
        turn.tool_calls.push(AssistantToolCallRecord {
            call_id,
            name,
            arguments_json: args,
        });
    }

    /// Route a tool result chunk — appends to the matching ToolResultRecord.
    /// Creates a stub record if the ToolCallStarted event hasn't arrived yet.
    pub fn tool_result_chunk(&mut self, request_id: u32, call_id: &str, data: &str) {
        let Some(&turn_id) = self.request_to_turn.get(&request_id) else {
            tracing::warn!(%request_id, "tool_result_chunk: unknown request");
            return;
        };
        let Some(turn) = self.turns.get_mut(&turn_id) else {
            tracing::warn!(%turn_id, "tool_result_chunk: unknown turn");
            return;
        };
        match turn.tool_results.iter_mut().find(|r| r.call_id == call_id) {
            Some(result) => push_capped(&mut result.content, data),
            None => {
                // Stub created out of order (chunk before ToolCallStarted).
                // Resolve the tool name from the turn's tool_calls so the
                // quiet/derived-default decision (which keys on the name)
                // is correct from the first chunk instead of flipping when
                // the real record lands.  Falls back to empty when the call
                // is unknown, which is never quiet → default expanded.
                let name = turn
                    .tool_calls
                    .iter()
                    .find(|tc| tc.call_id == call_id)
                    .map(|tc| tc.name.clone())
                    .unwrap_or_default();
                let mut content = String::new();
                push_capped(&mut content, data);
                turn.tool_results.push(ToolResultRecord {
                    call_id: call_id.to_string(),
                    name,
                    content,
                    is_error: false,
                    invocation_description: String::new(),
                });
            }
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = (&u32, &Turn)> {
        self.turns.iter()
    }
}

impl Default for SessionView {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn_with_tool_call(call_id: &str, name: &str) -> Turn {
        Turn {
            created_at: choreo_proto::TimestampMs::now(),
            undone: false,
            error: None,
            user_text: None,
            assistant_text: None,
            assistant_reasoning: None,
            tool_calls: vec![AssistantToolCallRecord {
                call_id: call_id.into(),
                name: name.into(),
                arguments_json: "{}".into(),
            }],
            token_usage: None,
            tool_results: vec![],
            displayed_images: vec![],
            reasoning_artifact: None,
            reasoning_producer: None,
        }
    }

    #[test]
    fn tool_result_chunk_stub_resolves_name_from_tool_calls() {
        // A chunk arriving before the real record (or before
        // ToolCallStarted) creates a stub.  The stub must carry the tool
        // name — resolved from `tool_calls` — so the TUI's quiet/
        // derived-default decision is correct from the first chunk instead
        // of flipping when the real record lands.
        let mut view = SessionView::new();
        view.insert_or_replace(1, turn_with_tool_call("call-1", "read_file"));
        view.request_to_turn.insert(7, 1);

        view.tool_result_chunk(7, "call-1", "line one\n");

        let turn = view.get(1).unwrap();
        assert_eq!(turn.tool_results.len(), 1);
        assert_eq!(turn.tool_results[0].name, "read_file");
        assert_eq!(turn.tool_results[0].call_id, "call-1");
        assert_eq!(turn.tool_results[0].content, "line one\n");
    }

    #[test]
    fn tool_result_chunk_appends_to_existing_record() {
        let mut view = SessionView::new();
        let mut turn = turn_with_tool_call("call-1", "sh");
        turn.tool_results.push(ToolResultRecord {
            call_id: "call-1".into(),
            name: "sh".into(),
            content: "first".into(),
            is_error: false,
            invocation_description: String::new(),
        });
        view.insert_or_replace(1, turn);
        view.request_to_turn.insert(7, 1);

        view.tool_result_chunk(7, "call-1", "second");

        let turn = view.get(1).unwrap();
        assert_eq!(turn.tool_results.len(), 1, "chunks append, never duplicate");
        assert_eq!(turn.tool_results[0].content, "firstsecond");
    }

    #[test]
    fn tool_result_chunk_caps_live_accumulation() {
        // A streaming tool that out-produces the budget must not balloon the
        // in-flight view: the accumulation stops at the daemon's 128 KiB cap
        // with one marker, and later chunks are dropped entirely.
        let mut view = SessionView::new();
        let mut turn = turn_with_tool_call("call-1", "sh");
        let prefix = "x".repeat(MAX_STREAMED_TOOL_CONTENT_BYTES - 10);
        turn.tool_results.push(ToolResultRecord {
            call_id: "call-1".into(),
            name: "sh".into(),
            content: prefix.clone(),
            is_error: false,
            invocation_description: String::new(),
        });
        view.insert_or_replace(1, turn);
        view.request_to_turn.insert(7, 1);

        // A 20-byte chunk only has 10 bytes of headroom: it is cut on a
        // char boundary and the marker appended once.
        view.tool_result_chunk(7, "call-1", "12345678901234567890");
        let content = &view.get(1).unwrap().tool_results[0].content;
        assert!(
            content.starts_with(&prefix),
            "accumulated content must keep the pre-cap prefix"
        );
        assert!(
            content.ends_with("...[truncated]"),
            "cap crossing must append the marker once"
        );
        assert!(
            content.len() <= MAX_STREAMED_TOOL_CONTENT_BYTES + "\n...[truncated]".len(),
            "content must stay within cap + marker"
        );

        // Everything after the cap is dropped.
        let len_before = content.len();
        view.tool_result_chunk(7, "call-1", "more data");
        assert_eq!(
            view.get(1).unwrap().tool_results[0].content.len(),
            len_before,
            "chunks past the cap must be dropped"
        );
    }

    #[test]
    fn tool_result_chunk_caps_giant_stub_chunk() {
        // A single first chunk larger than the cap (pathological) must be
        // capped even on the out-of-order stub-creation path.
        let mut view = SessionView::new();
        view.insert_or_replace(1, turn_with_tool_call("call-1", "sh"));
        view.request_to_turn.insert(7, 1);

        let giant = "y".repeat(MAX_STREAMED_TOOL_CONTENT_BYTES + 1000);
        view.tool_result_chunk(7, "call-1", &giant);

        let content = &view.get(1).unwrap().tool_results[0].content;
        assert!(
            content.ends_with("...[truncated]"),
            "stub must carry the marker"
        );
        assert!(
            content.len() <= MAX_STREAMED_TOOL_CONTENT_BYTES + "\n...[truncated]".len(),
            "stub content must stay within cap + marker"
        );
    }

    #[test]
    fn tool_result_chunk_unknown_call_keeps_empty_name() {
        // A chunk for a call_id with no matching tool_call creates a stub
        // with an empty name — never quiet, so the TUI defaults it to
        // expanded.
        let mut view = SessionView::new();
        view.insert_or_replace(1, turn_with_tool_call("call-1", "sh"));
        view.request_to_turn.insert(7, 1);

        view.tool_result_chunk(7, "nope", "data");

        let turn = view.get(1).unwrap();
        assert_eq!(turn.tool_results.len(), 1);
        assert_eq!(turn.tool_results[0].name, "");
    }

    #[test]
    fn tool_call_started_backfills_stub_name() {
        // A chunk arriving before ToolCallStarted creates a stub whose name
        // is unresolvable at chunk time; the start event must backfill it
        // so the TUI's quiet/default-collapse decision is correct once the
        // name is known — without waiting for the full turn to replace the
        // record.
        let mut view = SessionView::new();
        view.insert_or_replace(1, turn_with_tool_call("call-1", "sh"));
        view.request_to_turn.insert(7, 1);

        // Stub for a call whose tool_call has not landed yet → empty name.
        view.tool_result_chunk(7, "call-2", "data\n");
        assert_eq!(view.get(1).unwrap().tool_results[0].name, "");

        // The start event arrives: the name is backfilled onto the stub.
        view.tool_call_started(7, "call-2".into(), "read_file".into(), "{}".into());

        let turn = view.get(1).unwrap();
        assert_eq!(turn.tool_results[0].name, "read_file");
        assert_eq!(turn.tool_calls.len(), 2, "the call is recorded as usual");
    }
}
