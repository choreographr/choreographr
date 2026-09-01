use crate::daemon::DaemonCommand;
use crate::sessions::SessionCommand;
use choreo_proto::{
    ClientMessage, ContextConfig, DaemonMessage, ProtoError, SessionEvent, read_message,
    write_message,
};
use std::io::{self, BufReader, BufWriter, Write};
use std::net::{Shutdown, TcpStream};
#[cfg(unix)]
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use tracing::{debug, error, info, warn};
#[cfg(windows)]
use uds_windows::UnixStream;

/// Bound for joining a connection's writer thread during cleanup. A healthy
/// writer exits immediately on channel disconnect (dropping `writer_tx` in
/// `cleanup_client` disconnects `writer_rx`); the grace covers a writer
/// wedged in a blocking socket write — a client that is open but not reading
/// — which cannot exit on the channel disconnect alone (see the comment at
/// the call site in `cleanup_client`).
const WRITER_JOIN_GRACE: Duration = Duration::from_secs(5);

/// Socket write timeout applied to every connection's writer. Bounds a single
/// blocking `write` syscall so a wedged client — one whose socket receive
/// window is permanently zero — cannot stall its writer thread forever.
///
/// This is the mechanism that makes LAG EVICTION work without the daemon
/// holding a force-close handle on the connection (no retained socket clone,
/// no extra FD per connection): when the daemon evicts a lagging client it
/// enqueues the best-effort `Evicted` advisory and drops every sink; a
/// healthy writer flushes the advisory and closes its own socket (notify-
/// before-EOF), while a wedged writer hits this timeout on its in-flight
/// write, the write fails, and the writer shuts the socket down itself —
/// which unblocks the reader's blocking read and runs the normal
/// `cleanup_client` teardown. Either way the connection is reaped promptly
/// and its queued bytes released.
///
/// A slow-but-alive client is never falsely killed: the timeout is per
/// syscall, so a socket that makes any progress (each write completes in
/// under this) survives; only a client that stops reading entirely trips it,
/// which is exactly the lag condition eviction targets.
const WRITER_WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// A per-connection message sink implementing the single-writer contract.
///
/// Both transports (Unix socket and TCP/Noise) implement this so the writer
/// thread loop in [`writer_thread`] lives in exactly one place. The
/// `ShuttingDown` special case — flush the notification, close the socket,
/// stop draining — is what makes notify-before-EOF deterministic: the thread
/// that writes the message is the same thread that closes the socket.
trait ConnectionWriter {
    /// Serialize and send one message. Errors are fatal for the connection
    /// (the socket is broken) — the caller stops draining.
    fn send_message(&mut self, msg: &DaemonMessage) -> Result<(), String>;
    /// Close the underlying socket (both directions).
    fn shutdown(&mut self);
}

impl ConnectionWriter for BufWriter<UnixStream> {
    fn send_message(&mut self, msg: &DaemonMessage) -> Result<(), String> {
        write_message(self, msg).map_err(|e| e.to_string())?;
        self.flush().map_err(|e| e.to_string())
    }
    fn shutdown(&mut self) {
        let _ = self.get_ref().shutdown(Shutdown::Both);
    }
}

impl ConnectionWriter for choreo_transport::noise::NoiseStream {
    fn send_message(&mut self, msg: &DaemonMessage) -> Result<(), String> {
        self.send_daemon_message(msg).map_err(|e| e.to_string())
    }
    fn shutdown(&mut self) {
        let _ = self.get_ref().shutdown(Shutdown::Both);
    }
}

/// Drain a connection's writer channel — the connection's SOLE writer.
///
/// Each connection has exactly one writer thread, so messages on `rx` are
/// serialized and fragments of one logical message can never interleave.
/// `ShuttingDown` and `Evicted` are special-cased identically: each is
/// flushed, then the socket is closed HERE (by the writer thread itself), so
/// the client observes the notification before the EOF with no other thread
/// ever writing to or closing the socket. (ShuttingDown is only ever enqueued
/// by the daemon's shutdown broadcast; Evicted by a lag eviction.) An error at
/// any point stops the loop and SHUTS THE SOCKET DOWN — a send error can be a
/// broken pipe (socket gone) or a [`WRITER_WRITE_TIMEOUT`] on a wedged client
/// whose receive window is zero (socket still open); either way, shutting
/// down unblocks the reader's blocking read so `cleanup_client` reaps the
/// connection (shutdown on an already-broken socket is a harmless no-op).
///
/// Byte accounting: on EACH dequeue the per-client and daemon-wide lag
/// counters are decremented by the message's approximate wire size, the exact
/// counterpart of [`SubscriberSink::enqueue`]'s increment. Decrementing even
/// on a failed send keeps the daemon-wide backlog honest — the bytes left the
/// queue regardless of whether the socket accepted them, and the connection
/// is being torn down either way. Whatever is still QUEUED when the loop
/// stops (send error, or the `Evicted`/`ShuttingDown` stop) is drained below
/// the loop and decremented too, so an abandoned backlog can never stay
/// frozen in the daemon-wide counter and silently eat the global budget.
fn writer_thread<W: ConnectionWriter>(
    mut writer: W,
    rx: crossbeam_channel::Receiver<DaemonMessage>,
    bytes: Arc<AtomicUsize>,
    global: Arc<AtomicUsize>,
) {
    for msg in &rx {
        let size = msg.approx_wire_size();
        if let Err(e) = writer.send_message(&msg) {
            warn!("writer thread error: {e}");
            // The failing message still left the queue — account it so the
            // backlog reflects what is actually still queued, then stop.
            bytes.fetch_sub(size, Ordering::Relaxed);
            global.fetch_sub(size, Ordering::Relaxed);
            writer.shutdown();
            break;
        }
        bytes.fetch_sub(size, Ordering::Relaxed);
        global.fetch_sub(size, Ordering::Relaxed);
        if matches!(msg, DaemonMessage::ShuttingDown | DaemonMessage::Evicted) {
            writer.shutdown();
            break;
        }
    }
    // Drain-and-decrement whatever is still queued: after a send error or a
    // ShuttingDown/Evicted stop, the socket is closed and these messages will
    // never be written — but they were all counted at enqueue. Subtracting
    // them here keeps the daemon-wide total honest (the per-client counter
    // dies with the sink, but `global` is shared across every client: an
    // evicted client's abandoned backlog would otherwise stay frozen in it
    // forever and, accumulated across evictions, permanently exhaust the
    // global budget — cascading evictions of healthy clients). The drain is
    // non-blocking on purpose: the writer must exit promptly so the receiver
    // drops and any producer that enqueues after this point gets a failed
    // send, which it self-corrects (see [`SubscriberSink::send_accounted`]).
    //
    // The one residual race, bounded and accepted: a producer whose `send`
    // lands in the microsecond window between this drain's last pass and the
    // receiver being dropped (at function return) SUCCEEDS — the receiver is
    // still alive — and that message is never dequeued, so its bytes stay in
    // the daemon-wide counter forever. The leak is bounded to whatever a
    // producer manages to enqueue in that window — in practice zero or one
    // message (the daemon removes the sink from its maps in the same command
    // that starts this teardown, so no producer keeps broadcasting to it
    // beyond a straggler or two) — and a producer that sends after the
    // receiver is gone self-corrects, so the accounting stays honest to
    // within that tiny, event-bounded slack. It is not a strict one-message
    // guarantee, but it is never an unbounded stream.
    for msg in rx.try_iter() {
        let size = msg.approx_wire_size();
        bytes.fetch_sub(size, Ordering::Relaxed);
        global.fetch_sub(size, Ordering::Relaxed);
    }
}

/// Create a connection's writer channel and register it with the daemon,
/// returning the client id and both channel ends for the connection thread.
///
/// Registration happens HERE — in the acceptor, BEFORE the connection thread
/// is spawned — so a connection accepted concurrently with shutdown is
/// guaranteed to receive `ShuttingDown`:
///
/// * Unix: the accept loop registers (then spawns) before it can observe the
///   shutdown flag and break out to broadcast, so the register command is
///   enqueued before the broadcast on the same FIFO command channel.
/// * TCP: the accept thread registers before spawning the handshake thread,
///   and `run_server` joins the accept thread BEFORE broadcasting, so the
///   register (sent strictly before the accept thread exited) is ordered
///   before the broadcast in the command channel.
///
/// If registration were deferred to inside the connection thread, a handshake
/// still in flight when shutdown began could land its register after the
/// broadcast was processed — and that client would miss the notification.
pub(crate) fn register_client_writer(
    daemon_tx: &mpsc::Sender<DaemonCommand>,
) -> (
    u64,
    crate::broadcast::SubscriberSink,
    crossbeam_channel::Receiver<DaemonMessage>,
) {
    let (writer_tx, writer_rx) = crossbeam_channel::unbounded::<DaemonMessage>();
    let sink = crate::broadcast::SubscriberSink::new(writer_tx);
    let client_id = rand::random::<u64>();
    let _ = daemon_tx.send(DaemonCommand::RegisterClientWriter {
        client_id,
        writer: sink.clone(),
    });
    (client_id, sink, writer_rx)
}

