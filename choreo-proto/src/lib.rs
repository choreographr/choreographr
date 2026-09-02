mod error;
mod frame;
mod io;
mod size;
mod types;

pub use error::ProtoError;
pub use frame::{MAX_FRAME_SIZE, PROTOCOL_VERSION, decode_frame, encode_frame, encode_payload};
pub use io::{
    SOCKET_PATH_ENV, default_socket_path, read_message, read_payload, socket_path, write_message,
};
pub use types::{
    AccountInfo, AssistantToolCallRecord, CatalogProvider, ChatReasoningField, ClientMessage,
    ContextConfig, DaemonMessage, DiscardedToolCall, DisplayedImageRecord, ImageMetadata,
    ImageReference, InferenceError, OutputStream, ReasoningArtifact, ReasoningCapability,
    ReasoningProducer, RefreshStatus, SessionEvent, SessionStatus, SessionSummary, TimestampMs,
    TokenUsage, ToolResultRecord, Turn,
};

#[cfg(test)]
mod tests;
