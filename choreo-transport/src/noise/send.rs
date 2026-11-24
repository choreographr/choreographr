//! The data plane's send direction: fragmenting, encrypting and framing an
//! outbound message.
//!
//! Every write goes through one place — [`NoiseStream::write_frame`] — so the
//! continuation header, the brief `TransportState` lock, the length prefix and
//! the coalesced write exist exactly once, whether the message is empty, a single
//! fragment, or many. Two entry points share that body:
//! [`NoiseStream::send_message`] (the connection's own policy: a whole-message
//! budget derived from the wire bytes the message will occupy, plus the
//! connection's stall) and [`NoiseStream::send_message_by`] (the same stall, with
//! the caller's absolute deadline replacing the derived budget). The single-writer
//! right itself lives in [`super::slot`], shared with the read direction.
//!
//! Neither entry point ever writes a frame unbounded: every frame is handed to
//! [`choreo_proto::write_all_with_stall`], so no send path can leave the socket
//! under a policy other than the one it just armed.
//!
//! The connection's stall is read ONCE per message, at the shared entry point, and
//! threaded down to the framing loop — so the shared record is locked once per
//! message rather than once per syscall, and never while a write is in progress.
//! The same entry point is where a connection already poisoned by a failed transfer
//! is refused, before any byte reaches the socket.

use std::time::{Duration, Instant};

use choreo_proto::{MAX_FRAME_SIZE, message_budget};
use tracing::trace;
use zeroize::Zeroize;

use super::NoiseStream;
use super::frame::{
    FRAG_BUF_LEN, FRAGMENT_CONTINUATION, FRAGMENT_HEADER_LEN, FRAME_BUF_LEN, LEN_PREFIX_LEN,
    MAX_PLAINTEXT_CHUNK, encode_len, wire_len,
};
use crate::buf::{at_mut, slice, slice_mut};
use crate::error::TransportError;

