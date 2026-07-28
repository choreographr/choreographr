use std::io;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum ProtoError {
    #[error("postcard error: {0}")]
    Postcard(String),
    #[error("frame too large")]
    FrameTooLarge,
    #[error("trailing bytes in frame")]
    TrailingBytes,
    #[error("unsupported protocol version: {version}")]
    UnsupportedVersion { version: u8 },
    #[error(transparent)]
    Io(#[from] io::Error),
}

impl From<ProtoError> for io::Error {
    fn from(error: ProtoError) -> Self {
        match error {
            ProtoError::Io(io) => io,
            ProtoError::Postcard(_)
            | ProtoError::FrameTooLarge
            | ProtoError::TrailingBytes
            | ProtoError::UnsupportedVersion { .. } => {
                io::Error::new(io::ErrorKind::InvalidData, error)
            }
        }
    }
}
