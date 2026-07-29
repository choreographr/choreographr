use std::fmt::Debug;
use std::io;
use std::sync::mpsc;

use crate::openai::{ChatRequestMessage, ChatToolDefinition};
use crate::providers::StreamEvent;
use crate::providers::types::{CallerInfo, ChatTurnResult};
use crate::retry::RetryCallback;
use choreo_proto::InferenceError;

/// A single tool result to feed back into a Responses API turn.
#[derive(Debug, Clone)]
pub struct ToolResultItem {
    pub call_id: String,
    pub output: String,
    pub caller: Option<CallerInfo>,
}

/// Holds the common parameters for a chat completion turn.
pub struct ChatTurnRequest<'a> {
    pub model: &'a str,
    pub messages: &'a [ChatRequestMessage],
    pub tools: &'a [ChatToolDefinition],
    pub thinking_effort: String,
    pub on_retry: &'a mut Option<RetryCallback>,
    pub cancel_rx: Option<&'a mpsc::Receiver<()>>,
    /// Response ID from a previous turn (Responses API).
    pub previous_response_id: Option<&'a str>,
    /// Tool results from previous turn (Responses API).
    pub tool_results: &'a [ToolResultItem],
    /// Enable programmatic tool calling (Responses API, gpt-5.6+).
    pub programmatic_tool_calling: bool,
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
        on_event: &mut dyn FnMut(StreamEvent) -> io::Result<()>,
    ) -> Result<ChatTurnResult, InferenceError>;

    fn list_models(&self) -> Result<Vec<String>, InferenceError>;

    /// Returns whether programmatic tool calling should be enabled for the given model.
    /// The default implementation returns false.
    fn supports_programmatic_tool_calling(&self, _model: &str) -> bool {
        false
    }

    /// Return the context window size for the given model, if known.
    fn context_window_for_model(&self, _model: &str) -> Option<u32> {
        None
    }
}
