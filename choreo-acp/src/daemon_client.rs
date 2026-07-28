use choreo_proto::{ClientMessage, DaemonMessage, read_message, write_message};
use std::io::{BufReader, BufWriter, Write};
use std::os::unix::net::UnixStream;
use std::sync::mpsc;
use std::thread;
use tracing::{debug, error, info};

use crate::acp_jsonrpc;
use crate::error::AcpError;

// ---------------------------------------------------------------------------
// Unified event type consumed by the main event loop
// ---------------------------------------------------------------------------

/// Events produced by the ACP stdin reader thread and the daemon socket
/// reader thread.  The main event loop receives these from a single mpsc
/// channel and dispatches them.
#[derive(Debug)]
pub enum Event {
    /// A JSON-RPC request or notification received from the editor (stdin).
    AcpRequest(acp_jsonrpc::RpcMessage),
    /// The ACP stdin reader reached EOF (editor disconnected).
    AcpEof,
    /// A message received from Choreographr (Unix socket).
    DaemonMessage(DaemonMessage),
    /// The daemon connection was lost.
    DaemonDisconnected,
}

// ---------------------------------------------------------------------------
// Daemon connection handle
// ---------------------------------------------------------------------------

/// Handle to the daemon connection.
///
/// `writer_tx` is used to send `ClientMessage` values to the daemon writer
/// thread.  `join_handle` allows waiting for the daemon reader thread to
/// finish during shutdown.
pub struct DaemonClient {
    /// Send `ClientMessage` frames to the daemon writer thread.
    pub writer_tx: mpsc::Sender<ClientMessage>,
    /// Join handle for the daemon reader thread.
    pub join_handle: thread::JoinHandle<()>,
}

/// Spawn daemon I/O threads (one reader, one writer) connected to the
/// daemon's Unix socket.
///
/// `event_tx` is shared with the ACP reader — both threads send into the
/// same channel so the main loop can receive from a single receiver.
///
/// Returns a `DaemonClient` and the writer thread's join handle.  The caller
/// should also spawn the ACP reader with another clone of `event_tx`.
pub fn spawn_daemon_io(
    socket_path: &str,
    event_tx: mpsc::Sender<Event>,
) -> Result<(DaemonClient, thread::JoinHandle<()>), AcpError> {
    info!(socket_path, "connecting to daemon");

    let stream = UnixStream::connect(socket_path).map_err(|e| {
        error!(error = %e, socket_path, "failed to connect to daemon");
        AcpError::DaemonConnection(format!("{e}"))
    })?;

    let reader_stream = stream.try_clone()?;
    let mut writer_stream = BufWriter::new(stream);

    // Writer channel: the main loop sends ClientMessages here.
    let (writer_tx, writer_rx): (mpsc::Sender<ClientMessage>, _) = mpsc::channel();

    // ------------------------------------------------------------------
    // Writer thread
    //
    // Blocks on writer_rx.recv() and writes each message as a postcard-
    // encoded frame with a 4-byte big-endian length prefix.
    // ------------------------------------------------------------------
    let writer_handle = thread::Builder::new()
        .name("daemon-writer".into())
        .spawn(move || {
            info!("daemon writer thread started");
            for msg in writer_rx {
                debug!(?msg, "sending message to daemon");
                if let Err(e) = write_message(&mut writer_stream, &msg) {
                    error!(error = %e, "daemon writer error");
                    break;
                }
                // BufWriter buffers — flush after every frame so the
                // daemon receives it immediately.
                if let Err(e) = writer_stream.flush() {
                    error!(error = %e, "daemon writer flush error");
                    break;
                }
            }
            info!("daemon writer thread exiting");
        })
        .map_err(AcpError::Io)?;

    // ------------------------------------------------------------------
    // Reader thread
    //
    // Blocks on read_message() and forwards every decoded DaemonMessage
    // into the event channel.  EOF / connection reset are signalled as
    // DaemonDisconnected so the main loop can react.
    // ------------------------------------------------------------------
    let reader_handle = thread::Builder::new()
        .name("daemon-reader".into())
        .spawn(move || {
            info!("daemon reader thread started");
            let mut reader = BufReader::new(reader_stream);
            loop {
                match read_message::<_, DaemonMessage>(&mut reader) {
                    Ok(msg) => {
                        debug!(?msg, "received daemon message");
                        if event_tx.send(Event::DaemonMessage(msg)).is_err() {
                            // Main loop dropped the receiver — shutting down.
                            break;
                        }
                    }
                    // The daemon closed its side — terminate the reader.
                    Err(choreo_proto::ProtoError::Io(e))
                        if matches!(
                            e.kind(),
                            std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::ConnectionReset
                        ) =>
                    {
                        info!("daemon connection closed: {e}");
                        let _ = event_tx.send(Event::DaemonDisconnected);
                        break;
                    }
                    // Non-EOF I/O errors are fatal to the transport.
                    Err(choreo_proto::ProtoError::Io(e)) => {
                        error!(kind = %e.kind(), "daemon reader I/O error");
                        let _ = event_tx.send(Event::DaemonDisconnected);
                        break;
                    }
                    // Protocol-level decode errors are per-message failures.
                    // Because we use length-prefixed framing, a corrupt
                    // payload never desynchronises the stream — log and
                    // carry on.
                    Err(e) => {
                        error!(error = %e, "skipping corrupt daemon message");
                    }
                }
            }
            info!("daemon reader thread exiting");
        })
        .map_err(AcpError::Io)?;

    Ok((
        DaemonClient {
            writer_tx,
            join_handle: reader_handle,
        },
        writer_handle,
    ))
}
