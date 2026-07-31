use choreo_proto::{AssistantToolCallRecord, OutputStream, ToolResultRecord, Turn};
use std::collections::{BTreeMap, HashMap};

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
            Some(result) => result.content.push_str(data),
            None => {
                turn.tool_results.push(ToolResultRecord {
                    call_id: call_id.to_string(),
                    name: String::new(),
                    content: data.to_string(),
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
