//! The data plane's send direction: fragmenting, encrypting and framing an
//! outbound message.
//!
//! Every write goes through one place — [`NoiseStream::write_frame`] — so the
//! continuation header, the brief `TransportState` lock, the length prefix and
//! the coalesced write exist exactly once, whether the message is empty, a single
//! fragment, or many. Two entry points share that body:
//! [`NoiseStream::send_message`] (no bound of its own) and
//! [`NoiseStream::send_message_by`] (the whole message bounded by one absolute
//! deadline and each write by a per-syscall stall). The single-writer right itself
//! lives in [`super::slot`], shared with the read direction.

use std::io::Write;
use std::time::{Duration, Instant};

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

/// The two bounds [`NoiseStream::send_message_by`] applies to one message: an
/// absolute deadline for the whole message and a stall for any single write.
///
/// Kept as one value so the framing loop threads a single `Option<…>` through
/// (absent meaning "plain `write_all`, no policy of our own") and cannot thread
/// one bound while forgetting the other.
#[derive(Clone, Copy)]
struct WriteBound {
    /// When the whole message must be on the wire by.
    deadline: Instant,
    /// How long one write may wait for progress before the peer counts as
    /// wedged.
    stall: Duration,
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
    /// This entry point carries NO bound of its own: each fragment's write uses
    /// `std::io::Write::write_all` under whatever per-syscall write timeout the
    /// caller armed on the socket. Use [`Self::send_message_by`] when the WHOLE
    /// message must fit a total budget.
    ///
    /// # Errors
    ///
    /// Returns [`TransportError::ConcurrentAccess`] if another thread is
    /// concurrently sending on this stream, [`TransportError::InvalidFragment`]
    /// if the plaintext exceeds [`MAX_FRAME_SIZE`],
    /// [`TransportError::ConnectionClosed`]/I/O errors from the socket, or snow
    /// errors from encryption.
    pub fn send_message(&mut self, plaintext: &[u8]) -> Result<(), TransportError> {
        self.send_message_bounded(plaintext, None)
    }

    /// Send one encrypted message bounded by `deadline` in total and by `stall`
    /// on any single write.
    ///
    /// The difference from [`Self::send_message`] is the shape of the bounds.
    /// `deadline` covers the ENTIRE message — every fragment's write together —
    /// not a single syscall, so a large payload split across many fragments
    /// cannot take `fragments × timeout` to push. `stall` additionally caps how
    /// long one write may wait for progress, so a peer that has gone silent is
    /// noticed after `stall` rather than only when the whole-message budget
    /// expires.
    ///
    /// Both numbers are the CALLER's policy: the transport holds none of its
    /// own. It only applies what it is handed — via
    /// [`choreo_proto::write_all_with_stall`], which re-arms the socket's write
    /// stall to `min(deadline, now + stall) - now` before every syscall. A
    /// connection writer that sizes its budget by the message it is sending
    /// therefore decides both how patient to be with a slow link and how fast to
    /// give up on a wedged one.
    ///
    /// Do not mix the two entry points on one `NoiseStream`.
    /// [`choreo_proto::write_all_with_stall`] leaves the socket's write stall
    /// armed at the last value it computed and never clears it, so a later
    /// [`Self::send_message`] would inherit that leftover per-syscall timeout
    /// rather than the policy the caller intended. Pick one entry point for the
    /// life of the stream.
    ///
    /// # Errors
    ///
    /// As [`Self::send_message`] — including [`TransportError::ConcurrentAccess`]
    /// and [`TransportError::InvalidFragment`] — plus: a deadline that has already
    /// elapsed (or passes mid-message) and a write that accepts nothing for
    /// `stall` both make an armed write return `WouldBlock`/`TimedOut`, which
    /// converts into [`TransportError::Io`] through the `From<io::Error>` impl — a
    /// fired bound is reported as the socket's own error, not a bespoke variant.
    pub fn send_message_by(
        &mut self,
        plaintext: &[u8],
        deadline: Instant,
        stall: Duration,
    ) -> Result<(), TransportError> {
        self.send_message_bounded(plaintext, Some(WriteBound { deadline, stall }))
    }

