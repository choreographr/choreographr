//! The TCP/Noise connection threads: the responder handshake, then the encrypted
//! read loop.

use std::io;
use std::net::TcpStream;
use std::time::Duration;

use choreo_transport::key::TransportSecretKey;
use tracing::{debug, error, info};

use super::super::conn::{ClientConn, ConnThreadArgs};
use super::super::writer::NoiseConnectionWriter;
use crate::server::acl::SharedAcl;

/// TCP accept path: run the Noise responder handshake the client asked for,
/// then hand the encrypted stream to [`tcp_client_thread`].
///
/// The writer channel is registered with the daemon by the acceptor BEFORE
/// this function runs (see [`crate::server::connection::register_client_writer`]),
/// so every failure path here — unknown preamble, silent/garbage peer, rejected
/// handshake — must unregister via `ClientDisconnected`, exactly as a handshake
/// failure inside [`crate::server::connection::client_thread`] does. This keeps
/// the daemon's client map honest: a connection that never produced a working
/// transport must not leave a stale writer entry behind.
///
/// **The preamble byte authorizes nothing, but it is not unchecked either.**
/// It is a cleartext mode selector read before any keying material exists, so it
/// cannot carry authentication itself; everything that matters is authenticated
/// by the Noise handshake that follows — IK and XX both authenticate both
/// parties' static keys via the DH operations, so a man-in-the-middle cannot
/// impersonate either side (that would require the server's private key), and
/// the daemon's ACL check runs inside whichever handshake the client picked.
/// The 1-byte mode is mixed into the Noise prologue on both ends, so a peer that
/// picks IK and an intermediary that rewrites the byte to XX produce different
/// prologues and the handshake FAILS instead of running the other flow: the
/// choice is the client's and nobody else's.
///
/// The delegation is one call into
/// [`choreo_transport::handshake::handshake_responder_auto`], which reads the
/// byte, dispatches on it, and spends ONE deadline across both steps — so this
/// path cannot drift from the crate's own mode handling, and a peer stalling
/// before the byte buys no extra budget.
pub(crate) fn tcp_handshake_and_client_thread(
    tcp: TcpStream,
    transport_sk: TransportSecretKey,
    acl: &std::sync::Arc<SharedAcl>,
    args: ConnThreadArgs,
    peer_stall: Duration,
) -> io::Result<()> {
    // Disable Nagle before the responder handshake: the accepted socket is
    // otherwise untouched, and the handshake is a small request/response
    // sequence that would pay the Nagle + delayed-ACK tax on a high-RTT link.
    // The data plane re-applies it in the handshake's `finish` (the intrinsic
    // guarantee).
    choreo_transport::socket::tune_control_socket(&tcp);

    // The preamble read and the handshake share one absolute deadline, so an
    // unauthenticated peer that connects and sends nothing (or dribbles) is cut
    // off instead of holding this thread + FD open. `transport_sk` is owned, not
    // copied: it zeroizes on drop at the end of this function, so the identity
    // does not linger in this thread's stack for the connection's lifetime.
    let handshake =
        choreo_transport::handshake::handshake_responder_auto(tcp, transport_sk.as_bytes(), |pk| {
            acl.contains(pk)
        });
    // Released as soon as the handshake is over, on every path below.
    drop(transport_sk);

    let noise = match handshake {
        Ok((noise, mode)) => {
            debug!(mode = %mode, "TCP client handshake complete");
            noise
        }
        Err(e) => {
            error!(error = %e, "TCP client handshake rejected; closing");
            // Dropping the stream (closed inside the handshake) and
            // unregistering the writer keep the clients map honest.
            let _ = args
                .daemon_tx
                .send(crate::daemon::DaemonCommand::ClientDisconnected {
                    client_id: args.client_id,
                });
            return Ok(());
        }
    };

    tcp_client_thread(noise, args, peer_stall)
}

/// The encrypted read loop over an established
/// [`choreo_transport::noise::NoiseStream`].
///
/// The writer thread blocks on `writer_rx` and sends through the Noise sink: each
/// message is bounded by the transport's own connection policy — a whole-message
/// budget derived from the wire bytes it frames plus the connection's per-syscall
/// stall — so a large message to a wedged client cannot occupy the writer for the
/// whole message's worth of per-syscall stalls.
pub(crate) fn tcp_client_thread(
    noise: choreo_transport::noise::NoiseStream,
    args: ConnThreadArgs,
    peer_stall: Duration,
) -> io::Result<()> {
    let mut reader = noise;
    // ONE stall per connection, in both directions: it lives in the stream's shared
    // connection record, so the value set here is the one the read loop, the writer
    // thread and every later clone read on their next message — it is settable at any
    // time, before or after the writer clone is made. `set_stall` floors it through
    // `effective_stall`. The reader's wait *between* messages stays unbounded — an
    // idle client is not a wedged one — and a client that vanished outright is
    // caught by the socket's keepalive.
    reader.set_stall(peer_stall);
    let writer_buf = NoiseConnectionWriter::new(reader.try_clone()?);

    let client_id = args.client_id;
    let mut conn = ClientConn::new(args, writer_buf);

    // The writer channel was registered with the daemon by the acceptor
    // (register_client_writer) before this thread was spawned, so the shutdown
    // path can route `ShuttingDown` through this single writer thread (see
    // `client_thread`). The NoiseStream's TransportState lock is only safe to
    // take per-message because this is the sole sender.
    info!("TCP client connected: id={}", client_id);
    crate::metrics::record_client_connected();

    // Summary subscription is an explicit client decision on this transport,
    // exactly as on the Unix path: a Noise client opts in via
    // ClientMessageType::SubscribeSessionsSummary (dispatched in
    // dispatch_client_message). A TCP connection is NOT auto-registered here, so
    // a client that never asked is never sent broadcasts about other clients'
    // sessions.
    loop {
        match reader.recv_client_message() {
            Ok(msg) => {
                if let Err(e) = conn.dispatch(msg) {
                    debug!("daemon disconnected: {e}");
                    break;
                }
            }
            Err(choreo_transport::error::TransportError::ConnectionClosed) => {
                info!("TCP client closed connection");
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
