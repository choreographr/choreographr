use choreo_proto::TokenUsage;

/// Information about the caller that initiated a tool call.
/// Stored alongside tool call records for auditing/filtering.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CallerInfo {
    #[serde(rename = "type")]
    pub(crate) kind: String,
    pub(crate) caller_id: String,
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FinalTextResult {
    pub content: String,
    pub reasoning: Option<String>,
    pub usage: Option<TokenUsage>,
    pub response_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChatTurnResult {
    FinalText(FinalTextResult),
    ToolUse(ChatAssistantToolUse),
}
