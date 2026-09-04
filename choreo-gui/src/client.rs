use crate::state::{AppState, UiEvent};
use choreo_client_core::{
    ClientError, ConnectionMode, ShellCommand, bind_fresh_daemon, build_add_credential_message,
    dispatch_daemon_message, parse_input_line, record_unlock_key, resolve_private_key,
    run_daemon_connection_with_mode, shell_command_echo,
};
use choreo_proto::{ClientMessage, DaemonMessage, SessionEvent, socket_path};
use dioxus::prelude::*;
use futures_channel::mpsc::UnboundedSender;
use zeroize::Zeroize;

pub(crate) fn run_client(
    mode: ConnectionMode,
    client_rx: std::sync::mpsc::Receiver<ClientMessage>,
    ui_tx: UnboundedSender<UiEvent>,
) -> Result<(), ClientError> {
    let result = run_daemon_connection_with_mode(
        mode,
        |message| {
            if let Err(e) = ui_tx.unbounded_send(UiEvent::Daemon(message)) {
                tracing::error!("failed to send Daemon UI event: {e}");
            }
        },
        client_rx,
        None,
    );
    if result.is_ok()
        && let Err(e) = ui_tx.unbounded_send(UiEvent::ReaderClosed)
    {
        tracing::warn!("failed to send ReaderClosed UI event: {e}");
    }
    result
}

pub(crate) fn submit_input(
    state: &mut Signal<AppState>,
    daemon_tx: Option<std::sync::mpsc::Sender<ClientMessage>>,
) {
    let line = state.read().input.trim().to_string();
    state.write().input.clear();
    let command = {
        let mut guard = state.write();
        let attached = guard.attached_session_id;
        parse_input_line(&line, &mut guard.next_request_id, attached)
    };
    handle_shell_command(&mut state.write(), daemon_tx, command);
}

/// The address string used to key per-daemon unlock keys in known_servers:
/// the actual dial address for TCP, the unix socket path otherwise. This is
/// the same address the daemon's keystore binding is recorded against, so
/// every `Unlock`/`AddCredential`/record must use it consistently.
pub(crate) fn connection_addr() -> String {
    match crate::CONNECTION_MODE.get() {
        Some(ConnectionMode::UnixSocket(path)) => path.clone(),
        Some(ConnectionMode::Tcp { addr, .. }) => addr.clone(),
        Some(ConnectionMode::TcpPinned(addr)) => addr.clone(),
        // Unset (e.g. unit tests, or a mis-wired launcher): default to the
        // unix socket path, which is what a local default-mode connection uses.
        None => socket_path(),
    }
}

pub(crate) fn handle_shell_command(
    state: &mut AppState,
    daemon_tx: Option<std::sync::mpsc::Sender<ClientMessage>>,
    command: ShellCommand,
) {
    match command {
        ShellCommand::Empty => {}
        ShellCommand::InvalidCancel(value) => state
            .status_texts
            .push(format!("invalid request id: {value}")),
        ShellCommand::UnknownCommand(error) => state.status_texts.push(error),
        ShellCommand::Send(message) => send_client_message(state, daemon_tx, message),
        ShellCommand::Unlock { method } => match resolve_private_key(&method, &connection_addr()) {
            Ok(private_key) => {
                // Hold the key until the daemon confirms the unlock, then
                // record it per-daemon (see [`record_pending_unlock_key`]).
                state.pending_unlock_key = Some(private_key.clone());
                send_client_message(state, daemon_tx, ClientMessage::Unlock { private_key });
            }
            Err(e) => {
                state.status_texts.push(format!("[error] {e}"));
            }
        },
        ShellCommand::AddCredential {
            service,
            credential_type,
            fields,
        } => {
            match build_add_credential_message(&connection_addr(), service, credential_type, fields)
            {
                Ok((msg, key)) => {
                    // Record the key only after the daemon CONFIRMS it
                    // (CredentialAdded / Unlocked); the daemon may reject the
                    // key (misbound keystore) and we must not persist a rejected
                    // key. Hold it pending here, clear on error/confirmation.
                    state.pending_unlock_key = Some(key);
                    send_client_message(state, daemon_tx, msg);
                }
                Err(e) => state.status_texts.push(format!("[error] {e}")),
            }
        }
        ShellCommand::AclAdd { pubkey } => {
            // The daemon enforces local-only; forward like any other command
            // and surface the refusal if this GUI connection is remote.
            send_client_message(state, daemon_tx, ClientMessage::AclAdd { pubkey });
        }
        ShellCommand::RemoveCredential { service } => {
            send_client_message(
                state,
                daemon_tx,
                ClientMessage::RemoveCredential { service },
            );
        }
        ShellCommand::Undo => {
            send_client_message(state, daemon_tx, ClientMessage::Undo);
        }
        ShellCommand::Redo => {
            send_client_message(state, daemon_tx, ClientMessage::Redo);
        }
        ShellCommand::Continue => {
            if state.attached_session_id.is_some() {
                let request_id = state.next_request_id;
                state.next_request_id = state.next_request_id.wrapping_add(1);
                send_client_message(
                    state,
                    daemon_tx,
                    ClientMessage::ContinueGeneration { request_id },
                );
            } else {
                state.status_texts.push("no session attached".to_string());
            }
        }
        ShellCommand::Stop => {
            // Send Cancel with request_id 0 (CANCEL_ALL sentinel) to stop
            // whatever request is currently active on the attached session.
            if state.attached_session_id.is_some() {
                send_client_message(state, daemon_tx, ClientMessage::Cancel { request_id: 0 });
            } else {
                state.status_texts.push("no session attached".to_string());
            }
        }
        ShellCommand::RefreshModels { force } => {
            state.status_texts.push(if force {
                "refreshing models… (forced)".to_string()
            } else {
                "refreshing models…".to_string()
            });
            send_client_message(state, daemon_tx, ClientMessage::RefreshModels { force });
        }
    }
}

