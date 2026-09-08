//! The embedded (in-process) daemon transport — "Option B".
//!
//! An embedder (a GUI process) spawns the daemon core in-process and talks to
//! it over plain channels: `ClientMessage`s travel GUI→daemon as Rust values,
//! `DaemonMessage`s daemon→GUI likewise. The connection NEVER becomes bytes —
//! no msgpack, no AES-GCM, no socket — and there is no polling anywhere:
//!
//! * the GUI blocks on `EmbeddedLink::daemon_rx.recv()` (kernel-descheduled
//!   channel wait);
//! * the daemon's connection thread blocks on `for msg in client_rx`;
//! * channel close is the EOF in both directions.
//!
//! The state machine is the SAME [`ClientConn`] the Unix and TCP/Noise
//! transports run (see `server::connection`), so every daemon behavior —
//! session attach, lag accounting, eviction, the shutdown broadcast — is
//! transport-independent by construction. Only the read loop (channel instead
//! of socket) and the writer buffer ([`ChannelConnectionWriter`] in
//! `server::connection`, which forwards values instead of bytes) differ.

use crate::daemon::{DaemonCommand, DaemonState};
use crate::server::core::{CoreOptions, DaemonCore, start_daemon_core};
use choreo_proto::{ClientMessage, DaemonMessage};
use std::io;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::mpsc;
use std::thread;
use std::time::Instant;
use tracing::{error, info, warn};

/// Options for [`spawn_embedded`]. Currently empty — deliberately an
/// extensible struct (like the daemon CLI's flags) so future knobs
/// (`tcp_addr` to ALSO expose the embedded daemon over TCP/Noise, a
/// `tool_policy` override, metrics) can be added without breaking embedder
/// call sites. `Default::default()` is the intended construction.
#[derive(Debug, Default, Clone)]
pub struct EmbeddedOptions {}

/// The GUI's handle on one embedded connection.
///
/// * `client_tx` — send `ClientMessage`s INTO the daemon (values, no codec).
/// * `daemon_rx` — receive `DaemonMessage`s FROM the daemon (values). The
///   receiver ends with `Err`/`None` when the link is dropped, the daemon
///   shuts down (after a `DaemonMessage::ShuttingDown` value), or this client
///   is evicted (after an `Evicted` value) — notify-before-close, delivered
///   by the same single-writer contract the socket transports use.
pub struct EmbeddedLink {
    pub client_tx: crossbeam_channel::Sender<ClientMessage>,
    pub daemon_rx: crossbeam_channel::Receiver<DaemonMessage>,
}

/// An embedded daemon: the transport-independent core ([`DaemonCore`]) plus
/// the connection-thread accounting the accept paths do in `run_server`.
///
/// # Drop semantics
///
/// Dropping an `EmbeddedDaemon` WITHOUT calling [`EmbeddedDaemon::shutdown`]
/// is a defect the type makes loud (an `info!` log) but does not crash on:
/// dropping `daemon_tx` closes the command loop (which drains sessions and
/// MCP), but no `ShuttingDown` broadcast is sent and no connection threads
/// are bounded-joined, so a wedged connection thread could outlive the
/// embedder's intent. `shutdown()` is the required teardown path.
pub struct EmbeddedDaemon {
    /// The transport-independent core. `Some` until [`shutdown`](Self::shutdown)
    /// takes it; an `Option` field (not a plain field) because the struct has
    /// a `Drop` impl, which forbids moving fields out — the shutdown drain
    /// uses `take()` instead, which is a plain overwrite.
    core: Option<DaemonCore>,
    /// Command channel to the daemon command loop (clone of the core's, so
    /// `connect()` can register writers while the core keeps its own sender).
    daemon_tx: Option<mpsc::Sender<DaemonCommand>>,
    /// Daemon-wide delivery-lag byte counter (exception #6), cloned from the
    /// core so `connect()` hands the connection thread the SAME counter the
    /// command loop and session threads increment on enqueue.
    global_lag: Arc<AtomicUsize>,
    /// Shared live-connection counter backing MAX_CONCURRENT_CONNECTIONS —
    /// the SAME counter the Unix and TCP accept paths use, so the cap applies
    /// uniformly across all three transports.
    conn_count: Arc<AtomicUsize>,
    /// Ferry for connection-thread JoinHandles (same pattern as the TCP
    /// accept thread in `lifecycle.rs`): the accept path here is `connect()`
    /// itself, spawning on the CALLER's thread, so handles are sent over the
    /// channel and drained by `shutdown()`.
    handle_tx: mpsc::Sender<thread::JoinHandle<()>>,
    handle_rx: Option<mpsc::Receiver<thread::JoinHandle<()>>>,
    /// Set by `shutdown()` so `Drop` can tell "clean drain" from the
    /// documented defect (drop without shutdown).
    shut_down: bool,
}

