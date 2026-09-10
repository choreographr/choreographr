use choreo_proto::{ReasoningArtifact, TokenUsage};

/// Information about the caller that initiated a tool call.
/// Stored alongside tool call records for auditing/filtering.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CallerInfo {
    #[serde(rename = "type")]
    pub kind: String,
    pub caller_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatToolCall {
    pub id: String,
    pub name: String,
    pub arguments_json: String,
    pub caller: Option<CallerInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatAssistantToolUse {
    pub content: Option<String>,
    pub tool_calls: Vec<ChatToolCall>,
    pub reasoning: Option<String>,
    pub usage: Option<TokenUsage>,
    pub response_id: Option<String>,
    pub reasoning_artifact: Option<ReasoningArtifact>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FinalTextResult {
    pub content: String,
    /// The provider reported `finish_reason: "length"` (or equivalent) on a
    /// final-text turn: the answer was cut off by the output token limit, not
    /// finished. Purely informational for consumers — the daemon appends the
    /// user-visible truncation notice when it consumes the result; only the
    /// chat-completions adapter sets this today (other providers leave it
    /// `false`). `length` + tool calls is normal tool-loop flow, so a
    /// `ToolUse` turn never carries it.
    pub truncated: bool,
    pub reasoning: Option<String>,
    pub usage: Option<TokenUsage>,
    pub response_id: Option<String>,
    pub reasoning_artifact: Option<ReasoningArtifact>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ChatTurnResult {
    FinalText(FinalTextResult),
    ToolUse(ChatAssistantToolUse),
}

/// A single event emitted during a streaming LLM response.
///
/// Replaces the old `(CompletionChunkKind, String)` tuple with a
/// self-describing enum so each variant carries its data inline.  The
/// consumer receives these through the `on_event` callback of
/// [`chat_completion_turn_streaming`](crate::ProviderClient::chat_completion_turn_streaming)
/// and can use them for real-time UI updates.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum StreamEvent {
    Answer(String),
    Reasoning(String),
}
