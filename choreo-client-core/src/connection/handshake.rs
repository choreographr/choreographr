//! Shared Noise-over-TCP plumbing: the client keypair, handshake-failure
//! classification, the IK dial-and-serve wrapper, and the established-stream
//! service loop both TCP connection modes run.

use crate::error::ClientError;
use choreo_proto::{ClientMessage, DaemonMessage};
use choreo_transport::error::TransportError;
use choreo_transport::handshake::handshake_initiator;
use choreo_transport::key::ensure_transport_keypair;
use crossbeam_channel::Receiver as CrossbeamReceiver;
use std::thread;
use std::time::Duration;
use tracing::{error, info, warn};

/// Socket write timeout applied to a client connection's writer.
///
/// Bounds a single blocking `write` syscall so a wedged daemon — one whose
/// socket receive window is permanently zero — cannot stall the client's writer
/// thread forever. On a timeout the writer shuts the socket down (see
/// [`serve_noise_connection`]), which unblocks the reader's blocking
/// `recv_daemon_message` and tears the connection down. It matches the daemon's
/// own default writer write timeout (a daemon operator can widen its own with
/// `DaemonState::writer_write_timeout`; the pair only has to be close enough
/// that neither side waits on the other indefinitely). A slow-but-alive daemon is
/// never falsely killed: the timeout is per syscall, so any socket that makes
/// progress survives.
const CLIENT_WRITER_WRITE_TIMEOUT: Duration = Duration::from_secs(5);

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

/// Classify a handshake failure as `ConnectionRefused`.
///
/// The dial is a separate step on every TCP path, so a `ConnectionRefused`
/// coming back from a handshake can only mean the handshake itself failed —
/// which is exactly the case where the pinned-mode wrapper's re-pair guidance
/// belongs. A dial failure (daemon down, network down) must never be reported
/// that way, so the classification lives here, once, rather than at each site.
pub(super) fn handshake_refused<T>(result: Result<T, TransportError>) -> Result<T, ClientError> {
    result.map_err(|e| {
        ClientError::Io(std::io::Error::new(
            std::io::ErrorKind::ConnectionRefused,
            e,
        ))
    })
}

/// Noise IK handshake over an ALREADY-DIALED TCP stream, then the encrypted
/// session loop. Shared by [`super::run_daemon_tcp_connection`] and
/// [`super::run_daemon_tcp_connection_pinned`] so both paths run the identical
/// handshake sequence. A handshake failure surfaces as `ConnectionRefused` (see
/// [`handshake_refused`]) — which is why the caller must dial separately: only
/// then does a `ConnectionRefused` returned from here unambiguously mean the
/// HANDSHAKE failed, not the network.
pub(super) fn ik_handshake_and_serve(
    tcp: std::net::TcpStream,
    client_sk: &[u8; 32],
    server_pk: &[u8; 32],
    handle_daemon_message: impl FnMut(DaemonMessage),
    from_ui: CrossbeamReceiver<ClientMessage>,
    shutdown_rx: Option<CrossbeamReceiver<()>>,
) -> Result<(), ClientError> {
    // The IK initiator writes the 1-byte mode preamble as its first step (see
    // `handshake_initiator`); the raw `TransportError` is classified as
    // `ConnectionRefused` here, one layer above the dial.
    let noise = handshake_refused(handshake_initiator(tcp, client_sk, server_pk))?;
    serve_noise_connection(noise, handle_daemon_message, from_ui, shutdown_rx)
}

/// Shared tail of both TCP connection modes: the encrypted session loop over
/// an already-established `NoiseStream`.
///
/// Uses two blocking threads:
/// - Reader thread (the caller's): blocks on `NoiseStream::recv_daemon_message()`
/// - Writer thread: blocks in a `select_biased!` on `from_ui` (drained first)
///   and an internal writer-shutdown channel; the socket's write timeout is
///   bounded by [`CLIENT_WRITER_WRITE_TIMEOUT`], and a failed write shuts the
///   socket down so the reader unblocks
/// - Shutdown: blocks on `shutdown_rx.recv()`, then shuts down the TCP stream
///
/// Extracted so [`super::run_daemon_tcp_connection`] (IK) and
/// [`super::run_daemon_tcp_connection_xx_first_contact`] (XX) share one
/// writer/reader implementation — the two modes differ ONLY in preamble +
/// handshake, not in how the established transport is served.
pub(super) fn serve_noise_connection(
    mut noise: choreo_transport::noise::NoiseStream,
    mut handle_daemon_message: impl FnMut(DaemonMessage),
    from_ui: CrossbeamReceiver<ClientMessage>,
    shutdown_rx: Option<CrossbeamReceiver<()>>,
) -> Result<(), ClientError> {
    // Bound the writer's blocking socket writes so a wedged daemon (receive
    // window permanently zero) cannot stall the writer forever. The timeout
    // applies to every clone of this socket; when it fires the writer shuts the
    // socket down below, which unblocks the reader.
    noise
        .get_ref()
        .set_write_timeout(Some(CLIENT_WRITER_WRITE_TIMEOUT))?;

    // Channel to signal the writer thread to stop when the reader finishes.
    // `bounded(1)` crossbeam control-plane channel; the reader sends at most
    // one stop signal, so capacity 1 never blocks it (workspace convention).
    let (writer_shutdown_tx, writer_shutdown_rx) = crossbeam_channel::bounded::<()>(1);

    // Writer thread: event-driven `select_biased!` over the outgoing-message
    // channel and the internal stop signal, sending via NoiseStream.  The
    // message arm is FIRST (biased) so queued UI messages are drained before a
    // simultaneous stop is honoured.
    let mut writer = noise.try_clone().map_err(ClientError::Io)?;
    // The noise writer thread forwards each already-framed [`ClientMessage`]
    // produced by the front-end's [`crate::pending::PendingReplies`] table
    // (which allocated the request id so it can correlate the reply).
    let writer_handle = thread::spawn(move || {
        loop {
            crossbeam_channel::select_biased! {
                recv(from_ui) -> msg => match msg {
                    Ok(framed) => {
                        if let Err(e) = writer.send_client_message(&framed) {
                            warn!("writer thread error: {e}");
                            // A failed write — a broken pipe, or a wedged peer
                            // tripping the write timeout — means the socket is
                            // unusable. Close it so the reader's blocking
                            // `recv_daemon_message` unblocks and the connection
                            // tears down; otherwise the reader would wait
                            // forever on a transport that can never send again.
                            let _ = writer.get_ref().shutdown(std::net::Shutdown::Both);
                            break;
                        }
                    }
                    Err(_) => break,
                },
                recv(writer_shutdown_rx) -> _ => break,
            }
        }
    });

    // Optional shutdown signal: shuts down the TCP connection when triggered.
    if let Some(shutdown_rx) = shutdown_rx {
        let stream_ref = noise.get_ref().try_clone().map_err(ClientError::Io)?;
        thread::spawn(move || {
            // Crossbeam `recv()` returns a `Result`; a disconnected sender
            // still means "shut down", so the value is ignored either way.
            let _ = shutdown_rx.recv();
            let _ = stream_ref.shutdown(std::net::Shutdown::Both);
        });
    }

    // Reader loop: blocks on noise.recv_daemon_message() (no read timeout).
    // `recv_daemon_message` already maps the EOF-class read errors to
    // `ConnectionClosed`, so only that variant plus a genuine protocol/IO error
    // remain to classify here.
    loop {
        match noise.recv_daemon_message() {
            Ok(message) => {
                handle_daemon_message(message);
            }
            Err(TransportError::ConnectionClosed) => {
                info!("daemon closed the Noise connection");
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
