//! The embedded (in-process) connection thread.

use choreo_proto::{ClientMessage, DaemonMessage};
use tracing::{debug, info};

use super::super::conn::{ClientConn, ConnThreadArgs};
use super::super::writer::ChannelConnectionWriter;

/// Per-connection inputs for the embedded (in-process) transport, bundled
/// into one struct so the spawn call site stays a single argument. The
/// transport-independent half is the shared [`ConnThreadArgs`]; the embedded
/// path adds only its channel endpoints (it has no socket).
pub(crate) struct EmbeddedConnArgs {
    /// GUI→daemon message values. Channel close IS the EOF.
    pub client_rx: crossbeam_channel::Receiver<ClientMessage>,
    /// The writer's forward target = the GUI's read half.
    pub out_tx: crossbeam_channel::Sender<DaemonMessage>,
    /// The shared per-connection inputs; the embedded link is the LOCAL trust
    /// domain, so the caller sets `is_unix: true`.
    pub conn: ConnThreadArgs,
}

/// The embedded (in-process) connection thread — the third transport, next
/// to [`crate::server::connection::client_thread`] (Unix) and
/// [`crate::server::connection::tcp_client_thread`] (TCP/Noise).
///
/// The connection never becomes bytes: client messages arrive as Rust values
/// on `client_rx` and daemon messages leave as values on `out_tx` (via the
/// [`ChannelConnectionWriter`] the shared writer thread drains). There is no
/// error classification and no timeout machinery: the `for` loop over
/// `client_rx` ends exactly when the GUI drops its `EmbeddedLink` (channel
/// close IS the EOF), and `conn.finish()` runs the same teardown the socket
/// paths use.
///
/// `is_unix: true` — an embedded connection is the LOCAL trust domain, like
/// the Unix socket: both peers are the same process, so `/acl add` (and the
/// local-only command semantics generally) apply.
// The embedded connection has no error surface beyond dispatch logging: the
// `for` loop ends on channel close (the EOF) and teardown is infallible, so
// the thread entry returns () instead of a transparent Ok wrapper.
pub(crate) fn embedded_client_thread(args: EmbeddedConnArgs) {
    let EmbeddedConnArgs {
        client_rx,
        out_tx,
        conn: conn_args,
    } = args;

    let client_id = conn_args.client_id;
    let mut conn = ClientConn::new(conn_args, ChannelConnectionWriter::new(out_tx));

    // The writer channel was registered with the daemon by `connect()`
    // (register_client_writer) BEFORE this thread was spawned, so the
    // shutdown path can route `ShuttingDown` through this single writer
    // thread — same ordering invariant as the socket paths.
    info!("embedded client connected: id={}", client_id);
    crate::metrics::record_client_connected();

    for msg in client_rx {
        if let Err(e) = conn.dispatch(msg) {
            debug!("daemon disconnected: {e}");
            break;
        }
    }
    info!("embedded client disconnected: id={}", client_id);

    conn.finish();
}