pub(crate) fn send_client_message(
    state: &mut AppState,
    daemon_tx: Option<std::sync::mpsc::Sender<ClientMessage>>,
    message: ClientMessage,
) {
    let Some(sender) = daemon_tx else {
        state
            .status_texts
            .push("[client] not connected".to_string());
        return;
    };

    if let Some(echo) = shell_command_echo(&ShellCommand::Send(message.clone())) {
        state.status_texts.push(echo);
    }

    if let Err(error) = sender.send(message) {
        state
            .status_texts
            .push(format!("[client] failed to send command: {error}"));
    }
}

/// Handles session lifecycle messages (auto-attach, session creation, attaching).
///
/// Returns `true` if the message was fully handled and should skip
/// [`dispatch_daemon_message`]. Returns `false` if dispatch should run after
/// this function.
fn handle_session_message(
    state: &mut AppState,
    daemon_tx: &Option<std::sync::mpsc::Sender<ClientMessage>>,
    message: &DaemonMessage,
) -> bool {
    match message {
        DaemonMessage::Session {
            session_id: Some(session_id),
            event: SessionEvent::SessionCreated { .. } | SessionEvent::SessionAttached,
        } => {
            // The envelope's `session_id` is now `Option<u64>`; these two
            // events are always session-scoped (the daemon never emits them
            // with `None`), so binding `Some(session_id)` gives us the origin
            // session id directly. Record the new attached session id, but
            // let dispatch emit the informational text message.
            state.attached_session_id = Some(*session_id);
            false
        }
        DaemonMessage::Sessions { sessions } => {
            if sessions.is_empty() {
                state.status_texts.push("[daemon] no sessions".to_string());
            } else {
                state
                    .status_texts
                    .push(format!("[daemon] sessions ({})", sessions.len()));
                for session in sessions {
                    let prefix = if Some(session.session_id) == state.attached_session_id {
                        "*"
                    } else {
                        " "
                    };
                    let title = session.title.as_deref().unwrap_or("untitled");
                    let model = session.selected_model.as_deref().unwrap_or("-");
                    state.status_texts.push(format!(
                        "{} {}: \"{title}\" ({model}) — {} turns",
                        prefix, session.session_id, session.turn_count,
                    ));
                }
            }
            if state.attached_session_id.is_none()
                && let Some(sender) = daemon_tx
            {
                if let Some(first) = sessions.first() {
                    if let Err(e) = sender.send(ClientMessage::AttachSession {
                        session_id: first.session_id,
                    }) {
                        tracing::error!("failed to send AttachSession: {e}");
                    }
                } else {
                    if let Err(e) = sender.send(ClientMessage::CreateSession {
                        title: Some("default".to_string()),
                        parent_session_id: None,
                        working_dir: None,
                        context_config: None,
                        account_name: None,
                        selected_model: None,
                        reasoning_effort: None,
                    }) {
                        tracing::error!("failed to send CreateSession: {e}");
                    }
                }
            }
            true
        }
        _ => false,
    }
}

