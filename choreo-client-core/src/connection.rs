use crate::error::ClientError;
use choreo_proto::{ClientMessage, DaemonMessage, ProtoError, read_message, write_message};
use choreo_transport::error::TransportError;
use choreo_transport::handshake::{
    PREAMBLE_IK, PREAMBLE_XX, handshake_initiator, handshake_initiator_xx,
};
use choreo_transport::key::ensure_transport_keypair;
use std::io::{BufRead, BufReader, BufWriter, Write};
#[cfg(unix)]
use std::os::unix::net::UnixStream;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use tracing::{debug, error, info, warn};
// Windows: std::os::windows::net::UnixStream is unstable (E0658, feature
// `windows_unix_domain_sockets`, rust-lang/rust#150487), so uds_windows provides
// the same connect/try_clone/shutdown API over named pipes.
#[cfg(windows)]
use uds_windows::UnixStream;

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

pub fn run_daemon_connection(
    socket_path: &str,
    handle_daemon_message: impl FnMut(DaemonMessage),
    from_ui: mpsc::Receiver<choreo_proto::ClientMessage>,
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
    Tcp { addr: String, server_pk: [u8; 32] },
    /// Connect via TCP/Noise IK against the key PINNED in
    /// `known_servers.toml` for the address. The pin is loaded at connect
    /// time; a handshake failure is reported WITH the pinned fingerprint and
    /// the re-pair guidance, so a server key change is loud instead of an
    /// opaque connection error (the known_hosts behavior).
    TcpPinned(String),
}