/// Send a reply to a client's writer channel.
///
/// Replies ride the same unbounded channel as broadcasts, so they MUST keep
/// the lag counters consistent: `send_accounted` increments both before the
/// send (the writer thread decrements them on every dequeue) and self-
/// corrects both if the receiver is gone. The lag limits are NOT enforced
/// here: a reply is a request/response contract that must never be dropped
/// (lossless for replies was already true — a blocking send never dropped,
/// it just blocked; with an unbounded channel it can no longer block
/// either), and replies are small and infrequent next to broadcast streams,
/// so they cannot meaningfully inflate a lagging client's backlog. A dropped
/// reply on a dead receiver is fine: the connection is being torn down
/// anyway.
fn send_to_writer(ctx: &ClientCtx, msg: DaemonMessage) {
    ctx.writer.send_accounted(&msg, ctx.global_lag);
}

/// Shared per-client context passed through the dispatch and handler functions.
/// Bundles the channels and mutable per-connection state into one struct so
/// the call sites don't pass 5–6 individual arguments to every function.
struct ClientCtx<'a> {
    /// This connection's delivery sink (see `send_to_writer`).
    writer: &'a crate::broadcast::SubscriberSink,
    /// Daemon-wide lag counter, shared by every connection; replies must
    /// increment it so the writer thread's per-dequeue decrement stays
    /// balanced (see `send_to_writer`).
    global_lag: &'a AtomicUsize,
    daemon_tx: &'a mpsc::Sender<DaemonCommand>,
    attached_session_id: &'a mut Option<u64>,
    attached_session_tx: &'a mut Option<mpsc::Sender<SessionCommand>>,
    client_id: u64,
}

/// Clean up a client connection: detach from session, unregister the summary
/// subscriber, wait for the writer thread to drain, and record the disconnect
/// metric.  Owns the writer_tx sender and writer handle so both are consumed.
fn cleanup_client(
    attached_session_tx: Option<mpsc::Sender<SessionCommand>>,
    client_id: u64,
    daemon_tx: &mpsc::Sender<DaemonCommand>,
    writer: crate::broadcast::SubscriberSink,
    writer_handle: std::thread::JoinHandle<()>,
) {
    if let Some(ref tx) = attached_session_tx {
        let _ = tx.send(SessionCommand::Detach { client_id });
    }
    let _ = daemon_tx.send(DaemonCommand::ClientDisconnected { client_id });
    drop(writer);
    // Join the writer with a bound: a wedged writer (client open but not
    // reading) is stuck in a blocking socket write and cannot exit on the
    // channel disconnect alone. Cleanup must not hang the connection thread on
    // that forever; the daemon shutdown drain is the backstop, and the
    // concurrent-connection cap bounds how many wedged writers can accumulate.
    // A writer that times out is detached — it keeps its socket until the
    // client goes away, then exits on its own.
    crate::server::lifecycle::join_thread_bounded(
        writer_handle,
        Instant::now() + WRITER_JOIN_GRACE,
    );
    crate::metrics::record_client_disconnected();
}

/// Dispatch a decoded ClientMessage through the shared handler functions.
/// Returns an error only when the daemon has disconnected (caller should
/// terminate the client connection).
fn dispatch_client_message(msg: ClientMessage, ctx: &mut ClientCtx) -> io::Result<()> {
    match msg {
        ClientMessage::CreateSession {
            title,
            parent_session_id,
            working_dir,
            context_config,
            account_name,
            selected_model,
            reasoning_effort,
        } => {
            if !handle_client_create_session(
                title,
                parent_session_id,
                working_dir,
                context_config,
                account_name,
                selected_model,
                reasoning_effort,
                ctx,
            ) {
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionAborted,
                    "daemon disconnected",
                ));
            }
        }
        ClientMessage::AttachSession { session_id } => {
            if !handle_client_attach_session(session_id, ctx) {
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionAborted,
                    "daemon disconnected",
                ));
            }
        }
        ClientMessage::ListSessions => {
            debug!("client {}: ListSessions", ctx.client_id);
            let (reply, rx) = mpsc::channel();
            let _ = ctx.daemon_tx.send(DaemonCommand::ListSessions { reply });
            if let Ok(sessions) = rx.recv() {
                send_to_writer(ctx, DaemonMessage::Sessions { sessions });
            }
        }
        ClientMessage::SubscribeSessionsSummary => {
            let _ = ctx
                .daemon_tx
                .send(DaemonCommand::RegisterSummarySubscriber {
                    client_id: ctx.client_id,
                    writer: ctx.writer.clone(),
                });
        }
        ClientMessage::UnsubscribeSessionsSummary => {
            let _ = ctx
                .daemon_tx
                .send(DaemonCommand::UnregisterSummarySubscriber {
                    client_id: ctx.client_id,
                });
        }
        ClientMessage::RunInput { request_id, input } => {
            debug!("client {}: RunInput id={}", ctx.client_id, request_id);
            if let Some(tx) = ctx.attached_session_tx {
                let _ = tx.send(SessionCommand::RunInput { request_id, input });
            } else {
                // This connection-level reply has no origin session, so the
                // envelope carries `session_id: None` (used in every "no
                // session attached" arm in this dispatch). The TUI resolves
                // it as a connection-level failure via
                // `App::resolve_daemon_session`.
                send_to_writer(
                    ctx,
                    DaemonMessage::Session {
                        session_id: None,
                        event: SessionEvent::Failed {
                            request_id,
                            error: "no session attached".to_string(),
                        },
                    },
                );
            }
        }
        ClientMessage::Cancel { request_id } => {
            debug!("client {}: Cancel id={}", ctx.client_id, request_id);
            // Route through the daemon so it can also cancel child
            // sub-sessions without requiring a round-trip message.
            if let Some(session_id) = *ctx.attached_session_id {
                let _ = ctx.daemon_tx.send(DaemonCommand::CancelRequest {
                    session_id,
                    request_id,
                });
            }
        }
        ClientMessage::Undo => {
            debug!("client {}: Undo", ctx.client_id);
            if let Some(tx) = ctx.attached_session_tx {
                let _ = tx.send(SessionCommand::Undo);
            }
        }
        ClientMessage::Redo => {
            debug!("client {}: Redo", ctx.client_id);
            if let Some(tx) = ctx.attached_session_tx {
                let _ = tx.send(SessionCommand::Redo);
            }
        }
        ClientMessage::ContinueGeneration { request_id } => {
            debug!(
                "client {}: ContinueGeneration id={}",
                ctx.client_id, request_id
            );
            if let Some(tx) = ctx.attached_session_tx {
                let _ = tx.send(SessionCommand::RunInput {
                    request_id,
                    input: b"Continue.".to_vec(),
                });
            } else {
                send_to_writer(
                    ctx,
                    DaemonMessage::Session {
                        session_id: None,
                        event: SessionEvent::Failed {
                            request_id,
                            error: "no session attached".to_string(),
                        },
                    },
                );
            }
        }
        ClientMessage::Ping => {
            debug!("client {}: Ping", ctx.client_id);
            send_to_writer(ctx, DaemonMessage::Pong);
        }
        ClientMessage::SetModel { model } => {
            info!(
                "client {}: SetModel model={} attached={}",
                ctx.client_id,
                model,
                ctx.attached_session_tx.is_some()
            );
            if let Some(tx) = ctx.attached_session_tx {
                let _ = tx.send(SessionCommand::SetModel { model });
            } else {
                send_to_writer(
                    ctx,
                    DaemonMessage::Session {
                        session_id: None,
                        event: SessionEvent::ModelSelectionFailed {
                            model,
                            error: "no session attached".to_string(),
                        },
                    },
                );
            }
        }
        ClientMessage::SetReasoningEffort { effort } => {
            info!(
                "client {}: SetReasoningEffort effort={} attached={}",
                ctx.client_id,
                effort,
                ctx.attached_session_tx.is_some()
            );
            if let Some(tx) = ctx.attached_session_tx {
                let _ = tx.send(SessionCommand::SetReasoningEffort { effort });
            } else {
                send_to_writer(
                    ctx,
                    DaemonMessage::Session {
                        session_id: None,
                        event: SessionEvent::ReasoningEffortSetFailed {
                            effort,
                            error: "no session attached".to_string(),
                        },
                    },
                );
            }
        }
        ClientMessage::GetReasoningEffort => {
            if let Some(tx) = ctx.attached_session_tx {
                let (reply, rx) = mpsc::channel();
                let _ = tx.send(SessionCommand::GetReasoningEffort { reply });
                if let Ok(effort) = rx.recv() {
                    // Session-scoped reply to the attached session: carry its
                    // real id (do NOT fall back to the None sentinel).
                    send_to_writer(
                        ctx,
                        DaemonMessage::Session {
                            session_id: *ctx.attached_session_id,
                            event: SessionEvent::ReasoningEffortSet { effort },
                        },
                    );
                }
            } else {
                send_to_writer(
                    ctx,
                    DaemonMessage::Session {
                        session_id: None,
                        event: SessionEvent::ReasoningEffortSet {
                            effort: "off".to_string(),
                        },
                    },
                );
            }
        }
        ClientMessage::Unlock { private_key } => {
            info!("client {}: Unlock", ctx.client_id);
            handle_unlock_sync(ctx, private_key);
        }
        ClientMessage::AddCredential {
            service,
            encrypted_payload,
            unlock_key,
        } => {
            info!(
                "client {}: AddCredential service={}",
                ctx.client_id, service
            );
            handle_add_credential_sync(ctx, service, encrypted_payload, unlock_key);
        }
        ClientMessage::RemoveCredential { service } => {
            info!(
                "client {}: RemoveCredential service={}",
                ctx.client_id, service
            );
            handle_remove_credential_sync(ctx, service);
        }
        ClientMessage::ListModels => {
            debug!("client {}: ListModels", ctx.client_id);
            handle_list_models_sync(ctx, *ctx.attached_session_id);
        }
        ClientMessage::RefreshModels { force } => {
            debug!("client {}: RefreshModels force={}", ctx.client_id, force);
            handle_refresh_models_sync(ctx, force);
        }
        ClientMessage::DeleteSession { session_id } => {
            info!("client {}: DeleteSession id={}", ctx.client_id, session_id);
            handle_delete_session_sync(ctx, session_id);
        }
        ClientMessage::GetCredential { service } => {
            handle_get_credential_sync(ctx, service);
        }
        ClientMessage::AddAccount {
            name,
            provider,
            base_url,
            streaming,
            retry_max_attempts,
            connect_timeout_secs,
            request_timeout_secs,
            total_timeout_secs,
        } => {
            let result = request_daemon(ctx.daemon_tx, |reply| DaemonCommand::AddAccountCmd {
                name: name.clone(),
                provider,
                base_url,
                streaming,
                retry_max_attempts,
                connect_timeout_secs,
                request_timeout_secs,
                total_timeout_secs,
                reply,
            });
            match result {
                Ok(Ok(())) => {
                    send_to_writer(ctx, DaemonMessage::AccountAdded { name });
                }
                Ok(Err(e)) => {
                    send_to_writer(ctx, DaemonMessage::AccountAddFailed { name, error: e });
                }
                Err(_) => warn!("daemon disconnected while handling add account"),
            }
        }
        ClientMessage::RemoveAccount { name } => {
            let result = request_daemon(ctx.daemon_tx, |reply| DaemonCommand::RemoveAccountCmd {
                name: name.clone(),
                reply,
            });
            match result {
                Ok(Ok(())) => {
                    send_to_writer(ctx, DaemonMessage::AccountRemoved { name });
                }
                Ok(Err(e)) => {
                    send_to_writer(ctx, DaemonMessage::AccountRemoveFailed { name, error: e });
                }
                Err(_) => warn!("daemon disconnected while handling remove account"),
            }
        }
        ClientMessage::ListAccounts => {
            let result = request_daemon(ctx.daemon_tx, |reply| DaemonCommand::ListAccountsCmd {
                reply,
            });
            match result {
                Ok(Ok(accounts)) => {
                    send_to_writer(ctx, DaemonMessage::Accounts { accounts });
                }
                Ok(Err(e)) => {
                    send_to_writer(ctx, DaemonMessage::AccountListFailed { error: e });
                }
                Err(_) => warn!("daemon disconnected while handling list accounts"),
            }
        }
        ClientMessage::SetSessionAccount { name } => {
            handle_client_set_session_account(name, ctx);
        }
        ClientMessage::SubscribeAllActivity => {
            let _ = ctx
                .daemon_tx
                .send(DaemonCommand::RegisterActivitySubscriber {
                    client_id: ctx.client_id,
                    writer: ctx.writer.clone(),
                });
        }
        ClientMessage::UnsubscribeAllActivity => {
            let _ = ctx
                .daemon_tx
                .send(DaemonCommand::UnregisterActivitySubscriber {
                    client_id: ctx.client_id,
                });
        }
        _ => {
            warn!(
                "unhandled client message: {:?}",
                std::mem::discriminant(&msg)
            );
        }
    }
    Ok(())
}

