mod error;
mod frame;
mod io;
mod size;
mod types;

pub use error::ProtoError;
pub use frame::{MAX_FRAME_SIZE, PROTOCOL_VERSION, decode_frame, encode_frame, encode_payload};
pub use io::{
    DEFAULT_SOCKET_PATH, SOCKET_PATH_ENV, read_message, read_payload, socket_path, write_message,
};
pub use types::{
    AccountInfo, AssistantToolCallRecord, CatalogProvider, ChatReasoningField, ClientMessage,
    ContextConfig, DaemonMessage, DiscardedToolCall, DisplayedImageRecord, ImageMetadata,
    InferenceError, OutputStream, ReasoningArtifact, ReasoningCapability, ReasoningProducer,
    RefreshStatus, SessionStatus, SessionSummary, TimestampMs, TokenUsage, ToolResultRecord, Turn,
};

#[cfg(test)]
mod tests;
