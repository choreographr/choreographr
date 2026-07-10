use std::fmt::Debug;
use std::io;
use std::sync::mpsc;

use crate::openai::{ChatRequestMessage, ChatToolDefinition, ChatTurnResult, CompletionChunkKind};
use crate::retry::RetryCallback;
use tai_proto::InferenceError;

/// Trait that every provider client must implement.
/// Uses `&mut dyn FnMut` for the streaming callback to keep the trait object-safe.
pub trait ProviderClient: Debug + Send + Sync {
    fn chat_completion_turn(
        &self,
        model: &str,
        messages: &[ChatRequestMessage],
        tools: &[ChatToolDefinition],
        on_retry: &mut Option<RetryCallback>,
        cancel_rx: Option<&mpsc::Receiver<()>>,
    ) -> Result<ChatTurnResult, InferenceError>;

    fn chat_completion_turn_streaming(
        &self,
        model: &str,
        messages: &[ChatRequestMessage],
        tools: &[ChatToolDefinition],
        on_retry: &mut Option<RetryCallback>,
        cancel_rx: Option<&mpsc::Receiver<()>>,
        on_chunk: &mut dyn FnMut(CompletionChunkKind, String) -> io::Result<()>,
    ) -> Result<ChatTurnResult, InferenceError>;

    fn list_models(&self) -> Result<Vec<String>, InferenceError>;
}
