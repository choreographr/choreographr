//! The local-domain Unix-socket connection path.
//!
//! [`run_daemon_connection`] dials, then hands an established stream to
//! [`pump_connection`]; [`run_daemon_connection_with_autostart`] adds the
//! "start a daemon if nothing is listening" hook. [`run_daemon_reader`] is the
//! shared drain loop for a `BufRead` of framed daemon messages.
//!
//! Both directions are length-prefixed frames: the reader decodes
//! [`choreo_proto::read_message`], the writer encodes one whole frame and writes
//! it under the connection's time bounds. There is no `BufWriter` on the write
//! side — a frame is handed to the socket in one bounded write, so a buffered
//! layer would only add a flush that could silently swallow the error it was
//! meant to report.

use crate::error::ClientError;
use choreo_proto::{
    ClientMessage, DaemonMessage, ProtoError, UnixStream, WRITER_STALL, connect_unix, encode_frame,
    message_budget, read_message,
};
use crossbeam_channel::Receiver as CrossbeamReceiver;
use std::io::{BufRead, BufReader};
use std::net::Shutdown;
use std::time::Instant;

use super::serve::{CloseHandle, ConnectionSink, ConnectionSource, Inbound, serve, serve_source};

/// The inbound half of any length-prefixed message stream.
///
/// Generic over the reader so the same classification serves the connection path
/// (a `BufReader` over a socket) and the public [`run_daemon_reader`] (which
/// accepts any `BufRead` callers already hold).
struct FramedReader<R>(R);

impl<R: BufRead> ConnectionSource for FramedReader<R> {
    fn next_message(&mut self) -> Inbound {
        match read_message::<_, DaemonMessage>(&mut self.0) {
            Ok(message) => Inbound::Message(message),
            // Clean termination: the daemon closed its side of the connection.
            Err(ProtoError::Io(error))
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::ConnectionReset
                ) =>
            {
                Inbound::Closed
            }
            // Non-EOF I/O errors (broken pipe, connection aborted, etc.) are
            // fatal — the transport is gone.
            Err(ProtoError::Io(error)) => Inbound::Fatal(error.into()),
            // Protocol-level decode errors (Postcard, FrameTooLarge,
            // TrailingBytes, UnsupportedVersion) are per-message failures.
            // Because we use length-prefixed framing, a corrupt payload never
            // desynchronises the stream — log and carry on.
            Err(error) => {
                tracing::error!(%error, "skipping corrupt daemon message");
                Inbound::Skip
            }
        }
    }
}

impl ConnectionSink for UnixStream {
    fn send_message(&mut self, msg: &ClientMessage) -> Result<(), String> {
        // `encode_frame` yields the complete length-prefixed frame, so this is
        // one bounded write with nothing left to flush afterwards.
        let frame = encode_frame(msg).map_err(|error| error.to_string())?;
        let deadline = Instant::now() + message_budget(frame.len());
        choreo_proto::write_all_with_stall(self, deadline, WRITER_STALL, &frame)
            .map_err(|error| error.to_string())
    }

    fn close(&mut self) {
        let _ = self.shutdown(Shutdown::Both);
    }

    fn close_handle(&self) -> Option<CloseHandle> {
        // A duplicate closes the same connection; a failure to clone means the
        // socket is already unusable, in which case there is nothing to close.
        self.try_clone().ok().map(CloseHandle::Unix)
    }
}

/// Drain a `BufRead` of framed `DaemonMessages` until EOF.
///
/// Returns `Ok(())` when the stream ends cleanly (EOF / connection reset), and
/// carries on past a per-message decode failure — the framing is length-prefixed,
/// so a corrupt payload never desynchronises the stream.
///
/// # Errors
///
/// Returns [`ClientError`] when the stream ends on a non-EOF I/O error (broken
/// pipe, aborted connection) or a frame fails to decode fatally.
pub fn run_daemon_reader<R: BufRead>(
    reader: R,
    handle_daemon_message: impl FnMut(DaemonMessage),
) -> Result<(), ClientError> {
    serve_source(FramedReader(reader), handle_daemon_message)
}

