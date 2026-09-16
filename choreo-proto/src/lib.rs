mod error;
mod frame;
mod io;
// Public: the release-name source of truth (`release-name.txt`) is compiled in
// here and read by every shipped binary for `--version` and startup logs, and by
// CI for the GitHub release title.
pub mod release_name;
mod size;
mod types;

pub use error::ProtoError;
pub use frame::{MAX_FRAME_SIZE, PROTOCOL_VERSION, decode_frame, encode_frame, encode_payload};
pub use io::{
    SOCKET_PATH_ENV, UnixStream, connect_unix, default_socket_path, dial_error_means_no_listener,
    read_message, read_payload, socket_listening, socket_path, write_message,
};
pub use types::{
    AccountInfo, AssistantToolCallRecord, CatalogProvider, ChatReasoningField, ClientMessage,
    ContextConfig, DaemonMessage, DiscardedToolCall, DisplayedImageRecord, ImageMetadata,
    ImageReference, InferenceError, KeystoreState, OutputStream, ReasoningArtifact,
    ReasoningCapability, ReasoningProducer, RefreshStatus, SessionEvent, SessionStatus,
    SessionSummary, TimestampMs, TokenUsage, ToolResultRecord, Turn,
};

#[cfg(test)]
mod tests;
