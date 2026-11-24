//! The shared transport error type.

use choreo_proto::len_prefix::LenPrefixError;
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
    /// The peer violated the transport framing contract: an oversized fragment
    /// length, a reassembly that would exceed the codec's message cap or the
    /// per-message fragment-count bound, or a fragment too short to carry its
    /// authenticated continuation header. A wire-level violation leaves the
    /// stream unusable, because the framing it relied on is no longer
    /// trustworthy.
    ///
    /// A concurrent second sender or reader on one stream is deliberately NOT
    /// this: it is a caller misuse that leaves the stream intact, and it is
    /// reported as [`Self::ConcurrentAccess`] instead.
    #[error("invalid transport fragment: {0}")]
    InvalidFragment(String),
    /// A length or size invariant this crate relies on did not hold, so a frame
    /// could not be built, split, or accepted as one this implementation may put
    /// on the wire. This is a **local** fault — a bug in this build, or a caller
    /// asking for something the transport cannot express (an outgoing message
    /// past the codec's cap, a static key that is not the 32 bytes this crate's
    /// patterns guarantee) — and never a peer violation.
    ///
    /// It is kept apart from [`Self::InvalidFragment`] precisely because the
    /// caller-side classification differs — a peer framing violation is a
    /// handshake refusal worth surfacing as remediation, whereas a local
    /// invariant break must never be blamed on the peer (see
    /// `choreo-client-core`'s `is_handshake_refusal`).
    #[error("internal transport invariant violated: {0}")]
    InternalInvariant(String),
    /// A second sender or reader used one [`crate::noise::NoiseStream`] while
    /// another call still held that direction.
    ///
    /// Each direction has exactly one single-actor slot; a second actor would
    /// interleave two logical messages' fragments on the wire (two senders) or
    /// splice two logical messages together (two readers), neither raising an
    /// error. This is a caller bug, not a wire violation, so — unlike
    /// [`Self::InvalidFragment`] — the stream itself stays usable once the
    /// current holder returns.
    #[error("concurrent {what} on one NoiseStream")]
    ConcurrentAccess {
        /// The entry point that was already in use (`send_message` /
        /// `recv_message`), so an operator can tell which direction collided.
        what: &'static str,
    },
    /// A previous send or receive on this stream failed, so the encrypted byte
    /// stream is at an unknown offset and cannot be resumed. Callers already close
    /// the connection on any transport error; this makes that obligation visible
    /// rather than leaving a desynced stream usable.
    ///
    /// Deliberately conservative: the rule is "a failed transfer makes the stream
    /// unusable", including a failure that happened before any byte moved (an
    /// already-elapsed deadline). No error in either direction proves where the
    /// byte stream stands, and a caller that kept going on a desynced stream would
    /// have its next frame silently misparsed by the peer. Callers already close on
    /// any error, so the only cost is a reconnect that was happening anyway.
    ///
    /// Raised only at a direction's entry point, before the socket is touched, so
    /// it never re-poisons a stream that is already refused. It is NOT a handshake
    /// refusal — a transfer that failed after the handshake proves nothing about
    /// the peer's enrollment or the pinned key (see `choreo-client-core`'s
    /// `is_handshake_refusal`).
    #[error("Noise stream unusable after a failed transfer")]
    StreamUnusable,
    /// The Noise handshake did not complete within its total budget. The
    /// budget bounds the WHOLE unauthenticated phase, not just a single read: a
    /// peer that dribbles bytes to keep resetting the per-read socket timeout is
    /// still cut off (see `handshake::frame::read_handshake_exact`). On the
    /// responder side one budget covers the mode-preamble read AND the
    /// handshake, so stalling before the preamble buys no extra time. Both
    /// modes (IK and XX) raise it.
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
    /// The XX responder never transmitted its static key.
    ///
    /// XX always sends the responder's static in message 2, so its absence is a
    /// protocol-contract violation, not an authorization refusal: the caller
    /// must not present it as enrollment remediation (unlike [`Self::AuthFailed`]).
    #[error("XX handshake: server did not transmit its static key")]
    HandshakeStaticMissing,
    /// The peer presented credentials that the ACL check rejected.
    #[error("Authentication failed")]
    AuthFailed,
    /// The peer closed the connection (EOF before a full frame, or a
    /// connection reset) while this stream was reading. Produced by
    /// `NoiseStream::recv_message` when the underlying `read_exact` fails with an
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
    /// A textual transport public key (standard-alphabet base64) could not be
    /// parsed: not base64, or not exactly 32 bytes decoded.
    ///
    /// The [`String`] carries the decoder's verdict — a byte count or a decoder
    /// position — so an operator can tell a typo from a wrong-length value. It
    /// never carries key bytes.
    #[error("invalid transport public key: {0}")]
    InvalidPublicKey(String),
}

impl TransportError {
    /// Fold an outgoing length-prefix failure from [`choreo_proto::len_prefix`]
    /// into this crate's taxonomy.
    ///
    /// Every encode variant is a local fault: only this build produces an
    /// outgoing prefix, so a length it cannot express is a bug here rather than
    /// anything the peer did. `what` is the caller's framing context and stays in
    /// front of the codec's own message, so an operator still sees which path and
    /// which numbers.
    pub(crate) fn len_prefix_encode(what: &str, error: LenPrefixError) -> Self {
        Self::InternalInvariant(format!("{what} {error}"))
    }

    /// Fold a length prefix the shared codec rejected into this crate's
    /// taxonomy.
    ///
    /// An `AboveCap` declaration is a wire violation, so it keeps that verdict
    /// and the caller's context prefix. The other variants cannot be a peer's
    /// doing — every call site hands the codec an exactly `N`-byte head read from
    /// its own fixed buffer, and a length the prefix width can carry is a `usize`
    /// on every target this workspace builds for — so they are reported as a
    /// broken internal buffer invariant instead.
    pub(crate) fn len_prefix_decode(what: &str, error: LenPrefixError) -> Self {
        match error {
            LenPrefixError::AboveCap { .. } => Self::InvalidFragment(format!("{what} {error}")),
            LenPrefixError::TooWide { .. }
            | LenPrefixError::ShortPrefix { .. }
            | LenPrefixError::NotRepresentable => crate::buf::invalid_slice(what),
        }
    }
}
