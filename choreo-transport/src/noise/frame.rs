//! The data plane's wire layout: the fragment sizes, the 4-byte length-prefix
//! codec and the reassembly bound.
//!
//! Every size here derives from one hard external limit — snow's 65535-byte
//! per-message ciphertext cap (`MAXMSGLEN`) — so the arithmetic that splits a
//! payload into fragments and the arithmetic that validates a peer's declared
//! length end at the same [`MAX_CIPHERTEXT_LEN`] and cannot drift apart.
//!
//! [`Reassembly`] is here rather than in `recv` because it is the other half of
//! that same accounting: the per-frame sizes above bound one fragment, and the
//! reassembly cap bounds the message those fragments add up to.

use zeroize::Zeroizing;

use crate::error::TransportError;

/// AES-256-GCM authentication tag length, appended to every encrypted
/// message (snow's `MAXMSGLEN` counts the tag).
pub(super) const GCM_TAG_LEN: usize = 16;

/// Length of the authenticated per-fragment continuation header: one byte at
/// the START of each fragment's plaintext, so the AES-GCM tag covers it (see
/// [`FRAGMENT_CONTINUATION`]).
pub(super) const FRAGMENT_HEADER_LEN: usize = 1;

/// Size of the big-endian ciphertext length prefix that precedes every
/// data-plane frame.
pub(super) const LEN_PREFIX_LEN: usize = 4;

/// Largest ciphertext a single frame may carry: snow's per-message cap, which
/// is both what the sender can produce and what a peer may declare.
pub(super) const MAX_CIPHERTEXT_LEN: usize = 65535;

/// Maximum payload bytes in a single Noise fragment. snow caps one message's
/// ciphertext at 65535 bytes (MAXMSGLEN); the 16-byte GCM tag and the 1-byte
/// authenticated continuation header leave 65518 bytes of payload per
/// fragment.
pub(super) const MAX_PLAINTEXT_CHUNK: usize =
    MAX_CIPHERTEXT_LEN - GCM_TAG_LEN - FRAGMENT_HEADER_LEN;

/// Bit 0 of a fragment's first plaintext byte: 1 = more fragments of the same
/// logical message follow, 0 = this fragment terminates it. Because the byte
/// lives INSIDE the plaintext, the AES-GCM tag authenticates it — the wire
/// length prefix that precedes the ciphertext is NOT authenticated, so the
/// reassembly decision must not be trusted from there.
pub(super) const FRAGMENT_CONTINUATION: u8 = 0x01;

/// Size of the reused outbound ciphertext frame buffer: [`LEN_PREFIX_LEN`]
/// bytes of length-prefix headroom + the largest single Noise message's
/// ciphertext ([`MAX_CIPHERTEXT_LEN`]). The ciphertext is encrypted into
/// `send_buf[LEN_PREFIX_LEN..]`, the prefix is written into that headroom, and
/// one `write_all` sends the whole frame.
pub(super) const FRAME_BUF_LEN: usize = LEN_PREFIX_LEN + MAX_CIPHERTEXT_LEN;

/// Size of the reused per-fragment plaintext buffer: one authenticated
/// continuation header byte + the largest payload chunk.
pub(super) const FRAG_BUF_LEN: usize = FRAGMENT_HEADER_LEN + MAX_PLAINTEXT_CHUNK;

/// Encode a ciphertext length as its 4-byte big-endian prefix, refusing a
/// length this implementation cannot put on the wire.
///
/// The bound is the SAME cap [`decode_len`] enforces on a peer, stated from one
/// constant, so the sender can never produce a frame the receiver would refuse.
/// The encode/decode arithmetic itself lives in the shared [`crate::len_prefix`]
/// codec, so the data plane's 4-byte prefix and the handshake's 2-byte prefix
/// are one implementation parameterised by width and cap.
pub(super) fn encode_len(n: usize) -> Result<[u8; LEN_PREFIX_LEN], TransportError> {
    crate::len_prefix::encode(n, MAX_CIPHERTEXT_LEN, "encrypted frame length")
}

/// Decode a peer's length prefix and validate it against snow's per-message
/// cap.
///
/// This is the single place a peer-declared data-plane frame length is trusted,
/// and it runs before anything is allocated for the body: the prefix is NOT
/// authenticated (it precedes the GCM ciphertext), so a hostile or corrupted
/// peer could otherwise declare 4 GiB and make the reader allocate — and then
/// block in `read_exact` — that much. Any smaller tamper changes `ct_len` and
/// is caught by the GCM authentication instead: a wrong byte count never
/// decrypts, so a wire flip can never silently truncate or extend a message.
pub(super) fn decode_len(prefix: &[u8]) -> Result<usize, TransportError> {
    crate::len_prefix::decode::<LEN_PREFIX_LEN>(
        prefix,
        MAX_CIPHERTEXT_LEN,
        "fragment ciphertext length",
    )
}

