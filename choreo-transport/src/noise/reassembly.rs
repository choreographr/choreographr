//! One logical message's reassembly: the byte cap and the fragment-count bound.
//!
//! A data-plane message is split across one or more [`super::frame`] fragments,
//! and the per-frame cap there bounds a single fragment — not the message those
//! fragments add up to. This type owns BOTH message-level bounds: the running
//! plaintext total AND the number of fragments that make it up. The count is
//! required because the byte total cannot bound a run of continuation-marked
//! frames carrying NO payload (each adds nothing), while the receive loop still
//! iterates once per frame — so bytes alone leave an endless message possible.
//!
//! It is the only way the receive loop touches the total: a frame must be
//! [`accept_frame`](Reassembly::accept_frame)d to obtain an [`AcceptedFrame`],
//! and [`extend`](Reassembly::extend) demands one — so "append a fragment whose
//! length was never checked" is not expressible, and the check cannot be dropped
//! from the loop without the loop failing to compile.

use zeroize::Zeroizing;

use crate::error::TransportError;

use super::frame::{FRAGMENT_HEADER_LEN, GCM_TAG_LEN, MAX_FRAGMENTS_PER_MESSAGE};

/// One logical message's reassembly: the plaintext accumulated so far, the cap
/// that bounds it, and the fragment count.
///
/// Both bounds are enforced BEFORE a frame's body is read or the buffer grows,
/// so a message that never terminates costs at most `MAX_FRAME_SIZE` of
/// plaintext and [`MAX_FRAGMENTS_PER_MESSAGE`] frames rather than an unbounded
/// amount.
pub(super) struct Reassembly {
    plaintext: Zeroizing<Vec<u8>>,
    cap: usize,
    /// Fragments accepted so far; bounded by [`MAX_FRAGMENTS_PER_MESSAGE`].
    frames: usize,
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
            frames: 0,
        }
    }

    /// Validate a frame against the fragment count, then its declared ciphertext
    /// length against the byte cap and the plaintext accumulated so far, before
    /// anything is read or appended.
    ///
    /// A fragment contributes at most `ct_len - (tag + continuation header)`
    /// plaintext bytes; the subtraction saturates, so a frame too short to carry
    /// any payload counts as no contribution instead of wrapping into a huge
    /// one. The fragment count is checked first so a run of zero-payload frames
    /// is refused once the count is exhausted even though it never moves the byte
    /// total.
    ///
    /// # Errors
    ///
    /// [`TransportError::InvalidFragment`] when accepting this frame would push
    /// the message past the byte cap or the fragment-count bound.
    pub(super) fn accept_frame(&mut self, ct_len: usize) -> Result<AcceptedFrame, TransportError> {
        if self.frames >= MAX_FRAGMENTS_PER_MESSAGE {
            return Err(TransportError::InvalidFragment(format!(
                "message exceeds the {MAX_FRAGMENTS_PER_MESSAGE}-fragment limit"
            )));
        }
        let contribution = ct_len.saturating_sub(GCM_TAG_LEN + FRAGMENT_HEADER_LEN);
        if self.plaintext.len().saturating_add(contribution) > self.cap {
            return Err(TransportError::InvalidFragment(format!(
                "reassembled message exceeds the {}-byte limit",
                self.cap
            )));
        }
        self.frames += 1;
        Ok(AcceptedFrame(()))
    }

    /// Append an accepted fragment's decrypted payload.
    pub(super) fn extend(&mut self, _frame: AcceptedFrame, payload: &[u8]) {
        // `extend_from_slice` already reserves with amortized growth (`reserve`),
        // so an explicit `reserve` here would buy nothing — it would reserve the
        // same amount and reallocate the same number of times. Only an up-front
        // size guess could help, and the message's total size is unknown until
        // the continuation byte clears; reserving `cap` (64 MiB) up front would
        // just waste memory on the common small message. So do not add a reserve.
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
            frames: 0,
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
    /// continuation header — never moves the byte cap, so an empty-fragment
    /// stream cannot be refused by the byte total. That is exactly why the
    /// separate fragment-count bound exists: the byte cap stays put while the
    /// receive loop keeps iterating, so bytes alone cannot end such a stream.
    #[test]
    fn zero_payload_fragments_never_trip_the_cap() {
        let cap = 8usize;
        let mut assembly = Reassembly::with_cap(cap);
        // Fill the cap, so there is no room left for a payload byte at all.
        let filling = assembly.accept_frame(cap).expect("fills the cap");
        assembly.extend(filling, &[0xAB; 8]);
        assert_eq!(assembly.len(), cap);
        for ct_len in [0, 1, GCM_TAG_LEN, GCM_TAG_LEN + FRAGMENT_HEADER_LEN] {
            assert!(
                assembly.accept_frame(ct_len).is_ok(),
                "a {ct_len}-byte frame contributes no plaintext"
            );
        }
    }

    /// The fragment-count bound is what refuses a stream the byte cap cannot:
    /// each zero-payload frame adds nothing to the total, so only the count
    /// stops it. Driven directly through `accept_frame` with a small cap, so no
    /// socket and no 64 MiB allocation is involved.
    #[test]
    fn zero_payload_fragments_are_bounded_by_the_fragment_count() {
        let mut assembly = Reassembly::with_cap(8);
        // A tag plus header decrypts to a valid continuation byte and zero
        // payload, so `contribution` saturates to zero and the byte total never
        // moves.
        let empty_frame = GCM_TAG_LEN + FRAGMENT_HEADER_LEN;
        for n in 0..MAX_FRAGMENTS_PER_MESSAGE {
            assembly
                .accept_frame(empty_frame)
                .unwrap_or_else(|e| panic!("frame {n} is within the count bound: {e}"));
            assert_eq!(assembly.len(), 0, "a zero-payload frame appends nothing");
        }
        // The next frame is one past the bound and is refused, naming the limit.
        let err = assembly
            .accept_frame(empty_frame)
            .expect_err("the count bound must refuse the frame past it");
        assert!(
            err.to_string()
                .contains(&MAX_FRAGMENTS_PER_MESSAGE.to_string()),
            "the refusal must name the fragment limit: {err}"
        );
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
