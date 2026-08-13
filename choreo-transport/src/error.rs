use thiserror::Error;

#[derive(Error, Debug)]
pub enum TransportError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Noise protocol error: {0}")]
    Noise(#[from] snow::Error),
    #[error("Protocol error: {0}")]
    Protocol(#[from] choreo_proto::ProtoError),
    /// The peer (or this stream's usage) violated the transport framing
    /// contract: an oversized fragment length, a reassembly that would exceed
    /// the codec's message cap, or a concurrent second sender on one stream.
    /// The stream is unusable after this error.
    #[error("invalid transport fragment: {0}")]
    InvalidFragment(String),
    #[error("Authentication failed")]
    AuthFailed,
    #[error("Connection closed")]
    ConnectionClosed,
    #[error("could not determine config directory")]
    ConfigDirNotFound,
}
