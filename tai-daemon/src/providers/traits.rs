use std::fmt::Debug;
use std::io;
use std::sync::mpsc;

use crate::openai::{ChatRequestMessage, ChatToolDefinition, ChatTurnResult, CompletionChunkKind};
use crate::retry::RetryCallback;
use tai_proto::{InferenceError, ThinkingEffort};

/// A single tool result to feed back into a Responses API turn.
#[derive(Debug, Clone)]
pub struct ToolResultItem {
    pub call_id: String,
    pub output: String,
}

/// Holds the common parameters for a chat completion turn.
pub struct ChatTurnRequest<'a> {
    pub model: &'a str,
    pub messages: &'a [ChatRequestMessage],
    pub tools: &'a [ChatToolDefinition],
    pub thinking_effort: ThinkingEffort,
    pub on_retry: &'a mut Option<RetryCallback>,
    pub cancel_rx: Option<&'a mpsc::Receiver<()>>,
    /// Response ID from a previous turn (Responses API).
    pub previous_response_id: Option<&'a str>,
    /// Tool results from previous turn (Responses API).
    pub tool_results: &'a [ToolResultItem],
}

/// Trait that every provider client must implement.
/// Uses `&mut dyn FnMut` for the streaming callback to keep the trait object-safe.
pub trait ProviderClient: Debug + Send + Sync {
    /// Return the provider slug for catalog lookups.
    fn provider_slug(&self) -> &'static str;

    fn chat_completion_turn(
        &self,
        params: ChatTurnRequest<'_>,
    ) -> Result<ChatTurnResult, InferenceError>;

    fn chat_completion_turn_streaming(
        &self,
        params: ChatTurnRequest<'_>,
        on_chunk: &mut dyn FnMut(CompletionChunkKind, String) -> io::Result<()>,
    ) -> Result<ChatTurnResult, InferenceError>;

    fn list_models(&self) -> Result<Vec<String>, InferenceError>;
}