impl Default for ConnectionMode {
    fn default() -> Self {
        ConnectionMode::UnixSocket(choreo_proto::socket_path())
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
    handle_daemon_message: impl FnMut(DaemonMessage),
    from_ui: mpsc::Receiver<ClientMessage>,
    shutdown_rx: Option<mpsc::Receiver<()>>,
) -> Result<(), ClientError> {
    info!("connecting to daemon at {addr}");

    // Load the client transport keypair (generates one if absent).
    let (client_sk, _client_pk) =
        ensure_transport_keypair().map_err(|e| ClientError::Io(std::io::Error::other(e)))?;

    // Dial FIRST, as its own step. Keeping the dial and the handshake
    // separate is what lets the pinned-mode wrapper (see
    // `run_daemon_tcp_connection_pinned`) attach trust guidance to
    // HANDSHAKE failures only — a plain dial failure (daemon down, network
    // down) is reported verbatim and must never suggest re-pairing.
    let tcp = std::net::TcpStream::connect(addr).map_err(ClientError::Io)?;

    ik_handshake_and_serve(
        tcp,
        client_sk.as_bytes(),
        server_pk,
        handle_daemon_message,
        from_ui,
        shutdown_rx,
    )
}

/// The 1-byte handshake-mode preamble (TCP wire v5) goes out BEFORE any
/// handshake message, then the Noise IK handshake runs. Returns the raw
/// `TransportError` so callers can classify the failure (the
/// ConnectionRefused wrapping lives one layer up, in
/// [`ik_handshake_and_serve`]). Shared by the session-opening path and the
/// [`verify_daemon_authorization`] preflight so both exercise the exact
/// same wire sequence the daemon will judge.
fn ik_handshake_raw(
    mut tcp: std::net::TcpStream,
    client_sk: &[u8; 32],
    server_pk: &[u8; 32],
) -> Result<choreo_transport::noise::NoiseStream, TransportError> {
    // The preamble is unauthenticated by design — the daemon uses it
    // only to pick which equally-authenticated handshake to run; the IK
    // handshake itself authenticates both static keys and gates the ACL.
    // A single-byte write to a fresh blocking socket cannot meaningfully
    // block (it fits any socket buffer), so no timeout is armed for it.
    tcp.write_all(&[PREAMBLE_IK])?;
    handshake_initiator(tcp, client_sk, server_pk)
}

/// Preamble + Noise IK handshake over an ALREADY-DIALED TCP stream, then
/// the encrypted session loop. Shared by [`run_daemon_tcp_connection`] and
/// [`run_daemon_tcp_connection_pinned`] so both paths run the identical
/// preamble+handshake sequence. A handshake failure surfaces as
/// `ConnectionRefused` (the pre-existing wire convention) — which is why
/// the caller must dial separately: only then does a `ConnectionRefused`
/// returned from here unambiguously mean the HANDSHAKE failed, not the
/// network.
fn ik_handshake_and_serve(
    tcp: std::net::TcpStream,
    client_sk: &[u8; 32],
    server_pk: &[u8; 32],
    handle_daemon_message: impl FnMut(DaemonMessage),
    from_ui: mpsc::Receiver<ClientMessage>,
    shutdown_rx: Option<mpsc::Receiver<()>>,
) -> Result<(), ClientError> {
    let noise = ik_handshake_raw(tcp, client_sk, server_pk).map_err(|e| {
        ClientError::Io(std::io::Error::new(
            std::io::ErrorKind::ConnectionRefused,
            e,
        ))
    })?;
    serve_noise_connection(noise, handle_daemon_message, from_ui, shutdown_rx)
}

/// Connect to a daemon via Noise XX over TCP (first contact).
///
/// Use this when the client has NO pinned server public key for `addr`: the
/// XX handshake reveals the server's static key, which is handed to
/// `on_first_contact` BEFORE the encrypted session loop starts. The caller
/// is expected to render the key's fingerprint for a human and return
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
/// thread shape, same shutdown semantics — see [`serve_noise_connection`]).
pub fn run_daemon_tcp_connection_xx_first_contact(
    addr: &str,
    handle_daemon_message: impl FnMut(DaemonMessage),
    from_ui: mpsc::Receiver<ClientMessage>,
    shutdown_rx: Option<mpsc::Receiver<()>>,
    on_first_contact: impl FnOnce([u8; 32]) -> bool,
) -> Result<(), ClientError> {
    info!("first-contact connect to daemon at {addr}");

    // Load the client transport keypair (generates one if absent).
    let (client_sk, _client_pk) =
        ensure_transport_keypair().map_err(|e| ClientError::Io(std::io::Error::other(e)))?;

    // Connect TCP, declare first-contact mode, run Noise XX.
    let mut tcp = std::net::TcpStream::connect(addr).map_err(ClientError::Io)?;
    // Same preamble reasoning as the IK path: unauthenticated mode selector,
    // authenticated by nothing, needed for nothing — the handshake that
    // follows carries all the cryptographic guarantees.
    tcp.write_all(&[PREAMBLE_XX]).map_err(ClientError::Io)?;
    let (noise, server_pk) = handshake_initiator_xx(tcp, client_sk.as_bytes()).map_err(|e| {
        ClientError::Io(std::io::Error::new(
            std::io::ErrorKind::ConnectionRefused,
            e,
        ))
    })?;

    // Trust gate: the caller verifies the learned key out-of-band. On
    // refusal, drop the (already-established) encrypted transport without
    // sending a single protocol message — the connection simply closes.
    if !on_first_contact(server_pk) {
        info!("first-contact trust rejected by caller; closing connection");
        return Ok(());
    }

    serve_noise_connection(noise, handle_daemon_message, from_ui, shutdown_rx)
}

/// Shared tail of both TCP connection modes: the encrypted session loop over
/// an already-established `NoiseStream`.
///
/// Uses two blocking threads:
/// - Reader thread (the caller's): blocks on NoiseStream::recv_daemon_message()
/// - Writer thread: blocks on from_ui.recv_timeout()
/// - Shutdown: blocks on shutdown_rx.recv(), then shuts down the TCP stream
///
/// Extracted so [`run_daemon_tcp_connection`] (IK) and
/// [`run_daemon_tcp_connection_xx_first_contact`] (XX) share one writer/reader
/// implementation — the two modes differ ONLY in preamble + handshake, not in
/// how the established transport is served.
fn serve_noise_connection(
    mut noise: choreo_transport::noise::NoiseStream,
    mut handle_daemon_message: impl FnMut(DaemonMessage),
    from_ui: mpsc::Receiver<ClientMessage>,
    shutdown_rx: Option<mpsc::Receiver<()>>,
) -> Result<(), ClientError> {
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

/// Probe a daemon's static public key without establishing a session.
///
/// Performs the XX first-contact handshake ONLY: connect, preamble, the
/// three handshake messages, extract the learned server key, and DROP the
/// stream — no data-plane message is ever sent or received. This is the
/// building block UIs use to implement the trust flow synchronously (in a
/// normal-mode terminal, before any TUI/GUI starts): learn the key, show
/// the fingerprint, get the human's confirmation, pin, and only then open
/// the real connection with [`ConnectionMode::Tcp`] / IK.
///
/// The probe itself authenticates NOTHING about the server (there is no pin
/// yet — that is the point), but it does authenticate the probe TO the
/// daemon, so the daemon's ACL applies: probing requires the client's key
/// to be enrolled. The daemon ACL check completes inside the XX handshake
/// (after message 3), so a not-yet-enrolled client gets `Err` here too.
pub fn probe_server_key(addr: &str) -> Result<[u8; 32], ClientError> {
    info!("probing server key at {addr} (XX first contact)");

    let (client_sk, _client_pk) =
        ensure_transport_keypair().map_err(|e| ClientError::Io(std::io::Error::other(e)))?;

    let mut tcp = std::net::TcpStream::connect(addr).map_err(ClientError::Io)?;
    // Same preamble contract as the session-opening XX path.
    tcp.write_all(&[PREAMBLE_XX]).map_err(ClientError::Io)?;
    let (noise, server_pk) = handshake_initiator_xx(tcp, client_sk.as_bytes()).map_err(|e| {
        ClientError::Io(std::io::Error::new(
            std::io::ErrorKind::ConnectionRefused,
            e,
        ))
    })?;

    // Drop the established transport immediately: the probe never speaks
    // the protocol. Closing here is also what keeps the daemon's connection
    // slot usage bounded — the real connection opens fresh afterwards.
    drop(noise);
    debug!(addr, "server key probe complete; transport dropped");
    Ok(server_pk)
}

/// Why a preflight authorization check failed (see
/// [`verify_daemon_authorization`]). The two cases need DIFFERENT
/// remediation — "start the daemon / check the network" vs "get your key
/// enrolled" — so they are distinguished at the type level instead of by
/// string-matching an `io::ErrorKind` (a dial refusal and a handshake
/// rejection both involve connection-level I/O and must not be conflated).
#[derive(Debug, thiserror::Error)]
pub enum PreflightError {
    /// The daemon could not be reached at all (dial failure).
    #[error("cannot reach the daemon: {0}")]
    Unreachable(#[source] std::io::Error),
    /// The daemon was reached but the Noise IK handshake failed: the
    /// daemon either does not hold the expected server key, or its ACL
    /// did not admit this client's transport key. (An IK handshake is the
    /// ONLY probe that can detect the ACL rejection — the daemon aborts
    /// the handshake before message 2 — which is why this preflight runs
    /// IK even on a first-contact flow that already probed with XX.)
    #[error("the daemon rejected the connection handshake: {0}")]
    Rejected(#[source] TransportError),
}

/// The client's OWN Noise transport public key (generating the on-disk
/// keypair first if absent). This is the identity the daemon's ACL judges:
/// UIs embed it (and its fingerprint) in enrollment-remediation messages so
/// a TUI-only user never needs the daemon binary just to read out their key.
pub fn own_transport_pubkey() -> Result<[u8; 32], ClientError> {
    let (_sk, pk) =
        ensure_transport_keypair().map_err(|e| ClientError::Io(std::io::Error::other(e)))?;
    Ok(pk)
}

/// Verify that the daemon at `addr` will actually admit this client BEFORE
/// the caller commits to a full session (the TUI runs this before starting
/// any UI).
///
/// Dials `addr`, runs the complete Noise IK handshake against `server_pk`,
/// and immediately drops the established transport — no protocol message is
/// ever sent. A successful IK handshake proves BOTH properties at once:
/// the daemon holds the expected static key (the handshake authenticates
/// it) AND the daemon's ACL admitted this client's key (the responder
/// checks the ACL mid-handshake and closes the connection before completing
/// it when the client is not enrolled). No other check can detect the
/// enrollment case: the XX first-contact probe completes client-side before
/// the daemon's ACL check runs, so it always "succeeds" for un-enrolled
/// clients.
///
/// The cost is one extra handshake per connect — negligible against the
/// session it gates, and it converts "TUI starts, then dies with a cryptic
/// I/O error on first use" into a clear refusal before any UI exists.
pub fn verify_daemon_authorization(addr: &str, server_pk: &[u8; 32]) -> Result<(), PreflightError> {
    info!(
        addr,
        "authorization preflight: probing daemon with IK handshake"
    );

    let (client_sk, _client_pk) = ensure_transport_keypair()
        .map_err(|e| PreflightError::Unreachable(std::io::Error::other(e)))?;

    // Dial failure = the daemon is down / unreachable — reported as-is so
    // the caller's message can point at the network, not at enrollment.
    let tcp = std::net::TcpStream::connect(addr).map_err(PreflightError::Unreachable)?;

    // Handshake failure = rejection (wrong server key OR this client not
    // enrolled in the daemon's ACL). Classified as `Rejected`; the caller
    // renders the remediation.
    let noise =
        ik_handshake_raw(tcp, client_sk.as_bytes(), server_pk).map_err(PreflightError::Rejected)?;

    // The handshake succeeded — the daemon will admit us. Drop the
    // transport; the real session opens fresh (the daemon cleans the
    // preflight connection up through its normal disconnect path).
    drop(noise);
    debug!(addr, "authorization preflight passed");
    Ok(())
}

/// Connect via Noise IK against the key PINNED in `known_servers.toml` for
/// `addr` (the [`ConnectionMode::TcpPinned`] path).
///
/// The whole point of this wrapper is the failure UX: a HANDSHAKE failure
/// against the pinned key carries the pinned fingerprint and the explicit
/// re-pair instructions, so a server key change is a loud, actionable
/// message rather than an opaque error (the known_hosts behavior). The
/// dial is performed HERE, as a separate step, so a network-down daemon is
/// reported as a plain connect error WITHOUT the re-pair guidance — the
/// remediation advice is reserved for the one failure it actually applies
/// to (the server's key changed).
///
/// Errors if no pin exists for `addr` — callers must resolve first contact
/// (probe + confirm + [`KnownServers::pin`]) before using this mode.
pub fn run_daemon_tcp_connection_pinned(
    addr: &str,
    handle_daemon_message: impl FnMut(DaemonMessage),
    from_ui: mpsc::Receiver<ClientMessage>,
    shutdown_rx: Option<mpsc::Receiver<()>>,
) -> Result<(), ClientError> {
    let known = crate::known_servers::KnownServers::load()?;
    let pinned = known
        .lookup(addr)?
        .ok_or_else(|| {
            ClientError::Io(std::io::Error::other(format!(
                "no pinned server key for {addr}: complete first contact (probe + fingerprint confirmation) before using the pinned mode"
            )))
        })?;

    info!(
        addr,
        fingerprint = %choreo_transport::key::fingerprint(&pinned),
        "connecting with pinned server key"
    );

    // Dial as its own step (plain error, no trust guidance — the daemon
    // being down has nothing to do with the pin).
    let tcp = std::net::TcpStream::connect(addr).map_err(ClientError::Io)?;

    let (client_sk, _client_pk) =
        ensure_transport_keypair().map_err(|e| ClientError::Io(std::io::Error::other(e)))?;

    // The dial already succeeded, so a `ConnectionRefused` coming back from
    // `ik_handshake_and_serve` can only be its handshake-failure wrapper —
    // exactly the case where the re-pair guidance belongs. Later failures
    // (a mid-session disconnect) still pass through untouched: a broken
    // pipe has nothing to do with the pin and must not suggest re-pairing.
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
                // Multi-line guidance: the fingerprint is what the user
                // compares against the daemon operator's out-of-band
                // readout; the two follow-up lines separate the expected
                // remediation from the warning when the change was NOT
                // expected.
                let msg = format!(
                    "handshake with the pinned server key failed: {e}\n\
                     pinned fingerprint for {addr}: {}\n\
                     if the server's key legitimately changed, remove the entry for {addr} from known_servers.toml and reconnect to re-confirm the new fingerprint;\n\
                     if you did NOT expect a change, do not reconnect — investigate the network first",
                    choreo_transport::key::fingerprint(&pinned)
                );
                ClientError::Io(std::io::Error::other(msg))
            }
            _ => e,
        }
    })
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
        ConnectionMode::TcpPinned(addr) => {
            run_daemon_tcp_connection_pinned(&addr, handle_daemon_message, from_ui, shutdown_rx)
        }
    }
}