pub(crate) fn client_thread(
    stream: UnixStream,
    daemon_tx: mpsc::Sender<DaemonCommand>,
    client_id: u64,
    writer: crate::broadcast::SubscriberSink,
    writer_rx: crossbeam_channel::Receiver<DaemonMessage>,
    global_lag: Arc<AtomicUsize>,
) -> io::Result<()> {
    // Bound the writer's blocking socket writes so a wedged client (receive
    // window permanently zero) cannot stall it forever — this is what makes
    // lag eviction reap the connection without a daemon-held close handle.
    // The timeout applies to every clone of this socket.
    stream.set_write_timeout(Some(WRITER_WRITE_TIMEOUT))?;
    let reader = BufReader::new(stream.try_clone()?);
    let writer_buf = BufWriter::new(stream);

    // The writer thread decrements the SAME per-client byte counter the
    // daemon's sinks increment on enqueue, plus the daemon-wide counter.
    let bytes = Arc::clone(&writer.bytes_in_flight);
    let global = Arc::clone(&global_lag);
    let writer_handle =
        std::thread::spawn(move || writer_thread(writer_buf, writer_rx, bytes, global));

    let mut attached_session_tx: Option<mpsc::Sender<SessionCommand>> = None;
    let mut attached_session_id: Option<u64> = None;
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
                let mut ctx = ClientCtx {
                    writer: &writer,
                    global_lag: &global_lag,
                    daemon_tx: &daemon_tx,
                    attached_session_id: &mut attached_session_id,
                    attached_session_tx: &mut attached_session_tx,
                    client_id,
                };
                if let Err(e) = dispatch_client_message(msg, &mut ctx) {
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

    cleanup_client(
        attached_session_tx,
        client_id,
        &daemon_tx,
        writer,
        writer_handle,
    );
    Ok(())
}

/// TCP accept path: read the 1-byte handshake-mode preamble, run the
/// matching Noise responder handshake (IK or XX), and hand the encrypted
/// stream to [`tcp_client_thread`].
///
/// The writer channel is registered with the daemon by the acceptor BEFORE
/// this function runs (see `register_client_writer`), so every failure path
/// here — unknown preamble, silent/garbage peer, rejected handshake — must
/// unregister via `ClientDisconnected`, exactly as the old inline handshake
/// failure path in `server/lifecycle.rs` did. This keeps the daemon's
/// `client_writers` registry honest: a connection that never produced a
/// working transport must not leave a stale writer entry behind.
///
/// **The preamble is UNAUTHENTICATED by design.** It is a cleartext mode
/// selector read before any keying material exists, so it cannot carry
/// authentication itself. That is safe because it authorizes NOTHING: it
/// only selects which handshake runs. Everything that matters is
/// authenticated by the subsequent Noise handshake — IK and XX both
/// authenticate both parties' static keys via the DH operations, so a
/// man-in-the-middle cannot downgrade the mode or impersonate either side
/// (a MITM would have to complete the chosen handshake, which requires the
/// server's private key), and the daemon's ACL check runs inside whichever
/// handshake the client picked. The worst an attacker controls is which
/// of two equally-authenticated handshakes runs.
#[allow(clippy::too_many_arguments)]
pub(crate) fn tcp_handshake_and_client_thread(
    mut tcp: TcpStream,
    transport_sk: [u8; 32],
    acl: Arc<crate::server::acl::Acl>,
    daemon_tx: mpsc::Sender<DaemonCommand>,
    client_id: u64,
    writer: crate::broadcast::SubscriberSink,
    writer_rx: crossbeam_channel::Receiver<DaemonMessage>,
    global_lag: Arc<AtomicUsize>,
) -> io::Result<()> {
    // The preamble read runs BEFORE any authentication, so it is bounded by
    // the transport's absolute-deadline machinery (same as the handshake
    // itself): a peer that connects and sends nothing is cut off instead of
    // holding this thread + FD open forever.
    let preamble = match choreo_transport::handshake::read_handshake_preamble(&mut tcp) {
        Ok(p) => p,
        Err(e) => {
            warn!(
                error = %e,
                "TCP client never sent a valid handshake-mode preamble; closing"
            );
            // Drop `tcp` (closes the socket) and unregister the writer
            // channel this connection registered at accept time.
            let _ = daemon_tx.send(DaemonCommand::ClientDisconnected { client_id });
            return Ok(());
        }
    };

    // Dispatch on the mode byte. Each arm runs the full responder handshake
    // with the SAME ACL closure, so XX connections are authorized exactly
    // like IK ones (the check lives inside the handshake in both cases).
    let handshake_result = match preamble {
        choreo_transport::handshake::PREAMBLE_IK => {
            debug!("TCP client selected Noise IK handshake");
            choreo_transport::handshake::handshake_responder(tcp, &transport_sk, |pk| {
                acl.contains(pk)
            })
        }
        choreo_transport::handshake::PREAMBLE_XX => {
            debug!("TCP client selected Noise XX (first-contact) handshake");
            choreo_transport::handshake::handshake_responder_xx(tcp, &transport_sk, |pk| {
                acl.contains(pk)
            })
        }
        other => {
            warn!(
                preamble = other,
                "unknown handshake-mode preamble byte; closing connection"
            );
            let _ = daemon_tx.send(DaemonCommand::ClientDisconnected { client_id });
            return Ok(()); // dropping `tcp` closes the connection
        }
    };

    let noise = match handshake_result {
        Ok(noise) => noise,
        Err(e) => {
            error!(error = %e, "Noise handshake rejected");
            let _ = daemon_tx.send(DaemonCommand::ClientDisconnected { client_id });
            return Ok(());
        }
    };

    tcp_client_thread(noise, daemon_tx, client_id, writer, writer_rx, global_lag)
}

pub(crate) fn tcp_client_thread(
    noise: choreo_transport::noise::NoiseStream,
    daemon_tx: mpsc::Sender<DaemonCommand>,
    client_id: u64,
    writer: crate::broadcast::SubscriberSink,
    writer_rx: crossbeam_channel::Receiver<DaemonMessage>,
    global_lag: Arc<AtomicUsize>,
) -> io::Result<()> {
    // Writer thread: blocks on writer_rx, sends via NoiseStream encryption.
    // Bound the underlying socket's blocking writes (see WRITER_WRITE_TIMEOUT)
    // so a wedged client cannot stall the writer forever; the timeout applies
    // to every clone of the TcpStream.
    noise
        .get_ref()
        .set_write_timeout(Some(WRITER_WRITE_TIMEOUT))?;
    let writer_buf = noise.try_clone()?;

    let bytes = Arc::clone(&writer.bytes_in_flight);
    let global = Arc::clone(&global_lag);
    let writer_handle =
        std::thread::spawn(move || writer_thread(writer_buf, writer_rx, bytes, global));

    let mut attached_session_tx: Option<mpsc::Sender<SessionCommand>> = None;
    let mut attached_session_id: Option<u64> = None;
    // The writer channel was registered with the daemon by the acceptor
    // (register_client_writer) before this thread was spawned, so the shutdown
    // path can route `ShuttingDown` through this single writer thread (see
    // client_thread). The NoiseStream's TransportState lock is only safe to
    // take per-message because this is the sole sender.
    let mut reader = noise;
    info!("TCP client connected: id={}", client_id);
    crate::metrics::record_client_connected();

    // Summary subscription is an explicit client decision on this transport,
    // exactly as on the Unix path: a Noise client opts in via
    // ClientMessage::SubscribeSessionsSummary (dispatched in
    // dispatch_client_message). Previously every TCP connection was
    // auto-registered here, which pushed broadcasts about other clients'
    // sessions to clients that never asked.
    loop {
        match reader.recv_client_message() {
            Ok(msg) => {
                let mut ctx = ClientCtx {
                    writer: &writer,
                    global_lag: &global_lag,
                    daemon_tx: &daemon_tx,
                    attached_session_id: &mut attached_session_id,
                    attached_session_tx: &mut attached_session_tx,
                    client_id,
                };
                if let Err(e) = dispatch_client_message(msg, &mut ctx) {
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

    cleanup_client(
        attached_session_tx,
        client_id,
        &daemon_tx,
        writer,
        writer_handle,
    );
    Ok(())
}

/// Switch the client's attachment from the old session to a new one.
/// Skips detaching when re-attaching to the same session to avoid
/// killing the session's only subscriber.
fn switch_attached_session(
    new_session_id: u64,
    session_tx: mpsc::Sender<SessionCommand>,
    ctx: &mut ClientCtx,
) {
    // Don't detach when re-attaching to the same session.
    if Some(new_session_id) != *ctx.attached_session_id
        && let Some(old_tx) = ctx.attached_session_tx.as_ref()
    {
        let _ = old_tx.send(SessionCommand::Detach {
            client_id: ctx.client_id,
        });
    }
    let _ = session_tx.send(SessionCommand::Attach {
        client_id: ctx.client_id,
        tx: ctx.writer.clone(),
    });
    *ctx.attached_session_tx = Some(session_tx);
    *ctx.attached_session_id = Some(new_session_id);
}

#[expect(clippy::too_many_arguments)]
/// Handle a CreateSession client message. Returns false if the daemon
/// disconnected, signaling client_thread to return.
fn handle_client_create_session(
    title: Option<String>,
    parent_session_id: Option<u64>,
    working_dir: Option<String>,
    context_config: Option<ContextConfig>,
    account_name: Option<String>,
    selected_model: Option<String>,
    reasoning_effort: Option<String>,
    ctx: &mut ClientCtx,
) -> bool {
    info!("client {}: CreateSession", ctx.client_id);
    let cwd_str = working_dir.clone();
    let (reply, rx) = mpsc::channel();
    let _ = ctx.daemon_tx.send(DaemonCommand::CreateSession {
        title: title.clone(),
        parent_session_id,
        working_dir: working_dir.map(std::path::PathBuf::from),
        reasoning_effort: reasoning_effort.clone(),
        selected_model: selected_model.clone(),
        context_config,
        account_name: account_name.clone(),
        active_tool_groups: Vec::new(),
        reply,
    });
    match rx.recv() {
        Ok(Ok((sid, _session_tx))) => {
            // _session_tx is discarded here because the
            // daemon keeps its own clone in active_sessions
            // (keyed by sid).  When the client later calls
            // AttachSession the daemon returns another clone
            // — no need to hold one in the connection thread.
            //
            // Don't auto-attach or detach here — the TUI
            // attaches explicitly via AttachSession when
            // the user presses Enter on a session.
            // This keeps the old session alive when
            // creating from the session manager page.
            send_to_writer(
                ctx,
                DaemonMessage::Session {
                    session_id: Some(sid),
                    event: SessionEvent::SessionCreated {
                        title,
                        parent_session_id,
                        working_dir: cwd_str,
                        account_name,
                        selected_model,
                        reasoning_effort,
                    },
                },
            );
        }
        Ok(Err(e)) => {
            send_to_writer(
                ctx,
                DaemonMessage::Session {
                    session_id: None,
                    event: SessionEvent::SessionFailed {
                        operation: "create_session".into(),
                        error: e.to_string(),
                    },
                },
            );
        }
        Err(_) => return false,
    }
    true
}

/// Handle an AttachSession client message. Returns false if the daemon
/// disconnected, signaling client_thread to return.
fn handle_client_attach_session(session_id: u64, ctx: &mut ClientCtx) -> bool {
    info!("client {}: AttachSession id={}", ctx.client_id, session_id);
    let (reply, rx) = mpsc::channel();
    let _ = ctx
        .daemon_tx
        .send(DaemonCommand::AttachSession { session_id, reply });
    match rx.recv() {
        Ok(Ok(session_tx)) => {
            // Send SessionAttached before SessionCommand::Attach so that
            // the TUI's attached_session_id is set before SessionState
            // arrives — otherwise SessionState is silently dropped.
            send_to_writer(
                ctx,
                DaemonMessage::Session {
                    session_id: Some(session_id),
                    event: SessionEvent::SessionAttached,
                },
            );
            switch_attached_session(session_id, session_tx, ctx);
        }
        Ok(Err(e)) => {
            send_to_writer(
                ctx,
                DaemonMessage::Session {
                    session_id: None,
                    event: SessionEvent::SessionFailed {
                        operation: "attach_session".into(),
                        error: e.to_string(),
                    },
                },
            );
        }
        Err(_) => return false,
    }
    true
}

/// Handle a SetSessionAccount client message: verify the account exists
/// via the daemon, then set it on the attached session.
fn handle_client_set_session_account(name: String, ctx: &mut ClientCtx) {
    if let Some(tx) = ctx.attached_session_tx.as_ref() {
        // Verify the account exists before setting it.
        let (reply, rx) = mpsc::channel();
        let _ = ctx.daemon_tx.send(DaemonCommand::AccountExists {
            name: name.clone(),
            reply,
        });
        match rx.recv() {
            Ok(true) => {
                let _ = tx.send(SessionCommand::SetAccount { name });
            }
            _ => {
                // Session-scoped reply to the attached session: carry its
                // real id (do NOT fall back to the None sentinel).
                send_to_writer(
                    ctx,
                    DaemonMessage::Session {
                        session_id: *ctx.attached_session_id,
                        event: SessionEvent::SessionFailed {
                            operation: "set_account".into(),
                            error: format!("account '{name}' not found"),
                        },
                    },
                );
            }
        }
    } else {
        send_to_writer(
            ctx,
            DaemonMessage::Session {
                session_id: None,
                event: SessionEvent::SessionFailed {
                    operation: "set_account".into(),
                    error: "no session attached".to_string(),
                },
            },
        );
    }
}

/// Send a DaemonCommand that expects a reply and wait for the response.
/// Returns the reply value, or None if the daemon dropped the sender.
fn request_daemon<R>(
    daemon_tx: &mpsc::Sender<DaemonCommand>,
    make_cmd: impl FnOnce(mpsc::Sender<R>) -> DaemonCommand,
) -> Result<R, mpsc::RecvError> {
    let (reply, rx) = mpsc::channel();
    if daemon_tx.send(make_cmd(reply)).is_err() {
        return Err(mpsc::RecvError);
    }
    rx.recv()
}

fn handle_unlock_sync(ctx: &mut ClientCtx, private_key: Vec<u8>) {
    let result = request_daemon(ctx.daemon_tx, |reply| DaemonCommand::Unlock {
        private_key,
        reply,
    });
    match result {
        Ok(Ok(())) => {
            send_to_writer(ctx, DaemonMessage::Unlocked);
        }
        Ok(Err(e)) => {
            send_to_writer(ctx, DaemonMessage::LockedError { error: e });
        }
        Err(_) => warn!("daemon disconnected while handling unlock"),
    }
}

fn handle_list_models_sync(ctx: &mut ClientCtx, attached_session_id: Option<u64>) {
    let result = request_daemon(ctx.daemon_tx, |reply| DaemonCommand::ListModels {
        session_id: attached_session_id,
        reply,
    });
    match result {
        Ok(Ok((models, selected_model))) => {
            send_to_writer(
                ctx,
                DaemonMessage::Models {
                    models,
                    selected_model,
                },
            );
        }
        Ok(Err(e)) => {
            send_to_writer(ctx, DaemonMessage::ModelsFailed { error: e });
        }
        Err(_) => warn!("daemon disconnected while handling list models"),
    }
}

/// Handle a RefreshModels client message: forward the request to the daemon
/// (which hands it to the maintenance thread — the fetch never blocks this
/// connection), then route the reply back to the client. The request blocks
/// here until the maintenance thread has a result, which is the request/
/// response contract `/refresh-models` implies.
fn handle_refresh_models_sync(ctx: &mut ClientCtx, force: bool) {
    let result = request_daemon(ctx.daemon_tx, |reply| DaemonCommand::RefreshModels {
        force,
        reply,
    });
    match result {
        Ok(Ok(report)) => {
            send_to_writer(
                ctx,
                DaemonMessage::ModelsRefreshed {
                    providers: report.providers,
                    models: report.models,
                    status: report.status,
                },
            );
        }
        Ok(Err(e)) => {
            send_to_writer(ctx, DaemonMessage::ModelsRefreshFailed { error: e });
        }
        Err(_) => warn!("daemon disconnected while handling refresh models"),
    }
}

fn handle_get_credential_sync(ctx: &mut ClientCtx, service: String) {
    let result = request_daemon(ctx.daemon_tx, |reply| DaemonCommand::GetCredential {
        service: service.clone(),
        reply,
    });
    match result {
        Ok(Some(key)) => {
            send_to_writer(
                ctx,
                DaemonMessage::Credential {
                    service,
                    key: Some(key),
                },
            );
        }
        Ok(None) => {
            send_to_writer(ctx, DaemonMessage::Credential { service, key: None });
        }
        Err(_) => warn!("daemon disconnected while handling get credential"),
    }
}

fn handle_delete_session_sync(ctx: &mut ClientCtx, session_id: u64) {
    let result = request_daemon(ctx.daemon_tx, |reply| DaemonCommand::DeleteSession {
        session_id,
        reply,
    });
    match result {
        Ok(Ok(())) => {
            // The daemon broadcasts SessionDeleted to all summary
            // subscribers (including this client when it's viewing
            // the session list), so we don't duplicate it here.
        }
        Ok(Err(e)) => {
            send_to_writer(
                ctx,
                DaemonMessage::Session {
                    session_id: Some(session_id),
                    event: SessionEvent::SessionDeleteFailed {
                        error: e.to_string(),
                    },
                },
            );
        }
        Err(_) => warn!("daemon disconnected while handling delete session"),
    }
}

fn handle_add_credential_sync(
    ctx: &mut ClientCtx,
    service: String,
    encrypted_payload: Vec<u8>,
    unlock_key: Option<Vec<u8>>,
) {
    let result = request_daemon(ctx.daemon_tx, |reply| DaemonCommand::SaveCredential {
        service: service.clone(),
        encrypted_blob: encrypted_payload,
        unlock_key,
        reply,
    });
    match result {
        Ok(Ok(())) => {
            send_to_writer(ctx, DaemonMessage::CredentialAdded { service });
        }
        Ok(Err(e)) => {
            send_to_writer(
                ctx,
                DaemonMessage::CredentialAddFailed { service, error: e },
            );
        }
        Err(_) => warn!("daemon disconnected while handling add credential"),
    }
}

fn handle_remove_credential_sync(ctx: &mut ClientCtx, service: String) {
    let result = request_daemon(ctx.daemon_tx, |reply| DaemonCommand::RemoveCredentialCmd {
        service: service.clone(),
        reply,
    });
    match result {
        Ok(Ok(())) => {
            send_to_writer(ctx, DaemonMessage::CredentialRemoved { service });
        }
        Ok(Err(e)) => {
            send_to_writer(
                ctx,
                DaemonMessage::CredentialRemoveFailed { service, error: e },
            );
        }
        Err(_) => warn!("daemon disconnected while handling remove credential"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::broadcast::test_sink;

    /// A `ConnectionWriter` test double that forwards every written message
    /// to a channel and records shutdown calls on another. Message-passing
    /// only (no shared state across threads): the test reads the record
    /// after joining the writer thread.
    struct MockConnectionWriter {
        sent: mpsc::Sender<DaemonMessage>,
        shutdown_tx: mpsc::Sender<()>,
        /// Fail `send_message` on the Nth call (1-based) to exercise the
        /// error path of `writer_thread`.
        fail_on: Option<usize>,
        calls: usize,
    }

    impl ConnectionWriter for MockConnectionWriter {
        fn send_message(&mut self, msg: &DaemonMessage) -> Result<(), String> {
            self.calls += 1;
            if self.fail_on == Some(self.calls) {
                return Err("mock write failure".to_string());
            }
            let _ = self.sent.send(msg.clone());
            Ok(())
        }
        fn shutdown(&mut self) {
            let _ = self.shutdown_tx.send(());
        }
    }

    fn mock_writer(
        fail_on: Option<usize>,
    ) -> (
        MockConnectionWriter,
        mpsc::Receiver<DaemonMessage>,
        mpsc::Receiver<()>,
    ) {
        let (sent_tx, sent_rx) = mpsc::channel();
        let (shutdown_tx, shutdown_rx) = mpsc::channel();
        (
            MockConnectionWriter {
                sent: sent_tx,
                shutdown_tx,
                fail_on,
                calls: 0,
            },
            sent_rx,
            shutdown_rx,
        )
    }

    /// The core `writer_thread` contract: `ShuttingDown` is flushed, the
    /// socket is shut down HERE (by the writer thread itself), and draining
    /// stops — a message enqueued after the notification is never written,
    /// so the client observes the notification before the EOF.
    #[test]
    fn writer_thread_flushes_shutting_down_then_shuts_down_and_stops() {
        let (tx, rx) = crossbeam_channel::unbounded::<DaemonMessage>();
        let (writer, sent_rx, shutdown_rx) = mock_writer(None);
        let bytes = Arc::new(AtomicUsize::new(0));
        let global = Arc::new(AtomicUsize::new(0));
        let handle = std::thread::spawn({
            let bytes = Arc::clone(&bytes);
            let global = Arc::clone(&global);
            move || writer_thread(writer, rx, bytes, global)
        });

        tx.send(DaemonMessage::Pong).unwrap();
        tx.send(DaemonMessage::ShuttingDown).unwrap();
        // Queued after the notification: must never be written.
        tx.send(DaemonMessage::Pong).unwrap();

        handle.join().expect("writer thread panicked");
        let written: Vec<_> = sent_rx.try_iter().collect();
        assert_eq!(
            written,
            vec![DaemonMessage::Pong, DaemonMessage::ShuttingDown],
            "ShuttingDown must be flushed in order, then draining must stop"
        );
        assert!(
            shutdown_rx.try_recv().is_ok(),
            "the writer thread must close the socket itself after ShuttingDown"
        );
    }

    /// `Evicted` is handled exactly like `ShuttingDown`: flushed, socket shut
    /// down, draining stops — the lag-eviction advisory is also a
    /// notify-before-EOF on the graceful path (the daemon additionally
    /// force-closes the socket, but the ordering guarantee is preserved here).
    #[test]
    fn writer_thread_flushes_evicted_then_shuts_down_and_stops() {
        let (tx, rx) = crossbeam_channel::unbounded::<DaemonMessage>();
        let (writer, sent_rx, shutdown_rx) = mock_writer(None);
        let bytes = Arc::new(AtomicUsize::new(0));
        let global = Arc::new(AtomicUsize::new(0));
        let handle = std::thread::spawn({
            let bytes = Arc::clone(&bytes);
            let global = Arc::clone(&global);
            move || writer_thread(writer, rx, bytes, global)
        });

        tx.send(DaemonMessage::Pong).unwrap();
        tx.send(DaemonMessage::Evicted).unwrap();
        // Queued after the advisory: must never be written.
        tx.send(DaemonMessage::Pong).unwrap();

        handle.join().expect("writer thread panicked");
        let written: Vec<_> = sent_rx.try_iter().collect();
        assert_eq!(
            written,
            vec![DaemonMessage::Pong, DaemonMessage::Evicted],
            "Evicted must be flushed in order, then draining must stop"
        );
        assert!(
            shutdown_rx.try_recv().is_ok(),
            "the writer thread must close the socket itself after Evicted"
        );
    }

    /// A send error is fatal for the connection: the loop stops AND shuts the
    /// socket down. A send error is either a broken pipe (socket gone —
    /// shutdown is a harmless no-op) or a [`WRITER_WRITE_TIMEOUT`] on a wedged
    /// client whose receive window is zero (socket still open — shutdown is
    /// what unblocks the reader's blocking read so the connection is reaped).
    #[test]
    fn writer_thread_stops_and_shuts_down_on_send_error() {
        let (tx, rx) = crossbeam_channel::unbounded::<DaemonMessage>();
        // Fail on the second write: the first Pong goes out, the loop breaks.
        let (writer, sent_rx, shutdown_rx) = mock_writer(Some(2));
        let bytes = Arc::new(AtomicUsize::new(0));
        let global = Arc::new(AtomicUsize::new(0));
        let handle = std::thread::spawn({
            let bytes = Arc::clone(&bytes);
            let global = Arc::clone(&global);
            move || writer_thread(writer, rx, bytes, global)
        });

        tx.send(DaemonMessage::Pong).unwrap();
        tx.send(DaemonMessage::Pong).unwrap();
        tx.send(DaemonMessage::ShuttingDown).unwrap();
        drop(tx); // disconnect so the thread cannot linger

        handle.join().expect("writer thread panicked");
        let written: Vec<_> = sent_rx.try_iter().collect();
        assert_eq!(written.len(), 1, "writer must stop at the failing message");
        assert!(
            shutdown_rx.try_recv().is_ok(),
            "writer must shut the socket down on a send error so the reader is unblocked"
        );
    }

    /// A disconnected channel ends the loop cleanly without shutdown — the
    /// normal drain-to-exit path for a disconnected client.
    #[test]
    fn writer_thread_exits_cleanly_on_disconnect() {
        let (tx, rx) = crossbeam_channel::unbounded::<DaemonMessage>();
        let (writer, sent_rx, shutdown_rx) = mock_writer(None);
        let bytes = Arc::new(AtomicUsize::new(0));
        let global = Arc::new(AtomicUsize::new(0));
        let handle = std::thread::spawn({
            let bytes = Arc::clone(&bytes);
            let global = Arc::clone(&global);
            move || writer_thread(writer, rx, bytes, global)
        });

        tx.send(DaemonMessage::Pong).unwrap();
        drop(tx); // all senders gone: the for-loop drains and ends

        handle.join().expect("writer thread panicked");
        let written: Vec<_> = sent_rx.try_iter().collect();
        assert_eq!(written, vec![DaemonMessage::Pong]);
        assert!(shutdown_rx.try_recv().is_err());
    }

    /// The writer thread decrements the per-client and daemon-wide byte
    /// counters once per dequeued message, using each message's approximate
    /// wire size — the exact counterpart of `enqueue`'s increment. A
    /// two-message drain must zero both counters.
    #[test]
    fn writer_thread_decrements_byte_counters_per_message() {
        let (tx, rx) = crossbeam_channel::unbounded::<DaemonMessage>();
        let (writer, _sent_rx, _shutdown_rx) = mock_writer(None);
        let bytes = Arc::new(AtomicUsize::new(0));
        let global = Arc::new(AtomicUsize::new(0));
        let handle = std::thread::spawn({
            let bytes = Arc::clone(&bytes);
            let global = Arc::clone(&global);
            move || writer_thread(writer, rx, bytes, global)
        });

        let m1 = DaemonMessage::Session {
            session_id: Some(1),
            event: SessionEvent::Failed {
                request_id: 1,
                error: "a".repeat(100),
            },
        };
        let m2 = DaemonMessage::Session {
            session_id: Some(2),
            event: SessionEvent::Failed {
                request_id: 2,
                error: "b".repeat(50),
            },
        };
        let s1 = m1.approx_wire_size();
        let s2 = m2.approx_wire_size();

        // Pre-seed the counters exactly as `enqueue` would have (the two
        // messages are queued and counted before the writer starts).
        bytes.fetch_add(s1 + s2, Ordering::Relaxed);
        global.fetch_add(s1 + s2, Ordering::Relaxed);

        tx.send(m1).unwrap();
        tx.send(m2).unwrap();
        drop(tx);
        handle.join().expect("writer thread panicked");

        assert_eq!(
            bytes.load(Ordering::Relaxed),
            0,
            "every dequeued message must decrement the per-client counter"
        );
        assert_eq!(
            global.load(Ordering::Relaxed),
            0,
            "every dequeued message must decrement the daemon-wide counter"
        );
    }

    /// The abandoned-backlog drain: when the writer stops at `Evicted` (or a
    /// send error), messages queued AFTER the stop point are never written —
    /// but they were counted at enqueue. The post-loop drain must decrement
    /// both counters for them, or an evicted client's backlog would stay
    /// frozen in the daemon-wide total forever (the leak that could
    /// permanently exhaust the global budget). The abandoned messages must
    /// NOT appear on the wire.
    #[test]
    fn writer_thread_drains_and_decrements_abandoned_backlog() {
        let (tx, rx) = crossbeam_channel::unbounded::<DaemonMessage>();
        let (writer, sent_rx, _shutdown_rx) = mock_writer(None);
        let bytes = Arc::new(AtomicUsize::new(0));
        let global = Arc::new(AtomicUsize::new(0));
        let handle = std::thread::spawn({
            let bytes = Arc::clone(&bytes);
            let global = Arc::clone(&global);
            move || writer_thread(writer, rx, bytes, global)
        });

        let m1 = DaemonMessage::Session {
            session_id: Some(1),
            event: SessionEvent::Failed {
                request_id: 1,
                error: "a".repeat(100),
            },
        };
        let m2 = DaemonMessage::Session {
            session_id: Some(2),
            event: SessionEvent::Failed {
                request_id: 2,
                error: "b".repeat(50),
            },
        };
        let m3 = DaemonMessage::Session {
            session_id: Some(3),
            event: SessionEvent::Failed {
                request_id: 3,
                error: "c".repeat(25),
            },
        };
        let s1 = m1.approx_wire_size();
        let s2 = m2.approx_wire_size();
        let s3 = m3.approx_wire_size();

        // Pre-seed the counters exactly as `enqueue` would have for ALL four
        // messages (m1 + Evicted are written; m2/m3 are abandoned behind the
        // stop point).
        let evicted_size = DaemonMessage::Evicted.approx_wire_size();
        let total = s1 + s2 + s3 + evicted_size;
        bytes.fetch_add(total, Ordering::Relaxed);
        global.fetch_add(total, Ordering::Relaxed);

        tx.send(m1.clone()).unwrap();
        tx.send(DaemonMessage::Evicted).unwrap();
        // Queued behind the advisory: never written, but must be decremented
        // by the exit drain.
        tx.send(m2).unwrap();
        tx.send(m3).unwrap();

        handle.join().expect("writer thread panicked");
        let written: Vec<_> = sent_rx.try_iter().collect();
        assert_eq!(
            written,
            vec![m1.clone(), DaemonMessage::Evicted],
            "messages behind the advisory must never be written"
        );
        assert_eq!(
            bytes.load(Ordering::Relaxed),
            0,
            "abandoned backlog must be decremented from the per-client counter"
        );
        assert_eq!(
            global.load(Ordering::Relaxed),
            0,
            "abandoned backlog must be decremented from the daemon-wide counter"
        );
    }

    #[test]
    fn handle_unlock_sync_ok() {
        let (daemon_tx, daemon_rx) = mpsc::channel();
        let (sink, writer_rx) = test_sink();
        let global_lag = Arc::new(AtomicUsize::new(0));
        let mut none_id = None;
        let mut none_tx = None;
        let mut ctx = ClientCtx {
            writer: &sink,
            global_lag: &global_lag,
            daemon_tx: &daemon_tx,
            attached_session_id: &mut none_id,
            attached_session_tx: &mut none_tx,
            client_id: 0,
        };
        std::thread::spawn(move || {
            if let Ok(DaemonCommand::Unlock { reply, .. }) = daemon_rx.recv() {
                let _ = reply.send(Ok(()));
            }
        });
        handle_unlock_sync(&mut ctx, vec![0u8; 32]);
        let msg = writer_rx.recv().unwrap();
        assert!(matches!(msg, DaemonMessage::Unlocked));
    }

    #[test]
    fn handle_unlock_sync_err() {
        let (daemon_tx, daemon_rx) = mpsc::channel();
        let (sink, writer_rx) = test_sink();
        let global_lag = Arc::new(AtomicUsize::new(0));
        let mut none_id = None;
        let mut none_tx = None;
        let mut ctx = ClientCtx {
            writer: &sink,
            global_lag: &global_lag,
            daemon_tx: &daemon_tx,
            attached_session_id: &mut none_id,
            attached_session_tx: &mut none_tx,
            client_id: 0,
        };
        std::thread::spawn(move || {
            if let Ok(DaemonCommand::Unlock { reply, .. }) = daemon_rx.recv() {
                let _ = reply.send(Err("wrong password".into()));
            }
        });
        handle_unlock_sync(&mut ctx, vec![0u8; 32]);
        let msg = writer_rx.recv().unwrap();
        assert!(matches!(msg, DaemonMessage::LockedError { .. }));
        if let DaemonMessage::LockedError { error } = &msg {
            assert_eq!(error, "wrong password");
        }
    }

    #[test]
    fn handle_unlock_sync_disconnected() {
        let (daemon_tx, daemon_rx) = mpsc::channel::<DaemonCommand>();
        let (sink, writer_rx) = test_sink();
        let global_lag = Arc::new(AtomicUsize::new(0));
        let mut none_id = None;
        let mut none_tx = None;
        let mut ctx = ClientCtx {
            writer: &sink,
            global_lag: &global_lag,
            daemon_tx: &daemon_tx,
            attached_session_id: &mut none_id,
            attached_session_tx: &mut none_tx,
            client_id: 0,
        };
        drop(daemon_rx);
        handle_unlock_sync(&mut ctx, vec![0u8; 32]);
        assert!(writer_rx.try_recv().is_err());
    }

    #[test]
    fn handle_list_models_sync_ok() {
        let (daemon_tx, daemon_rx) = mpsc::channel();
        let (sink, writer_rx) = test_sink();
        let global_lag = Arc::new(AtomicUsize::new(0));
        let mut none_id = None;
        let mut none_tx = None;
        let mut ctx = ClientCtx {
            writer: &sink,
            global_lag: &global_lag,
            daemon_tx: &daemon_tx,
            attached_session_id: &mut none_id,
            attached_session_tx: &mut none_tx,
            client_id: 0,
        };
        std::thread::spawn(move || {
            if let Ok(DaemonCommand::ListModels { reply, .. }) = daemon_rx.recv() {
                let _ = reply.send(Ok((
                    vec!["gpt-4".into(), "gpt-3.5".into()],
                    Some("gpt-4".into()),
                )));
            }
        });
        handle_list_models_sync(&mut ctx, None);
        let msg = writer_rx.recv().unwrap();
        assert!(matches!(msg, DaemonMessage::Models { .. }));
    }

    #[test]
    fn handle_refresh_models_sync_ok() {
        // The connection thread asks the daemon for a refresh; the daemon
        // (via the maintenance thread) replies with a report, which the
        // connection routes to the client as ModelsRefreshed.
        let (daemon_tx, daemon_rx) = mpsc::channel();
        let (sink, writer_rx) = test_sink();
        let global_lag = Arc::new(AtomicUsize::new(0));
        let mut none_id = None;
        let mut none_tx = None;
        let mut ctx = ClientCtx {
            writer: &sink,
            global_lag: &global_lag,
            daemon_tx: &daemon_tx,
            attached_session_id: &mut none_id,
            attached_session_tx: &mut none_tx,
            client_id: 0,
        };
        std::thread::spawn(move || {
            if let Ok(DaemonCommand::RefreshModels { force, reply }) = daemon_rx.recv() {
                assert!(force);
                let _ = reply.send(Ok(crate::catalog::RefreshReport {
                    providers: 208,
                    models: 1234,
                    status: choreo_proto::RefreshStatus::Updated,
                }));
            }
        });
        handle_refresh_models_sync(&mut ctx, true);
        let msg = writer_rx.recv().unwrap();
        assert!(matches!(
            &msg,
            DaemonMessage::ModelsRefreshed {
                providers: 208,
                models: 1234,
                status: choreo_proto::RefreshStatus::Updated,
            }
        ));
    }

    #[test]
    fn handle_refresh_models_sync_err() {
        let (daemon_tx, daemon_rx) = mpsc::channel();
        let (sink, writer_rx) = test_sink();
        let global_lag = Arc::new(AtomicUsize::new(0));
        let mut none_id = None;
        let mut none_tx = None;
        let mut ctx = ClientCtx {
            writer: &sink,
            global_lag: &global_lag,
            daemon_tx: &daemon_tx,
            attached_session_id: &mut none_id,
            attached_session_tx: &mut none_tx,
            client_id: 0,
        };
        std::thread::spawn(move || {
            if let Ok(DaemonCommand::RefreshModels { reply, .. }) = daemon_rx.recv() {
                let _ = reply.send(Err("daemon is locked".into()));
            }
        });
        handle_refresh_models_sync(&mut ctx, false);
        let msg = writer_rx.recv().unwrap();
        assert!(
            matches!(&msg, DaemonMessage::ModelsRefreshFailed { error } if error == "daemon is locked")
        );
    }

    #[test]
    fn handle_list_models_sync_err() {
        let (daemon_tx, daemon_rx) = mpsc::channel();
        let (sink, writer_rx) = test_sink();
        let global_lag = Arc::new(AtomicUsize::new(0));
        let mut none_id = None;
        let mut none_tx = None;
        let mut ctx = ClientCtx {
            writer: &sink,
            global_lag: &global_lag,
            daemon_tx: &daemon_tx,
            attached_session_id: &mut none_id,
            attached_session_tx: &mut none_tx,
            client_id: 0,
        };
        std::thread::spawn(move || {
            if let Ok(DaemonCommand::ListModels { reply, .. }) = daemon_rx.recv() {
                let _ = reply.send(Err("daemon is locked".into()));
            }
        });
        handle_list_models_sync(&mut ctx, None);
        let msg = writer_rx.recv().unwrap();
        assert!(matches!(msg, DaemonMessage::ModelsFailed { .. }));
        if let DaemonMessage::ModelsFailed { error } = &msg {
            assert_eq!(error, "daemon is locked");
        }
    }

    #[test]
    fn handle_get_credential_sync_some() {
        let (daemon_tx, daemon_rx) = mpsc::channel();
        let (sink, writer_rx) = test_sink();
        let global_lag = Arc::new(AtomicUsize::new(0));
        let mut none_id = None;
        let mut none_tx = None;
        let mut ctx = ClientCtx {
            writer: &sink,
            global_lag: &global_lag,
            daemon_tx: &daemon_tx,
            attached_session_id: &mut none_id,
            attached_session_tx: &mut none_tx,
            client_id: 0,
        };
        std::thread::spawn(move || {
            if let Ok(DaemonCommand::GetCredential { service, reply }) = daemon_rx.recv() {
                assert_eq!(service, "openai");
                let _ = reply.send(Some("sk-123".into()));
            }
        });
        handle_get_credential_sync(&mut ctx, "openai".into());
        let msg = writer_rx.recv().unwrap();
        assert!(matches!(msg, DaemonMessage::Credential { .. }));
        if let DaemonMessage::Credential { service, key } = &msg {
            assert_eq!(service, "openai");
            assert_eq!(key.as_deref(), Some("sk-123"));
        }
    }

    #[test]
    fn handle_get_credential_sync_none() {
        let (daemon_tx, daemon_rx) = mpsc::channel();
        let (sink, writer_rx) = test_sink();
        let global_lag = Arc::new(AtomicUsize::new(0));
        let mut none_id = None;
        let mut none_tx = None;
        let mut ctx = ClientCtx {
            writer: &sink,
            global_lag: &global_lag,
            daemon_tx: &daemon_tx,
            attached_session_id: &mut none_id,
            attached_session_tx: &mut none_tx,
            client_id: 0,
        };
        std::thread::spawn(move || {
            if let Ok(DaemonCommand::GetCredential { service, reply }) = daemon_rx.recv() {
                assert_eq!(service, "openai");
                let _ = reply.send(None);
            }
        });
        handle_get_credential_sync(&mut ctx, "openai".into());
        let msg = writer_rx.recv().unwrap();
        assert!(matches!(msg, DaemonMessage::Credential { .. }));
        if let DaemonMessage::Credential { service, key } = &msg {
            assert_eq!(service, "openai");
            assert!(key.is_none());
        }
    }

    #[test]
    fn switch_session_to_different_sends_detach_to_old() {
        let (old_tx, old_rx) = mpsc::channel();
        let (new_tx, new_rx) = mpsc::channel::<SessionCommand>();
        let (sink, _writer_rx) = test_sink();
        let global_lag = Arc::new(AtomicUsize::new(0));
        let (daemon_tx, _daemon_rx) = mpsc::channel();
        let mut attached_id = Some(1u64);
        let mut attached_tx = Some(old_tx);
        let mut ctx = ClientCtx {
            writer: &sink,
            global_lag: &global_lag,
            daemon_tx: &daemon_tx,
            attached_session_id: &mut attached_id,
            attached_session_tx: &mut attached_tx,
            client_id: 42,
        };

        switch_attached_session(2, new_tx, &mut ctx);

        // Detach sent to old session
        assert!(matches!(
            old_rx.try_recv().ok(),
            Some(SessionCommand::Detach { client_id: 42 })
        ));
        // Attach sent to new session
        assert!(matches!(
            new_rx.try_recv().ok(),
            Some(SessionCommand::Attach { client_id: 42, .. })
        ));
        // State updated to new session
        assert_eq!(attached_id, Some(2));
    }

    #[test]
    fn switch_session_same_skips_detach() {
        let (old_tx, old_rx) = mpsc::channel();
        let (new_tx, new_rx) = mpsc::channel::<SessionCommand>();
        let (sink, _writer_rx) = test_sink();
        let global_lag = Arc::new(AtomicUsize::new(0));
        let (daemon_tx, _daemon_rx) = mpsc::channel();
        let mut attached_id = Some(1u64);
        let mut attached_tx = Some(old_tx);
        let mut ctx = ClientCtx {
            writer: &sink,
            global_lag: &global_lag,
            daemon_tx: &daemon_tx,
            attached_session_id: &mut attached_id,
            attached_session_tx: &mut attached_tx,
            client_id: 42,
        };

        switch_attached_session(1, new_tx, &mut ctx);

        // No Detach sent — same session id
        assert!(old_rx.try_recv().is_err());
        // Attach still sent (caller expects the subscription)
        assert!(matches!(
            new_rx.try_recv().ok(),
            Some(SessionCommand::Attach { client_id: 42, .. })
        ));
        // State stays at session 1
        assert_eq!(attached_id, Some(1));
    }

    #[test]
    fn handle_delete_session_sync_success_no_message_sent() {
        let (daemon_tx, daemon_rx) = mpsc::channel();
        let (sink, writer_rx) = test_sink();
        let global_lag = Arc::new(AtomicUsize::new(0));
        let mut none_id = None;
        let mut none_tx = None;
        let mut ctx = ClientCtx {
            writer: &sink,
            global_lag: &global_lag,
            daemon_tx: &daemon_tx,
            attached_session_id: &mut none_id,
            attached_session_tx: &mut none_tx,
            client_id: 0,
        };
        std::thread::spawn(move || {
            if let Ok(DaemonCommand::DeleteSession { reply, .. }) = daemon_rx.recv() {
                let _ = reply.send(Ok(()));
            }
        });
        handle_delete_session_sync(&mut ctx, 42);
        // On success, no message is sent to writer (broadcast handles it)
        assert!(writer_rx.try_recv().is_err());
    }

    #[test]
    fn handle_delete_session_sync_error() {
        let (daemon_tx, daemon_rx) = mpsc::channel();
        let (sink, writer_rx) = test_sink();
        let global_lag = Arc::new(AtomicUsize::new(0));
        let mut none_id = None;
        let mut none_tx = None;
        let mut ctx = ClientCtx {
            writer: &sink,
            global_lag: &global_lag,
            daemon_tx: &daemon_tx,
            attached_session_id: &mut none_id,
            attached_session_tx: &mut none_tx,
            client_id: 0,
        };
        std::thread::spawn(move || {
            if let Ok(DaemonCommand::DeleteSession { reply, .. }) = daemon_rx.recv() {
                let _ = reply.send(Err(io::Error::other("db error")));
            }
        });
        handle_delete_session_sync(&mut ctx, 42);
        let msg = writer_rx.recv().unwrap();
        assert!(matches!(
            msg,
            DaemonMessage::Session {
                event: SessionEvent::SessionDeleteFailed { .. },
                ..
            }
        ));
        if let DaemonMessage::Session {
            session_id: Some(session_id),
            event: SessionEvent::SessionDeleteFailed { error },
        } = &msg
        {
            assert_eq!(*session_id, 42);
            assert_eq!(error, "db error");
        }
    }

    #[test]
    fn handle_delete_session_sync_disconnected() {
        let (daemon_tx, daemon_rx) = mpsc::channel::<DaemonCommand>();
        let (sink, writer_rx) = test_sink();
        let global_lag = Arc::new(AtomicUsize::new(0));
        let mut none_id = None;
        let mut none_tx = None;
        let mut ctx = ClientCtx {
            writer: &sink,
            global_lag: &global_lag,
            daemon_tx: &daemon_tx,
            attached_session_id: &mut none_id,
            attached_session_tx: &mut none_tx,
            client_id: 0,
        };
        drop(daemon_rx);
        handle_delete_session_sync(&mut ctx, 42);
        assert!(writer_rx.try_recv().is_err());
    }

    #[test]
    fn switch_session_from_none_no_detach() {
        let (new_tx, new_rx) = mpsc::channel::<SessionCommand>();
        let (sink, _writer_rx) = test_sink();
        let global_lag = Arc::new(AtomicUsize::new(0));
        let (daemon_tx, _daemon_rx) = mpsc::channel();
        let mut attached_id: Option<u64> = None;
        let mut attached_tx: Option<mpsc::Sender<SessionCommand>> = None;
        let mut ctx = ClientCtx {
            writer: &sink,
            global_lag: &global_lag,
            daemon_tx: &daemon_tx,
            attached_session_id: &mut attached_id,
            attached_session_tx: &mut attached_tx,
            client_id: 42,
        };

        switch_attached_session(1, new_tx, &mut ctx);

        assert_eq!(attached_id, Some(1));
        assert!(matches!(
            new_rx.try_recv().ok(),
            Some(SessionCommand::Attach { client_id: 42, .. })
        ));
    }

    // ── Undo dispatch ────────────────────────────────────────────────────

    #[test]
    fn dispatch_undo_when_attached_sends_undo_command() {
        let (daemon_tx, _daemon_rx) = mpsc::channel();
        let (sink, _writer_rx) = test_sink();
        let global_lag = Arc::new(AtomicUsize::new(0));
        let (session_tx, session_rx) = mpsc::channel();
        let mut attached_id = Some(1u64);
        let mut attached_tx = Some(session_tx);
        let mut ctx = ClientCtx {
            writer: &sink,
            global_lag: &global_lag,
            daemon_tx: &daemon_tx,
            attached_session_id: &mut attached_id,
            attached_session_tx: &mut attached_tx,
            client_id: 42,
        };

        dispatch_client_message(ClientMessage::Undo, &mut ctx).unwrap();

        assert!(matches!(
            session_rx.try_recv().ok(),
            Some(SessionCommand::Undo)
        ));
    }

    #[test]
    fn dispatch_undo_when_not_attached_is_noop() {
        let (daemon_tx, _daemon_rx) = mpsc::channel::<DaemonCommand>();
        let (sink, writer_rx) = test_sink();
        let global_lag = Arc::new(AtomicUsize::new(0));
        let mut none_id = None;
        let mut none_tx = None;
        let mut ctx = ClientCtx {
            writer: &sink,
            global_lag: &global_lag,
            daemon_tx: &daemon_tx,
            attached_session_id: &mut none_id,
            attached_session_tx: &mut none_tx,
            client_id: 0,
        };

        dispatch_client_message(ClientMessage::Undo, &mut ctx).unwrap();

        // No message should appear on writer or session channels.
        assert!(writer_rx.try_recv().is_err());
    }

    // ── Redo dispatch ────────────────────────────────────────────────────

    #[test]
    fn dispatch_redo_when_attached_sends_redo_command() {
        let (daemon_tx, _daemon_rx) = mpsc::channel();
        let (sink, _writer_rx) = test_sink();
        let global_lag = Arc::new(AtomicUsize::new(0));
        let (session_tx, session_rx) = mpsc::channel();
        let mut attached_id = Some(1u64);
        let mut attached_tx = Some(session_tx);
        let mut ctx = ClientCtx {
            writer: &sink,
            global_lag: &global_lag,
            daemon_tx: &daemon_tx,
            attached_session_id: &mut attached_id,
            attached_session_tx: &mut attached_tx,
            client_id: 42,
        };

        dispatch_client_message(ClientMessage::Redo, &mut ctx).unwrap();

        assert!(matches!(
            session_rx.try_recv().ok(),
            Some(SessionCommand::Redo)
        ));
    }

    #[test]
    fn dispatch_redo_when_not_attached_is_noop() {
        let (daemon_tx, _daemon_rx) = mpsc::channel::<DaemonCommand>();
        let (sink, writer_rx) = test_sink();
        let global_lag = Arc::new(AtomicUsize::new(0));
        let mut none_id = None;
        let mut none_tx = None;
        let mut ctx = ClientCtx {
            writer: &sink,
            global_lag: &global_lag,
            daemon_tx: &daemon_tx,
            attached_session_id: &mut none_id,
            attached_session_tx: &mut none_tx,
            client_id: 0,
        };

        dispatch_client_message(ClientMessage::Redo, &mut ctx).unwrap();

        assert!(writer_rx.try_recv().is_err());
    }

    // ── ContinueGeneration dispatch ──────────────────────────────────────

    #[test]
    fn dispatch_continue_generation_when_attached_sends_run_input() {
        let (daemon_tx, _daemon_rx) = mpsc::channel();
        let (sink, _writer_rx) = test_sink();
        let global_lag = Arc::new(AtomicUsize::new(0));
        let (session_tx, session_rx) = mpsc::channel();
        let mut attached_id = Some(1u64);
        let mut attached_tx = Some(session_tx);
        let mut ctx = ClientCtx {
            writer: &sink,
            global_lag: &global_lag,
            daemon_tx: &daemon_tx,
            attached_session_id: &mut attached_id,
            attached_session_tx: &mut attached_tx,
            client_id: 42,
        };

        dispatch_client_message(
            ClientMessage::ContinueGeneration { request_id: 7 },
            &mut ctx,
        )
        .unwrap();

        let cmd = session_rx.try_recv().expect("should receive RunInput");
        assert!(matches!(
            &cmd,
            SessionCommand::RunInput {
                request_id: 7,
                input,
            } if input == b"Continue."
        ));
    }

    #[test]
    fn dispatch_continue_generation_when_not_attached_sends_failed() {
        let (daemon_tx, _daemon_rx) = mpsc::channel::<DaemonCommand>();
        let (sink, writer_rx) = test_sink();
        let global_lag = Arc::new(AtomicUsize::new(0));
        let mut none_id = None;
        let mut none_tx = None;
        let mut ctx = ClientCtx {
            writer: &sink,
            global_lag: &global_lag,
            daemon_tx: &daemon_tx,
            attached_session_id: &mut none_id,
            attached_session_tx: &mut none_tx,
            client_id: 0,
        };

        dispatch_client_message(
            ClientMessage::ContinueGeneration { request_id: 7 },
            &mut ctx,
        )
        .unwrap();

        let msg = writer_rx.recv().expect("should receive Failed");
        assert!(matches!(
            &msg,
            DaemonMessage::Session {
                session_id: None,
                event: SessionEvent::Failed {
                    request_id: 7,
                    error,
                },
            } if error == "no session attached"
        ));
    }
}
