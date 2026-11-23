//! The Noise IK and XX handshake state machines over a `TcpStream`.
//!
//! The state machines themselves are the stock `snow` patterns
//! `Noise_IK_25519_AESGCM_SHA256` ([`handshake_initiator`] /
//! [`handshake_responder`]) and `Noise_XX_25519_AESGCM_SHA256`
//! ([`handshake_initiator_xx`] / [`handshake_responder_xx`]). Nothing about
//! those patterns, their key schedule, or the cipher is modified. What this
//! module adds is application-level framing around the handshake: the 1-byte
//! mode preamble that names the pattern ([`HandshakeMode`] / [`PREAMBLE_IK`] /
//! [`PREAMBLE_XX`]), the 2-byte big-endian handshake length prefix (see the
//! `frame` submodule), and — in [`crate::noise`] — the 4-byte data-plane prefix
//! carrying an authenticated continuation byte.
//!
//! The mode byte is written and read by the handshake entry points themselves,
//! not by their callers: an initiator writes the byte for the flow it is about
//! to run, and a responder reads it before running anything. A peer therefore
//! cannot announce one mode and then speak another, because there is no second
//! place for the two to disagree.
//!
//! The byte authorizes nothing on its own: it only selects which pattern to
//! run. Both patterns authenticate both static keys, so a peer that claims one
//! mode and speaks another simply fails its handshake — there is no mode
//! confusion to exploit. The selection is additionally part of the handshake
//! transcript: every flow mixes [`PROLOGUE_TAG`] plus its own mode byte into the
//! Noise prologue, which snow hashes into the handshake hash before any message
//! is processed. The mode is therefore authenticated by the same exchange it
//! selects — an intermediary that rewrites the cleartext preamble byte on the
//! path produces a prologue mismatch and a failed handshake, and a
//! cross-pattern handshake fails structurally rather than incidentally.
//!
//! One budget covers a connection's whole unauthenticated phase. A responder
//! takes a single absolute deadline from [`handshake_responder_auto`] and spends
//! it on the preamble read and then on the handshake, so a peer cannot hold a
//! connection thread and socket FD for two budgets by stalling before the mode
//! byte and then again inside the handshake. Every read and write is bounded by
//! the time remaining until that deadline —
//! `HANDSHAKE_TIMEOUT` by default, or an explicit budget via the `_with_timeout`
//! variants.
//!
//! The flows live in the `ik` and `xx` submodules, one pair of functions each,
//! and both share the single framing path in `frame` so the length prefix and
//! the bounded I/O have exactly one implementation.

mod frame;
mod ik;
mod xx;

use std::net::TcpStream;
use std::time::{Duration, Instant};

use crate::error::TransportError;

pub use ik::{
    handshake_initiator, handshake_initiator_with_timeout, handshake_responder,
    handshake_responder_with_timeout,
};
pub use xx::{
    handshake_initiator_xx, handshake_initiator_xx_with_timeout, handshake_responder_xx,
    handshake_responder_xx_with_timeout,
};

// The bounds-checked slice helpers are shared with the data plane in
// `crate::noise`, which reaches them through this module's path.
pub(crate) use frame::{at, at_mut, slice, slice_mut};

