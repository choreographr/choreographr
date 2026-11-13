use choreo_proto::{ReasoningArtifact, TokenUsage};

// Re-exported so an adapter names these through one module: the finish outcome
// lives in `choreo_proto` because `InferenceError` carries it, and a discarded
// call rides on both the turn results and that error.
pub use choreo_proto::{DiscardedToolCall, FinishOutcome};

/// Information about the caller that initiated a tool call.
/// Stored alongside tool call records for auditing/filtering.
///
/// This is the wire-side shape of the attribution: a Responses API
/// `function_call` item carries it verbatim, and the answer to that call must
/// copy it back unchanged (see
/// <https://developers.openai.com/api/docs/guides/tools-programmatic-tool-calling>).
/// Its persisted counterpart is [`choreo_proto::ToolCallCaller`] — the same fact
/// at the layer below, which cannot name this type because the dependency runs
/// the other way.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CallerInfo {
    /// The caller kind tag (`"direct"` or `"programmatic"`), serialized as
    /// the wire field `type`.
    #[serde(rename = "type")]
    pub kind: String,
    /// Provider-assigned identifier of the caller the tool call originated
    /// from. A `direct` caller carries none, so an absent value deserializes
    /// empty and an empty value is not serialized: the attribution then
    /// round-trips as the bare `{"type": "direct"}` the provider sent instead
    /// of gaining an empty `caller_id`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub caller_id: String,
}

impl From<&CallerInfo> for choreo_proto::ToolCallCaller {
    fn from(caller: &CallerInfo) -> Self {
        choreo_proto::ToolCallCaller {
            kind: caller.kind.clone(),
            caller_id: caller.caller_id.clone(),
        }
    }
}

impl From<&choreo_proto::ToolCallCaller> for CallerInfo {
    fn from(caller: &choreo_proto::ToolCallCaller) -> Self {
        CallerInfo {
            kind: caller.kind.clone(),
            caller_id: caller.caller_id.clone(),
        }
    }
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
    /// Tool calls the provider sent with arguments that were not valid JSON, so
    /// they were dropped from [`Self::tool_calls`]. Empty on a clean reply.
    ///
    /// Reported rather than forgotten: a reply that mixes runnable calls with a
    /// cut-off one still did its real work, but the lost call must reach the
    /// model on the next turn or it will report work that never happened.
    pub discarded_tool_calls: Vec<DiscardedToolCall>,
    /// How the provider said the turn ended (see [`FinishOutcome`]).
    pub finish: FinishOutcome,
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
    /// How the provider said the turn ended (see [`FinishOutcome`]).
    ///
    /// [`FinishOutcome::Length`] on a final-text turn means the answer was cut
    /// off by the output token limit, not finished: the daemon appends a
    /// user-visible truncation notice. `length` + tool calls is normal tool-loop
    /// flow, so a [`ChatTurnResult::ToolUse`] turn carries the outcome but no
    /// notice is ever shown for it. Adapters that cannot read a finish reason
    /// report [`FinishOutcome::Absent`], which asserts nothing about why the
    /// reply ended.
    pub finish: FinishOutcome,
    /// Tool calls the provider returned with arguments that were not valid
    /// JSON (typically cut off mid-response), so they were discarded as unsafe
    /// to execute — empty on an ordinary final answer.
    ///
    /// A final answer can carry a casualty: a reply whose calls were all
    /// truncated but that also carried visible text is still an answer, so the
    /// text is surfaced here instead of being replaced by an error. The dropped
    /// calls are not silently forgotten, though: the daemon reads this list to
    /// route the turn into the same truncated-tool-call recovery a text-less
    /// one gets — the prose is kept in the transcript, the user is told which
    /// calls were lost, and the model is asked to retry in smaller steps.
    /// Without that signal the turn looks like a finished answer and ends the
    /// request mid-task.
    pub discarded_tool_calls: Vec<DiscardedToolCall>,
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
/// A self-describing enum: each variant carries its data inline instead of
/// sharing one payload slot, so a consumer receives these through the
/// `on_event` callback of
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
