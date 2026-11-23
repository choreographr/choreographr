//! Shared Noise-over-TCP plumbing: the client keypair, handshake-failure
//! classification, the IK dial-and-serve wrapper, and the established-stream
//! service loop both TCP connection modes run.

use crate::error::ClientError;
use choreo_proto::{ClientMessage, DaemonMessage, WRITER_STALL, encode_payload, message_budget};
use choreo_transport::error::TransportError;
use choreo_transport::handshake::handshake_initiator;
use choreo_transport::key::ensure_transport_keypair;
use choreo_transport::noise::NoiseStream;
use crossbeam_channel::Receiver as CrossbeamReceiver;
use std::net::Shutdown;
use std::time::Instant;

use super::serve::{CloseHandle, ConnectionSink, ConnectionSource, Inbound, serve};

/// This process's client transport keypair, generating one on first use.
///
/// Every connect path needs the same identity (the daemon's ACL judges it) and
/// the same failure mapping, so both live here instead of in each site. The
/// `io::Error` shape is deliberate: the callers wrap it in their own error
/// variant — [`ClientError::Io`] for a session, `PreflightError::Identity`
/// for the preflight — without repeating the conversion.
pub(super) fn client_transport_keypair()
-> Result<(choreo_transport::key::TransportSecretKey, [u8; 32]), std::io::Error> {
    ensure_transport_keypair().map_err(std::io::Error::other)
}

/// Whether a handshake failure is a REFUSAL — the daemon would not continue
/// with this client — as opposed to the connection merely failing.
///
/// The distinction drives user-facing advice ("your key may not be enrolled",
/// "the pinned server key may have changed"), so it must not be guessed from
/// "the handshake did not complete". These count:
///
/// - the responder-side refusals, [`TransportError::AuthFailed`] (the ACL check
///   rejected this client's key), [`TransportError::Noise`] (the exchange did not
///   authenticate), [`TransportError::HandshakeModeMismatch`] /
///   [`TransportError::HandshakeModeUnknown`] (the peer does not speak the
///   handshake asked for), and [`TransportError::InvalidFragment`] (the peer
///   violated the framing contract mid-handshake);
/// - an EOF-class [`TransportError::Io`], because that is the only shape a
///   refusal takes at an INITIATOR. The responder refuses by aborting the
///   exchange and closing the socket — the Noise protocol carries no in-band
///   "rejected" message, and telling an unenrolled peer which check it failed
///   would be an oracle — so the initiator's next handshake read simply hits EOF.
///   `UnexpectedEof` is a clean close and `ConnectionReset` an abortive one; both
///   mean the peer declined to finish, and without them the enrollment and
///   re-pair guidance could never fire.
///
/// Everything else is NOT a refusal: [`TransportError::HandshakeTimeout`] (the
/// peer is reachable but slow — a timeout, not a verdict), any other I/O kind
/// (broken pipe, aborted socket), [`TransportError::ConnectionClosed`], a
/// [`TransportError::Protocol`] framing error, and the local key-file variants.
pub(super) fn is_handshake_refusal(error: &TransportError) -> bool {
    match error {
        TransportError::AuthFailed
        | TransportError::Noise(_)
        | TransportError::HandshakeModeMismatch { .. }
        | TransportError::HandshakeModeUnknown { .. }
        | TransportError::InvalidFragment(_) => true,
        TransportError::Io(io) => matches!(
            io.kind(),
            std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::ConnectionReset
        ),
        _ => false,
    }
}

/// Map a handshake failure to the [`ClientError`] that preserves its meaning.
///
/// A refusal keeps today's `ConnectionRefused` shape, because the callers key
/// their remediation advice on exactly that kind: the dial is a separate step on
/// every TCP path, so a `ConnectionRefused` coming back from a handshake can only
/// mean the handshake itself was refused — which is where the pinned-mode
/// re-pair guidance belongs. Everything else keeps its own meaning instead of
/// being flattened into a refusal: a handshake timeout is `TimedOut`, a socket
/// failure is that failure, a peer close is `ConnectionAborted`, and a wire-codec
/// error keeps [`ClientError::Proto`].
pub(super) fn handshake_error(error: TransportError) -> ClientError {
    use std::io::ErrorKind;

    if is_handshake_refusal(&error) {
        return ClientError::Io(std::io::Error::new(ErrorKind::ConnectionRefused, error));
    }

    match error {
        // The whole unauthenticated phase ran out of its budget: the peer is
        // reachable but never finished, which is a timeout, not a refusal.
        TransportError::HandshakeTimeout => ClientError::Io(std::io::Error::new(
            ErrorKind::TimedOut,
            TransportError::HandshakeTimeout,
        )),
        // A socket failure keeps its own kind and payload.
        TransportError::Io(io) => ClientError::Io(io),
        // The peer closed mid-handshake without refusing anything.
        closed @ TransportError::ConnectionClosed => {
            ClientError::Io(std::io::Error::new(ErrorKind::ConnectionAborted, closed))
        }
        // A framing/decode error is the codec's own verdict, so it keeps its
        // dedicated variant.
        TransportError::Protocol(proto) => ClientError::Proto(proto),
        // Anything else (the remaining responder-side refusals are already
        // handled above, so this is the local key-file variants) is preserved
        // verbatim without inventing a refusal.
        other => ClientError::Io(std::io::Error::other(other)),
    }
}

