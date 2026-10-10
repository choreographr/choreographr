use choreo_proto::{ReasoningArtifact, TokenUsage};

/// Information about the caller that initiated a tool call.
/// Stored alongside tool call records for auditing/filtering.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CallerInfo {
    /// The caller kind tag (`"direct"` or `"programmatic"`), serialized as
    /// the wire field `type`.
    #[serde(rename = "type")]
    pub kind: String,
    /// Provider-assigned identifier of the caller the tool call originated
    /// from.
    pub caller_id: String,
}

/// One tool call the assistant requested in a completed turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatToolCall {
    /// Provider-assigned id, echoed back with the tool result so the provider
    /// can correlate the call with its result.
    pub id: String,
    /// Tool name (must match a definition the request advertised).
    pub name: String,
    /// The tool arguments as the raw JSON string the provider sent — kept as a
    /// string, never re-serialized, so a truncated or malformed payload can be
    /// detected and reported instead of silently normalized.
    pub arguments_json: String,
    /// Optional caller attribution (programmatic tool calling).
    pub caller: Option<CallerInfo>,
}

/// A turn in which the assistant requested one or more tool calls, carrying
/// any accompanying text, reasoning, and token usage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatAssistantToolUse {
    /// Text the assistant emitted alongside the tool calls, when any.
    pub content: Option<String>,
    /// The tool calls the assistant requested this turn.
    pub tool_calls: Vec<ChatToolCall>,
    /// Display text of the assistant's reasoning, when the provider produced
    /// it.
    pub reasoning: Option<String>,
    /// Token accounting for the turn, when the provider reported it.
    pub usage: Option<TokenUsage>,
    /// Provider response id (Responses API chaining via
    /// `previous_response_id`), when the provider sent one.
    pub response_id: Option<String>,
    /// Opaque reasoning round-trip artifact captured by the producing adapter
    /// at parse time; replayed verbatim on the next turn in that adapter's own
    /// wire format.
    pub reasoning_artifact: Option<ReasoningArtifact>,
}

/// A turn that ended in a final answer rather than a tool request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FinalTextResult {
    /// The assistant's final answer text.
    pub content: String,
    /// The provider reported `finish_reason: "length"` (or equivalent) on a
    /// final-text turn: the answer was cut off by the output token limit, not
    /// finished. Purely informational for consumers — the daemon appends the
    /// user-visible truncation notice when it consumes the result; only the
    /// chat-completions adapter sets this today (other providers leave it
    /// `false`). `length` + tool calls is normal tool-loop flow, so a
    /// `ToolUse` turn never carries it.
    pub truncated: bool,
    /// Display text of the assistant's reasoning, when the provider produced
    /// it.
    pub reasoning: Option<String>,
    /// Token accounting for the turn, when the provider reported it.
    pub usage: Option<TokenUsage>,
    /// Provider response id (Responses API chaining), when the provider sent
    /// one.
    pub response_id: Option<String>,
    /// Opaque reasoning round-trip artifact captured by the producing adapter
    /// at parse time.
    pub reasoning_artifact: Option<ReasoningArtifact>,
}

/// The outcome of a chat turn: either a final answer, or a request for tool
/// use (the caller runs the tools and feeds their results into the next turn).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ChatTurnResult {
    /// The assistant produced a final answer.
    FinalText(FinalTextResult),
    /// The assistant requested one or more tool calls.
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
    /// A chunk of the assistant's answer text.
    Answer(String),
    /// A chunk of the assistant's reasoning (display-only) text.
    Reasoning(String),
}
