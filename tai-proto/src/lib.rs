mod error;
mod frame;
mod io;
mod types;

pub use error::ProtoError;
pub use frame::{MAX_FRAME_SIZE, PROTOCOL_VERSION, decode_frame, encode_frame};
#[cfg_attr(not(test), allow(unused_imports))]
pub(crate) use io::socket_path_impl;
pub use io::{
    DEFAULT_SOCKET_PATH, SOCKET_PATH_ENV, read_message_sync, read_payload_sync, socket_path,
    write_message_sync,
};
pub use types::{
    AccountInfo, AssistantToolCallRecord, ClientMessage, ContextConfig, DaemonMessage,
    DisplayedImageRecord, ImageMetadata, InferenceError, MAX_IMAGE_CHUNK_SIZE, OutputStream,
    SessionMessage, SessionStatus, SessionSummary, ThinkingEffort, TokenUsage,
};

#[cfg(test)]
mod tests;