/// Default total-duration budget for a Noise handshake. The
/// `handshake_*_with_timeout` variants take their own budget; the plain
/// functions delegate to them with this constant.
///
/// The budget is an ABSOLUTE deadline, not a per-read socket timeout: every
/// handshake read is bounded by the time remaining until it (see the `frame`
/// submodule's read helper), so a peer that dribbles bytes to keep resetting
/// a per-recv timeout is still cut off at the deadline. The responder side
/// spends ONE budget on the preamble read plus the handshake (see
/// [`handshake_responder_auto`]), so stalling before the mode byte buys no
/// extra time. It is cleared before the `TransportState` is handed over; the
/// data plane has no timeout by design — readers block until a message or EOF,
/// and the daemon's shutdown path closes sockets to unblock them.
///
/// The handshake runs BEFORE any authentication (the responder's ACL check
/// happens mid-handshake), so without a bound an unauthenticated peer could
/// hold a connection thread + socket FD open forever by connecting and
/// sending nothing — a resource-exhaustion vector on the daemon's TCP
/// listener. 10 s is far beyond the single round trip a healthy handshake
/// needs (sub-millisecond on loopback, a few ms on a real network).
pub(crate) const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Domain-separation tag mixed into every handshake prologue (see
/// [`HandshakeMode::prologue`]).
///
/// The prologue is hashed into the Noise handshake hash before any handshake
/// message is processed, so its content is covered by exactly the same
/// authentication as the key exchange. This tag pins the transcript to THIS
/// protocol: another use of the same stock pattern and the same static keys
/// derives a different handshake hash, so the two exchanges can never be
/// confused for one another. The `-1` suffix leaves room for a future
/// incompatible prologue without colliding with this one.
pub const PROLOGUE_TAG: &[u8] = b"choreographr-noise-1";

/// Which Noise handshake a TCP connection runs, and therefore which 1-byte
/// preamble its initiator sends first.
///
/// The two modes differ only in what the initiator knows up front: IK's
/// initiator already has the responder's static key (two messages), XX's meets
/// it for the first time (three messages, the responder's static arriving in
/// message 2 for out-of-band confirmation). Both authenticate both static keys;
/// neither is a fallback for the other, so a connection runs exactly the mode
/// its initiator chose.
///
/// This type exists so the choice cannot drift from the flow that implements it:
/// one value carries the wire byte AND the prologue that binds that byte into
/// the handshake transcript, so a caller cannot send one mode's byte and run the
/// other's handshake (see [`handshake_responder_auto`] for the receiving side).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HandshakeMode {
    /// Noise IK — the authenticated mode; the initiator already knows the
    /// responder's static key.
    Ik,
    /// Noise XX — first contact; the initiator learns the responder's static key
    /// from the handshake and verifies it out of band.
    Xx,
}

impl HandshakeMode {
    /// The 1-byte preamble this mode puts on the wire before any handshake
    /// message ([`PREAMBLE_IK`] / [`PREAMBLE_XX`]).
    #[must_use]
    pub const fn byte(self) -> u8 {
        match self {
            Self::Ik => PREAMBLE_IK,
            Self::Xx => PREAMBLE_XX,
        }
    }

    /// Decode a preamble byte, or `None` when it names no mode this build
    /// defines (including the reserved `0x00`).
    #[must_use]
    pub const fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            PREAMBLE_IK => Some(Self::Ik),
            PREAMBLE_XX => Some(Self::Xx),
            _ => None,
        }
    }

    /// Build the Noise prologue for this mode: [`PROLOGUE_TAG`] followed by the
    /// mode byte.
    ///
    /// Both ends of one handshake pass the SAME mode — the IK initiator and the
    /// IK responder both use [`HandshakeMode::Ik`], the XX pair both
    /// [`HandshakeMode::Xx`] — which is what makes the binding meaningful: the
    /// prologue mismatch that any other choice would produce is a handshake
    /// failure, so the mode a connection uses is the mode both peers committed
    /// to inside the transcript. There is no negotiation and no version number,
    /// so there is nothing for an intermediary to steer the selection with.
    ///
    /// Returns the tag and the byte concatenated; the caller keeps the buffer
    /// alive for as long as the `snow` builder it hands it to, which borrows it.
    #[must_use]
    pub fn prologue(self) -> Vec<u8> {
        // One allocation of exactly the final size: the tag plus the single mode
        // byte. The buffer is tiny (21 bytes) and lives only for one builder.
        let mut prologue = Vec::with_capacity(PROLOGUE_TAG.len() + 1);
        prologue.extend_from_slice(PROLOGUE_TAG);
        prologue.push(self.byte());
        prologue
    }
}