/// Connect to a daemon over the Unix socket at `socket_path`, then serve the
/// established connection (see `pump_connection`).
///
/// # Errors
///
/// Returns [`ClientError`] if the unix socket cannot be connected, the client
/// end of the connection cannot be cloned, or the connection loop hits an I/O or
/// protocol error.
pub fn run_daemon_connection(
    socket_path: &str,
    handle_daemon_message: impl FnMut(DaemonMessage),
    from_ui: CrossbeamReceiver<ClientMessage>,
    shutdown_rx: Option<CrossbeamReceiver<()>>,
) -> Result<(), ClientError> {
    tracing::info!("connecting to daemon at {socket_path}");
    let stream = connect_unix(socket_path)?;
    pump_connection(stream, handle_daemon_message, from_ui, shutdown_rx)
}

/// Connect to the daemon at `socket_path`, starting one via `ensure_daemon` if
/// (and ONLY if) the initial dial itself finds nothing listening.
///
/// There is no pre-flight probe: the first [`connect_unix`] IS the real
/// connection attempt, and when the daemon is up it is used directly — the
/// stream is never thrown away. Only a dial failure classified as "nothing is
/// listening" (via [`choreo_proto::dial_error_means_no_listener`], shared
/// with the daemon side so the classification can never drift) invokes
/// `ensure_daemon`, after which the connection is retried. Any other dial
/// error is returned unchanged — autostart cannot fix a permission problem,
/// for example.
///
/// Race note: between the failed first dial and the post-autostart retry, a
/// third party could bind the socket, or an auto-exit daemon could be racing
/// its own shutdown. The retry dial therefore surfaces its errors verbatim —
/// the caller sees exactly what the retry saw, never a synthetic "autostart
/// failed".
///
/// # Errors
///
/// Returns the error from the first dial if it is classified as more than
/// "nothing listening", or the retry dial's error verbatim if the autostart
/// hook ran but the connection still failed.
pub fn run_daemon_connection_with_autostart(
    socket_path: &str,
    ensure_daemon: &mut dyn FnMut() -> Result<(), ClientError>,
    handle_daemon_message: impl FnMut(DaemonMessage),
    from_ui: CrossbeamReceiver<ClientMessage>,
    shutdown_rx: Option<CrossbeamReceiver<()>>,
) -> Result<(), ClientError> {
    tracing::info!("connecting to daemon at {socket_path}");
    let stream = match connect_unix(socket_path) {
        Ok(stream) => stream,
        Err(error) if choreo_proto::dial_error_means_no_listener(&error) => {
            tracing::info!(%error, "no daemon listening on the socket; requesting autostart");
            ensure_daemon()?;
            tracing::info!("autostart done; retrying connection to daemon at {socket_path}");
            connect_unix(socket_path)?
        }
        Err(error) => return Err(error.into()),
    };
    pump_connection(stream, handle_daemon_message, from_ui, shutdown_rx)
}

/// Drive an ESTABLISHED daemon connection: the reader takes a duplicate of the
/// stream, the stream itself becomes the connection's outbound sink, and
/// [`serve`] runs both halves until the connection ends. Split out of
/// [`run_daemon_connection`] so the autostart variant can hand over a stream it
/// already dialed (the successful first dial is never discarded).
fn pump_connection(
    stream: UnixStream,
    handle_daemon_message: impl FnMut(DaemonMessage),
    from_ui: CrossbeamReceiver<ClientMessage>,
    shutdown_rx: Option<CrossbeamReceiver<()>>,
) -> Result<(), ClientError> {
    let reader = FramedReader(BufReader::new(stream.try_clone()?));
    serve(reader, stream, handle_daemon_message, from_ui, shutdown_rx)
}
