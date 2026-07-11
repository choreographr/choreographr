use crate::error::ClientError;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::os::unix::net::UnixStream;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use tai_proto::{ClientMessage, DaemonMessage, ProtoError, read_message, write_message};
use tai_transport::error::TransportError;
use tai_transport::key::ensure_transport_keypair;
use tai_transport::noise::handshake_initiator;
use tracing::{debug, error, info, warn};

/// Read DaemonMessages from `reader` in a blocking loop, calling
/// `handle_daemon_message` for each successfully decoded message.
///
/// Returns `Ok(())` when the stream ends cleanly (EOF / connection reset).
/// Returns `Err` on protocol or I/O errors.
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
            // Any other protocol or I/O error is fatal.
            Err(error) => return Err(error.into()),
        }
    }
    info!("daemon reader loop ended normally");
    Ok(())
}

pub fn run_daemon_connection(
    socket_path: &str,
    handle_daemon_message: impl FnMut(DaemonMessage),
    from_ui: mpsc::Receiver<tai_proto::ClientMessage>,
    shutdown_rx: Option<mpsc::Receiver<()>>,
) -> Result<(), ClientError> {
    info!("connecting to daemon at {socket_path}");
    let stream = UnixStream::connect(socket_path)?;
    let reader = BufReader::new(stream.try_clone()?);
    let mut writer = BufWriter::new(stream);

    // Channel to signal the writer thread to stop when the reader finishes.
    let (writer_shutdown_tx, writer_shutdown_rx) = mpsc::channel::<()>();
    const SHUTDOWN_POLL_INTERVAL: Duration = Duration::from_millis(100);

    let writer_handle = thread::spawn(move || {
        loop {
            match from_ui.recv_timeout(SHUTDOWN_POLL_INTERVAL) {
                Ok(msg) => {
                    if let Err(e) = write_message(&mut writer, &msg) {
                        warn!("writer thread write error: {e}");
                        break;
                    }
                    let _ = writer.flush();
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    // Poll the shutdown signal periodically so we don't hang
                    // indefinitely on recv() when the daemon disconnects.
                    if writer_shutdown_rx.try_recv().is_ok() {
                        break;
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
    });

    if let Some(shutdown_rx) = shutdown_rx {
        let shutdown_stream = reader.get_ref().try_clone()?;
        thread::spawn(move || {
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

/// Selects the transport for connecting to a daemon.
#[derive(Clone, Debug)]
pub enum ConnectionMode {
    /// Connect via Unix domain socket at the given path.
    UnixSocket(String),
    /// Connect via TCP/Noise IK at the given address with the server's
    /// 32-byte X25519 public key (resolved before constructing this variant).
    Tcp {
        addr: String,
        server_pk: [u8; 32],
    },
}

impl Default for ConnectionMode {
    fn default() -> Self {
        ConnectionMode::UnixSocket(tai_proto::socket_path())
    }
}

/// Connect to a daemon via Noise IK over TCP.
///
/// Uses two blocking threads:
/// - Reader thread: blocks on NoiseStream::recv_daemon_message()
/// - Writer thread: blocks on from_ui.recv_timeout()
/// - Shutdown: blocks on shutdown_rx.recv(), then shuts down the TCP stream
///
/// The reader thread has no read timeout — it blocks until a message arrives
/// or the connection is closed. The writer thread uses a short timeout on its
/// channel receive so it can also check for shutdown signals.
pub fn run_daemon_tcp_connection(
    addr: &str,
    server_pk: &[u8; 32],
    mut handle_daemon_message: impl FnMut(DaemonMessage),
    from_ui: mpsc::Receiver<ClientMessage>,
    shutdown_rx: Option<mpsc::Receiver<()>>,
) -> Result<(), ClientError> {
    info!("connecting to daemon at {addr}");

    // Load the client transport keypair (generates one if absent).
    let (client_sk, _client_pk) =
        ensure_transport_keypair().map_err(|e| ClientError::Io(std::io::Error::other(e)))?;

    // Connect TCP and perform Noise IK handshake.
    let tcp = std::net::TcpStream::connect(addr).map_err(ClientError::Io)?;
    let mut noise = handshake_initiator(tcp, client_sk.as_bytes(), server_pk).map_err(|e| {
        ClientError::Io(std::io::Error::new(
            std::io::ErrorKind::ConnectionRefused,
            e,
        ))
    })?;

    // Channel to signal writer thread to stop when reader finishes.
    let (writer_shutdown_tx, writer_shutdown_rx) = mpsc::channel::<()>();
    const SHUTDOWN_POLL_INTERVAL: Duration = Duration::from_millis(100);

    // Writer thread: blocks on from_ui.recv_timeout(), sends via NoiseStream.
    // The timeout is only so the writer can check the shutdown signal —
    // no socket-level timeout is set.
    let mut writer = noise.try_clone().map_err(ClientError::Io)?;
    let writer_handle = thread::spawn(move || {
        loop {
            match from_ui.recv_timeout(SHUTDOWN_POLL_INTERVAL) {
                Ok(msg) => {
                    if let Err(e) = writer.send_client_message(&msg) {
                        warn!("writer thread error: {e}");
                        break;
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    // Check for shutdown signal so we don't hang on recv.
                    if writer_shutdown_rx.try_recv().is_ok() {
                        break;
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
    });

    // Optional shutdown signal: shuts down the TCP connection when triggered.
    if let Some(shutdown_rx) = shutdown_rx {
        let stream_ref = noise.get_ref().try_clone().map_err(ClientError::Io)?;
        thread::spawn(move || {
            let _ = shutdown_rx.recv();
            let _ = stream_ref.shutdown(std::net::Shutdown::Both);
        });
    }

    // Reader loop: blocks on noise.recv_daemon_message() (no read timeout).
    loop {
        match noise.recv_daemon_message() {
            Ok(message) => {
                handle_daemon_message(message);
            }
            Err(TransportError::ConnectionClosed) => {
                info!("daemon closed Noise IK connection");
                break;
            }
            // I/O errors from the underlying stream after shutdown:
            // treat them the same as ConnectionClosed.
            Err(TransportError::Io(ref e))
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::UnexpectedEof
                        | std::io::ErrorKind::ConnectionReset
                        | std::io::ErrorKind::ConnectionAborted
                        | std::io::ErrorKind::BrokenPipe
                ) =>
            {
                info!("daemon connection closed: {e}");
                break;
            }
            Err(e) => {
                error!(error = %e, "daemon reader error");
                break;
            }
        }
    }

    // Signal writer to stop and wait for it.
    let _ = writer_shutdown_tx.send(());
    let _ = writer_handle.join();
    info!("daemon reader loop ended normally");
    Ok(())
}

/// Connect to a daemon using the given connection mode.
/// Dispatches to the appropriate connection function.
pub fn run_daemon_connection_with_mode(
    mode: ConnectionMode,
    handle_daemon_message: impl FnMut(DaemonMessage),
    from_ui: mpsc::Receiver<ClientMessage>,
    shutdown_rx: Option<mpsc::Receiver<()>>,
) -> Result<(), ClientError> {
    match mode {
        ConnectionMode::UnixSocket(path) => {
            run_daemon_connection(&path, handle_daemon_message, from_ui, shutdown_rx)
        }
        ConnectionMode::Tcp { addr, server_pk } => run_daemon_tcp_connection(
            &addr,
            &server_pk,
            handle_daemon_message,
            from_ui,
            shutdown_rx,
        ),
    }
}