impl std::fmt::Display for HandshakeMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Ik => "IK",
            Self::Xx => "XX",
        })
    }
}

/// Handshake-mode preamble byte: Noise IK (the existing authenticated
/// mode — the client knows the server's static key in advance).
///
/// Every TCP connection starts with exactly one cleartext mode byte before any
/// handshake message. The byte is not secret and carries no authentication of
/// its own — it only selects which pattern to run — but it is not merely
/// advisory either: both ends mix it into the Noise prologue
/// ([`HandshakeMode::prologue`]), so the value a peer actually sent is covered
/// by the handshake hash and cannot be altered on the path. `0x00` is
/// deliberately reserved (never assigned) so an all-zero garbage fill or a
/// trailing-NUL bug can never be mistaken for a valid mode.
pub const PREAMBLE_IK: u8 = 0x01;

/// Handshake-mode preamble byte: Noise XX (first-contact mode — the client does
/// NOT know the server's static key in advance and learns it from the
/// handshake; the caller then verifies the key out-of-band before any
/// protocol traffic flows).
///
/// Same wire position and same prologue binding as [`PREAMBLE_IK`].
pub const PREAMBLE_XX: u8 = 0x02;

/// Read the 1-byte handshake-mode preamble from `stream`, using the default
/// `HANDSHAKE_TIMEOUT` budget (see [`read_handshake_preamble_with_timeout`]).
///
/// The responder entry points already do this as their first step
/// ([`handshake_responder`] / [`handshake_responder_xx`] /
/// [`handshake_responder_auto`]); this is the standalone form for a caller that
/// wants the mode without running a handshake.
///
/// # Errors
///
/// Returns [`TransportError::HandshakeModeUnknown`] if the byte names no mode
/// this build defines, [`TransportError::HandshakeTimeout`] if the peer does not
/// send it within the `HANDSHAKE_TIMEOUT` budget, or any propagated socket I/O
/// error (EOF before the byte reads as `UnexpectedEof`).
pub fn read_handshake_preamble(stream: &mut TcpStream) -> Result<HandshakeMode, TransportError> {
    read_handshake_preamble_with_timeout(stream, HANDSHAKE_TIMEOUT)
}

/// [`read_handshake_preamble`] with an explicit total-duration budget.
///
/// # Errors
///
/// As [`read_handshake_preamble`], with the budget given by `timeout`.
pub fn read_handshake_preamble_with_timeout(
    stream: &mut TcpStream,
    timeout: Duration,
) -> Result<HandshakeMode, TransportError> {
    read_preamble(stream, Instant::now() + timeout)
}

/// Decode the preamble byte on `stream`, bounded by `deadline`.
///
/// The single read behind every responder entry point and the public readers, so
/// the byte's position, its bound and its validation cannot differ between them.
pub(super) fn read_preamble(
    stream: &mut TcpStream,
    deadline: Instant,
) -> Result<HandshakeMode, TransportError> {
    let byte = frame::read_handshake_exact(stream, deadline, 1)?;
    // The bounded read above returned exactly 1 byte, so the index is in bounds
    // by construction; the access is bounds-checked anyway (see the slice
    // helpers' comment) rather than panicking.
    let byte = *at(&byte, 0, "preamble byte")?;
    HandshakeMode::from_byte(byte).ok_or(TransportError::HandshakeModeUnknown { byte })
}

/// Reject a peer that asked for a mode other than `expected`.
pub(super) fn require_mode(
    got: HandshakeMode,
    expected: HandshakeMode,
) -> Result<(), TransportError> {
    if got == expected {
        Ok(())
    } else {
        Err(TransportError::HandshakeModeMismatch {
            expected: expected.byte(),
            got: got.byte(),
        })
    }
}

