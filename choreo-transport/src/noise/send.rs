//! The data plane's send direction: fragmenting, encrypting and framing an
//! outbound message, plus the single-writer slot that keeps two senders from
//! interleaving on one stream.
//!
//! Every write goes through one place — [`NoiseStream::write_frame`] — so the
//! continuation header, the brief `TransportState` lock, the length prefix and
//! the coalesced `write_all` exist exactly once, whether the message is empty,
//! a single fragment, or many.

use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use choreo_proto::MAX_FRAME_SIZE;
use tracing::trace;
use zeroize::Zeroize;

use super::NoiseStream;
use super::frame::{
    FRAG_BUF_LEN, FRAGMENT_CONTINUATION, FRAGMENT_HEADER_LEN, FRAME_BUF_LEN, LEN_PREFIX_LEN,
    MAX_PLAINTEXT_CHUNK, encode_len,
};
use crate::buf::{at_mut, slice, slice_mut};
use crate::error::TransportError;

/// The stream's single-writer slot, shared across every `try_clone` handle of
/// one connection.
///
/// Fragments of one logical message must never interleave with another
/// message's on the wire, which holds only while exactly one thread sends per
/// stream. The daemon and the client give each connection ONE writer thread, so
/// this is a structural invariant — the slot turns a future violation into a
/// loud protocol error instead of silent corruption.
pub(super) struct SendSlot(Arc<AtomicBool>);

impl SendSlot {
    /// A free (unheld) slot.
    pub(super) fn new() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }

    /// Claim the slot for one send, or fail if another send already holds it.
    ///
    /// The returned [`SendGuard`] releases the slot on every exit path,
    /// including an early `?` — without that, a sender that errored mid-message
    /// would leave the flag set and wedge the stream.
    pub(super) fn acquire(&self) -> Result<SendGuard, TransportError> {
        // Relaxed is sufficient here and in the guard: the flag publishes no
        // data (the `TransportState` mutex provides its own ordering for the
        // cipher state, and socket bytes are kernel-buffered), so exclusion
        // needs only the atomic coherence of the flag itself — a concurrent
        // `swap(true)` sees either `false` (the previous holder's drop already
        // happened) or `true` (still in flight), never both, so exactly one
        // sender proceeds at a time.
        if self.0.swap(true, Ordering::Relaxed) {
            return Err(TransportError::InvalidFragment(
                "concurrent send_message on one NoiseStream".to_string(),
            ));
        }
        Ok(SendGuard(Arc::clone(&self.0)))
    }
}

impl Clone for SendSlot {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

/// Holds a [`SendSlot`] for the duration of one send and frees it on drop.
///
/// It owns its own `Arc` clone rather than borrowing the field, so it does not
/// hold a borrow of the stream across the send body.
pub(super) struct SendGuard(Arc<AtomicBool>);

impl Drop for SendGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Relaxed);
    }
}

