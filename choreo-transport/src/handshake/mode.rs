//! The Noise handshake mode: which pattern a connection runs, and everything
//! the wire and the transcript need to agree with that choice.
//!
//! [`HandshakeMode`] owns the whole selection: the 1-byte preamble an
//! initiator sends first ([`HandshakeMode::byte`] / [`HandshakeMode::from_byte`]),
//! the stock `snow` pattern string that implements it
//! ([`HandshakeMode::pattern`]), and the Noise prologue that binds the byte
//! into the handshake transcript ([`HandshakeMode::prologue`]). Keeping all
//! three on one value is what makes "announce one mode and speak another"
//! unrepresentable — there is no second place for the wire, the pattern and
//! the prologue to disagree.

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

/// Length of a handshake prologue: [`PROLOGUE_TAG`] plus the single mode byte
/// (see [`HandshakeMode::prologue`]).
pub const PROLOGUE_LEN: usize = PROLOGUE_TAG.len() + 1;

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
/// One value carries the wire byte AND the prologue that binds that byte into
/// the handshake transcript, so a caller cannot send one mode's byte and run the
/// other's handshake (see `super::handshake_responder_auto` for the receiving
/// side).
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

    /// The stock `snow` pattern string this mode runs.
    ///
    /// Its only consumer inside the crate is the shared state-machine builder,
    /// but it is public because it is the one authoritative spelling of the
    /// pattern a connection negotiates: a test harness that drives `snow`
    /// directly (to write frames the public API cannot express) must use THIS
    /// string rather than its own copy, or it silently validates a different
    /// pattern.
    #[must_use]
    pub const fn pattern(self) -> &'static str {
        match self {
            Self::Ik => "Noise_IK_25519_AESGCM_SHA256",
            Self::Xx => "Noise_XX_25519_AESGCM_SHA256",
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
    /// A fixed-size array, not a `Vec`: the prologue is a compile-time constant
    /// length, so a per-handshake heap allocation buys nothing. The caller keeps
    /// the array alive for as long as the `snow` builder it hands it to, which
    /// borrows it.
    #[must_use]
    pub fn prologue(self) -> [u8; PROLOGUE_LEN] {
        let mut prologue = [0u8; PROLOGUE_LEN];
        // `split_at_mut` never fails here: PROLOGUE_LEN is defined as the tag
        // length plus one, so the tag occupies `..PROLOGUE_TAG.len()` and the
        // mode byte is exactly the one remaining slot.
        let (tag, mode_byte) = prologue.split_at_mut(PROLOGUE_TAG.len());
        tag.copy_from_slice(PROLOGUE_TAG);
        if let Some(slot) = mode_byte.first_mut() {
            *slot = self.byte();
        }
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

/// Handshake-mode preamble byte: Noise IK (the authenticated mode — the client
/// knows the server's static key in advance).
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

#[cfg(test)]
mod tests {
    use snow::Builder;

    use super::{HandshakeMode, PREAMBLE_IK, PREAMBLE_XX, PROLOGUE_LEN, PROLOGUE_TAG};
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

    /// Each mode names the stock pattern it runs, and the two differ — the
    /// pattern string is what the shared builder hands to `snow`, so a copy of
    /// it here would be a copy that could drift.
    #[test]
    fn each_mode_names_its_own_pattern() {
        assert_eq!(HandshakeMode::Ik.pattern(), "Noise_IK_25519_AESGCM_SHA256");
        assert_eq!(HandshakeMode::Xx.pattern(), "Noise_XX_25519_AESGCM_SHA256");
        assert_ne!(HandshakeMode::Ik.pattern(), HandshakeMode::Xx.pattern());
    }

    /// The prologue helper lays down the tag and then the mode byte, so the two
    /// modes produce prologues of the same length that differ only in that last
    /// byte — the difference that has to reach the handshake hash.
    #[test]
    fn prologue_binds_the_tag_and_the_mode() {
        let ik = HandshakeMode::Ik.prologue();
        let xx = HandshakeMode::Xx.prologue();

        assert_eq!(ik.len(), PROLOGUE_LEN);
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
        let pattern = HandshakeMode::Xx.pattern();

        let initiator_sk = [7u8; 32];
        let responder_sk = [9u8; 32];

        let mut initiator = Builder::new(pattern.parse()?)
            .local_private_key(&initiator_sk)?
            .prologue(initiator_prologue)?
            .build_initiator()?;
        let mut responder = Builder::new(pattern.parse()?)
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
