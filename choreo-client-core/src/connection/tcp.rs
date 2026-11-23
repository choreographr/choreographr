//! The Noise-over-TCP connection variants.
//!
//! [`run_daemon_tcp_connection`] dials and runs the IK handshake against an
//! explicit server key; [`run_daemon_tcp_connection_pinned`] does the same
//! against the key pinned in `known_servers.toml`, adding re-pair guidance on a
//! handshake failure; [`run_daemon_tcp_connection_xx_first_contact`] runs the XX
//! handshake and gates on an out-of-band key confirmation. The trust/enrollment
//! preflight helpers live in `super::preflight`.

use crate::error::ClientError;
use choreo_proto::{ClientMessage, DaemonMessage};
use choreo_transport::handshake::handshake_initiator_xx;
use crossbeam_channel::Receiver as CrossbeamReceiver;
use tracing::info;

use super::handshake::{
    client_transport_keypair, handshake_error, ik_handshake_and_serve, serve_noise_connection,
};

/// Dial a daemon's TCP control plane and apply the control-plane socket policy.
///
/// Every TCP connect path — the connection variants here and the preflight in
/// `super::preflight` — dials the same way, so the dial (which stays its own
/// step — see the callers' trust-guidance notes) and the `NODELAY` tuning live
/// here instead of being repeated at each site. The tuning is applied BEFORE the
/// handshake so the handshake's small round trips are not delayed by Nagle;
/// [`choreo_transport::handshake`]'s `finish` re-applies it when the socket
/// reaches the data plane.
pub(super) fn dial_control_tcp(addr: &str) -> std::io::Result<std::net::TcpStream> {
    let tcp = std::net::TcpStream::connect(addr)?;
    choreo_transport::socket::tune_control_socket(&tcp);
    Ok(tcp)
}

/// Connect to a daemon via Noise IK over TCP.
///
/// Uses two blocking threads:
/// - Reader thread: blocks on `NoiseStream::recv_daemon_message()`
/// - Writer thread: blocks in a `select_biased!` on `from_ui` (drained first)
///   and an internal writer-shutdown channel
/// - Shutdown: blocks on `shutdown_rx.recv()`, then shuts down the TCP stream
///
/// The reader thread has no read timeout — it blocks until a message arrives
/// or the connection is closed. The writer thread blocks until a real message
/// or the internal stop signal arrives (no polling).
///
/// # Errors
///
/// Returns [`ClientError`] if dialing fails, the Noise IK handshake fails,
/// or any reader/writer I/O error kills the connection.
pub fn run_daemon_tcp_connection(
    addr: &str,
    server_pk: &[u8; 32],
    handle_daemon_message: impl FnMut(DaemonMessage),
    from_ui: CrossbeamReceiver<ClientMessage>,
    shutdown_rx: Option<CrossbeamReceiver<()>>,
) -> Result<(), ClientError> {
    info!("connecting to daemon at {addr}");

    // Load the client transport keypair (generates one if absent).
    let (client_sk, _client_pk) = client_transport_keypair()?;

    // Dial FIRST, as its own step. Keeping the dial and the handshake
    // separate is what lets the pinned-mode wrapper (see
    // `run_daemon_tcp_connection_pinned`) attach trust guidance to a REFUSED
    // HANDSHAKE only — a plain dial failure (daemon down, network down) is
    // reported verbatim and must never suggest re-pairing, and neither must a
    // handshake that merely timed out.
    let tcp = dial_control_tcp(addr).map_err(ClientError::Io)?;

    ik_handshake_and_serve(
        tcp,
        client_sk.as_bytes(),
        server_pk,
        handle_daemon_message,
        from_ui,
        shutdown_rx,
    )
}