/// Perform the Noise handshake the peer asked for as the **responder** (server
/// side), using the default `HANDSHAKE_TIMEOUT` budget.
///
/// Reads the 1-byte mode preamble, then runs the matching IK or XX responder —
/// one deadline covering both, so a peer that stalls before the mode byte gets no
/// extra time before the handshake's own budget starts. Returns the transport and
/// the mode that was run, so a caller can log or account for the choice.
///
/// This is the server-side entry point: a responder does not know in advance
/// which mode a client will request, and this dispatches on the byte rather than
/// making every caller re-implement the read-then-match.
///
/// * `stream` — the accepted TCP stream from the listener.
/// * `static_sk` — the server's transport secret (32-byte X25519 secret key).
/// * `check_client` — receives the client's 32-byte static public key and
///   returns `true` if the client is authorized.
///
/// # Errors
///
/// [`TransportError::HandshakeModeUnknown`] if the preamble names no mode,
/// [`TransportError::AuthFailed`] if the client is not authorized or never sent
/// its static key, [`TransportError::HandshakeTimeout`] if the preamble plus
/// handshake exceed the `HANDSHAKE_TIMEOUT` budget, or the snow/`TransportError`
/// I/O and protocol errors raised by the exchange itself.
pub fn handshake_responder_auto<F>(
    stream: TcpStream,
    static_sk: &[u8; 32],
    check_client: F,
) -> Result<(crate::noise::NoiseStream, HandshakeMode), TransportError>
where
    F: FnOnce(&[u8; 32]) -> bool,
{
    handshake_responder_auto_with_timeout(stream, static_sk, check_client, HANDSHAKE_TIMEOUT)
}

/// [`handshake_responder_auto`] with an explicit total-duration budget for the
/// whole unauthenticated phase (preamble read plus handshake). Exposed so
/// integration tests can exercise the timeout path in milliseconds instead of
/// waiting out the 10 s default.
///
/// # Errors
///
/// As [`handshake_responder_auto`], with the budget given by `timeout`.
pub fn handshake_responder_auto_with_timeout<F>(
    mut stream: TcpStream,
    static_sk: &[u8; 32],
    check_client: F,
    timeout: Duration,
) -> Result<(crate::noise::NoiseStream, HandshakeMode), TransportError>
where
    F: FnOnce(&[u8; 32]) -> bool,
{
    let deadline = Instant::now() + timeout;
    let mode = read_preamble(&mut stream, deadline)?;
    // Dispatch to the flow the peer asked for, handing it the SAME deadline so
    // the preamble read is not free extra time. Both arms take the stream by
    // value and the identical ACL closure, so XX is authorized exactly like IK.
    let noise = match mode {
        HandshakeMode::Ik => ik::responder(stream, static_sk, check_client, deadline)?,
        HandshakeMode::Xx => xx::responder(stream, static_sk, check_client, deadline)?,
    };
    Ok((noise, mode))
}

#[cfg(test)]
mod tests {
    use snow::Builder;

    use super::{HandshakeMode, PREAMBLE_IK, PREAMBLE_XX, PROLOGUE_TAG};
    use crate::error::TransportError;

    /// The mode owns its wire byte in both directions: the two modes encode to
    /// distinct bytes, decode back to themselves, and every other byte —
    /// including the reserved `0x00` — names no mode.
    #[test]
    fn mode_round_trips_through_its_wire_byte() {
        assert_eq!(HandshakeMode::Ik.byte(), PREAMBLE_IK);
        assert_eq!(HandshakeMode::Xx.byte(), PREAMBLE_XX);
        assert_ne!(PREAMBLE_IK, PREAMBLE_XX, "the two modes are distinct");

        assert_eq!(
            HandshakeMode::from_byte(PREAMBLE_IK),
            Some(HandshakeMode::Ik)
        );
        assert_eq!(
            HandshakeMode::from_byte(PREAMBLE_XX),
            Some(HandshakeMode::Xx)
        );
        for unknown in [0x00u8, 0x03, 0xFF] {
            assert_eq!(
                HandshakeMode::from_byte(unknown),
                None,
                "{unknown:#04x} must not name a mode"
            );
        }
        // The display form is what the error messages and the daemon's log use.
        assert_eq!(HandshakeMode::Ik.to_string(), "IK");
        assert_eq!(HandshakeMode::Xx.to_string(), "XX");
    }

