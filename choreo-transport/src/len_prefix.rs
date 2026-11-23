//! One big-endian length-prefix codec, shared by both framing paths.
//!
//! The handshake prefixes a 2-byte (u16) body length and the data plane prefixes
//! a 4-byte (u32) ciphertext length, but the rules are otherwise identical: the
//! length is big-endian, and the cap that bounds it is the same value the
//! receiver refuses above. Those two rules live here once, parameterised by the
//! prefix width `N` and the cap, so each path keeps only its own constants
//! (width and cap) and the arithmetic that turns a length into bytes and back
//! cannot drift between what the sender produces and what the receiver accepts.
//!
//! Both functions are allocation-free on the encode side (a fixed `[u8; N]`) and
//! bounds-checked on both, so a broken internal invariant becomes the crate's
//! [`TransportError::InternalInvariant`] rather than a panic in a connection
//! thread. A length a *peer* declared above the cap is a wire violation and is
//! reported as [`TransportError::InvalidFragment`] instead — the two are kept
//! apart so a caller can tell a bug in this build from a malicious peer.

use crate::buf::invalid_slice;
use crate::error::TransportError;

/// Encode `n` as a big-endian length prefix of `N` bytes, refusing a length the
/// caller's `cap` cannot express or the width cannot hold.
///
/// `N` is a compile-time prefix width (2 for the handshake, 4 for the data
/// plane) and `cap` is the same bound the matching [`decode`] enforces on a
/// peer, so a sender can never produce a frame the receiver refuses. The
/// conversion is checked rather than cast even though a valid `cap` makes it
/// infallible: an over-long frame is a broken internal invariant, and this
/// reports it as [`TransportError::InternalInvariant`] (never a peer violation)
/// instead of truncating the prefix into a frame no peer could reassemble. A
/// `cap` wider than the width can express is guarded the same way — the cap
/// alone cannot stop a value the `N` bytes cannot carry.
pub(crate) fn encode<const N: usize>(
    n: usize,
    cap: usize,
    what: &'static str,
) -> Result<[u8; N], TransportError> {
    // One message for both failure shapes — "above the caller's cap" and "too
    // wide for the prefix" — because either way the frame is not one this
    // implementation may put on the wire, which is a local bug, not a peer's.
    let too_long = || {
        TransportError::InternalInvariant(format!(
            "{what} {n} exceeds the {cap}-byte cap (cannot be encoded)"
        ))
    };
    if n > cap {
        return Err(too_long());
    }
    let value = u64::try_from(n).map_err(|_| too_long())?;
    // A width narrower than 8 bytes cannot represent a value whose higher
    // bytes are non-zero: `to_be_bytes` would keep only the low `N` bytes and
    // silently describe a shorter frame than the caller asked for (a 2-byte
    // prefix over 70_000 would encode as 4464). Refuse it rather than truncate.
    // `N < 8` keeps the shift defined, and `N > 8` is already refused by the
    // `checked_sub` below.
    if N < 8 && (value >> (8 * N)) != 0 {
        return Err(too_long());
    }
    let be = value.to_be_bytes();
    // The value occupies the low `N` bytes of the 8-byte big-endian form. A
    // width the form cannot cover (N > 8) is refused rather than panicking on
    // the slice below; `N` is only ever 2 or 4 in this crate.
    let tail = be
        .get(be.len().checked_sub(N).ok_or_else(too_long)?..)
        .ok_or_else(too_long)?;
    let mut out = [0u8; N];
    out.copy_from_slice(tail);
    Ok(out)
}

