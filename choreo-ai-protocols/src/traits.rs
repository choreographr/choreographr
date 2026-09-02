use std::fmt::Debug;
use std::io;

use crate::openai::{ChatRequestMessage, ChatToolDefinition};
use crate::retry::RetryCallback;
use crate::types::{CallerInfo, ChatTurnResult, StreamEvent};
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
    /// Cancellation channel.  A crossbeam receiver so the retry backoff and
    /// SSE waits can `select!` on it alongside their own channels instead of
    /// polling with `recv_timeout`.
    pub cancel_rx: Option<&'a crossbeam_channel::Receiver<()>>,
    /// Response ID from a previous turn (Responses API).
    pub previous_response_id: Option<&'a str>,
    /// Tool results from previous turn (Responses API).
    pub tool_results: &'a [ToolResultItem],
    /// Enable programmatic tool calling (Responses API, gpt-5.6+).
    pub programmatic_tool_calling: bool,
    /// Session identity for gateways that route per session (opencode zen/go
    /// sticky routing). The daemon session id in string form; sent as
    /// `x-opencode-session` to the known gateway providers only.
    pub session_id: String,
    /// Per-turn request id (same id the daemon broadcasts in `SessionEvent::
    /// Started`); sent as `x-opencode-request` alongside `session_id`.
    pub request_id: String,
}

/// Trait that every provider client must implement.
/// Uses `&mut dyn FnMut` for the streaming callback to keep the trait object-safe.
pub trait ProviderClient: Debug + Send + Sync {
    /// Return the provider slug for catalog lookups.
    fn provider_slug(&self) -> &str;

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