/// One logical message's reassembly: the plaintext accumulated so far, and the
/// cap that bounds it.
///
/// The per-frame cap in [`decode_len`] bounds one fragment; it does not bound a
/// message, because a peer can send an endless stream of continuation-marked
/// fragments that are each individually legal. This type owns the bound on the
/// running total, and it is the only way the receive loop touches that total:
/// a frame's declared length must be [`accept_frame`](Self::accept_frame)d to
/// obtain an [`AcceptedFrame`], and [`extend`](Self::extend) demands one — so
/// "append a fragment whose length was never checked" is not expressible, and
/// the check cannot be dropped from the loop without the loop failing to
/// compile.
///
/// The bound is enforced on the frame's **unauthenticated** length prefix,
/// BEFORE that frame's body is read or the buffer grows, so a message that never
/// terminates costs at most `MAX_FRAME_SIZE` of plaintext rather than an
/// unbounded amount.
pub(super) struct Reassembly {
    plaintext: Zeroizing<Vec<u8>>,
    cap: usize,
}

/// Proof that one frame's declared ciphertext length was checked against the
/// running total. Only [`Reassembly::accept_frame`] mints one.
#[derive(Debug)]
pub(super) struct AcceptedFrame(());

impl Reassembly {
    /// A fresh reassembly bounded by the codec's `MAX_FRAME_SIZE`.
    pub(super) fn new() -> Self {
        Self::with_cap(choreo_proto::MAX_FRAME_SIZE)
    }

    /// A fresh reassembly bounded by an arbitrary cap. The tests drive a small
    /// one, so the cap's exact boundary is exercised without pushing 64 MiB
    /// through a socket.
    fn with_cap(cap: usize) -> Self {
        Self {
            plaintext: Zeroizing::new(Vec::new()),
            cap,
        }
    }

    /// Validate a frame's declared ciphertext length against the cap and the
    /// plaintext accumulated so far, before anything is read or appended.
    ///
    /// A fragment contributes at most `ct_len - (tag + continuation header)`
    /// plaintext bytes; the subtraction saturates, so a frame too short to carry
    /// any payload counts as no contribution instead of wrapping into a huge
    /// one.
    ///
    /// # Errors
    ///
    /// [`TransportError::InvalidFragment`] when accepting this frame would push
    /// the message past the cap.
    pub(super) fn accept_frame(&self, ct_len: usize) -> Result<AcceptedFrame, TransportError> {
        let contribution = ct_len.saturating_sub(GCM_TAG_LEN + FRAGMENT_HEADER_LEN);
        if self.plaintext.len().saturating_add(contribution) > self.cap {
            return Err(TransportError::InvalidFragment(format!(
                "reassembled message exceeds the {}-byte limit",
                self.cap
            )));
        }
        Ok(AcceptedFrame(()))
    }

    /// Append an accepted fragment's decrypted payload.
    pub(super) fn extend(&mut self, _frame: AcceptedFrame, payload: &[u8]) {
        self.plaintext.extend_from_slice(payload);
    }

    /// How many plaintext bytes have been accumulated so far.
    #[cfg(test)]
    fn len(&self) -> usize {
        self.plaintext.len()
    }

