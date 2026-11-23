//! The local-domain Unix-socket connection path.
//!
//! [`run_daemon_connection`] dials, then hands an established stream to
//! [`pump_connection`]; [`run_daemon_connection_with_autostart`] adds the
//! "start a daemon if nothing is listening" hook. [`run_daemon_reader`] is the
//! shared drain loop for a `BufRead` of framed daemon messages.

use crate::error::ClientError;
use choreo_proto::{
    ClientMessage, DaemonMessage, ProtoError, UnixStream, connect_unix, read_message, write_message,
};
use crossbeam_channel::Receiver as CrossbeamReceiver;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::thread;
use tracing::{debug, error, info, warn};

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
    mut reader: R,
    mut handle_daemon_message: impl FnMut(DaemonMessage),
) -> Result<(), ClientError> {
    loop {
        debug!("daemon reader waiting for message");
        match read_message::<_, DaemonMessage>(&mut reader) {
            Ok(message) => {
                debug!("received daemon message");
                handle_daemon_message(message);
            }
            // Clean termination: the daemon closed its side of the connection.
            Err(ProtoError::Io(error))
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::ConnectionReset
                ) =>
            {
                break;
            }
            // Non-EOF I/O errors (broken pipe, connection aborted, etc.)
            // are also fatal — the transport is gone.
            Err(ProtoError::Io(error)) => {
                error!(kind = %error.kind(), "daemon reader I/O error");
                return Err(error.into());
            }
            // Protocol-level decode errors (Postcard, FrameTooLarge,
            // TrailingBytes, UnsupportedVersion) are per-message failures.
            // Because we use length-prefixed framing, a corrupt payload
            // never desynchronises the stream — log and carry on.
            Err(error) => {
                error!(%error, "skipping corrupt daemon message");
            }
        }
    }
    info!("daemon reader loop ended normally");
    Ok(())
}

/// Connect to a daemon over the Unix socket at `socket_path`, then serve the
/// established connection (see `pump_connection`).
///
/// # Errors
///
/// Returns [`ClientError`] if the unix socket cannot be connected, the
/// Noise handshake fails, or the connection loop hits an I/O or protocol
/// error.
pub fn run_daemon_connection(
    socket_path: &str,
    handle_daemon_message: impl FnMut(DaemonMessage),
    from_ui: CrossbeamReceiver<ClientMessage>,
    shutdown_rx: Option<CrossbeamReceiver<()>>,
) -> Result<(), ClientError> {
    info!("connecting to daemon at {socket_path}");
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
    info!("connecting to daemon at {socket_path}");
    let stream = match connect_unix(socket_path) {
        Ok(stream) => stream,
        Err(error) if choreo_proto::dial_error_means_no_listener(&error) => {
            info!(%error, "no daemon listening on the socket; requesting autostart");
            ensure_daemon()?;
            info!("autostart done; retrying connection to daemon at {socket_path}");
            connect_unix(socket_path)?
        }
        Err(error) => return Err(error.into()),
    };
    pump_connection(stream, handle_daemon_message, from_ui, shutdown_rx)
}

/// Drive an ESTABLISHED daemon connection: spawn the writer thread, install
/// the optional shutdown hook, and block in the reader loop until the daemon
/// closes the stream. Split out of `run_daemon_connection` so the autostart
/// variant can hand over a stream it already dialed (the successful first
/// dial is never discarded).
fn pump_connection(
    stream: UnixStream,
    handle_daemon_message: impl FnMut(DaemonMessage),
    from_ui: CrossbeamReceiver<ClientMessage>,
    shutdown_rx: Option<CrossbeamReceiver<()>>,
) -> Result<(), ClientError> {
    let reader = BufReader::new(stream.try_clone()?);
    let mut writer = BufWriter::new(stream);

    // Channel to signal the writer thread to stop when the reader finishes.
    // `bounded(1)` is a crossbeam control-plane channel (workspace convention):
    // the reader sends at most one stop signal, so capacity 1 never blocks it.
    let (writer_shutdown_tx, writer_shutdown_rx) = crossbeam_channel::bounded::<()>(1);

    // This writer thread is the connection's single send site. Every outbound
    // value is already a framed [`ClientMessage`]: the front-end owns a
    // [`crate::pending::PendingReplies`] table, allocates the per-connection
    // request id itself, and forwards the frame here so it can resolve the
    // reply by that id (the daemon echoes it onto the matching reply).
    let writer_handle = thread::spawn(move || {
        loop {
            // Event-driven wait: block until EITHER a UI message arrives OR the
            // reader signals shutdown — no polling.  The message arm is FIRST
            // and the select is BIASED, so a queue that already holds messages
            // is fully drained before a simultaneous stop signal is honoured,
            // matching the previous drain-then-stop behaviour exactly.
            crossbeam_channel::select_biased! {
                recv(from_ui) -> msg => match msg {
                    Ok(framed) => {
                        if let Err(e) = write_message(&mut writer, &framed) {
                            warn!("writer thread write error: {e}");
                            break;
                        }
                        let _ = writer.flush();
                    }
                    // `from_ui` closed: the UI is done sending.
                    Err(_) => break,
                },
                // The reader finished (or dropped its sender): stop the writer.
                recv(writer_shutdown_rx) -> _ => break,
            }
        }
    });

    if let Some(shutdown_rx) = shutdown_rx {
        let shutdown_stream = reader.get_ref().try_clone()?;
        thread::spawn(move || {
            // Crossbeam `recv()` returns a `Result`; a disconnected sender
            // still means "shut down", so the value is ignored either way.
            let _ = shutdown_rx.recv();
            let _ = shutdown_stream.shutdown(std::net::Shutdown::Both);
        });
    }

    let reader_result = run_daemon_reader(reader, handle_daemon_message);
    // Signal the writer to stop and wait for it to flush pending writes.
    let _ = writer_shutdown_tx.send(());
    let _ = writer_handle.join();
    reader_result
}