impl NoiseStream {
    /// Send an encrypted message.
    ///
    /// Encrypts the plaintext into one or more ciphertext fragments (leaving
    /// the input untouched), then writes each fragment's 4-byte big-endian
    /// ciphertext length prefix followed by the ciphertext. Payloads larger
    /// than `MAX_PLAINTEXT_CHUNK` are split into multiple fragments; each
    /// fragment's plaintext starts with an AUTHENTICATED continuation byte
    /// (`FRAGMENT_CONTINUATION`) that tells the receiver whether to keep
    /// reading.
    ///
    /// The shared `TransportState` lock is held only per-chunk, during the
    /// encrypt call, and is NEVER held across the blocking `write_all`s —
    /// the reader must always be able to acquire it (briefly) and drain the
    /// socket, or two peers sending large messages concurrently would
    /// deadlock once the socket buffers filled. A runtime single-writer guard
    /// (the stream's `send_slot`) rejects a concurrent second `send_message`
    /// with a protocol error rather than interleaving fragments.
    ///
    /// The ciphertext is encrypted into the stream's reused `send_buf` field
    /// — never allocated per call — and each frame is written as ONE
    /// coalesced `write_all` (4-byte prefix + ciphertext, one syscall per
    /// fragment). The staged plaintext in `send_frag` is wiped before this
    /// returns (see the callers of `write_frame`).
    ///
    /// # Errors
    ///
    /// Returns [`TransportError::InvalidFragment`] if another thread is
    /// concurrently sending on this stream or the plaintext exceeds
    /// [`MAX_FRAME_SIZE`], [`TransportError::ConnectionClosed`]/I/O errors
    /// from the socket, or snow errors from encryption.
    pub fn send_message(&mut self, plaintext: &[u8]) -> Result<(), TransportError> {
        let _guard = self.send_slot.acquire()?;
        match self.send_message_inner(plaintext) {
            // Wipe exactly the region this send wrote: the plaintext staging
            // buffer holds a copy of this (possibly secret) message until the
            // next send would overwrite it, so it is cleared on success too.
            // Wiping only the used prefix (rather than the whole `FRAG_BUF_LEN`
            // allocation, which `Vec::zeroize` would clear along with the
            // spare capacity) keeps the guarantee at a fraction of the cost and
            // leaves the buffer at full length for the next call. The
            // `Zeroizing` payload a typed caller holds is wiped independently
            // on its own drop.
            Ok(used) => {
                if let Some(written) = self.send_frag.get_mut(..used) {
                    written.zeroize();
                }
                Ok(())
            }
            // A failed send may have written part of the payload before the
            // error (a socket error mid-chunk), so the extent is unknown: wipe
            // the whole buffer rather than guess.
            Err(e) => {
                self.send_frag.zeroize();
                Err(e)
            }
        }
    }

    /// The send body, returning how many bytes of `send_frag` the last frame
    /// staged (its plaintext varies in length with the payload) so the caller
    /// can wipe exactly that prefix.
    fn send_message_inner(&mut self, plaintext: &[u8]) -> Result<usize, TransportError> {
        // Mirror the receiver's reassembly cap so an over-limit message is
        // rejected at the source instead of by the peer's recv_message.
        if plaintext.len() > MAX_FRAME_SIZE {
            return Err(TransportError::InvalidFragment(format!(
                "outgoing message exceeds the {MAX_FRAME_SIZE}-byte limit"
            )));
        }
        trace!(plaintext_len = plaintext.len(), "sending Noise message");

        // Guarantee the reused per-stream buffers are big enough for the
        // largest single fragment. They are lazy: `new`/`try_clone` start them
        // empty, and this grows them once to their worst-case size, so nothing
        // is allocated per message or per fragment afterwards.
        if self.send_buf.len() < FRAME_BUF_LEN {
            self.send_buf.resize(FRAME_BUF_LEN, 0);
        }
        if self.send_frag.len() < FRAG_BUF_LEN {
            self.send_frag.resize(FRAG_BUF_LEN, 0);
        }

        let mut remaining = plaintext.len();
        let mut used = 0;
        for chunk in plaintext.chunks(MAX_PLAINTEXT_CHUNK) {
            // This chunk is non-final iff it did not consume all remaining
            // plaintext; a full-size chunk in the middle leaves a remainder,
            // while the final chunk (even one exactly MAX_PLAINTEXT_CHUNK
            // bytes) leaves none.
            let more = remaining > chunk.len();
            remaining -= chunk.len();
            // Every chunk's plaintext sits at the front of the buffer, so the
            // furthest byte any chunk of this send reached is what the caller
            // has to wipe.
            used = used.max(self.write_frame(chunk, more)?);
        }

        if used == 0 {
            // `chunks` yields nothing for an empty payload, so the loop above
            // wrote nothing — but the peer's `recv_message` blocks on a length
            // prefix, so an empty message still needs exactly one wire frame.
            // Emit a single fragment carrying only the cleared continuation
            // header, which `recv_message` returns as an empty message.
            used = self.write_frame(&[], false)?;
        }
        Ok(used)
    }