/// The policy one outbound message is written under: an absolute deadline for the
/// whole message and the connection's stall for any single write.
///
/// Kept as one value, built once per message, so the framing loop threads a single
/// `&WriteBound` through and cannot thread one bound while forgetting the other.
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
    /// encrypt call, and is NEVER held across the blocking writes —
    /// the reader must always be able to acquire it (briefly) and drain the
    /// socket, or two peers sending large messages concurrently would
    /// deadlock once the socket buffers filled. A runtime single-writer guard
    /// (the stream's `send_slot`) rejects a concurrent second `send_message`
    /// with a protocol error rather than interleaving fragments.
    ///
    /// The ciphertext is encrypted into the stream's reused `send_buf` field
    /// — never allocated per call — and each frame is handed to ONE bounded
    /// write (4-byte prefix + ciphertext together, so a fragment is one
    /// bounded write rather than two). The staged plaintext in `send_frag` is
    /// wiped before this returns (see the callers of `write_frame`).
    ///
    /// The write is bounded by the connection's policy in both directions: a
    /// whole-message budget derived from the bytes this message will occupy on the
    /// wire (see `frame::wire_len`) through [`choreo_proto::message_budget`], plus
    /// the connection's per-syscall stall ([`Self::set_stall`]). A peer that stops
    /// accepting bytes is therefore noticed after the stall, while a slow-but-alive
    /// link gets the time the message's own size implies.
    ///
    /// Use [`Self::send_message_by`] only when the caller has a whole-message
    /// budget of its own to impose.
    ///
    /// # Errors
    ///
    /// Returns [`TransportError::ConcurrentAccess`] if another thread is
    /// concurrently sending on this stream, [`TransportError::StreamUnusable`] if a
    /// previous transfer failed on this connection, [`TransportError::InternalInvariant`]
    /// if the plaintext exceeds [`MAX_FRAME_SIZE`] (a caller asking for more than
    /// the codec's message cap — never a peer's doing, so it is kept out of
    /// [`TransportError::InvalidFragment`]),
    /// [`TransportError::ConnectionClosed`]/I/O errors from the socket, or snow
    /// errors from encryption. Every error except the three pre-flight refusals
    /// leaves the connection unusable.
    pub fn send_message(&mut self, plaintext: &[u8]) -> Result<(), TransportError> {
        self.send_message_bounded(plaintext, None)
    }

    /// Send one encrypted message whose whole-message budget is `deadline`.
    ///
    /// `deadline` replaces ONLY the total budget — the per-syscall stall stays the
    /// connection's own ([`Self::set_stall`]), so a peer that has gone silent is
    /// noticed after the stall rather than only when the whole-message budget
    /// expires. The deadline covers the ENTIRE message — every fragment's write
    /// together — not a single syscall, so a large payload split across many
    /// fragments cannot take `fragments × timeout` to push.
    ///
    /// [`Self::send_message`] is the normal entry point: it derives that budget
    /// from the message's wire length, which is what a writer that has no budget of
    /// its own wants. This one exists for a caller that does have one (an enclosing
    /// operation's deadline). The two may be mixed freely on one stream: both apply
    /// the same stall and arm the socket themselves before writing, so neither
    /// leaves the other a socket timeout to inherit.
    ///
    /// # Errors
    ///
    /// As [`Self::send_message`] — including [`TransportError::ConcurrentAccess`]
    /// and [`TransportError::InternalInvariant`] — plus: a deadline that has already
    /// elapsed (or passes mid-message) and a write that accepts nothing for the
    /// connection's stall both make an armed write return `WouldBlock`/`TimedOut`,
    /// which converts into [`TransportError::Io`] through the `From<io::Error>` impl
    /// — a fired bound is reported as the socket's own error, not a bespoke variant.
    /// A fired bound also poisons the connection: an elapsed deadline fails before
    /// its first syscall, and no error proves where the byte stream stands.
    pub fn send_message_by(
        &mut self,
        plaintext: &[u8],
        deadline: Instant,
    ) -> Result<(), TransportError> {
        self.send_message_bounded(plaintext, Some(deadline))
    }

    /// The shared entry point: take the single-writer slot, apply the two local
    /// pre-flight refusals, run the framing, and wipe `send_frag` on the way out.
    /// `deadline` distinguishes the two public entry points (`None` means "derive
    /// the budget from the wire length"); everything else is identical between
    /// them.
    fn send_message_bounded(
        &mut self,
        plaintext: &[u8],
        deadline: Option<Instant>,
    ) -> Result<(), TransportError> {
        let _guard = self.send_slot.acquire()?;

        // Mirror the receiver's reassembly cap so an over-limit message is
        // rejected at the source instead of by the peer's recv_message. This is a
        // LOCAL policy violation (a caller handed us more than this transport can
        // frame), so it is `InternalInvariant`, not `InvalidFragment`: the peer did
        // nothing, and a caller that reads it as a wire violation would be
        // misdiagnosing its own bug as the remote end's. It runs before any budget
        // arithmetic, so an over-limit caller never earns a budget for a message
        // that cannot be framed — and before the unusable check below, so a
        // caller-side limit breach never poisons a connection whose wire was never
        // touched.
        if plaintext.len() > MAX_FRAME_SIZE {
            return Err(TransportError::InternalInvariant(format!(
                "outgoing message of {} bytes exceeds the {MAX_FRAME_SIZE}-byte cap",
                plaintext.len()
            )));
        }

        // A previous failed transfer left the byte stream at an unknown offset, so
        // nothing more may be written: this returns before the socket is touched,
        // which is also why it cannot re-poison (the flag is already set) and why
        // the `ConcurrentAccess` refusal above poisons nothing either (the slot is
        // taken before any byte moves).
        if self.is_unusable() {
            return Err(TransportError::StreamUnusable);
        }

        // ONE read of the connection's stall per message, threaded down to the
        // framing loop rather than re-locking the shared record per frame.
        let stall = self.stall();
        match self.send_message_inner(plaintext, deadline, stall) {
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
                // ONE place per direction poisons the connection: every error the
                // framing body can return means the frame it was writing may be
                // partly on the wire, so the byte stream is at an unknown offset and
                // no later send or receive may use it.
                self.mark_unusable();
                Err(e)
            }
        }
    }

    /// The send body, returning how many bytes of `send_frag` the last frame
    /// staged (its plaintext varies in length with the payload) so the caller
    /// can wipe exactly that prefix.
    ///
    /// `stall` is the connection's per-syscall stall, read once by the caller so
    /// this loop never re-locks the shared record.
    fn send_message_inner(
        &mut self,
        plaintext: &[u8],
        deadline: Option<Instant>,
        stall: Duration,
    ) -> Result<usize, TransportError> {
        trace!(plaintext_len = plaintext.len(), "sending Noise message");

        // ONE policy value for the whole message, built here so every fragment is
        // written under the same pair of bounds. The budget comes from the wire
        // length — the plaintext plus the framing this send is about to add — so
        // the floor rate describes the bytes that actually cross the link; a
        // caller-supplied deadline overrides only that total, never the stall,
        // which stays the connection's own.
        let bound = WriteBound {
            deadline: deadline
                .unwrap_or_else(|| Instant::now() + message_budget(wire_len(plaintext.len()))),
            stall,
        };

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
            used = used.max(self.write_frame(chunk, more, &bound)?);
        }

        if used == 0 {
            // `chunks` yields nothing for an empty payload, so the loop above
            // wrote nothing — but the peer's `recv_message` blocks on a length
            // prefix, so an empty message still needs exactly one wire frame.
            // Emit a single fragment carrying only the cleared continuation
            // header, which `recv_message` returns as an empty message.
            used = self.write_frame(&[], false, &bound)?;
        }
        Ok(used)
    }

    /// Stage `payload` behind the authenticated continuation header, encrypt it
    /// under a brief `TransportState` lock, then write the 4-byte length prefix
    /// and the ciphertext together as one bounded write.
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
    /// `bound` is this message's policy (see [`WriteBound`]): every frame is
    /// written through [`choreo_proto::write_all_with_stall`], which re-arms the
    /// socket's write stall to the tighter of the remaining whole-message budget
    /// and the connection's stall before each syscall. There is deliberately no
    /// unbounded arm — a frame must never leave the socket under a policy other
    /// than the one just armed, so no later send can inherit a leftover timeout.
    fn write_frame(
        &mut self,
        payload: &[u8],
        more: bool,
        bound: &WriteBound,
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
            let mut shared = self
                .shared
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            shared.transport.write_message(
                slice(&self.send_frag, ..staged, "send_frag header+chunk")?,
                // Ciphertext goes AFTER the length-prefix headroom; the prefix
                // is laid into that headroom afterwards — encrypting over the
                // whole buffer would let the prefix overwrite the first
                // ciphertext bytes.
                slice_mut(&mut self.send_buf, LEN_PREFIX_LEN.., "send_buf ciphertext")?,
            )?
        };

        // One coalesced frame per fragment: the prefix is laid into the headroom
        // ahead of the ciphertext, so both leave in the same bounded write rather
        // than the prefix costing a second one.
        let prefix = encode_len(n)?;
        slice_mut(
            &mut self.send_buf,
            ..LEN_PREFIX_LEN,
            "send_buf length prefix",
        )?
        .copy_from_slice(&prefix);
        let frame = slice(&self.send_buf, ..LEN_PREFIX_LEN + n, "send_buf frame")?;
        // One arm covers both bounds: the whole-message budget (so the fragments
        // together fit it, not `fragments × timeout`) and the connection's stall
        // (so a silent peer is noticed long before the total budget of a large
        // frame would expire).
        choreo_proto::write_all_with_stall(&mut self.tcp, bound.deadline, bound.stall, frame)?;
        Ok(staged)
    }
}
