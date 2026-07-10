use crate::error::ClientError;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::os::unix::net::UnixStream;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use tai_proto::{DaemonMessage, ProtoError, read_message, write_message};
use tracing::{debug, info, warn};

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
