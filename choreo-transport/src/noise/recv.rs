//! The data plane's receive direction: reading length-prefixed fragments,
//! decrypting them, and reassembling the logical message from the
//! AUTHENTICATED continuation headers.

use std::io::{self, Read};
use std::net::TcpStream;

use tracing::{debug, trace};
use zeroize::{Zeroize, Zeroizing};

use super::NoiseStream;
use super::frame::{FRAGMENT_CONTINUATION, FRAGMENT_HEADER_LEN, LEN_PREFIX_LEN, decode_len};
use super::reassembly::Reassembly;
use crate::buf::{at, slice, slice_mut};
use crate::error::TransportError;

/// Classify a raw stream error: the EOF-class kinds that mean "the peer
/// closed the connection" become [`TransportError::ConnectionClosed`] instead
/// of a raw `Io(UnexpectedEof)`. `read_exact` surfaces a graceful close as
/// `UnexpectedEof` (EOF before the buffer filled) and a reset/abort as
/// `ConnectionReset`/`BrokenPipe`; all mean the peer is gone, which the
/// daemon and client read loops want to distinguish from a protocol failure
/// (they log a graceful disconnect and run cleanup). The match arms that
/// branch on [`TransportError::ConnectionClosed`] depend on this
/// classification.
fn classify_stream_error(e: io::Error) -> TransportError {
    match e.kind() {
        io::ErrorKind::UnexpectedEof
        | io::ErrorKind::ConnectionReset
        | io::ErrorKind::ConnectionAborted
        | io::ErrorKind::BrokenPipe => {
            debug!("Noise stream closed by peer");
            TransportError::ConnectionClosed
        }
        _ => e.into(),
    }
}

/// Read **some** bytes of the next message with no time bound at all, returning how
/// many arrived.
///
/// This is the transport's one unbounded read, and it is deliberate: it is where a
/// connection legitimately sits when the peer has nothing to say — an idle client, a
/// front-end waiting on a turn — so a bound must start at the first byte that
/// arrives, not before it. Bounding this read is exactly the bug that would kill
/// every idle connection.
///
/// It exists as its own step because the socket's read timeout is sticky (`std` has
/// no way to clear it other than setting another), so "unbounded from here until the
/// first byte" cannot be expressed by passing a deadline to the bounded helper.
///
/// `0` bytes means the peer closed cleanly between messages, reported as
/// [`TransportError::ConnectionClosed`]. The caller must pass a non-empty buffer.
fn read_some_or_closed(stream: &mut TcpStream, buf: &mut [u8]) -> Result<usize, TransportError> {
    debug_assert!(
        !buf.is_empty(),
        "the unbounded first read needs a byte to read into"
    );
    loop {
        match stream.read(buf) {
            Ok(0) => {
                debug!("Noise stream closed by peer");
                return Err(TransportError::ConnectionClosed);
            }
            Ok(n) => return Ok(n),
            // A signal interrupting the syscall says nothing about the peer.
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(classify_stream_error(e)),
        }
    }
}

impl NoiseStream {
    /// Receive a decrypted message into a `Zeroizing` buffer.
    ///
    /// The returned buffer is [`Zeroizing`]: a received frame may carry secret
    /// material (a credential riding `AddCredential`, for instance), so it is
    /// wiped when the caller drops it instead of being left in freed memory —
    /// the mirror of what [`choreo_proto::encode_payload`] guarantees for an
    /// outgoing frame, and of the `send_frag` wipe [`Self::send_message`]
    /// performs. This covers the RAW FRAME buffer; the typed helpers
    /// ([`Self::recv_daemon_message`] / [`Self::recv_client_message`]) decode it
    /// into an owned message whose `String`s are the caller's to drop —
    /// zeroizing a deserialized value is out of scope (see the crate docs on the
    /// typed path's much narrower wipe policy for a known non-secret variant).
    ///
    /// # Errors
    ///
    /// As `recv_message_plain`.
    pub fn recv_message(&mut self) -> Result<Zeroizing<Vec<u8>>, TransportError> {
        Ok(Zeroizing::new(self.recv_message_plain()?))
    }