    /// Stage `payload` behind the authenticated continuation header, encrypt it
    /// under a brief `TransportState` lock, then write the 4-byte length prefix
    /// and the ciphertext as ONE `write_all`.
    ///
    /// Returns the number of `send_frag` bytes staged (header + payload) so the
    /// caller can wipe exactly that prefix.
    ///
    /// Serializing sends is the caller's job (the slot), so the
    /// `TransportState` lock is only needed to make the encrypt call atomic
    /// against the reader's decrypts. It must NEVER be held across the blocking
    /// `write_all` below: a reader waiting on the lock would stop draining the
    /// socket, and a peer doing the same in the other direction would deadlock
    /// once the socket buffers filled. The frame is therefore encrypted under a
    /// brief lock and written outside it, so the reader can always acquire the
    /// lock and drain.
    fn write_frame(&mut self, payload: &[u8], more: bool) -> Result<usize, TransportError> {
        let staged = FRAGMENT_HEADER_LEN
            .checked_add(payload.len())
            .ok_or_else(|| crate::buf::invalid_slice("send_frag stage"))?;
        *at_mut(&mut self.send_frag, 0, "send_frag continuation header")? =
            if more { FRAGMENT_CONTINUATION } else { 0 };
        slice_mut(
            &mut self.send_frag,
            FRAGMENT_HEADER_LEN..staged,
            "send_frag plaintext",
        )?
        .copy_from_slice(payload);

        let n = {
            let mut transport = self
                .transport
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            transport.write_message(
                slice(&self.send_frag, ..staged, "send_frag header+chunk")?,
                // Ciphertext goes AFTER the length-prefix headroom; the prefix
                // is laid into that headroom afterwards — encrypting over the
                // whole buffer would let the prefix overwrite the first
                // ciphertext bytes.
                slice_mut(&mut self.send_buf, LEN_PREFIX_LEN.., "send_buf ciphertext")?,
            )?
        };

        // One coalesced write per frame: the prefix is laid into the headroom
        // ahead of the ciphertext, so a single write_all sends both (one
        // syscall per fragment instead of two).
        let prefix = encode_len(n)?;
        slice_mut(
            &mut self.send_buf,
            ..LEN_PREFIX_LEN,
            "send_buf length prefix",
        )?
        .copy_from_slice(&prefix);
        self.tcp.write_all(slice(
            &self.send_buf,
            ..LEN_PREFIX_LEN + n,
            "send_buf frame",
        )?)?;
        Ok(staged)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The single-writer slot is exclusive while a send holds it and free again
    /// the moment the guard drops — the invariant that keeps two senders from
    /// interleaving fragments. Pure state, no socket, no thread, no timer.
    #[test]
    fn send_slot_is_exclusive_until_the_guard_drops() {
        let slot = SendSlot::new();

        let first = slot.acquire().expect("a free slot must be claimable");
        assert!(
            slot.acquire().is_err(),
            "a second sender must be refused while the first holds the slot"
        );

        // Dropping the guard frees the slot, so the stream is usable again
        // after a completed (or failed) send.
        drop(first);
        let second = slot
            .acquire()
            .expect("the slot must be free after the drop");
        drop(second);
        slot.acquire().expect("and free again after a second drop");
    }

    /// Clones share one slot: a `try_clone` handle is the same connection, so
    /// two clones must contend for the same single-writer right rather than
    /// each getting its own.
    #[test]
    fn cloned_slots_share_one_writer_right() {
        let slot = SendSlot::new();
        let clone = slot.clone();

        let held = slot.acquire().expect("claim on the original");
        assert!(
            clone.acquire().is_err(),
            "a clone must see the original's claim"
        );
        drop(held);
        clone.acquire().expect("the clone must see the release");
    }
}
