//! The shared transport error type.

use thiserror::Error;

/// Errors surfaced by the transport: handshake, framing, and socket failures.
#[derive(Error, Debug)]
pub enum TransportError {
    /// An underlying socket I/O failure that is not a peer close.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// A failure inside the snow Noise state machine.
    #[error("Noise protocol error: {0}")]
    Noise(#[from] snow::Error),
    /// A wire-protocol error from the shared codec.
    #[error("Protocol error: {0}")]
    Protocol(#[from] choreo_proto::ProtoError),
    /// The peer (or this stream's usage) violated the transport framing
    /// contract: an oversized fragment length, a reassembly that would exceed
    /// the codec's message cap, or a concurrent second sender on one stream.
    /// The stream is unusable after this error.
    #[error("invalid transport fragment: {0}")]
    InvalidFragment(String),
    /// The Noise IK handshake did not complete within its total budget. The
    /// budget bounds the WHOLE handshake, not just a single read: a peer that
    /// dribbles bytes to keep resetting the per-read socket timeout is still
    /// cut off (see `handshake::read_handshake_exact`).
    #[error("Noise handshake timed out")]
    HandshakeTimeout,
    /// The peer presented credentials that the ACL check rejected.
    #[error("Authentication failed")]
    AuthFailed,
    /// The peer closed the connection (EOF before a full frame, or a
    /// connection reset) while this stream was reading. Produced by
    /// `noise::recv_message` when the underlying `read_exact` fails with an
    /// EOF-class error kind; callers treat it as a graceful disconnect,
    /// distinct from a protocol failure.
    #[error("Connection closed")]
    ConnectionClosed,
    /// The OS exposed no configuration directory to place the keypair in.
    #[error("could not determine config directory")]
    ConfigDirNotFound,
}