    /// The shared entry point: take the single-writer slot, run the framing, and
    /// wipe `send_frag` on the way out. `bound` distinguishes the two public
    /// entry points; everything else is identical between them.
    fn send_message_bounded(
        &mut self,
        plaintext: &[u8],
        bound: Option<WriteBound>,
    ) -> Result<(), TransportError> {
        let _guard = self.send_slot.acquire()?;
        match self.send_message_inner(plaintext, bound) {
            // Wipe exactly the region this send wrote: the plaintext staging
            // buffer holds a copy of this (possibly secret) message until the
            // next send would overwrite it, so it is cleared on success too.
            // Wiping only the used prefix (rather than the whole `FRAG_BUF_LEN`
            // allocation, which `Vec::zeroize` would clear along with the
            // spare capacity) keeps the guarantee at a fraction of the cost and
            // leaves the buffer at full length for the next call. The
            // `Zeroizing` payload a typed caller holds is wiped independently
            // on its own drop.
            //
            // This cannot leave a PRIOR message's residue: `used` is the
            // furthest byte ANY fragment of THIS message staged (every fragment
            // stages from the front), so every byte this call wrote is inside
            // `..used`, and every earlier call zeroed its own, possibly larger,
            // extent. A smaller message after a bigger one therefore wipes its
            // own prefix over already-zero bytes — nothing stale survives beyond
            // it. Do not widen this to the whole allocation on the suspicion that
            // residue remains; there is none to wipe.
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
    fn send_message_inner(
        &mut self,
        plaintext: &[u8],
        bound: Option<WriteBound>,
    ) -> Result<usize, TransportError> {
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
            used = used.max(self.write_frame(chunk, more, bound)?);
        }

        if used == 0 {
            // `chunks` yields nothing for an empty payload, so the loop above
            // wrote nothing — but the peer's `recv_message` blocks on a length
            // prefix, so an empty message still needs exactly one wire frame.
            // Emit a single fragment carrying only the cleared continuation
            // header, which `recv_message` returns as an empty message.
            used = self.write_frame(&[], false, bound)?;
        }
        Ok(used)
    }

    /// Stage `payload` behind the authenticated continuation header, encrypt it
    /// under a brief `TransportState` lock, then write the 4-byte length prefix
    /// and the ciphertext as ONE write.
    ///
    /// Returns the number of `send_frag` bytes staged (header + payload) so the
    /// caller can wipe exactly that prefix.
    ///
    /// Serializing sends is the caller's job (the slot), so the
    /// `TransportState` lock is only needed to make the encrypt call atomic
    /// against the reader's decrypts. It must NEVER be held across the blocking
    /// write below: a reader waiting on the lock would stop draining the socket,
    /// and a peer doing the same in the other direction would deadlock once the
    /// socket buffers filled. The frame is therefore encrypted under a brief lock
    /// and written outside it, so the reader can always acquire the lock and
    /// drain.
    ///
    /// `bound` picks the write policy: `Some` bounds the whole message through
    /// [`choreo_proto::write_all_with_stall`] (re-arming the socket stall to the
    /// tighter of the remaining budget and the per-syscall stall before each
    /// syscall), while `None` keeps a plain `write_all` under whatever
    /// per-syscall timeout the caller armed on the socket.
    fn write_frame(
        &mut self,
        payload: &[u8],
        more: bool,
        bound: Option<WriteBound>,
    ) -> Result<usize, TransportError> {
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
        // ahead of the ciphertext, so a single write sends both (one syscall
        // per fragment instead of two).
        let prefix = encode_len(n)?;
        slice_mut(
            &mut self.send_buf,
            ..LEN_PREFIX_LEN,
            "send_buf length prefix",
        )?
        .copy_from_slice(&prefix);
        let frame = slice(&self.send_buf, ..LEN_PREFIX_LEN + n, "send_buf frame")?;
        match bound {
            // The caller owns both policy numbers for this message: the total
            // budget (so the fragments together fit it, not `fragments ×
            // timeout`) and the per-syscall stall (so a silent peer is noticed
            // long before the total budget of a large frame would expire).
            Some(bound) => choreo_proto::write_all_with_stall(
                &mut self.tcp,
                bound.deadline,
                bound.stall,
                frame,
            )?,
            // No bound: rely on whatever per-syscall write timeout the caller
            // armed on the socket (`write_all` under SO_SNDTIMEO).
            None => self.tcp.write_all(frame)?,
        }
        Ok(staged)
    }
}
