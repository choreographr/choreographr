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
    /// cut off (see `handshake::frame::read_handshake_exact`). On the responder
    /// side one budget covers the mode-preamble read AND the handshake, so
    /// stalling before the preamble buys no extra time.
    #[error("Noise handshake timed out")]
    HandshakeTimeout,
    /// The peer opened a TCP connection with a preamble byte that names no
    /// handshake mode this build defines (including the reserved `0x00`).
    ///
    /// The byte is the first thing on the wire, before any handshake message, so
    /// this is a protocol-level refusal rather than an authentication failure:
    /// the connection closes without running any handshake.
    #[error("unknown handshake-mode byte {byte:#04x}")]
    HandshakeModeUnknown {
        /// The byte as it arrived, for the operator's diagnosis.
        byte: u8,
    },
    /// A mode-specific responder was asked to run one Noise handshake but the
    /// peer requested the other. The mode-selecting byte and the flow that
    /// follows it must agree, so this is a protocol violation, not an
    /// authorization failure; a server that accepts either mode dispatches on
    /// the byte instead (`handshake::handshake_responder_auto`).
    #[error(
        "peer requested handshake-mode byte {got:#04x} but this responder runs {expected:#04x}"
    )]
    HandshakeModeMismatch {
        /// The byte the responder was built for.
        expected: u8,
        /// The byte the peer actually sent.
        got: u8,
    },
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
    /// No transport secret key file exists: this instance has no identity yet.
    ///
    /// Returned only by the read-only [`crate::key::load_transport_keypair`],
    /// which refuses to generate one — creating an identity belongs to daemon
    /// startup, so a query reports the absence and the operator can start the
    /// daemon rather than discovering a key that was minted by the question.
    #[error("no transport secret key at {path}")]
    MissingSecretKey {
        /// Path the secret was expected at, rendered for display.
        path: String,
    },
    /// The on-disk transport secret key file exists but holds no usable key:
    /// neither a legacy raw 32-byte key nor base64 that decodes to 32 bytes.
    ///
    /// Regeneration is deliberately refused rather than performed, because the
    /// secret is the instance's long-lived identity — minting a new one would
    /// orphan every ACL entry and every pinned server key. The `detail` carries
    /// the decoder's verdict (a byte count or position), never key material.
    #[error("transport secret key file {path} is not a valid key (regeneration refused): {detail}")]
    InvalidSecretKey {
        /// Path of the offending file, rendered for display.
        path: String,
        /// Why the contents could not be decoded — lengths and decoder
        /// positions only, never the file's bytes.
        detail: String,
    },
}
