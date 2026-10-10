//! Wire protocol for the Choreographr daemon/client transport: the framed
//! message envelope, the wire types it carries, and the socket I/O helpers.

// Part of the ARCHITECTURE.md → rustdoc migration (see AGENTS.md → Documentation):
// every public item carries docs, enforced as a hard error by clippy-strict's
// `-D warnings`.
#![warn(missing_docs)]

mod error;
mod frame;
mod io;
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
    ClientMessageType, ContextConfig, DaemonMessage, DaemonMessageType, DiscardedToolCall,
    DisplayedImageRecord, ImageKey, ImageMetadata, ImageReference, InferenceError, KeystoreState,
    McpServerStatus, MessageKind, OutputStream, ReasoningArtifact, ReasoningCapability,
    ReasoningProducer, RefreshStatus, SessionEvent, SessionStatus, SessionSummary, TimestampMs,
    TokenUsage, ToolResultRecord, Turn,
};

#[cfg(test)]
mod tests;
