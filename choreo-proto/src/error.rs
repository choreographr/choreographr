use std::io;
use thiserror::Error;

/// The unified error type for the wire codec and the socket I/O helpers.
///
/// Returned by the `frame`/`io` encode and decode functions. The [`From`] impl
/// below folds every non-I/O variant into a
/// [`std::io::ErrorKind::InvalidData`] `std::io::Error`, so a caller sitting on
/// an I/O boundary can surface a protocol failure through the standard error
/// type without a bespoke conversion.
#[derive(Error, Debug)]
pub enum ProtoError {
    /// The payload could not be serialized to, or deserialized from, the frame
    /// body (the inner codec failed); the string is the codec's own message.
    #[error("codec error: {0}")]
    Codec(String),
    /// A frame declared a body larger than [`MAX_FRAME_SIZE`](crate::MAX_FRAME_SIZE);
    /// rejected from the length header before any large allocation.
    #[error("frame too large")]
    FrameTooLarge,
    /// The decoder consumed the expected payload but bytes remained in the
    /// frame, so the peer and this decoder disagree on the body length.
    #[error("trailing bytes in frame")]
    TrailingBytes,
    /// The frame's version byte names a protocol version this build does not
    /// speak (see [`PROTOCOL_VERSION`](crate::PROTOCOL_VERSION)).
    #[error("unsupported protocol version: {version}")]
    UnsupportedVersion {
        /// The version byte the peer declared in its frame header.
        version: u8,
    },
    /// An underlying socket read or write failed.
    #[error(transparent)]
    Io(#[from] io::Error),
}

impl From<ProtoError> for io::Error {
    fn from(error: ProtoError) -> Self {
        match error {
            ProtoError::Io(io) => io,
            ProtoError::Codec(_)
            | ProtoError::FrameTooLarge
            | ProtoError::TrailingBytes
            | ProtoError::UnsupportedVersion { .. } => {
                io::Error::new(io::ErrorKind::InvalidData, error)
            }
        }
    }
}
