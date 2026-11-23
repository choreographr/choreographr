//! The data plane's wire layout: the fragment sizes and the 4-byte
//! length-prefix codec.
//!
//! Every size here derives from one hard external limit — snow's 65535-byte
//! per-message ciphertext cap (`MAXMSGLEN`) — so the arithmetic that splits a
//! payload into fragments and the arithmetic that validates a peer's declared
//! length end at the same [`MAX_CIPHERTEXT_LEN`] and cannot drift apart.

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

/// Would appending a fragment of `ct_len` ciphertext bytes push a reassembly
/// that has already accumulated `assembled` plaintext bytes past the codec's
/// `MAX_FRAME_SIZE`?
///
/// The per-frame cap in [`decode_len`] bounds one fragment; it does not bound a
/// message, because a peer can send an endless stream of continuation-marked
/// fragments that are each individually legal. This is the bound on the running
/// total, and the receive loop checks it against each frame's length prefix
/// **before** that frame's body is read or the buffer grows — so a message that
/// never terminates costs at most `MAX_FRAME_SIZE` of plaintext, not an
/// unbounded amount.
///
/// A fragment contributes at most `ct_len - (tag + continuation header)`
/// plaintext bytes, so the two saturate rather than masking a short frame into
/// a huge contribution.
pub(super) fn reassembly_exceeds_cap(assembled: usize, ct_len: usize) -> bool {
    assembled.saturating_add(ct_len.saturating_sub(GCM_TAG_LEN + FRAGMENT_HEADER_LEN))
        > choreo_proto::MAX_FRAME_SIZE
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

    /// The cumulative reassembly cap is exact at its boundary: the last frame
    /// that still lands on `MAX_FRAME_SIZE` is accepted and the next one is
    /// refused. A full fragment contributes 65518 plaintext bytes, which is
    /// what these two cases differ by.
    ///
    /// This is the whole bound a peer can reach by streaming
    /// continuation-marked fragments, and it is arithmetic — no cipher, no
    /// socket, no allocation — so it is pinned here rather than by pushing
    /// `MAX_FRAME_SIZE` bytes of ciphertext through a loopback socket, which
    /// would cost seconds in an unoptimized build to prove the same fact.
    #[test]
    fn reassembly_cap_is_exact_at_the_boundary() {
        let contribution = MAX_CIPHERTEXT_LEN - (GCM_TAG_LEN + FRAGMENT_HEADER_LEN);
        let cap = choreo_proto::MAX_FRAME_SIZE;

        // Filling the cap exactly is allowed...
        assert!(!reassembly_exceeds_cap(
            cap - contribution,
            MAX_CIPHERTEXT_LEN
        ));
        // ...and one byte of actual plaintext more is not.
        assert!(reassembly_exceeds_cap(
            cap - contribution + 1,
            MAX_CIPHERTEXT_LEN
        ));
        // An already-full buffer has no room for any fragment that carries a
        // payload byte.
        assert!(reassembly_exceeds_cap(cap, MAX_CIPHERTEXT_LEN));
    }

    /// A fragment that carries no plaintext — exactly a GCM tag plus the
    /// continuation header, or fewer bytes than that — never trips the cap, so
    /// an empty-fragment stream cannot be refused for the wrong reason (it is
    /// refused by the continuation-header check instead, and costs one wire
    /// frame each).
    #[test]
    fn zero_payload_fragments_never_trip_the_cap() {
        let cap = choreo_proto::MAX_FRAME_SIZE;
        for ct_len in [0, 1, GCM_TAG_LEN, GCM_TAG_LEN + FRAGMENT_HEADER_LEN] {
            assert!(
                !reassembly_exceeds_cap(cap, ct_len),
                "a {ct_len}-byte frame contributes no plaintext"
            );
        }
        // And the accounting saturates instead of wrapping: an accumulated
        // total already past the cap is still over it, never back under.
        assert!(reassembly_exceeds_cap(usize::MAX, MAX_CIPHERTEXT_LEN));
    }
}
