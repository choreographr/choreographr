//! Daemon-connection entry points: dial, handshake, and serve.
//!
//! Every `run_daemon_*` function takes the UI's outgoing-message channel plus
//! an optional shutdown channel, and blocks on a reader loop until the daemon
//! closes the stream — pairing a writer thread that drains the UI channel
//! (biased, so queued messages flush before a stop) with the reader thread
//! that forwards each decoded message to the caller's handler. The variants
//! split by transport ([`ConnectionMode`]): Unix socket, TCP/Noise IK (pinned
//! or explicit key), TCP/Noise XX first contact, and an in-process channel
//! pair to an embedded daemon. The preflight helpers ([`probe_server_key`],
//! [`verify_daemon_authorization`], [`own_transport_pubkey`]) let a front-end
//! establish trust and enrollment before committing to a session.
//!
//! The implementation splits by concern: `serve` owns the writer/reader/teardown
//! loop every transport shares plus the writer's time bounds, `unix` is the
//! local-domain socket path, `tcp` the Noise-over-TCP variants and the preflight
//! helpers, `handshake` the shared Noise handshake plumbing and the
//! established-stream source/sink, and `inproc` the embedded in-process channel
//! transport. This module owns the transport-selecting type and the
//! [`run_daemon_connection_with_mode`] dispatch that ties them together.

mod handshake;
mod inproc;
mod serve;
mod tcp;
mod unix;

pub use tcp::{
    PreflightError, own_transport_pubkey, probe_server_key, run_daemon_tcp_connection,
    run_daemon_tcp_connection_pinned, run_daemon_tcp_connection_xx_first_contact,
    verify_daemon_authorization,
};
pub use unix::{run_daemon_connection, run_daemon_connection_with_autostart, run_daemon_reader};

use crate::error::ClientError;
use choreo_proto::{ClientMessage, DaemonMessage};
// In-process transport: raw channel ends from an `choreo_daemon::embedded::EmbeddedLink`.
// Pure values — client-core never depends on choreo-daemon; the GUI creates
// the link and stuffs the ends into `ConnectionMode::InProcess`.
use crossbeam_channel::{Receiver as CrossbeamReceiver, Sender as CrossbeamSender};
use std::fmt;

/// Selects the transport for connecting to a daemon.
///
// Clone stays derived: crossbeam channel ends are `Clone`, so a cloned mode
// shares the same underlying channel pair (fine — cloning a mode is only used
// to stash it for `connection_addr`-style display lookups).
#[derive(Clone)]
pub enum ConnectionMode {
    /// Connect via Unix domain socket at the given path.
    UnixSocket(String),
    /// Connect via TCP/Noise IK at the given address with the server's
    /// 32-byte X25519 public key (resolved before constructing this variant).
    Tcp {
        /// The `host:port` to dial.
        addr: String,
        /// The server's 32-byte X25519 static public key, which the Noise IK
        /// handshake authenticates.
        server_pk: [u8; 32],
    },
    /// Connect via TCP/Noise IK against the key PINNED in
    /// `known_servers.toml` for the address. The pin is loaded at connect
    /// time; a handshake failure is reported WITH the pinned public key and
    /// the re-pair guidance, so a server key change is loud instead of an
    /// opaque connection error (the `known_hosts` behavior).
    TcpPinned(String),
    /// Connect to an EMBEDDED daemon in the same process: `daemon_tx` is the
    /// client→daemon channel end, `daemon_rx` the daemon→client end (the raw
    /// pair from `choreo_daemon::embedded::EmbeddedLink`, moved here as
    /// values). Messages travel as VALUES — no codec, no socket. Channel
    /// close IS the EOF in both directions, exactly like the daemon-side
    /// embedded path. client-core never links choreo-daemon: the GUI (the
    /// owner of the `EmbeddedDaemon`) creates the link and hands over the ends.
    InProcess {
        /// The client→daemon channel end: the writer forwards UI messages
        /// here.
        daemon_tx: CrossbeamSender<ClientMessage>,
        /// The daemon→client channel end the reader loop drains until the
        /// daemon drops its sender.
        daemon_rx: CrossbeamReceiver<DaemonMessage>,
    },
}

// Manual Debug instead of a derive: crossbeam channel ends are not `Debug`,
// and the embedded link carries no printable identity — it is rendered as a
// placeholder. Every other variant formats EXACTLY as the derive output would
// (`UnixSocket("path")`, `Tcp { addr: "…", server_pk: [..] }`,
// `TcpPinned("addr")`) so log/error messages keep their previous shape.
impl fmt::Debug for ConnectionMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConnectionMode::UnixSocket(path) => f.debug_tuple("UnixSocket").field(path).finish(),
            ConnectionMode::Tcp { addr, server_pk } => f
                .debug_struct("Tcp")
                .field("addr", addr)
                .field("server_pk", server_pk)
                .finish(),
            ConnectionMode::TcpPinned(addr) => f.debug_tuple("TcpPinned").field(addr).finish(),
            // The ends are opaque by design: printing channel internals would
            // leak nothing useful and might suggest a stable identity that
            // does not exist.
            ConnectionMode::InProcess { .. } => f.write_str("InProcess(<embedded link>)"),
        }
    }
}

impl Default for ConnectionMode {
    fn default() -> Self {
        ConnectionMode::UnixSocket(choreo_proto::socket_path())
    }
}

/// Connect to a daemon using the given connection mode.
/// Dispatches to the appropriate connection function.
///
/// # Errors
///
/// Returns [`ClientError`] as raised by the connection mode actually used
/// (unix, TCP variants, first-contact preflight, or in-process).
pub fn run_daemon_connection_with_mode(
    mode: ConnectionMode,
    handle_daemon_message: impl FnMut(DaemonMessage),
    from_ui: CrossbeamReceiver<ClientMessage>,
    shutdown_rx: Option<CrossbeamReceiver<()>>,
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
        ConnectionMode::TcpPinned(addr) => {
            run_daemon_tcp_connection_pinned(&addr, handle_daemon_message, from_ui, shutdown_rx)
        }
        ConnectionMode::InProcess {
            daemon_tx,
            daemon_rx,
        } => inproc::run_daemon_connection_in_process(
            daemon_tx,
            daemon_rx,
            handle_daemon_message,
            from_ui,
            shutdown_rx,
        ),
    }
}

#[cfg(test)]
mod tests;
