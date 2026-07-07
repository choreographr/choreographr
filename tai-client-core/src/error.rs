use std::io;
use tai_proto::ProtoError;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum ClientError {
    #[error(transparent)]
    Proto(#[from] ProtoError),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Utf8(#[from] std::string::FromUtf8Error),
    #[error("image byte length does not fit in memory")]
    ImageTooLarge,
    #[error("image {image_id} exceeded advertised size")]
    ImageExceedsSize { image_id: u32 },
    #[error("duplicate image {image_id} for request {request_id}")]
    DuplicateImage { image_id: u32, request_id: u32 },
    #[error("unknown image {image_id} for request {request_id}")]
    UnknownImage { image_id: u32, request_id: u32 },
    #[error(
        "image {image_id} for request {request_id} ended with {actual} bytes, expected {expected}"
    )]
    ImageSizeMismatch {
        image_id: u32,
        request_id: u32,
        expected: u64,
        actual: u64,
    },
}

/// Convert an mpsc send error (or any displayable error) into a
/// `ClientError::Io(BrokenPipe)`.  This is the standard pattern when
/// the daemon connection drops and we need to propagate the error
/// through `client_tx.send(...).map_err(broken_pipe)?`.
pub fn broken_pipe(err: impl std::fmt::Display) -> ClientError {
    ClientError::Io(io::Error::new(io::ErrorKind::BrokenPipe, err.to_string()))
}

impl From<ClientError> for io::Error {
    fn from(error: ClientError) -> Self {
        match error {
            ClientError::Proto(proto) => io::Error::from(proto),
            ClientError::Io(io) => io,
            ClientError::Utf8(e) => io::Error::new(io::ErrorKind::InvalidData, e),
            ClientError::ImageTooLarge
            | ClientError::ImageExceedsSize { .. }
            | ClientError::DuplicateImage { .. }
            | ClientError::UnknownImage { .. }
            | ClientError::ImageSizeMismatch { .. } => {
                io::Error::new(io::ErrorKind::InvalidData, error)
            }
        }
    }
}