/// Decode a big-endian length prefix of `N` bytes and validate it against
/// `cap`, before the caller allocates or reads that many bytes.
///
/// This is the single place a peer-declared length is trusted on each path, so
/// the validation runs before anything is sized for the body: a hostile or
/// corrupted peer that declares more than `cap` is refused with
/// [`TransportError::InvalidFragment`] instead of making the receiver allocate
/// and then block reading that much.
pub(crate) fn decode<const N: usize>(
    prefix: &[u8],
    cap: usize,
    what: &'static str,
) -> Result<usize, TransportError> {
    let bytes = prefix
        .get(..N)
        .and_then(|head| <&[u8; N]>::try_from(head).ok())
        .ok_or_else(|| invalid_slice(what))?;
    // Fold the big-endian bytes into a u64 (never wider than the 8-byte form),
    // then narrow with a checked conversion so a value that cannot be a usize on
    // this target is the crate's protocol error, not a silent truncation.
    let value = bytes
        .iter()
        .fold(0u64, |acc, &byte| (acc << 8) | u64::from(byte));
    let declared = usize::try_from(value).map_err(|_| invalid_slice(what))?;
    if declared > cap {
        return Err(TransportError::InvalidFragment(format!(
            "{what} {declared} exceeds the {cap}-byte cap"
        )));
    }
    Ok(declared)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both widths round-trip through big-endian bytes, the high byte comes
    /// first, and the encode bound matches the decode bound at the cap on each
    /// width — the two properties that let a sender never produce what the
    /// receiver refuses.
    #[test]
    fn round_trips_and_bounds_at_the_cap() {
        // 2-byte (handshake) width, a small cap.
        let cap2 = 1024usize;
        for len in [0usize, 1, 96, cap2] {
            let encoded: [u8; 2] = encode(len, cap2, "frame").expect("encodes");
            assert_eq!(
                decode::<2>(&encoded, cap2, "frame").expect("decodes"),
                len,
                "2-byte prefix must round-trip"
            );
        }
        let two: [u8; 2] = encode(0x01_02, cap2, "frame").expect("encodes");
        assert_eq!(two, [0x01, 0x02], "big-endian: high byte first");

        // 4-byte (data-plane) width, a large cap.
        let cap4 = 65_535usize;
        for len in [0usize, 1, 65_518, cap4] {
            let encoded: [u8; 4] = encode(len, cap4, "frame").expect("encodes");
            assert_eq!(
                decode::<4>(&encoded, cap4, "frame").expect("decodes"),
                len,
                "4-byte prefix must round-trip"
            );
        }
        let four: [u8; 4] = encode(0xAB_CD, cap4, "frame").expect("encodes");
        assert_eq!(
            four,
            [0x00, 0x00, 0xAB, 0xCD],
            "big-endian: high byte first"
        );
    }

    /// A length above the cap is refused on both sides and the refusal names
    /// the cap, so an operator sees WHY a frame was rejected. The ceiling is
    /// checked with saturating inputs that could not be a legal frame.
    #[test]
    fn above_the_cap_is_refused_on_both_sides() {
        let cap = 65_535usize;
        let over: Result<[u8; 4], _> = encode(cap + 1, cap, "frame");
        let err = over.expect_err("over the cap must not encode");
        assert!(
            err.to_string().contains(&cap.to_string()),
            "the refusal must name the cap: {err}"
        );

        let declared = u32::try_from(cap + 1).expect("fits u32").to_be_bytes();
        let err = decode::<4>(&declared, cap, "frame").expect_err("over the cap must not decode");
        assert!(
            err.to_string().contains(&cap.to_string()),
            "the refusal must name the cap: {err}"
        );

        // The whole 32-bit range a hostile peer can declare is refused above
        // the cap, including the maximum.
        let err = decode::<4>(&u32::MAX.to_be_bytes(), cap, "frame")
            .expect_err("the maximum 32-bit length must be rejected");
        assert!(matches!(err, TransportError::InvalidFragment(_)));
    }

    /// A `cap` wider than the prefix width cannot be trusted to bound the value:
    /// the guard refuses a length the `N` bytes cannot hold instead of
    /// truncating it into a shorter frame. The widest value the width can
    /// express still encodes, so the guard only rejects genuine overflow.
    #[test]
    fn a_cap_wider_than_the_prefix_refuses_rather_than_truncating() {
        // 0xFFFF is the largest value a 2-byte prefix can carry; it must encode
        // as the two all-ones bytes and decode back unchanged.
        let widest = encode::<2>(0xFFFF, 0xFFFF, "frame").expect("the widest value encodes");
        assert_eq!(widest, [0xFF, 0xFF]);
        assert_eq!(
            decode::<2>(&widest, 0xFFFF, "frame").expect("decodes"),
            0xFFFF
        );

        // 70_000 fits a u32 but not two bytes; with a 70_000 cap the cap check
        // alone would let it through and truncate the prefix to 4464. This is a
        // local bug, so it is an `InternalInvariant`, not a peer violation.
        let err = encode::<2>(70_000, 70_000, "frame").expect_err("70000 must not truncate");
        assert!(
            matches!(err, TransportError::InternalInvariant(_)),
            "a width overflow must be the crate's internal-invariant error: {err}"
        );
        assert!(
            err.to_string().contains("70000"),
            "the refusal must name the length: {err}"
        );
    }

    /// A prefix that is shorter than the declared width is the crate's
    /// internal-invariant error (both call sites pass an exactly `N`-byte head),
    /// not a panic — the bounds-checked `get` is what lets the no-panic policy
    /// hold on both framing paths.
    #[test]
    fn a_short_prefix_is_an_internal_invariant() {
        assert!(matches!(
            decode::<4>(&[0u8, 0, 1], 65_535, "frame"),
            Err(TransportError::InternalInvariant(_))
        ));
        assert!(matches!(
            decode::<2>(&[], 1024, "frame"),
            Err(TransportError::InternalInvariant(_))
        ));
    }
}
