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
//! protocol error rather than a panic in a connection thread.

use crate::buf::invalid_slice;
use crate::error::TransportError;

/// Encode `n` as a big-endian length prefix of `N` bytes, refusing a length the
/// caller's `cap` cannot express.
///
/// `N` is a compile-time prefix width (2 for the handshake, 4 for the data
/// plane) and `cap` is the same bound the matching [`decode`] enforces on a
/// peer, so a sender can never produce a frame the receiver refuses. The
/// conversion is checked rather than cast even though a valid `cap` makes it
/// infallible: an over-long frame is a broken internal invariant, and this
/// reports it as the crate's protocol error instead of truncating the prefix
/// into a frame no peer could reassemble.
pub(crate) fn encode<const N: usize>(
    n: usize,
    cap: usize,
    what: &'static str,
) -> Result<[u8; N], TransportError> {
    // One message for both failure shapes — "above the caller's cap" and "too
    // wide for the prefix" — because either way the frame is not one this
    // implementation may put on the wire.
    let too_long =
        || TransportError::InvalidFragment(format!("{what} {n} exceeds the {cap}-byte cap"));
    if n > cap {
        return Err(too_long());
    }
    let value = u64::try_from(n).map_err(|_| too_long())?;
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

    /// A prefix that is shorter than the declared width is the crate's protocol
    /// error, not a panic — the bounds-checked `get` is what lets the no-panic
    /// policy hold on both framing paths.
    #[test]
    fn a_short_prefix_is_a_protocol_error() {
        assert!(matches!(
            decode::<4>(&[0u8, 0, 1], 65_535, "frame"),
            Err(TransportError::InvalidFragment(_))
        ));
        assert!(matches!(
            decode::<2>(&[], 1024, "frame"),
            Err(TransportError::InvalidFragment(_))
        ));
    }
}
