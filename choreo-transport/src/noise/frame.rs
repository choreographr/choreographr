//! The data plane's wire layout: the fragment sizes and the length-prefix codec.
//!
//! Every size here derives from one hard external limit — snow's 65535-byte
//! per-message ciphertext cap (`MAXMSGLEN`) — so the arithmetic that splits a
//! payload into fragments and the arithmetic that validates a peer's declared
//! length end at the same [`MAX_CIPHERTEXT_LEN`] and cannot drift apart.
//!
//! The message-level bounds that the fragments add up to — the running byte
//! total and the fragment count — live in the sibling [`super::reassembly`]
//! module, because they are the other half of this same accounting: the
//! per-frame sizes here bound one fragment, and `Reassembly` bounds the message
//! those fragments make up.

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

/// Most fragments a legitimate sender produces for one message.
///
/// A message is capped at [`choreo_proto::MAX_FRAME_SIZE`] and each fragment
/// carries at most [`MAX_PLAINTEXT_CHUNK`] plaintext bytes, so a legitimate
/// sender emits `MAX_FRAME_SIZE / MAX_PLAINTEXT_CHUNK` full fragments (1024
/// today) plus at most one shorter remainder — the `+ 1` — and an empty payload
/// emits its single frame within that same allowance. The byte total alone
/// cannot bound a peer that sends an endless run of continuation-marked frames
/// carrying NO payload: each contributes zero bytes, so the byte cap never
/// trips while the receive loop runs forever at roughly one wire frame per
/// iteration. This count is what bounds that stream.
pub(super) const MAX_FRAGMENTS_PER_MESSAGE: usize =
    choreo_proto::MAX_FRAME_SIZE / MAX_PLAINTEXT_CHUNK + 1;

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
/// cap, and against the minimum a fragment must carry.
///
/// This is the single place a peer-declared data-plane frame length is trusted,
/// and it runs before anything is allocated for the body: the prefix is NOT
/// authenticated (it precedes the GCM ciphertext), so a hostile or corrupted
/// peer could otherwise declare 4 GiB and make the reader allocate — and then
/// block in `read_exact` — that much. Any smaller tamper changes `ct_len` and
/// is caught by the GCM authentication instead: a wrong byte count never
/// decrypts, so a wire flip can never silently truncate or extend a message.
///
/// A frame too short to carry even the GCM tag plus the authenticated
/// continuation header can never come from a legitimate sender, so it is
/// refused here rather than handed to snow: it is cheaper, and it makes the
/// minimum an explicit bound instead of relying on `read_message` to fail on an
/// undersized buffer.
pub(super) fn decode_len(prefix: &[u8]) -> Result<usize, TransportError> {
    let declared = crate::len_prefix::decode::<LEN_PREFIX_LEN>(
        prefix,
        MAX_CIPHERTEXT_LEN,
        "fragment ciphertext length",
    )?;
    let minimum = GCM_TAG_LEN + FRAGMENT_HEADER_LEN;
    if declared < minimum {
        return Err(TransportError::InvalidFragment(format!(
            "fragment ciphertext length {declared} is below the {minimum}-byte minimum (tag + continuation header)"
        )));
    }
    Ok(declared)
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
        // A frame must carry at least the tag plus the continuation header, so
        // the smallest legal ciphertext is that minimum, not zero.
        for len in [
            GCM_TAG_LEN + FRAGMENT_HEADER_LEN,
            1 + GCM_TAG_LEN + FRAGMENT_HEADER_LEN,
            MAX_PLAINTEXT_CHUNK,
            MAX_CIPHERTEXT_LEN,
        ] {
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

    /// A declared length below the tag-plus-header minimum is refused before
    /// snow sees it: such a frame cannot carry a legitimate continuation
    /// decision, so a well-behaved receiver rejects it up front rather than
    /// decrypting an undersized ciphertext.
    #[test]
    fn a_frame_below_the_tag_and_header_minimum_is_refused() {
        let minimum = GCM_TAG_LEN + FRAGMENT_HEADER_LEN;
        for short in [0usize, 1, GCM_TAG_LEN, minimum - 1] {
            let encoded = encode_len(short).expect("a short length still encodes");
            let err = decode_len(&encoded).expect_err("below the minimum is refused");
            assert!(
                matches!(err, TransportError::InvalidFragment(_)),
                "a short frame is a peer violation: {err}"
            );
            assert!(
                err.to_string().contains(&minimum.to_string()),
                "the refusal must name the minimum: {err}"
            );
        }
        // Exactly the minimum decodes, so the boundary is the minimum itself.
        let at_min = encode_len(minimum).expect("the minimum encodes");
        assert_eq!(decode_len(&at_min).expect("the minimum decodes"), minimum);
    }

    /// The buffer sizes both directions allocate follow from the frame cap: a
    /// full fragment's plaintext plus its tag is exactly one ciphertext, and
    /// one outbound frame is that ciphertext plus the prefix headroom.
    #[test]
    fn buffer_sizes_derive_from_the_frame_cap() {
        assert_eq!(FRAG_BUF_LEN + GCM_TAG_LEN, MAX_CIPHERTEXT_LEN);
        assert_eq!(FRAME_BUF_LEN, LEN_PREFIX_LEN + FRAG_BUF_LEN + GCM_TAG_LEN);
    }
}