    /// Finish the message, handing the caller the plaintext buffer un-wiped:
    /// the caller decides whether to clear it (see `recv_message_plain`).
    pub(super) fn into_plaintext(mut self) -> Vec<u8> {
        std::mem::take(&mut *self.plaintext)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The data plane wires the shared codec with a 4-byte prefix and snow's
    /// ciphertext cap: a length at the cap round-trips, the high byte comes
    /// first, and one past the cap is refused on both sides by naming the cap.
    /// The codec arithmetic itself is pinned in `crate::len_prefix`; this keeps
    /// only the wiring — width and cap — the data plane chooses.
    #[test]
    fn length_prefix_uses_the_u32_width_and_the_frame_cap() {
        for len in [0usize, 1, MAX_PLAINTEXT_CHUNK, MAX_CIPHERTEXT_LEN] {
            let encoded = encode_len(len).expect("length encodes");
            assert_eq!(
                decode_len(&encoded).expect("prefix decodes"),
                len,
                "length prefix must round-trip"
            );
        }
        // Big-endian in a 4-byte field: a small length fills the prefix's tail.
        assert_eq!(encode_len(0x01_02).expect("encodes"), [0, 0, 0x01, 0x02]);

        // One past the cap is refused on both sides, naming the cap; the whole
        // 32-bit range a hostile peer can declare is refused with it, which is
        // what keeps an unauthenticated prefix from costing memory.
        assert!(encode_len(MAX_CIPHERTEXT_LEN + 1).is_err());
        for over in [
            u32::try_from(MAX_CIPHERTEXT_LEN).expect("fits u32") + 1,
            u32::MAX,
        ] {
            let err = decode_len(&over.to_be_bytes()).expect_err("over the cap");
            assert!(
                err.to_string().contains(&MAX_CIPHERTEXT_LEN.to_string()),
                "the refusal must name the cap: {err}"
            );
        }
    }

    /// The buffer sizes both directions allocate follow from the frame cap: a
    /// full fragment's plaintext plus its tag is exactly one ciphertext, and
    /// one outbound frame is that ciphertext plus the prefix headroom.
    #[test]
    fn buffer_sizes_derive_from_the_frame_cap() {
        assert_eq!(FRAG_BUF_LEN + GCM_TAG_LEN, MAX_CIPHERTEXT_LEN);
        assert_eq!(FRAME_BUF_LEN, LEN_PREFIX_LEN + FRAG_BUF_LEN + GCM_TAG_LEN);
    }

    /// The cumulative reassembly cap is exact at its boundary: the fragment
    /// that lands on the cap is accepted and one payload byte more is refused,
    /// naming the cap. The bound is arithmetic, so a 100-byte cap pins it as
    /// exactly as the real 64 MiB one would — without allocating 64 MiB in a
    /// debug build. That the production bound IS the codec's message cap is a
    /// separate one-line assertion.
    #[test]
    fn reassembly_cap_is_exact_at_the_boundary() {
        let cap = 100usize;
        // A frame whose ciphertext carries `payload` plaintext bytes.
        let frame_len = |payload: usize| GCM_TAG_LEN + FRAGMENT_HEADER_LEN + payload;
        let assembled = |len: usize| Reassembly {
            plaintext: Zeroizing::new(vec![0u8; len]),
            cap,
        };

        // Filling the cap exactly is allowed...
        assert!(assembled(0).accept_frame(frame_len(cap)).is_ok());
        // ...and one byte of actual plaintext more is not, and the refusal
        // names the cap the message would have blown.
        let refused = assembled(1).accept_frame(frame_len(cap));
        let Err(err) = refused else {
            panic!("one byte past the cap must be refused");
        };
        assert!(err.to_string().contains(&cap.to_string()), "{err}");
        // An already-full buffer has no room for any fragment that carries a
        // payload byte.
        assert!(assembled(cap).accept_frame(frame_len(1)).is_err());

        // And the bound a real stream is held to is the codec's message cap.
        assert_eq!(Reassembly::new().cap, choreo_proto::MAX_FRAME_SIZE);
    }

    /// A fragment that carries no plaintext — exactly a GCM tag plus the
    /// continuation header, or fewer bytes than that — never trips the cap, so
    /// an empty-fragment stream cannot be refused for the wrong reason (it is
    /// refused by the continuation-header check instead, and costs one wire
    /// frame each).
    #[test]
    fn zero_payload_fragments_never_trip_the_cap() {
        let cap = 8usize;
        let mut assembly = Reassembly::with_cap(cap);
        // Fill the cap, so there is no room left for a payload byte at all.
        assembly.extend(
            assembly.accept_frame(cap).expect("fills the cap"),
            &[0xAB; 8],
        );
        assert_eq!(assembly.len(), cap);
        for ct_len in [0, 1, GCM_TAG_LEN, GCM_TAG_LEN + FRAGMENT_HEADER_LEN] {
            assert!(
                assembly.accept_frame(ct_len).is_ok(),
                "a {ct_len}-byte frame contributes no plaintext"
            );
        }
    }

    /// Driving the type the way the receive loop does: each frame must be
    /// accepted before its payload is appended, the running total is what the
    /// next check sees, and a refused frame leaves the total untouched. This is
    /// the sequence a peer streaming continuation fragments produces, and the
    /// reason the cap bounds memory rather than just one frame.
    #[test]
    fn a_fragment_sequence_is_bounded_by_the_running_total() {
        let cap = 100usize;
        // A frame whose ciphertext carries `payload` plaintext bytes.
        let frame_len = |payload: usize| GCM_TAG_LEN + FRAGMENT_HEADER_LEN + payload;

        let mut assembly = Reassembly::with_cap(cap);
        for payload in [10usize, 40, 50] {
            let accepted = assembly
                .accept_frame(frame_len(payload))
                .expect("a fragment that lands inside the cap is accepted");
            assembly.extend(accepted, &[0xAB; 50][..payload]);
        }
        assert_eq!(assembly.len(), cap, "the three fragments fill the cap");

        // One more payload byte would cross it, and the total is unchanged
        // because the frame is refused BEFORE anything is appended.
        assert!(assembly.accept_frame(frame_len(1)).is_err());
        assert_eq!(assembly.len(), cap, "a refused frame appends nothing");

        assert_eq!(
            assembly.into_plaintext().len(),
            cap,
            "the finished message carries what was appended"
        );
    }
}
