//! The data plane's receive direction: reading length-prefixed fragments,
//! decrypting them, and reassembling the logical message from the
//! AUTHENTICATED continuation headers.

use std::io::{self, Read};
use std::net::TcpStream;

use choreo_proto::MAX_FRAME_SIZE;
use tracing::{debug, trace};
use zeroize::{Zeroize, Zeroizing};

use super::NoiseStream;
use super::frame::{self, FRAGMENT_CONTINUATION, FRAGMENT_HEADER_LEN, LEN_PREFIX_LEN, decode_len};
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

/// `read_exact` that classifies peer-close error kinds via
/// [`classify_stream_error`], so `recv_message` can surface a graceful
/// disconnect as [`TransportError::ConnectionClosed`].
fn read_exact_or_closed(stream: &mut TcpStream, buf: &mut [u8]) -> Result<(), TransportError> {
    stream.read_exact(buf).map_err(classify_stream_error)
}

impl NoiseStream {
    /// Receive a decrypted message.
    ///
    /// Reads one or more length-prefixed ciphertext fragments, decrypting
    /// each into a shared plaintext buffer, until a fragment whose
    /// AUTHENTICATED continuation header is clear arrives — that fragment
    /// terminates the logical message. If the peer closes the connection
    /// (EOF before a full frame, or a reset) `Err(TransportError::ConnectionClosed)`
    /// is returned — the read-loop callers treat that as a graceful
    /// disconnect, distinct from a protocol failure.
    ///
    /// The returned buffer is [`Zeroizing`]: a received frame may carry secret
    /// material (a credential riding `AddCredential`, for instance), so it is
    /// wiped when the caller drops it instead of being left in freed memory —
    /// the mirror of what [`choreo_proto::encode_payload`] guarantees for an
    /// outgoing frame, and of the `send_frag` wipe
    /// [`Self::send_message`] performs.
    ///
    /// # Errors
    ///
    /// Returns [`TransportError::ConnectionClosed`] on EOF before a complete
    /// message, [`TransportError::InvalidFragment`] on a frame exceeding the
    /// per-frame or per-message cap or a continuation-header violation, and I/O
    /// or snow errors otherwise.
    pub fn recv_message(&mut self) -> Result<Zeroizing<Vec<u8>>, TransportError> {
        let mut plaintext = Zeroizing::new(Vec::new());
        loop {
            // Read the raw frame bytes WITHOUT holding the TransportState
            // lock: snow tracks separate send and recv nonce counters, so an
            // interleaved send_message from another thread is safe, and a
            // sender can never be blocked by this reader. If recv_message
            // instead held the mutex while blocked on read_exact, a
            // concurrent sender (e.g. the daemon's shutdown-notification
            // thread sending ShuttingDown) would block forever on a reader
            // waiting for data that may never come — a shutdown deadlock.
            let mut len_buf = [0u8; LEN_PREFIX_LEN];
            read_exact_or_closed(&mut self.tcp, &mut len_buf)?;
            // The prefix is unauthenticated and is validated against snow's
            // per-message cap BEFORE anything is allocated for the body (see
            // `frame::decode_len`).
            let ct_len = decode_len(&len_buf)?;
            // Cumulative cap during reassembly: the codec's MAX_FRAME_SIZE is
            // the effective per-message limit, and enforcing it HERE rather
            // than after reassembly in decode_frame bounds memory for an
            // endless stream of continuation-marked fragments, each of which is
            // individually legal. See `frame::reassembly_exceeds_cap`.
            if frame::reassembly_exceeds_cap(plaintext.len(), ct_len) {
                return Err(TransportError::InvalidFragment(format!(
                    "reassembled message exceeds the {MAX_FRAME_SIZE}-byte limit"
                )));
            }
            // Reuse the per-stream ciphertext buffer: `resize` only adjusts
            // the length (growing if this fragment is the largest seen), so
            // capacity carries across messages — no per-fragment allocation.
            self.recv_ct_buf.resize(ct_len, 0);
            read_exact_or_closed(&mut self.tcp, &mut self.recv_ct_buf)?;

            // Take the lock briefly to decrypt this one fragment, then
            // release it before the next blocking read (see above).
            self.recv_pt_buf.resize(ct_len, 0);
            let n = self
                .transport
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .read_message(&self.recv_ct_buf, &mut self.recv_pt_buf)?;
            // Every fragment from this implementation carries the 1-byte
            // continuation header, so a zero-length plaintext is impossible
            // from a legitimate peer; guard the index below rather than panic
            // on a hostile or corrupted 16-byte ciphertext (a bare GCM tag).
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
            plaintext.extend_from_slice(slice(
                &self.recv_pt_buf,
                FRAGMENT_HEADER_LEN..n,
                "recv_pt_buf payload",
            )?);
            // Wipe the decrypted fragment now that its payload has been copied
            // out: the reusable buffer would otherwise hold the last received
            // message's plaintext until the next fragment overwrites it, which
            // is the receive-side counterpart of the `send_frag` wipe (a
            // received frame may carry a credential). Only the decrypted extent
            // is wiped, so the buffer keeps its length for the next fragment.
            slice_mut(&mut self.recv_pt_buf, ..n, "recv_pt_buf plaintext")?.zeroize();

            if !more {
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
