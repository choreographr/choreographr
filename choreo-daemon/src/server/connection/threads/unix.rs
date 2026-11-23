//! The local-domain (Unix socket) connection thread.

use std::io::{self, BufReader};
use std::time::Duration;

use choreo_proto::{ClientMessage, ProtoError, UnixStream, read_message};
use tracing::{debug, error, info, warn};

use super::super::conn::{ClientConn, ConnThreadArgs};
use super::super::writer::UnixConnectionWriter;

/// Serve one accepted local-domain connection.
///
/// The socket is already narrowed to the owner (0600 inside an owner-only
/// directory), but the kernel's own answer about the peer is checked before a
/// single message is read: the local socket is fully trusted (local-only commands
/// such as `AclAdd`, no transport authentication), so a foreign uid must never
/// reach the dispatcher. Only Linux exposes the peer's uid without extra ceremony
/// (`SO_PEERCRED`); elsewhere the file mode remains the control.
///
/// This reader is deliberately **unbounded**, unlike the TCP one. The threat a read
/// bound answers on the network path does not exist here: the peer is the same uid
/// (checked above), `AF_UNIX` is node-local so no slow link can carry it, and a
/// client whose process dies takes the socket with it — the reader sees EOF. What
/// remains is a live-but-wedged local process, which would pin one of
/// `MAX_CONCURRENT_CONNECTIONS` slots; that is a nuisance its owner can clear by
/// restarting, and it does not justify putting a clock on the local path.
pub(crate) fn client_thread(
    stream: UnixStream,
    args: ConnThreadArgs,
    peer_stall: Duration,
) -> io::Result<()> {
    // Capture the id before `args` is consumed by `ClientConn::new`; the
    // rejection path below and the connect log both need it.
    let client_id = args.client_id;

    // Defense in depth for the local trust domain (see the function docs).
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        use rustix::process::geteuid;
        match rustix::net::sockopt::socket_peercred(&stream) {
            Ok(cred) if cred.uid == geteuid() => {}
            Ok(cred) => {
                warn!(
                    peer_uid = cred.uid.as_raw(),
                    our_uid = geteuid().as_raw(),
                    "rejecting Unix socket connection from a foreign uid"
                );
                // Mirror the TCP handshake-failure path: the acceptor already
                // registered this client's writer channel, so unregister it
                // rather than leaving a stale entry in the daemon's map.
                let _ = args
                    .daemon_tx
                    .send(crate::daemon::DaemonCommand::ClientDisconnected { client_id });
                return Ok(());
            }
            Err(e) => {
                // No peer credentials means the socket is in a state we cannot
                // validate; treat it as foreign rather than trusting it.
                warn!(
                    error = %e,
                    "could not read the Unix socket peer credentials; rejecting connection"
                );
                let _ = args
                    .daemon_tx
                    .send(crate::daemon::DaemonCommand::ClientDisconnected { client_id });
                return Ok(());
            }
        }
    }

    // The writer gets the accepted socket and the reader a duplicate of it. The
    // writer sink bounds every frame itself (one per-message budget plus a
    // per-syscall stall), so no socket-level write timeout is armed here — the
    // stall is re-armed per syscall inside `write_all_with_stall`, which is what
    // makes a partly-accepted frame fail instead of waiting out a whole message.
    let reader = BufReader::new(stream.try_clone()?);
    let writer = UnixConnectionWriter::new(stream, peer_stall);

    let mut conn = ClientConn::new(args, writer);

    // The writer channel was registered with the daemon by the acceptor
    // (register_client_writer) before this thread was spawned, so the shutdown
    // path can route `ShuttingDown` through this single writer thread instead
    // of writing to the socket from another thread.
    info!("client connected: id={}", client_id);
    crate::metrics::record_client_connected();

    let mut reader = reader;
    loop {
        match read_message::<_, ClientMessage>(&mut reader) {
            Ok(msg) => {
                if let Err(e) = conn.dispatch(msg) {
                    debug!("daemon disconnected: {e}");
                    break;
                }
            }
            Err(ProtoError::Io(e))
                if matches!(
                    e.kind(),
                    io::ErrorKind::UnexpectedEof | io::ErrorKind::ConnectionReset
                ) =>
            {
                debug!("client disconnected");
                break;
            }
            Err(e) => {
                error!(error = %e, "failed to read client message");
                break;
            }
        }
    }

    conn.finish();
    Ok(())
}
