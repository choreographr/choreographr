use crate::daemon::DaemonCommand;
use crate::sessions::SessionCommand;
use choreo_proto::{
    ClientMessage, ContextConfig, DaemonMessage, ProtoError, read_message, write_message,
};
use std::io::{self, BufReader, BufWriter, Write};
use std::os::unix::net::UnixStream;
use std::sync::mpsc;
use std::sync::mpsc::SyncSender;
use tracing::{debug, error, info, warn};

/// Per-subscriber channel capacity for session message broadcast.
/// Limits how many messages the session thread can enqueue before the
/// client's writer thread drains them.  Creates natural backpressure
/// from the TUI event loop back to the daemon session thread, replacing
/// the previous thread::sleep() pacing for tool result chunks.
pub(crate) const SUBSCRIBER_CHANNEL_CAPACITY: usize = 128;

/// Shared per-client context passed through the dispatch and handler functions.
/// Bundles the channels and mutable per-connection state into one struct so
/// the call sites don't pass 5–6 individual arguments to every function.
struct ClientCtx<'a> {
    writer_tx: &'a SyncSender<DaemonMessage>,
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
    writer_tx: SyncSender<DaemonMessage>,
    writer_handle: std::thread::JoinHandle<()>,
) {
    if let Some(ref tx) = attached_session_tx {
        let _ = tx.send(SessionCommand::Detach { client_id });
    }
    let _ = daemon_tx.send(DaemonCommand::UnregisterSummarySubscriber { client_id });
    drop(writer_tx);
    let _ = writer_handle.join();
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
            max_turns,
            context_config,
            account_name,
            selected_model,
            reasoning_effort,
        } => {
            if !handle_client_create_session(
                title,
                parent_session_id,
                working_dir,
                max_turns,
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
                let _ = ctx.writer_tx.send(DaemonMessage::Sessions { sessions });
            }
        }
        ClientMessage::SubscribeSessionsSummary => {
            let _ = ctx
                .daemon_tx
                .send(DaemonCommand::RegisterSummarySubscriber {
                    client_id: ctx.client_id,
                    writer: ctx.writer_tx.clone(),
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
                let _ = ctx.writer_tx.send(DaemonMessage::Failed {
                    request_id,
                    error: "no session attached".to_string(),
                });
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
                let _ = ctx.writer_tx.send(DaemonMessage::Failed {
                    request_id,
                    error: "no session attached".to_string(),
                });
            }
        }
        ClientMessage::Ping => {
            debug!("client {}: Ping", ctx.client_id);
            let _ = ctx.writer_tx.send(DaemonMessage::Pong);
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
                let _ = ctx.writer_tx.send(DaemonMessage::ModelSelectionFailed {
                    model,
                    error: "no session attached".to_string(),
                });
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
                let _ = ctx.writer_tx.send(DaemonMessage::ReasoningEffortSetFailed {
                    effort,
                    error: "no session attached".to_string(),
                });
            }
        }
        ClientMessage::GetReasoningEffort => {
            if let Some(tx) = ctx.attached_session_tx {
                let (reply, rx) = mpsc::channel();
                let _ = tx.send(SessionCommand::GetReasoningEffort { reply });
                if let Ok(effort) = rx.recv() {
                    let _ = ctx
                        .writer_tx
                        .send(DaemonMessage::ReasoningEffortSet { effort });
                }
            } else {
                let _ = ctx.writer_tx.send(DaemonMessage::ReasoningEffortSet {
                    effort: "off".to_string(),
                });
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
        } => {
            let result = request_daemon(ctx.daemon_tx, |reply| DaemonCommand::AddAccountCmd {
                name: name.clone(),
                provider,
                base_url,
                streaming,
                retry_max_attempts,
                connect_timeout_secs,
                request_timeout_secs,
                reply,
            });
            match result {
                Ok(Ok(())) => {
                    let _ = ctx.writer_tx.send(DaemonMessage::AccountAdded { name });
                }
                Ok(Err(e)) => {
                    let _ = ctx
                        .writer_tx
                        .send(DaemonMessage::AccountAddFailed { name, error: e });
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
                    let _ = ctx.writer_tx.send(DaemonMessage::AccountRemoved { name });
                }
                Ok(Err(e)) => {
                    let _ = ctx
                        .writer_tx
                        .send(DaemonMessage::AccountRemoveFailed { name, error: e });
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
                    let _ = ctx.writer_tx.send(DaemonMessage::Accounts { accounts });
                }
                Ok(Err(e)) => {
                    let _ = ctx
                        .writer_tx
                        .send(DaemonMessage::AccountListFailed { error: e });
                }
                Err(_) => warn!("daemon disconnected while handling list accounts"),
            }
        }
        ClientMessage::SetSessionAccount { name } => {
            handle_client_set_session_account(name, ctx);
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
) -> io::Result<()> {
    let reader = BufReader::new(stream.try_clone()?);
    let mut writer = BufWriter::new(stream);

    let (writer_tx, writer_rx) = mpsc::sync_channel::<DaemonMessage>(SUBSCRIBER_CHANNEL_CAPACITY);
    let writer_handle = std::thread::spawn(move || {
        for msg in writer_rx {
            if write_message(&mut writer, &msg).is_err() {
                break;
            }
            let _ = writer.flush();
        }
    });

    let mut attached_session_tx: Option<mpsc::Sender<SessionCommand>> = None;
    let mut attached_session_id: Option<u64> = None;
    let client_id = rand::random::<u64>();
    info!("client connected: id={}", client_id);
    crate::metrics::record_client_connected();

    let mut reader = reader;
    loop {
        match read_message::<_, ClientMessage>(&mut reader) {
            Ok(msg) => {
                let mut ctx = ClientCtx {
                    writer_tx: &writer_tx,
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
        writer_tx,
        writer_handle,
    );
    Ok(())
}

pub(crate) fn tcp_client_thread(
    noise: choreo_transport::noise::NoiseStream,
    daemon_tx: mpsc::Sender<DaemonCommand>,
) -> io::Result<()> {
    let (writer_tx, writer_rx) = mpsc::sync_channel::<DaemonMessage>(SUBSCRIBER_CHANNEL_CAPACITY);

    // Writer thread: blocks on writer_rx, sends via NoiseStream encryption.
    let mut writer = noise.try_clone()?;
    let writer_handle = std::thread::spawn(move || {
        for msg in writer_rx {
            if let Err(e) = writer.send_daemon_message(&msg) {
                warn!("tcp writer thread error: {e}");
                break;
            }
        }
    });

    let mut attached_session_tx: Option<mpsc::Sender<SessionCommand>> = None;
    let mut attached_session_id: Option<u64> = None;
    let client_id = rand::random::<u64>();
    let mut reader = noise;
    info!("TCP client connected: id={}", client_id);
    crate::metrics::record_client_connected();

    // Register the summary subscriber so daemon push notifications
    // go through the writer thread (writer_tx channel).
    let _ = daemon_tx.send(DaemonCommand::RegisterSummarySubscriber {
        client_id,
        writer: writer_tx.clone(),
    });

    loop {
        match reader.recv_client_message() {
            Ok(msg) => {
                let mut ctx = ClientCtx {
                    writer_tx: &writer_tx,
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
        writer_tx,
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
        tx: ctx.writer_tx.clone(),
    });
    *ctx.attached_session_tx = Some(session_tx);
    *ctx.attached_session_id = Some(new_session_id);
}

#[allow(clippy::too_many_arguments)]
/// Handle a CreateSession client message. Returns false if the daemon
/// disconnected, signaling client_thread to return.
fn handle_client_create_session(
    title: Option<String>,
    parent_session_id: Option<u64>,
    working_dir: Option<String>,
    max_turns: Option<u32>,
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
        max_turns,
        reasoning_effort,
        selected_model,
        context_config,
        account_name,
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
            let _ = ctx.writer_tx.send(DaemonMessage::SessionCreated {
                session_id: sid,
                title,
                parent_session_id,
                working_dir: cwd_str,
                max_turns,
            });
        }
        Ok(Err(e)) => {
            let _ = ctx.writer_tx.send(DaemonMessage::SessionFailed {
                operation: "create_session".into(),
                error: e.to_string(),
            });
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
            let _ = ctx
                .writer_tx
                .send(DaemonMessage::SessionAttached { session_id });
            switch_attached_session(session_id, session_tx, ctx);
        }
        Ok(Err(e)) => {
            let _ = ctx.writer_tx.send(DaemonMessage::SessionFailed {
                operation: "attach_session".into(),
                error: e.to_string(),
            });
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
                let _ = ctx.writer_tx.send(DaemonMessage::SessionFailed {
                    operation: "set_account".into(),
                    error: format!("account '{name}' not found"),
                });
            }
        }
    } else {
        let _ = ctx.writer_tx.send(DaemonMessage::SessionFailed {
            operation: "set_account".into(),
            error: "no session attached".to_string(),
        });
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
            let _ = ctx.writer_tx.send(DaemonMessage::Unlocked);
        }
        Ok(Err(e)) => {
            let _ = ctx.writer_tx.send(DaemonMessage::LockedError { error: e });
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
            let _ = ctx.writer_tx.send(DaemonMessage::Models {
                models,
                selected_model,
            });
        }
        Ok(Err(e)) => {
            let _ = ctx.writer_tx.send(DaemonMessage::ModelsFailed { error: e });
        }
        Err(_) => warn!("daemon disconnected while handling list models"),
    }
}

fn handle_get_credential_sync(ctx: &mut ClientCtx, service: String) {
    let result = request_daemon(ctx.daemon_tx, |reply| DaemonCommand::GetCredential {
        service: service.clone(),
        reply,
    });
    match result {
        Ok(Some(key)) => {
            let _ = ctx.writer_tx.send(DaemonMessage::Credential {
                service,
                key: Some(key),
            });
        }
        Ok(None) => {
            let _ = ctx
                .writer_tx
                .send(DaemonMessage::Credential { service, key: None });
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
            let _ = ctx.writer_tx.send(DaemonMessage::SessionDeleteFailed {
                session_id,
                error: e.to_string(),
            });
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
            let _ = ctx
                .writer_tx
                .send(DaemonMessage::CredentialAdded { service });
        }
        Ok(Err(e)) => {
            let _ = ctx
                .writer_tx
                .send(DaemonMessage::CredentialAddFailed { service, error: e });
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
            let _ = ctx
                .writer_tx
                .send(DaemonMessage::CredentialRemoved { service });
        }
        Ok(Err(e)) => {
            let _ = ctx
                .writer_tx
                .send(DaemonMessage::CredentialRemoveFailed { service, error: e });
        }
        Err(_) => warn!("daemon disconnected while handling remove credential"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handle_unlock_sync_ok() {
        let (daemon_tx, daemon_rx) = mpsc::channel();
        let (writer_tx, writer_rx) = mpsc::sync_channel(SUBSCRIBER_CHANNEL_CAPACITY);
        let mut none_id = None;
        let mut none_tx = None;
        let mut ctx = ClientCtx {
            writer_tx: &writer_tx,
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
        let (writer_tx, writer_rx) = mpsc::sync_channel(SUBSCRIBER_CHANNEL_CAPACITY);
        let mut none_id = None;
        let mut none_tx = None;
        let mut ctx = ClientCtx {
            writer_tx: &writer_tx,
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
        let (writer_tx, writer_rx) = mpsc::sync_channel(SUBSCRIBER_CHANNEL_CAPACITY);
        let mut none_id = None;
        let mut none_tx = None;
        let mut ctx = ClientCtx {
            writer_tx: &writer_tx,
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
        let (writer_tx, writer_rx) = mpsc::sync_channel(SUBSCRIBER_CHANNEL_CAPACITY);
        let mut none_id = None;
        let mut none_tx = None;
        let mut ctx = ClientCtx {
            writer_tx: &writer_tx,
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
    fn handle_list_models_sync_err() {
        let (daemon_tx, daemon_rx) = mpsc::channel();
        let (writer_tx, writer_rx) = mpsc::sync_channel(SUBSCRIBER_CHANNEL_CAPACITY);
        let mut none_id = None;
        let mut none_tx = None;
        let mut ctx = ClientCtx {
            writer_tx: &writer_tx,
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
        let (writer_tx, writer_rx) = mpsc::sync_channel(SUBSCRIBER_CHANNEL_CAPACITY);
        let mut none_id = None;
        let mut none_tx = None;
        let mut ctx = ClientCtx {
            writer_tx: &writer_tx,
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
        let (writer_tx, writer_rx) = mpsc::sync_channel(SUBSCRIBER_CHANNEL_CAPACITY);
        let mut none_id = None;
        let mut none_tx = None;
        let mut ctx = ClientCtx {
            writer_tx: &writer_tx,
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
        let (writer_tx, _writer_rx) =
            mpsc::sync_channel::<DaemonMessage>(SUBSCRIBER_CHANNEL_CAPACITY);
        let (daemon_tx, _daemon_rx) = mpsc::channel();
        let mut attached_id = Some(1u64);
        let mut attached_tx = Some(old_tx);
        let mut ctx = ClientCtx {
            writer_tx: &writer_tx,
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
        let (writer_tx, _writer_rx) =
            mpsc::sync_channel::<DaemonMessage>(SUBSCRIBER_CHANNEL_CAPACITY);
        let (daemon_tx, _daemon_rx) = mpsc::channel();
        let mut attached_id = Some(1u64);
        let mut attached_tx = Some(old_tx);
        let mut ctx = ClientCtx {
            writer_tx: &writer_tx,
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
        let (writer_tx, writer_rx) = mpsc::sync_channel(SUBSCRIBER_CHANNEL_CAPACITY);
        let mut none_id = None;
        let mut none_tx = None;
        let mut ctx = ClientCtx {
            writer_tx: &writer_tx,
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
        let (writer_tx, writer_rx) = mpsc::sync_channel(SUBSCRIBER_CHANNEL_CAPACITY);
        let mut none_id = None;
        let mut none_tx = None;
        let mut ctx = ClientCtx {
            writer_tx: &writer_tx,
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
        assert!(matches!(msg, DaemonMessage::SessionDeleteFailed { .. }));
        if let DaemonMessage::SessionDeleteFailed { session_id, error } = &msg {
            assert_eq!(*session_id, 42);
            assert_eq!(error, "db error");
        }
    }

    #[test]
    fn handle_delete_session_sync_disconnected() {
        let (daemon_tx, daemon_rx) = mpsc::channel::<DaemonCommand>();
        let (writer_tx, writer_rx) = mpsc::sync_channel(SUBSCRIBER_CHANNEL_CAPACITY);
        let mut none_id = None;
        let mut none_tx = None;
        let mut ctx = ClientCtx {
            writer_tx: &writer_tx,
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
        let (writer_tx, _writer_rx) =
            mpsc::sync_channel::<DaemonMessage>(SUBSCRIBER_CHANNEL_CAPACITY);
        let (daemon_tx, _daemon_rx) = mpsc::channel();
        let mut attached_id: Option<u64> = None;
        let mut attached_tx: Option<mpsc::Sender<SessionCommand>> = None;
        let mut ctx = ClientCtx {
            writer_tx: &writer_tx,
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
        let (writer_tx, _writer_rx) =
            mpsc::sync_channel::<DaemonMessage>(SUBSCRIBER_CHANNEL_CAPACITY);
        let (session_tx, session_rx) = mpsc::channel();
        let mut attached_id = Some(1u64);
        let mut attached_tx = Some(session_tx);
        let mut ctx = ClientCtx {
            writer_tx: &writer_tx,
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
        let (writer_tx, writer_rx) =
            mpsc::sync_channel::<DaemonMessage>(SUBSCRIBER_CHANNEL_CAPACITY);
        let mut none_id = None;
        let mut none_tx = None;
        let mut ctx = ClientCtx {
            writer_tx: &writer_tx,
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
        let (writer_tx, _writer_rx) =
            mpsc::sync_channel::<DaemonMessage>(SUBSCRIBER_CHANNEL_CAPACITY);
        let (session_tx, session_rx) = mpsc::channel();
        let mut attached_id = Some(1u64);
        let mut attached_tx = Some(session_tx);
        let mut ctx = ClientCtx {
            writer_tx: &writer_tx,
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
        let (writer_tx, writer_rx) =
            mpsc::sync_channel::<DaemonMessage>(SUBSCRIBER_CHANNEL_CAPACITY);
        let mut none_id = None;
        let mut none_tx = None;
        let mut ctx = ClientCtx {
            writer_tx: &writer_tx,
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
        let (writer_tx, _writer_rx) =
            mpsc::sync_channel::<DaemonMessage>(SUBSCRIBER_CHANNEL_CAPACITY);
        let (session_tx, session_rx) = mpsc::channel();
        let mut attached_id = Some(1u64);
        let mut attached_tx = Some(session_tx);
        let mut ctx = ClientCtx {
            writer_tx: &writer_tx,
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
        let (writer_tx, writer_rx) =
            mpsc::sync_channel::<DaemonMessage>(SUBSCRIBER_CHANNEL_CAPACITY);
        let mut none_id = None;
        let mut none_tx = None;
        let mut ctx = ClientCtx {
            writer_tx: &writer_tx,
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
            DaemonMessage::Failed {
                request_id: 7,
                error,
            } if error == "no session attached"
        ));
    }
}