/// Connect to a daemon via Noise XX over TCP (first contact).
///
/// Use this when the client has NO pinned server public key for `addr`: the
/// XX handshake reveals the server's static key, which is handed to
/// `on_first_contact` BEFORE the encrypted session loop starts. The caller
/// is expected to render the key's base64 for a human and return
/// `true` only after an out-of-band confirmation (this is the trust gate —
/// on `false` the connection is closed and NOTHING is sent, in particular
/// no `Unlock`, so a first-contact MITM can never harvest the daemon's
/// private key). The confirmed key is NOT stored here — pinning to
/// `known_servers.toml` is the caller's job (phase 2), because the trust
/// decision belongs to the UI layer, not the transport plumbing.
///
/// The writer thread only starts after `on_first_contact` returns `true`,
/// so any `ClientMessage` already queued by the UI (including the TUI's
/// auto-`Unlock`) is structurally held back until the trust decision is
/// made — the gating is by construction, not by convention.
///
/// Otherwise identical to [`run_daemon_tcp_connection`] (same reader/writer
/// thread shape, same shutdown semantics — see `serve_noise_connection`).
///
/// # Errors
///
/// Returns [`ClientError`] if dialing fails, the XX handshake fails, the
/// learned server key is rejected by `on_first_contact`, or an I/O error
/// kills the connection.
pub fn run_daemon_tcp_connection_xx_first_contact(
    addr: &str,
    handle_daemon_message: impl FnMut(DaemonMessage),
    from_ui: CrossbeamReceiver<ClientMessage>,
    shutdown_rx: Option<CrossbeamReceiver<()>>,
    on_first_contact: impl FnOnce([u8; 32]) -> bool,
) -> Result<(), ClientError> {
    info!("first-contact connect to daemon at {addr}");

    // Load the client transport keypair (generates one if absent).
    let (client_sk, _client_pk) = client_transport_keypair()?;

    // Connect TCP, then run Noise XX — the initiator writes the first-contact
    // mode preamble itself, so the declared mode and the flow cannot disagree.
    let tcp = dial_control_tcp(addr).map_err(ClientError::Io)?;
    let (noise, server_pk) =
        handshake_initiator_xx(tcp, client_sk.as_bytes()).map_err(handshake_error)?;

    // Trust gate: the caller verifies the learned key out-of-band. On
    // refusal, drop the (already-established) encrypted transport without
    // sending a single protocol message — the connection simply closes.
    if !on_first_contact(server_pk) {
        info!("first-contact trust rejected by caller; closing connection");
        return Ok(());
    }

    serve_noise_connection(noise, handle_daemon_message, from_ui, shutdown_rx)
}

/// Connect via Noise IK against the key PINNED in `known_servers.toml` for
/// `addr` (the [`super::ConnectionMode::TcpPinned`] path).
///
/// The whole point of this wrapper is the failure UX: a HANDSHAKE failure
/// against the pinned key carries the pinned public key and the explicit
/// re-pair instructions, so a server key change is a loud, actionable
/// message rather than an opaque error (the `known_hosts` behavior). The
/// dial is performed HERE, as a separate step, so a network-down daemon is
/// reported as a plain connect error WITHOUT the re-pair guidance — the
/// remediation advice is reserved for the one failure it actually applies
/// to (the server's key changed).
///
/// Errors if no pin exists for `addr` — callers must resolve first contact
/// (probe + confirm + [`crate::known_servers::KnownServers::pin`]) before
/// using this mode.
///
/// # Errors
///
/// Returns [`ClientError`] if dialing or the pinned-key IK handshake fails,
/// or an I/O error kills the connection.
pub fn run_daemon_tcp_connection_pinned(
    addr: &str,
    handle_daemon_message: impl FnMut(DaemonMessage),
    from_ui: CrossbeamReceiver<ClientMessage>,
    shutdown_rx: Option<CrossbeamReceiver<()>>,
) -> Result<(), ClientError> {
    let known = crate::known_servers::KnownServers::load()?;
    let pinned = known
        .lookup(addr)?
        .ok_or_else(|| {
            ClientError::Io(std::io::Error::other(format!(
                "no pinned server key for {addr}: complete first contact (probe + base64 confirmation) before using the pinned mode"
            )))
        })?;

    info!(
        addr,
        pubkey = %choreo_transport::key::pubkey_base64(&pinned),
        "connecting with pinned server key"
    );

    // The keypair is loaded BEFORE the dial, as on every other connect path, so
    // a local identity failure never leaves a dialed socket to drop.
    let (client_sk, _client_pk) = client_transport_keypair()?;

    // Dial as its own step (plain error, no trust guidance — the daemon
    // being down has nothing to do with the pin).
    let tcp = dial_control_tcp(addr).map_err(ClientError::Io)?;

    // The dial already succeeded, so a `ConnectionRefused` coming back from
    // `ik_handshake_and_serve` can only be its handshake-REFUSAL mapping —
    // exactly the case where the re-pair guidance belongs. Later failures (a
    // mid-session disconnect) and non-refusals (a handshake timeout, a broken
    // socket) still pass through untouched: neither has anything to do with the
    // pin and neither must suggest re-pairing.
    ik_handshake_and_serve(
        tcp,
        client_sk.as_bytes(),
        &pinned,
        handle_daemon_message,
        from_ui,
        shutdown_rx,
    )
    .map_err(|e| {
        match &e {
            ClientError::Io(io)
                if io.kind() == std::io::ErrorKind::ConnectionRefused =>
            {
                // Multi-line guidance: the public key is what the user
                // compares against the daemon operator's out-of-band
                // readout; the two follow-up lines separate the expected
                // remediation from the warning when the change was NOT
                // expected.
                let msg = format!(
                    "handshake with the pinned server key failed: {e}\n\
                     pinned server public key for {addr}: {}\n\
                     if the server's key legitimately changed, remove the entry for {addr} from known_servers.toml and reconnect to re-confirm the new public key;\n\
                     if you did NOT expect a change, do not reconnect — investigate the network first",
                    choreo_transport::key::pubkey_base64(&pinned)
                );
                ClientError::Io(std::io::Error::other(msg))
            }
            _ => e,
        }
    })
}