/// Assemble an embedded daemon from already-opened [`DaemonState`]: the
/// transport-independent core (command loop, catalog maintenance, config
/// watchers) and NOTHING transport-specific — no Unix socket, no TCP
/// listener, no signal threads, no metrics server.
///
/// `CoreOptions { acl: None, config_watchers: true }`: the embedder owns its
/// ACL policy upstream (an app sandbox has no `authorized_clients.toml` to
/// share), but the config watchers are ON because they degrade gracefully —
/// accounts.toml / models-overlay auto-reload still work inside an app
/// sandbox where the standard config dir resolves, and simply never fire
/// where it does not.
pub fn spawn_embedded(state: DaemonState, _opts: EmbeddedOptions) -> io::Result<EmbeddedDaemon> {
    info!("spawning embedded daemon core");
    let core = start_daemon_core(
        state,
        CoreOptions {
            acl: None,
            config_watchers: true,
        },
    )?;
    let daemon_tx = core.daemon_tx.clone();
    let conn_count = Arc::clone(&core.conn_count);
    let global_lag = Arc::clone(&core.global_lag);
    // JoinHandle ferry: connect() sends each connection thread's handle here;
    // shutdown() drains and bounded-joins them (same pattern as the TCP
    // accept thread's `tcp_client_tx`/`tcp_client_rx` in lifecycle.rs).
    let (handle_tx, handle_rx) = mpsc::channel();
    info!("embedded daemon core started");
    Ok(EmbeddedDaemon {
        core: Some(core),
        daemon_tx: Some(daemon_tx),
        global_lag,
        conn_count,
        handle_tx,
        handle_rx: Some(handle_rx),
        shut_down: false,
    })
}

impl EmbeddedDaemon {
    /// Open a new embedded connection, mirroring the TCP accept arm in
    /// `run_server`:
    ///
    /// 1. take a [`ConnectionSlot`] (MAX_CONCURRENT_CONNECTIONS applies to
    ///    embedded connections too — a wedged GUI is bounded like a wedged
    ///    socket client);
    /// 2. register the writer channel with the daemon BEFORE spawning the
    ///    connection thread (ordering invariant, see
    ///    `register_client_writer`: a concurrently-shutting-down client is
    ///    guaranteed to still receive `ShuttingDown`);
    /// 3. spawn [`embedded_client_thread`], ferrying its JoinHandle over the
    ///    handle channel.
    ///
    /// `connect()` returns only after the connection thread is spawned, so
    /// early GUI sends (`ListSessions`, `SubscribeSessionsSummary`, …)
    /// simply QUEUE in the unbounded channel instead of racing a "socket not
    /// ready yet" window: there is no handshake, so there is nothing to be
    /// not-ready.
    pub fn connect(&self) -> io::Result<EmbeddedLink> {
        // Enforce the concurrent-connection cap exactly like both accept
        // paths: at the cap the connection is refused (not queued).
        let Some(slot) = crate::server::lifecycle::try_take_connection_slot(&self.conn_count)
        else {
            warn!(
                "embedded connection rejected: at the {} concurrent-connection cap",
                crate::server::lifecycle::MAX_CONCURRENT_CONNECTIONS
            );
            return Err(io::Error::new(
                io::ErrorKind::ConnectionRefused,
                "too many concurrent connections",
            ));
        };
        let (client_tx, client_rx) = crossbeam_channel::unbounded::<ClientMessage>();
        let (out_tx, out_rx) = crossbeam_channel::unbounded::<DaemonMessage>();
        // Register BEFORE spawning (see register_client_writer): the register
        // command is enqueued before any shutdown broadcast can be, so a
        // client connected concurrently with shutdown cannot miss
        // ShuttingDown.
        let Some(daemon_tx) = self.daemon_tx.clone() else {
            // shutdown() already ran (or is running): refuse cleanly. The
            // slot was taken above and is dropped here, releasing its count.
            return Err(io::Error::other(
                "embedded daemon is shut down; cannot connect",
            ));
        };
        let (client_id, writer, writer_rx) =
            crate::server::connection::register_client_writer(&daemon_tx);
        // Cloned BEFORE the closure: the closure must capture only owned
        // values — `&self` is not Send, so anything read off `self` inside
        // the spawned thread would fail to compile.
        let global_lag = Arc::clone(&self.global_lag);
        info!(client_id, "embedded client connecting");
        crate::metrics::record_connection_accepted();
        let handle = thread::spawn(move || {
            // Held for the connection thread's whole lifetime: released
            // (decrementing the cap counter) on exit, even on panic.
            let _slot = slot;
            let args = crate::server::connection::EmbeddedConnArgs {
                client_rx,
                out_tx,
                daemon_tx,
                client_id,
                writer,
                writer_rx,
                global_lag,
            };
            if let Err(e) = crate::server::connection::embedded_client_thread(args) {
                error!(error = %e, "embedded client error");
            }
        });
        // Ferry the handle for the shutdown drain (same pattern as the TCP
        // accept thread). A send to a live `handle_tx` cannot fail while the
        // struct is alive: a sender clone is held for its lifetime. The
        // clone is taken BEFORE the closure so the closure captures only the
        // Sender (a Receiver is not Sync and cannot be shared into the
        // spawn's `&self` reference).
        let handle_tx = self.handle_tx.clone();
        let _ = handle_tx.send(handle);
        Ok(EmbeddedLink {
            client_tx,
            daemon_rx: out_rx,
        })
    }

