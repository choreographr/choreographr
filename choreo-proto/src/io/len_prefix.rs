//! One big-endian length-prefix codec, parameterised by prefix width and cap.
//!
//! Three framing paths use it: the message envelope's 4-byte payload prefix
//! (`read_payload` in the sibling `framing` module) and the transport's two
//! Noise paths — the handshake's 2-byte (u16) body length and the data plane's
//! 4-byte (u32) ciphertext length. The rules are identical on all three: the
//! length is big-endian, and the cap that bounds it is the same value the
//! receiver refuses above. Stating them once, parameterised by the prefix width
//! `N` and the cap, leaves each path only its own two constants and makes it
//! impossible for the arithmetic that turns a length into bytes to drift from
//! the arithmetic that reads it back.
//!
//! It lives here because every one of those paths sits above this crate:
//! `choreo-proto` owns the message envelope itself, and `choreo-transport`
//! layers its Noise framing on top of this crate's payloads.
//!
//! The encode side is allocation-free (it returns a fixed `[u8; N]`) and both
//! directions are bounds-checked, so no input a caller can supply makes this
//! module panic. It reports the fact of a refusal and never a verdict, because
//! the verdict depends on who supplied the length: a length a **peer** declared
//! above the cap is the caller's peer-violation verdict, while an over-long
//! value this build tried to **encode** is the caller's local-bug verdict. Each
//! caller names its own, which is why [`LenPrefixError`] stays neutral.

use thiserror::Error;

/// Why a length could not be encoded into, or decoded from, a big-endian
/// length prefix.
///
/// The variants name distinct facts rather than a caller's classification: the
/// same [`Self::AboveCap`] is a peer violation on a decode and a local bug on
/// an encode. Every field is a plain integer, so a caller can copy a refusal,
/// classify it, and still render it.
#[derive(Error, Debug, Clone, Copy)]
pub enum LenPrefixError {
    /// A length above the caller's cap.
    #[error("{len} exceeds the {cap}-byte cap")]
    AboveCap {
        /// The length that was over the cap: declared by a peer, or requested
        /// by a caller.
        len: usize,
        /// The bound it broke — the caller's frame or message cap.
        cap: usize,
    },
    /// A value the prefix width cannot express (encode only).
    #[error("{len} exceeds the {width}-byte prefix's range")]
    TooWide {
        /// The length that does not fit the prefix.
        len: usize,
        /// The compile-time prefix width `N` that cannot carry it.
        width: usize,
    },
    /// A prefix slice shorter than the declared width (decode only).
    #[error("a {width}-byte length prefix is required")]
    ShortPrefix {
        /// The compile-time prefix width `N` the slice fell short of.
        width: usize,
    },
    /// A peer-declared value that is not representable as a `usize` on this target.
    #[error("the declared length is not representable as a usize on this target")]
    NotRepresentable,
}

/// Encode `n` as a big-endian length prefix of `N` bytes, refusing a length the
/// caller's `cap` cannot express or the width cannot hold.
///
/// `N` is a compile-time prefix width (2 for the handshake, 4 for the data
/// plane and the message envelope) and `cap` is the same bound the matching
/// [`decode`] enforces on a peer, so a sender can never produce a frame the
/// receiver refuses. The conversion is checked rather than cast even though a
/// valid `cap` makes it infallible: an over-long frame is a broken caller, and
/// refusing it here is what keeps the prefix from being truncated into a frame
/// no peer could reassemble.
///
/// # Errors
///
/// [`LenPrefixError::AboveCap`] when `n` exceeds `cap`, and
/// [`LenPrefixError::TooWide`] when the `N` bytes cannot carry `n`.
pub fn encode<const N: usize>(n: usize, cap: usize) -> Result<[u8; N], LenPrefixError> {
    if n > cap {
        return Err(LenPrefixError::AboveCap { len: n, cap });
    }
    // A `usize` wider than the 8-byte big-endian form cannot be expressed by a
    // prefix of any width, so it is the same refusal as a value this width
    // cannot carry.
    let value = u64::try_from(n).map_err(|_| LenPrefixError::TooWide { len: n, width: N })?;
    // A width narrower than 8 bytes cannot represent a value whose higher
    // bytes are non-zero: `to_be_bytes` would keep only the low `N` bytes and
    // silently describe a shorter frame than the caller asked for (a 2-byte
    // prefix over 70_000 would encode as 4464). Refuse it rather than truncate.
    // `N < 8` keeps the shift defined, and `N > 8` is already refused by the
    // `checked_sub` below.
    if N < 8 && (value >> (8 * N)) != 0 {
        return Err(LenPrefixError::TooWide { len: n, width: N });
    }
    let be = value.to_be_bytes();
    // The value occupies the low `N` bytes of the 8-byte big-endian form. A
    // width the form cannot cover (N > 8) is refused rather than panicking on
    // the slice below; the callers use only 2 and 4.
    let too_wide = || LenPrefixError::TooWide { len: n, width: N };
    let tail = be
        .get(be.len().checked_sub(N).ok_or_else(too_wide)?..)
        .ok_or_else(too_wide)?;
    let mut out = [0u8; N];
    out.copy_from_slice(tail);
    Ok(out)
}