    /// Receive a decrypted message into a plain buffer the caller owns.
    ///
    /// The receive is bounded in **two phases**, and the split is what keeps an idle
    /// connection alive:
    ///
    /// 1. **Waiting for the next message is unbounded.** A connection with nothing in
    ///    flight is idle, not broken — a front-end waiting on a turn, a client left
    ///    open overnight — so the read timeout is cleared and the first byte is read
    ///    with no bound at all. A vanished peer is caught by the connection's TCP
    ///    keepalive, not here.
    /// 2. **A message in flight is bounded by a per-syscall stall** (see
    ///    [`NoiseStream::read_stall`]): every read after that first byte fails if it
    ///    makes no progress for the stall. There is deliberately no wall clock on the
    ///    whole message: a large payload on a slow link legitimately takes minutes,
    ///    and a bound on total time would kill exactly the client the transport is
    ///    meant to serve.
    ///
    /// Reads one or more length-prefixed ciphertext fragments, decrypting
    /// each into a shared plaintext buffer, until a fragment whose
    /// AUTHENTICATED continuation header is clear arrives — that fragment
    /// terminates the logical message. If the peer closes the connection
    /// (EOF before a full frame, or a reset) `Err(TransportError::ConnectionClosed)`
    /// is returned — the read-loop callers treat that as a graceful
    /// disconnect, distinct from a protocol failure.
    ///
    /// The reassembly buffer is `Zeroizing` for the DURATION of the receive, so
    /// an error mid-message (a GCM failure on a later fragment, a cap breach)
    /// wipes the partial plaintext rather than dropping it unwiped. On success
    /// the buffer is moved out un-wiped, so the caller decides whether to clear
    /// it: [`Self::recv_message`] wraps it in `Zeroizing`, and the typed receive
    /// helper clears it only when the decoded variant may carry a secret — the
    /// bulk non-secret `Image` reply skips the memset (see
    /// [`Self::recv_daemon_message`]).
    ///
    /// # Errors
    ///
    /// Returns [`TransportError::ConcurrentAccess`] if another thread is
    /// concurrently receiving on this stream, [`TransportError::ConnectionClosed`]
    /// on EOF before a complete message, [`TransportError::Io`] (a `TimedOut`) when
    /// the peer stalls mid-message for longer than the stall, and
    /// [`TransportError::InvalidFragment`] on a
    /// frame exceeding the per-frame cap or below the tag-plus-header minimum, the
    /// per-message byte cap or the fragment-count bound, or a continuation-header
    /// violation, plus any other I/O or snow errors.
    pub(super) fn recv_message_plain(&mut self) -> Result<Vec<u8>, TransportError> {
        // The inbound single-reader right: two concurrent receives on one
        // connection would splice two logical messages together with no error
        // at all, so the slot turns that misuse into a protocol error — the
        // mirror of what `send_message` takes for the outbound direction.
        let _guard = self.recv_slot.acquire()?;

        // Phase 1 — unbounded. The previous message left the socket's read timeout
        // armed at the last stall the bounded reads computed, and that option is
        // sticky, so it must be cleared before the wait for the next message or this
        // "idle" read would inherit a five-second bound and tear down a healthy idle
        // connection. This is the receive-side counterpart of the send path's rule
        // that one entry point owns the socket's bounds.
        self.tcp.set_read_timeout(None)?;
        let mut len_buf = [0u8; LEN_PREFIX_LEN];
        // The one read with no bound: whatever arrives next starts a message. `0`
        // bytes here is the peer closing between messages.
        let mut prefix_filled = read_some_or_closed(&mut self.tcp, &mut len_buf)?;

        // Phase 2 — bounded by the stall from here on.
        let mut assembly = Reassembly::new();
        loop {
            // Read the raw frame bytes WITHOUT holding the TransportState
            // lock: snow tracks separate send and recv byte counters, so an
            // interleaved send_message from another thread is safe, and a
            // sender can never be blocked by this reader. If recv_message
            // instead held the mutex while blocked on a read, the
            // connection's own writer thread (which sends everything, including
            // ShuttingDown, for this connection) would block forever on a reader
            // waiting for data that may never come — a shutdown deadlock.
            //
            // The first iteration may already hold part of the prefix from the
            // unbounded read above; later fragments read their whole prefix here.
            choreo_proto::read_exact_with_stall(
                &mut self.tcp,
                self.read_stall,
                slice_mut(&mut len_buf, prefix_filled.., "length prefix tail")?,
            )
            .map_err(classify_stream_error)?;
            prefix_filled = 0;
            // The prefix is unauthenticated and is validated against snow's
            // per-message cap BEFORE anything is allocated for the body (see
            // `frame::decode_len`).
            let ct_len = decode_len(&len_buf)?;
            // Cumulative cap during reassembly: the codec's MAX_FRAME_SIZE is
            // the effective per-message limit, and enforcing it HERE — before
            // this frame's body is read or the buffer grows — bounds memory for
            // an endless stream of continuation-marked fragments, each of which
            // is individually legal. `accept_frame` mints the token `extend`
            // requires, so a fragment can never be appended unchecked.
            let accepted = assembly.accept_frame(ct_len)?;
            // Reuse the per-stream ciphertext buffer: `resize` only adjusts
            // the length (growing if this fragment is the largest seen), so
            // capacity carries across messages — no per-fragment allocation.
            self.recv_ct_buf.resize(ct_len, 0);
            choreo_proto::read_exact_with_stall(
                &mut self.tcp,
                self.read_stall,
                &mut self.recv_ct_buf,
            )
            .map_err(classify_stream_error)?;

            // Take the lock briefly to decrypt this one fragment, then
            // release it before the next blocking read (see above).
            self.recv_pt_buf.resize(ct_len, 0);
            let n = self
                .transport
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .read_message(&self.recv_ct_buf, &mut self.recv_pt_buf)?;
            // Every fragment from this implementation carries the 1-byte
            // continuation header, so a zero-length plaintext is impossible from
            // a legitimate peer; `decode_len` already refuses a ciphertext below
            // tag + header, so `n >= 1` for any frame that reaches here. The guard
            // is kept as defence in depth — it keeps the index below panic-free
            // even if that minimum bound were ever loosened.
            if n == 0 {
                return Err(TransportError::InvalidFragment(
                    "fragment carries no authenticated continuation header".to_string(),
                ));
            }
            // The continuation flag is read from the AUTHENTICATED plaintext
            // (the GCM tag covers it), so a wire-level tamper with the length
            // prefix cannot change the reassembly decision — see the prefix
            // validation above. `n >= 1` held from the guard above, so both
            // accesses are in bounds; bounds-checked anyway (slice helpers).
            let more = (*at(&self.recv_pt_buf, 0, "recv_pt_buf continuation header")?
                & FRAGMENT_CONTINUATION)
                != 0;
            assembly.extend(
                accepted,
                slice(
                    &self.recv_pt_buf,
                    FRAGMENT_HEADER_LEN..n,
                    "recv_pt_buf payload",
                )?,
            );
            // Wipe the decrypted fragment now that its payload has been copied
            // out: the reusable buffer would otherwise hold the last received
            // message's plaintext until the next fragment overwrites it, which
            // is the receive-side counterpart of the `send_frag` wipe (a
            // received frame may carry a credential). Only the decrypted extent
            // is wiped, so the buffer keeps its length for the next fragment.
            //
            // `..n` is the complete secret extent, not a shortcut: `read_message`
            // writes only the `n` decrypted bytes at the front, so `recv_pt_buf[n..]`
            // is never plaintext — it is zero from the `resize` that grew the
            // buffer, or was zeroed by an earlier fragment's wipe. A wider wipe
            // would only memset zeros.
            slice_mut(&mut self.recv_pt_buf, ..n, "recv_pt_buf plaintext")?.zeroize();

            if !more {
                // Move the buffer out un-wiped: the caller chooses whether to
                // clear it (`recv_message` wraps it in `Zeroizing`; the typed
                // path clears it only for a secret-bearing variant). The
                // `Zeroizing` guard the assembly used is left holding an empty
                // `Vec`, so its drop zeroizes nothing.
                let plaintext = assembly.into_plaintext();
                trace!(message_len = plaintext.len(), "received Noise message");
                return Ok(plaintext);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// EOF-class read errors are classified as `TransportError::ConnectionClosed`
    /// (a graceful peer disconnect) rather than a raw `Io` error — the daemon's
    /// and client's read loops branch on that variant to log a graceful
    /// disconnect and run cleanup. Pure mapping test: no sockets, no sleeps.
    #[test]
    fn eof_class_read_errors_map_to_connection_closed() {
        use std::io::ErrorKind;
        for kind in [
            ErrorKind::UnexpectedEof,
            ErrorKind::ConnectionReset,
            ErrorKind::ConnectionAborted,
            ErrorKind::BrokenPipe,
        ] {
            let err = io::Error::new(kind, "peer gone");
            assert!(
                matches!(classify_stream_error(err), TransportError::ConnectionClosed),
                "{kind:?} must classify as ConnectionClosed"
            );
        }
        // A non-EOF error stays a raw Io error, not a graceful disconnect.
        let other = io::Error::new(io::ErrorKind::PermissionDenied, "nope");
        assert!(matches!(
            classify_stream_error(other),
            TransportError::Io(_)
        ));
    }
}
