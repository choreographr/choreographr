mod error;
mod frame;
mod io;
mod types;

pub use error::ProtoError;
pub use frame::{decode_frame, encode_frame, MAX_FRAME_SIZE, PROTOCOL_VERSION};
pub use io::{
    read_message_sync, read_payload_sync, socket_path, write_message_sync, DEFAULT_SOCKET_PATH,
    SOCKET_PATH_ENV,
};
#[cfg_attr(not(test), allow(unused_imports))]
pub(crate) use io::socket_path_impl;
pub use types::{
    AssistantToolCallRecord, ClientMessage, DaemonMessage, DisplayedImageRecord, ImageMetadata,
    MAX_IMAGE_CHUNK_SIZE, OutputStream, SessionMessage, SessionStatus, SessionSummary,
};

#[cfg(test)]
mod tests;