/// Noise IK handshake over an ALREADY-DIALED TCP stream, then the encrypted
/// session loop. Shared by [`super::run_daemon_tcp_connection`] and
/// [`super::run_daemon_tcp_connection_pinned`] so both paths run the identical
/// handshake sequence. A handshake REFUSAL surfaces as `ConnectionRefused` (see
/// [`handshake_error`]) — which is why the caller must dial separately: only
/// then does a `ConnectionRefused` returned from here unambiguously mean the
/// HANDSHAKE was refused, not the network.
pub(super) fn ik_handshake_and_serve(
    tcp: std::net::TcpStream,
    client_sk: &[u8; 32],
    server_pk: &[u8; 32],
    handle_daemon_message: impl FnMut(DaemonMessage),
    from_ui: CrossbeamReceiver<ClientMessage>,
    shutdown_rx: Option<CrossbeamReceiver<()>>,
) -> Result<(), ClientError> {
    // The IK initiator writes the 1-byte mode preamble as its first step (see
    // `handshake_initiator`); the raw `TransportError` is classified here, one
    // layer above the dial.
    let noise = handshake_initiator(tcp, client_sk, server_pk).map_err(handshake_error)?;
    serve_noise_connection(noise, handle_daemon_message, from_ui, shutdown_rx)
}

/// The encrypted session loop over an already-established [`NoiseStream`].
///
/// The transport only carries bytes; the connection decides the write policy.
/// Both halves run through [`serve`], so this mode has exactly the same
/// drain-before-stop, failed-write teardown, and shutdown semantics as every
/// other transport:
/// - Reader (the calling thread): blocks on `recv_daemon_message`; a clean peer
///   close ends the loop, any other transport error is fatal.
/// - Writer thread: `select_biased!` on `from_ui` (drained first) and the
///   internal stop channel, each message written under [`WRITER_STALL`] and the
///   size-scaled [`message_budget`]. A failed write shuts the socket down, which
///   unblocks the reader.
/// - Shutdown: blocks on `shutdown_rx.recv()`, then shuts the TCP stream down.
///
/// Extracted so [`super::run_daemon_tcp_connection`] (IK) and
/// [`super::run_daemon_tcp_connection_xx_first_contact`] (XX) share one
/// writer/reader implementation — the two modes differ ONLY in preamble +
/// handshake, not in how the established transport is served.
pub(super) fn serve_noise_connection(
    noise: NoiseStream,
    handle_daemon_message: impl FnMut(DaemonMessage),
    from_ui: CrossbeamReceiver<ClientMessage>,
    shutdown_rx: Option<CrossbeamReceiver<()>>,
) -> Result<(), ClientError> {
    // The reader keeps the original stream; the writer gets a clone. Both share
    // the `TransportState` and the per-direction slots, so this is the same
    // one-reader/one-writer split every other transport uses.
    let writer = noise.try_clone().map_err(ClientError::Io)?;
    serve(noise, writer, handle_daemon_message, from_ui, shutdown_rx)
}

impl ConnectionSource for NoiseStream {
    fn next_message(&mut self) -> Inbound {
        match self.recv_daemon_message() {
            Ok(message) => Inbound::Message(message),
            // `recv_daemon_message` already maps the EOF-class read errors to
            // `ConnectionClosed`, so that variant alone is the clean peer close.
            Err(TransportError::ConnectionClosed) => Inbound::Closed,
            // Every other transport error kills the stream: unlike the unix
            // path there is no per-message recoverable failure here (a decode
            // error is a protocol error on an encrypted stream that is no longer
            // trustworthy).
            Err(error) => Inbound::Fatal(ClientError::Io(std::io::Error::other(error))),
        }
    }
}

impl ConnectionSink for NoiseStream {
    fn send_message(&mut self, message: &ClientMessage) -> Result<(), String> {
        // The encrypted stream writes its own length-prefixed fragments, so it
        // takes the payload rather than a pre-framed message. Both time bounds
        // are the connection's: a silent peer is noticed after `WRITER_STALL`,
        // while a large frame is allowed the time its size implies.
        let payload = encode_payload(message).map_err(|error| error.to_string())?;
        let deadline = Instant::now() + message_budget(payload.len());
        self.send_message_by(&payload, deadline, WRITER_STALL)
            .map_err(|error| error.to_string())
    }

    fn close(&mut self) {
        let _ = self.get_ref().shutdown(Shutdown::Both);
    }

    fn close_handle(&self) -> Option<CloseHandle> {
        self.get_ref().try_clone().ok().map(CloseHandle::Tcp)
    }
}