    /// The prologue helper lays down the tag and then the mode byte, so the two
    /// modes produce prologues of the same length that differ only in that last
    /// byte — the difference that has to reach the handshake hash.
    #[test]
    fn prologue_binds_the_tag_and_the_mode() {
        let ik = HandshakeMode::Ik.prologue();
        let xx = HandshakeMode::Xx.prologue();

        assert_eq!(ik.len(), PROLOGUE_TAG.len() + 1);
        assert_eq!(ik.len(), 21, "20-byte tag plus one mode byte");
        assert!(ik.starts_with(PROLOGUE_TAG));
        assert_eq!(ik.last().copied(), Some(PREAMBLE_IK));
        assert_eq!(xx.last().copied(), Some(PREAMBLE_XX));
        assert_ne!(ik, xx, "the mode byte must change the prologue");
    }

    /// Run one complete XX exchange in memory and report whether it succeeds.
    /// Pure crypto: no sockets, no timers, no sleeps. `snow` fails the
    /// exchange when the two prologues differ, because the prologue is mixed
    /// into the handshake hash that every AEAD tag in the exchange covers.
    fn run_xx_exchange(
        initiator_prologue: &[u8],
        responder_prologue: &[u8],
    ) -> Result<(), TransportError> {
        const PATTERN: &str = "Noise_XX_25519_AESGCM_SHA256";

        let initiator_sk = [7u8; 32];
        let responder_sk = [9u8; 32];

        let mut initiator = Builder::new(PATTERN.parse()?)
            .local_private_key(&initiator_sk)?
            .prologue(initiator_prologue)?
            .build_initiator()?;
        let mut responder = Builder::new(PATTERN.parse()?)
            .local_private_key(&responder_sk)?
            .prologue(responder_prologue)?
            .build_responder()?;

        // The handshake messages for an empty payload are well under a
        // kilobyte; snow caps every write at 65535, so these buffers are the
        // same bound the production frame path uses.
        let mut frame = vec![0u8; 65_535];
        let mut scratch = vec![0u8; 65_535];

        // Message 1 (-> e): unencrypted, so it is accepted either way.
        let n = initiator.write_message(&[], &mut frame)?;
        responder.read_message(&frame[..n], &mut scratch)?;

        // Message 2 (<- e, ee, s, es): the first encrypted message. Its tags
        // are keyed from the handshake hash, which includes the prologue — a
        // mismatched prologue fails right here.
        let n = responder.write_message(&[], &mut frame)?;
        initiator.read_message(&frame[..n], &mut scratch)?;

        // Message 3 (-> s, es): completes the exchange, proving the two sides
        // derived the same transport keys.
        let n = initiator.write_message(&[], &mut frame)?;
        responder.read_message(&frame[..n], &mut scratch)?;

        Ok(())
    }

    /// Same mode on both ends: the exchange completes, so the prologue the
    /// production flows use for one mode is agreed on by both of its halves.
    #[test]
    fn matching_prologues_complete_the_handshake() {
        let prologue = HandshakeMode::Xx.prologue();
        assert!(
            run_xx_exchange(&prologue, &prologue).is_ok(),
            "both sides using the same prologue must complete the handshake"
        );
    }

    /// Different modes on the two ends: the exchange fails, which is the whole
    /// point of binding the mode — an intermediary that rewrites the preamble
    /// byte cannot make the peers run different modes undetected.
    #[test]
    fn mismatched_prologues_fail_the_handshake() {
        assert!(
            run_xx_exchange(&HandshakeMode::Ik.prologue(), &HandshakeMode::Xx.prologue()).is_err(),
            "a prologue mismatch must fail the handshake"
        );
    }
}