/// Decode a big-endian length prefix of `N` bytes and validate it against
/// `cap`, before the caller allocates or reads that many bytes.
///
/// This is the single place a peer-declared length is trusted on each path, so
/// the validation runs before anything is sized for the body: a hostile or
/// corrupted peer that declares more than `cap` is reported here instead of
/// making the receiver allocate — and then block reading — that much.
///
/// # Errors
///
/// [`LenPrefixError::ShortPrefix`] when `prefix` holds fewer than `N` bytes,
/// [`LenPrefixError::NotRepresentable`] when the declared value is not a
/// `usize` on this target, and [`LenPrefixError::AboveCap`] when it exceeds
/// `cap`.
pub fn decode<const N: usize>(prefix: &[u8], cap: usize) -> Result<usize, LenPrefixError> {
    let bytes = prefix
        .get(..N)
        .and_then(|head| <&[u8; N]>::try_from(head).ok())
        .ok_or(LenPrefixError::ShortPrefix { width: N })?;
    // Fold the big-endian bytes into a u64 (never wider than the 8-byte form),
    // then narrow with a checked conversion so a value that cannot be a usize on
    // this target is reported, not silently truncated.
    let value = bytes
        .iter()
        .fold(0u64, |acc, &byte| (acc << 8) | u64::from(byte));
    let declared = usize::try_from(value).map_err(|_| LenPrefixError::NotRepresentable)?;
    if declared > cap {
        return Err(LenPrefixError::AboveCap { len: declared, cap });
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
            let encoded: [u8; 2] = encode(len, cap2).expect("encodes");
            assert_eq!(
                decode::<2>(&encoded, cap2).expect("decodes"),
                len,
                "2-byte prefix must round-trip"
            );
        }
        let two: [u8; 2] = encode(0x01_02, cap2).expect("encodes");
        assert_eq!(two, [0x01, 0x02], "big-endian: high byte first");

        // 4-byte (data-plane) width, a large cap.
        let cap4 = 65_535usize;
        for len in [0usize, 1, 65_518, cap4] {
            let encoded: [u8; 4] = encode(len, cap4).expect("encodes");
            assert_eq!(
                decode::<4>(&encoded, cap4).expect("decodes"),
                len,
                "4-byte prefix must round-trip"
            );
        }
        let four: [u8; 4] = encode(0xAB_CD, cap4).expect("encodes");
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
        let over: Result<[u8; 4], _> = encode(cap + 1, cap);
        let err = over.expect_err("over the cap must not encode");
        assert!(
            matches!(err, LenPrefixError::AboveCap { .. }),
            "an over-cap encode must report the cap it broke: {err}"
        );
        assert!(
            err.to_string().contains(&cap.to_string()),
            "the refusal must name the cap: {err}"
        );

        let declared = u32::try_from(cap + 1).expect("fits u32").to_be_bytes();
        let err = decode::<4>(&declared, cap).expect_err("over the cap must not decode");
        assert!(
            matches!(err, LenPrefixError::AboveCap { .. }),
            "an over-cap declaration must report the cap it broke: {err}"
        );
        assert!(
            err.to_string().contains(&cap.to_string()),
            "the refusal must name the cap: {err}"
        );

        // The whole 32-bit range a hostile peer can declare is refused above
        // the cap, including the maximum.
        let err = decode::<4>(&u32::MAX.to_be_bytes(), cap)
            .expect_err("the maximum 32-bit length must be rejected");
        assert!(matches!(err, LenPrefixError::AboveCap { .. }));
    }

    /// A `cap` wider than the prefix width cannot be trusted to bound the value:
    /// the guard refuses a length the `N` bytes cannot hold instead of
    /// truncating it into a shorter frame. The widest value the width can
    /// express still encodes, so the guard only rejects genuine overflow.
    #[test]
    fn a_cap_wider_than_the_prefix_refuses_rather_than_truncating() {
        // 0xFFFF is the largest value a 2-byte prefix can carry; it must encode
        // as the two all-ones bytes and decode back unchanged.
        let widest = encode::<2>(0xFFFF, 0xFFFF).expect("the widest value encodes");
        assert_eq!(widest, [0xFF, 0xFF]);
        assert_eq!(decode::<2>(&widest, 0xFFFF).expect("decodes"), 0xFFFF);

        // 70_000 fits a u32 but not two bytes; with a 70_000 cap the cap check
        // alone would let it through and truncate the prefix to 4464, so the
        // width guard is what refuses it.
        let err = encode::<2>(70_000, 70_000).expect_err("70000 must not truncate");
        assert!(
            matches!(err, LenPrefixError::TooWide { .. }),
            "a width overflow must be the width refusal: {err}"
        );
        assert!(
            err.to_string().contains("70000"),
            "the refusal must name the length: {err}"
        );
    }

    /// A prefix that is shorter than the declared width is a reported error
    /// (both call sites pass an exactly `N`-byte head), not a panic — the
    /// bounds-checked `get` is what lets the no-panic policy hold on every
    /// framing path.
    #[test]
    fn a_short_prefix_is_refused() {
        assert!(matches!(
            decode::<4>(&[0u8, 0, 1], 65_535),
            Err(LenPrefixError::ShortPrefix { width: 4 })
        ));
        assert!(matches!(
            decode::<2>(&[], 1024),
            Err(LenPrefixError::ShortPrefix { width: 2 })
        ));
    }
}
