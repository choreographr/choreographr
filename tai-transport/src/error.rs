use thiserror::Error;

#[derive(Error, Debug)]
pub enum TransportError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Noise protocol error: {0}")]
    Noise(#[from] snow::Error),
    #[error("Protocol error: {0}")]
    Protocol(#[from] tai_proto::ProtoError),
    #[error("Authentication failed")]
    AuthFailed,
    #[error("Connection closed")]
    ConnectionClosed,
    #[error("could not determine config directory")]
    ConfigDirNotFound,
}