pub(crate) fn apply_daemon_message(
    state: &mut AppState,
    message: DaemonMessage,
    daemon_tx: Option<std::sync::mpsc::Sender<ClientMessage>>,
) -> Result<(), ClientError> {
    if handle_session_message(state, &daemon_tx, &message) {
        return Ok(());
    }

    // Unlock/AddCredential outcome (per-daemon keystore): resolve what the
    // daemon did with the pending key — the key carried by the most recent
    // `Unlock`/`AddCredential` — and reconcile the client-side record with it.
    match &message {
        // CONFIRMED success (explicit Unlock, an AddCredential that
        // implicitly unlocked, or our auto-bind of an unbound daemon).
        // Record the pending key per-daemon NOW — the whole point of the
        // per-daemon keystore design is that a key is only trusted/recorded
        // after the daemon accepts it (TOFU adopt, or a binding match). A
        // rejected key never reaches here.
        DaemonMessage::Unlocked | DaemonMessage::Bound | DaemonMessage::CredentialAdded { .. } => {
            if let Some(key) = state.pending_unlock_key.take()
                && let Err(e) = record_unlock_key(&connection_addr(), &key)
            {
                state
                    .status_texts
                    .push(format!("[error] failed to record unlock key: {e}"));
            }
        }
        // Verify-only operation against an UNBOUND keystore (the daemon has
        // no binding at all — the pending key was a verify attempt that can
        // never succeed). Drop the pending key, then AUTO-BIND once per
        // connection: `bind_fresh_daemon` mints a fresh CSPRNG key, records
        // it into known_servers PRE-SEND, and returns the `BindKeystore`
        // message; the daemon replies `Bound`, which the arm above treats
        // exactly like `Unlocked`. A second `KeystoreUnbound` after the bind
        // was sent is surfaced as an error, never a re-bind (bind-loop guard).
        DaemonMessage::KeystoreUnbound { .. } => {
            if let Some(mut key) = state.pending_unlock_key.take() {
                key.zeroize();
                tracing::info!(
                    "daemon keystore is unbound; discarding the verify-only pending key"
                );
            }
            if state.keystore_bind_attempted {
                tracing::warn!("keystore still unbound after a bind attempt; not re-binding");
                state.status_texts.push(
                    "[daemon] keystore still unbound after bind attempt — reconnect to \
                           retry"
                        .to_string(),
                );
            } else {
                state.keystore_bind_attempted = true;
                match bind_fresh_daemon(&connection_addr()) {
                    Ok((key, msg)) => {
                        tracing::info!("auto-binding unbound daemon with a fresh key");
                        // Same pending-confirm flow as Unlock/AddCredential:
                        // the `Bound` reply records the minted key.
                        state.pending_unlock_key = Some(key.to_vec());
                        send_client_message(state, daemon_tx, msg);
                    }
                    Err(e) => {
                        tracing::warn!(%e, "auto-bind failed");
                        state
                            .status_texts
                            .push(format!("[error] auto-bind failed: {e}"));
                    }
                }
            }
        }
        // REJECTED (Unlock/AddCredential failure). Drop the pending key
        // (zeroized — it is secret material) so a later, unrelated
        // confirmation cannot attribute the rejection back. Deliberately does
        // NOT delete the known_servers record: a rejection is not proof the
        // key is bad (the daemon reports transient failures through the same
        // error), and the keystore binding is TOFU-immortal so a confirmed
        // record can never be wrong. Manual re-pair (`remove(addr)`) is the
        // recovery path for a genuinely wrong key.
        DaemonMessage::LockedError { .. } | DaemonMessage::CredentialAddFailed { .. } => {
            if let Some(mut key) = state.pending_unlock_key.take() {
                key.zeroize();
                tracing::info!(
                    "daemon rejected the presented unlock key; the known_servers record is kept"
                );
            }
        }
        _ => {}
    }

    dispatch_daemon_message(&message, state);
    Ok(())
}
