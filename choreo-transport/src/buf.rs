//! Bounds-checked buffer access for the transport's framing paths.
//!
//! The handshake framing and the data plane both slice a handful of
//! fixed-layout buffers: a length prefix, a ciphertext body, a plaintext
//! chunk. Every offset involved is an internal size invariant rather than
//! peer-controlled input, but a workspace-wide deny on `indexing_slicing`
//! exists because an out-of-range slice panics, and a panic inside a
//! connection thread is a crash surface. These helpers turn a broken
//! invariant into [`TransportError::InvalidFragment`] instead, so the
//! no-panic policy holds on both paths.
//!
//! They live here — outside both `handshake` and `noise` — because the data
//! plane has no business reaching through the handshake module for them.

use std::slice;

use crate::error::TransportError;

/// The crate's protocol error for a broken internal buffer-size invariant.
pub(crate) fn invalid_slice(what: &str) -> TransportError {
    TransportError::InvalidFragment(format!("internal buffer size invariant violated ({what})"))
}

/// `&buf[i]` composed with the bounds check.
pub(crate) fn at<'a>(
    buf: &'a [u8],
    i: usize,
    what: &'static str,
) -> Result<&'a u8, TransportError> {
    buf.get(i).ok_or_else(|| invalid_slice(what))
}

/// Mutable counterpart of [`at`].
pub(crate) fn at_mut<'a>(
    buf: &'a mut [u8],
    i: usize,
    what: &'static str,
) -> Result<&'a mut u8, TransportError> {
    buf.get_mut(i).ok_or_else(|| invalid_slice(what))
}

/// `&buf[range]` composed with the bounds check: `get` returns `None` only if
/// an internal buffer-size invariant broke (e.g. the handshake frame buffers,
/// or the reused worst-case Noise frame buffers), which becomes the crate's
/// protocol error instead of a panic.
pub(crate) fn slice<'a, S>(
    buf: &'a [u8],
    range: S,
    what: &'static str,
) -> Result<&'a [u8], TransportError>
where
    S: slice::SliceIndex<[u8], Output = [u8]>,
{
    buf.get(range).ok_or_else(|| invalid_slice(what))
}

/// Mutable counterpart of [`slice`].
pub(crate) fn slice_mut<'a, S>(
    buf: &'a mut [u8],
    range: S,
    what: &'static str,
) -> Result<&'a mut [u8], TransportError>
where
    S: slice::SliceIndex<[u8], Output = [u8]>,
{
    buf.get_mut(range).ok_or_else(|| invalid_slice(what))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The helpers return the crate's protocol error for an out-of-range index
    /// or range instead of panicking, which is what lets the no-panic lint
    /// policy stay in force on both framing paths.
    #[test]
    fn slice_helpers_error_out_of_range() {
        let buf = [1u8, 2, 3];

        assert!(
            matches!(at(&buf, 3, "at"), Err(TransportError::InvalidFragment(_))),
            "an out-of-range index must be an error, not a panic"
        );
        assert!(
            matches!(
                slice(&buf, 2..5, "slice"),
                Err(TransportError::InvalidFragment(_))
            ),
            "an out-of-range range must be an error, not a panic"
        );
        assert!(
            matches!(
                slice(&buf, 4.., "slice"),
                Err(TransportError::InvalidFragment(_))
            ),
            "an out-of-range start must be an error, not a panic"
        );

        let mut owned = [1u8, 2, 3];
        assert!(
            matches!(
                at_mut(&mut owned, 3, "at_mut"),
                Err(TransportError::InvalidFragment(_))
            ),
            "an out-of-range mutable index must be an error, not a panic"
        );
        assert!(
            matches!(
                slice_mut(&mut owned, 2..5, "slice_mut"),
                Err(TransportError::InvalidFragment(_))
            ),
            "an out-of-range mutable range must be an error, not a panic"
        );

        // The in-range cases still work, so the helpers are not erroring
        // unconditionally.
        assert_eq!(at(&buf, 1, "at").expect("in range"), &2);
        assert_eq!(slice(&buf, ..2, "slice").expect("in range"), &[1u8, 2]);
        assert_eq!(slice(&buf, 2.., "slice tail").expect("in range"), &[3u8]);
    }
}