    /// Drain the embedded daemon — the same sequence `run_server` runs at
    /// shutdown, minus the sockets:
    ///
    /// 1. `BroadcastShuttingDown` — every connected client's writer thread
    ///    delivers `DaemonMessage::ShuttingDown` as a VALUE, then closes its
    ///    channel, so the GUI observes the notification before the close;
    /// 2. `Shutdown` — the command loop drains sessions and MCP;
    /// 3. drop the command channel and join the command-loop thread;
    /// 4. drain the JoinHandle ferry and bounded-join every connection
    ///    thread against the shared [`CONNECTION_DRAIN_GRACE`] deadline.
    ///
    /// Consuming `self` is the point: no link can be opened after or during
    /// the drain, and `Drop` is suppressed for the taken fields.
    pub fn shutdown(mut self) {
        info!("embedded daemon: shutting down");
        // Stage 1 + 2: the two commands on the same FIFO command channel, so
        // the broadcast is processed before the stop — every connected client
        // gets ShuttingDown through its writer thread first.
        if let Some(daemon_tx) = self.daemon_tx.take() {
            let _ = daemon_tx.send(DaemonCommand::BroadcastShuttingDown);
            let _ = daemon_tx.send(DaemonCommand::Shutdown);
            // Dropping the last command sender closes the command loop.
            drop(daemon_tx);
        }
        // Stage 3: the command loop's teardown (session joins, MCP shutdown)
        // happens inside the thread; joining here waits for all of it.
        if let Some(core) = self.core.take() {
            if let Err(e) = core.cmd_handle.join() {
                error!("command thread panicked during shutdown: {e:?}");
            }
            info!("embedded daemon: command loop joined");
        }
        // Stage 4: bounded-join every connection thread (and, through
        // cleanup, its writer thread) against ONE shared deadline, so N
        // wedged clients cost ~one grace period, not N × grace.
        if let Some(handle_rx) = self.handle_rx.take() {
            let handles: Vec<_> = handle_rx.try_iter().collect();
            let deadline = Instant::now() + crate::server::lifecycle::CONNECTION_DRAIN_GRACE;
            info!(
                connection_threads = handles.len(),
                "draining embedded connection threads"
            );
            for handle in handles {
                crate::server::lifecycle::join_thread_bounded(handle, deadline);
            }
        }
        self.shut_down = true;
        info!("embedded daemon: shutdown complete");
    }
}

impl Drop for EmbeddedDaemon {
    fn drop(&mut self) {
        if !self.shut_down {
            // Dropping without shutdown() leaves the core DETACHED: dropping
            // daemon_tx closes the command loop (sessions/MCP drain happens
            // in the loop's teardown), but no ShuttingDown was broadcast and
            // no connection threads were joined — wedged ones can outlive
            // the embedder's intent. Loud info, no panic: the defect is
            // observable in logs, not a crash surface.
            info!(
                "EmbeddedDaemon dropped without shutdown(); the daemon core is left \
                 detached — call shutdown() for the ordered drain"
            );
        }
    }
}
