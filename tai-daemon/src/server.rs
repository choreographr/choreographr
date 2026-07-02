use crate::openai::{OpenAiClient, RequestFormat, load_service_config};
use crate::requests::{emit_demo_image, execute_chat_tool_request, execute_plain_request};
use crate::sessions::{
    ActiveRequest, DaemonState, SessionState, broadcast_message_appended, broadcast_to_session,
    default_session_id, list_sessions, require_attached_session, session_by_id,
    session_snapshot, update_subscription,
};
use crate::tools::x;
use std::{collections::HashMap, io, path::Path, sync::Arc, time::Duration};
use tai_keystore::{Keystore, ServiceCredential, keystore_path};
use tai_proto::{ClientMessage, DaemonMessage, SessionMessage, read_message, write_message};
use tokio::{
    io::AsyncWriteExt,
    net::{UnixListener, UnixStream},
    signal::unix::{signal, SignalKind},
    sync::{Mutex, mpsc},
    task::JoinSet,
};
use tracing::{debug, error, info, warn};

const REQUEST_TIMEOUT_SECS: u64 = 300;

macro_rules! send_or_warn {
    ($tx:expr, $msg:expr) => {
        if let Err(e) = $tx.send($msg).await {
            warn!(error = %e, "failed to send daemon message, client likely disconnected");
        }
    };
}

macro_rules! try_session {
    ($state:expr, $attached:expr, $tx:expr) => {
        match require_attached_session($state, $attached, $tx).await? {
            Some((session_id, session)) => (session_id, session),
            None => return Ok(()),
        }
    };
}

macro_rules! try_client {
    ($state:expr, $tx:expr) => {
        match require_openai_client($state, $tx).await? {
            Some(c) => c,
            None => return Ok(()),
        }
    };
}

// ---------------------------------------------------------------------------
// Server lifecycle
// ---------------------------------------------------------------------------

async fn wait_for_shutdown() {
    let mut sigint = signal(SignalKind::interrupt()).expect("failed to install SIGINT handler");
    let mut sigterm = signal(SignalKind::terminate()).expect("failed to install SIGTERM handler");
    tokio::select! {
        _ = sigint.recv() => info!("received SIGINT, shutting down tai-daemon"),
        _ = sigterm.recv() => info!("received SIGTERM, shutting down tai-daemon"),
    }
}

const SHUTDOWN_DRAIN_SECS: u64 = 10;

pub async fn run_server(socket_path: &str, state: DaemonState) -> io::Result<()> {
    if Path::new(socket_path).exists() {
        info!(%socket_path, "removing stale socket");
        std::fs::remove_file(socket_path)?;
    }

    let listener = UnixListener::bind(socket_path)?;
    info!(%socket_path, "tai-daemon listening");

    let mut client_handles = JoinSet::new();

    let result = loop {
        tokio::select! {
            accept_result = listener.accept() => {
                let (stream, _) = accept_result?;
                debug!("accepted client connection");
                let state = Arc::clone(&state);
                client_handles.spawn(async move {
                    if let Err(error) = handle_client(stream, state).await {
                        error!(error = %error, "client error");
                    }
                });
            }
            _ = wait_for_shutdown() => break Ok(()),
        }
    };

    info!("draining active client connections ({}s timeout)...", SHUTDOWN_DRAIN_SECS);
    let drained = tokio::time::timeout(
        Duration::from_secs(SHUTDOWN_DRAIN_SECS),
        async {
            while let Some(result) = client_handles.join_next().await {
                if let Err(e) = result && !e.is_cancelled() {
                    warn!(error = %e, "client handler panicked during drain");
                }
            }
        },
    )
    .await;

    if drained.is_err() {
        warn!("shutdown drain timed out, aborting remaining client handlers");
        client_handles.abort_all();
    }

    if Path::new(socket_path).exists() {
        info!(%socket_path, "removing socket");
        std::fs::remove_file(socket_path)?;
    }

    result
}

// ---------------------------------------------------------------------------
// Connection-level helpers
// ---------------------------------------------------------------------------

async fn require_openai_client(
    state: &DaemonState,
    tx: &mpsc::Sender<DaemonMessage>,
) -> io::Result<Option<Arc<OpenAiClient>>> {
    let client = {
        let guard = state.lock().await;
        guard.openai_client.as_ref().map(Arc::clone)
    };
    match client {
        Some(c) => Ok(Some(c)),
        None => {
            send_or_warn!(
                tx,
                DaemonMessage::LockedError {
                    error: "daemon is locked. use /unlock <passphrase> to unlock".to_string(),
                }
            );
            Ok(None)
        }
    }
}

async fn try_keystore_path(
    tx: &mpsc::Sender<DaemonMessage>,
    build_error: impl FnOnce(String) -> DaemonMessage,
) -> io::Result<Option<std::path::PathBuf>> {
    match keystore_path() {
        Ok(p) => Ok(Some(p)),
        Err(e) => {
            send_or_warn!(tx, build_error(format!("failed to determine keystore path: {e}")));
            Ok(None)
        }
    }
}

// ---------------------------------------------------------------------------
// Main client handler
// ---------------------------------------------------------------------------

pub async fn handle_client(
    stream: UnixStream,
    state: DaemonState,
) -> io::Result<()> {
    let (mut reader, mut writer) = stream.into_split();
    let (tx, mut rx) = mpsc::channel::<DaemonMessage>(128);
    let client_id = {
        let mut guard = state.lock().await;
        let client_id = guard.next_client_id;
        guard.next_client_id = guard.next_client_id.wrapping_add(1);
        client_id
    };
    let mut attached_session_id = default_session_id(&state).await;
    if let Some(session_id) = attached_session_id {
        update_subscription(&state, client_id, None, Some(session_id), &tx).await;
    }

    debug!("starting client handler");

    let writer_task = tokio::spawn(async move {
        while let Some(message) = rx.recv().await {
            debug!(?message, "sending daemon message");
            write_message(&mut writer, &message).await?;
        }
        debug!("writer task shutting down");
        writer.shutdown().await
    });

    loop {
        match read_message::<_, ClientMessage>(&mut reader).await {
            Ok(message) => {
                debug!(?message, "received client message");
                handle_client_message(
                    message,
                    &state,
                    &tx,
                    client_id,
                    &mut attached_session_id,
                )
                .await?;
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::UnexpectedEof | io::ErrorKind::ConnectionReset
                ) =>
            {
                debug!(error = %error, "client disconnected");
                break;
            }
            Err(error) => {
                error!(error = %error, "failed to read client message");
                return Err(error);
            }
        }
    }

    update_subscription(&state, client_id, attached_session_id, None, &tx).await;
    drop(tx);
    writer_task.abort();
    match writer_task.await {
        Ok(Ok(())) => {}
        Ok(Err(error))
            if matches!(
                error.kind(),
                io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset
            ) =>
        {
            debug!(error = %error, "writer task ended after client disconnect");
        }
        Ok(Err(error)) => return Err(error),
        Err(error) if error.is_cancelled() => {}
        Err(error) => return Err(io::Error::other(error)),
    }
    debug!("client handler finished");
    Ok(())
}

// ---------------------------------------------------------------------------
// Message dispatch
// ---------------------------------------------------------------------------

async fn handle_client_message(
    msg: ClientMessage,
    state: &DaemonState,
    tx: &mpsc::Sender<DaemonMessage>,
    client_id: u64,
    attached_session_id: &mut Option<u64>,
) -> io::Result<()> {
    match msg {
        ClientMessage::CreateSession { title } => {
            handle_create_session(state, tx, client_id, attached_session_id, title).await
        }
        ClientMessage::ListSessions => {
            let sessions = list_sessions(state).await;
            send_or_warn!(tx, DaemonMessage::Sessions { sessions });
            Ok(())
        }
        ClientMessage::AttachSession { session_id } => {
            handle_attach_session(state, tx, client_id, attached_session_id, session_id).await
        }
        ClientMessage::GetSessionState { session_id } => {
            let Some(session) = session_by_id(state, session_id).await else {
                send_or_warn!(tx, DaemonMessage::SessionFailed {
                    operation: "get_session_state".to_string(),
                    error: format!("unknown session: {session_id}"),
                });
                return Ok(());
            };
            let snapshot = session_snapshot(session_id, &session).await;
            send_or_warn!(tx, snapshot);
            Ok(())
        }
        ClientMessage::RunInput { request_id, input } => {
            handle_run_input(state, tx, client_id, *attached_session_id, request_id, input).await
        }
        ClientMessage::TestImage { request_id } => {
            handle_test_image(state, tx, *attached_session_id, request_id).await
        }
        ClientMessage::Cancel { request_id } => {
            handle_cancel(state, tx, client_id, *attached_session_id, request_id).await
        }
        ClientMessage::Ping => {
            debug!("responding to ping");
            send_or_warn!(tx, DaemonMessage::Pong);
            Ok(())
        }
        ClientMessage::ListModels => {
            handle_list_models(state, tx, *attached_session_id).await
        }
        ClientMessage::SetModel { model } => {
            handle_set_model(state, tx, client_id, *attached_session_id, model).await
        }
        ClientMessage::Unlock { passphrase } => {
            handle_unlock(state, tx, passphrase).await
        }
        ClientMessage::Lock => {
            handle_lock(state, tx).await
        }
        ClientMessage::GetCredential { service } => {
            handle_get_credential(state, tx, service).await
        }
        ClientMessage::AddApiKey { service, passphrase, key } => {
            handle_add_api_key(tx, service, passphrase, key).await
        }
        ClientMessage::AddXCredential { service, passphrase, api_key, api_key_secret, access_token, access_token_secret, bearer_token } => {
            handle_add_x_credential(tx, service, passphrase, api_key, api_key_secret, access_token, access_token_secret, bearer_token).await
        }
        ClientMessage::RemoveCredential { service, passphrase } => {
            handle_remove_credential(tx, service, passphrase).await
        }
    }
}

// ---------------------------------------------------------------------------
// Session management handlers
// ---------------------------------------------------------------------------

async fn handle_create_session(
    state: &DaemonState,
    tx: &mpsc::Sender<DaemonMessage>,
    client_id: u64,
    attached_session_id: &mut Option<u64>,
    title: Option<String>,
) -> io::Result<()> {
    let (session_id, session) = {
        let mut guard = state.lock().await;
        let session_id = guard.next_session_id;
        guard.next_session_id = guard.next_session_id.wrapping_add(1);
        let session = Arc::new(Mutex::new(SessionState {
            title: title.clone(),
            selected_model: None,
            messages: Vec::new(),
            active_requests: HashMap::new(),
            subscribers: HashMap::new(),
        }));
        guard.sessions.insert(session_id, Arc::clone(&session));
        (session_id, session)
    };
    update_subscription(
        state,
        client_id,
        *attached_session_id,
        Some(session_id),
        tx,
    )
    .await;
    *attached_session_id = Some(session_id);
    send_or_warn!(tx, DaemonMessage::SessionCreated {
        session_id,
        title: title.clone(),
    });
    send_or_warn!(tx, DaemonMessage::SessionAttached { session_id });
    let snapshot = session_snapshot(session_id, &session).await;
    send_or_warn!(tx, snapshot);
    Ok(())
}

async fn handle_attach_session(
    state: &DaemonState,
    tx: &mpsc::Sender<DaemonMessage>,
    client_id: u64,
    attached_session_id: &mut Option<u64>,
    session_id: u64,
) -> io::Result<()> {
    let Some(session) = session_by_id(state, session_id).await else {
        send_or_warn!(tx, DaemonMessage::SessionFailed {
            operation: "attach_session".to_string(),
            error: format!("unknown session: {session_id}"),
        });
        return Ok(());
    };
    update_subscription(
        state,
        client_id,
        *attached_session_id,
        Some(session_id),
        tx,
    )
    .await;
    *attached_session_id = Some(session_id);
    send_or_warn!(tx, DaemonMessage::SessionAttached { session_id });
    let snapshot = session_snapshot(session_id, &session).await;
    send_or_warn!(tx, snapshot);
    Ok(())
}

// ---------------------------------------------------------------------------
// Request handlers
// ---------------------------------------------------------------------------

async fn handle_run_input(
    state: &DaemonState,
    tx: &mpsc::Sender<DaemonMessage>,
    client_id: u64,
    attached_session_id: Option<u64>,
    request_id: u32,
    input: Vec<u8>,
) -> io::Result<()> {
    let (session_id, session) = try_session!(state, attached_session_id, tx);

    let text = String::from_utf8_lossy(&input).trim().to_string();
    if text.is_empty() {
        warn!(request_id, "request failed: empty input");
        send_or_warn!(tx, DaemonMessage::Started { request_id });
        send_or_warn!(tx, DaemonMessage::Failed {
            request_id,
            error: "empty input".to_string(),
        });
        return Ok(());
    }

    let client = try_client!(state, tx);

    let model = {
        let mut guard = session.lock().await;
        if let Some(existing) = guard.active_requests.get(&request_id) {
            if existing.handle.is_finished() {
                guard.active_requests.remove(&request_id);
            } else {
                warn!(request_id, session_id, "duplicate request id rejected");
                drop(guard);
                send_or_warn!(tx, DaemonMessage::Failed {
                    request_id,
                    error: "request id already active".to_string(),
                });
                return Ok(());
            }
        }
        let Some(model) = guard.selected_model.clone() else {
            warn!(request_id, session_id, "request failed: no model selected");
            send_or_warn!(tx, DaemonMessage::Started { request_id });
            send_or_warn!(tx, DaemonMessage::Failed {
                request_id,
                error: "no model selected".to_string(),
            });
            return Ok(());
        };
        let message = SessionMessage::UserText {
            content: text.clone(),
        };
        guard.messages.push(message.clone());
        drop(guard);
        broadcast_message_appended(&session, message, Some(client_id)).await;
        model
    };

    let request_format = client.config().request_format_for_model(&model);
    info!(request_id, session_id, input_len = input.len(), selected_model = %model, ?request_format, "starting request");
    let session_clone = Arc::clone(&session);
    let handle = tokio::spawn(async move {
        broadcast_to_session(
            &session_clone,
            DaemonMessage::Started { request_id },
            None,
        )
        .await;

        let result = tokio::time::timeout(
            Duration::from_secs(REQUEST_TIMEOUT_SECS),
            async {
                match request_format {
                    RequestFormat::Responses => {
                        execute_plain_request(
                            &client,
                            &session_clone,
                            &model,
                            request_id,
                        )
                        .await
                    }
                    RequestFormat::ChatCompletions => {
                        execute_chat_tool_request(
                            &client,
                            &session_clone,
                            &model,
                            request_id,
                        )
                        .await
                    }
                }
            },
        )
        .await;

        let inner_result = match result {
            Ok(inner) => inner,
            Err(_elapsed) => {
                warn!(request_id, session_id, "request timed out");
                broadcast_to_session(
                    &session_clone,
                    DaemonMessage::Failed {
                        request_id,
                        error: format!("request timed out after {REQUEST_TIMEOUT_SECS}s"),
                    },
                    None,
                )
                .await;
                return;
            }
        };

        match inner_result {
            Ok(()) => {
                info!(request_id, session_id, "request completed");
                broadcast_to_session(
                    &session_clone,
                    DaemonMessage::Done { request_id },
                    None,
                )
                .await;
            }
            Err(error) => {
                warn!(request_id, session_id, error = %error, "request failed");
                broadcast_to_session(
                    &session_clone,
                    DaemonMessage::Failed {
                        request_id,
                        error: format!("model request failed: {error}"),
                    },
                    None,
                )
                .await;
            }
        }
    });

    session
        .lock()
        .await
        .active_requests
        .insert(request_id, ActiveRequest { handle });
    Ok(())
}

async fn handle_test_image(
    state: &DaemonState,
    tx: &mpsc::Sender<DaemonMessage>,
    attached_session_id: Option<u64>,
    request_id: u32,
) -> io::Result<()> {
    try_session!(state, attached_session_id, tx);
    info!(request_id, "sending demo image");
    send_or_warn!(tx, DaemonMessage::Started { request_id });
    match emit_demo_image(tx, request_id, 1).await {
        Ok(()) => {
            send_or_warn!(tx, DaemonMessage::Done { request_id });
        }
        Err(e) => {
            warn!(
                request_id,
                error = %e,
                "client disconnected before image could be delivered"
            );
        }
    }
    Ok(())
}

async fn handle_cancel(
    state: &DaemonState,
    tx: &mpsc::Sender<DaemonMessage>,
    client_id: u64,
    attached_session_id: Option<u64>,
    request_id: u32,
) -> io::Result<()> {
    let (session_id, session) = try_session!(state, attached_session_id, tx);
    if let Some(active_request) =
        session.lock().await.active_requests.remove(&request_id)
    {
        info!(request_id, session_id, "cancelling active request");
        active_request.handle.abort();
        send_or_warn!(tx, DaemonMessage::Cancelled { request_id });
        broadcast_to_session(
            &session,
            DaemonMessage::Cancelled { request_id },
            Some(client_id),
        )
        .await;
    } else {
        warn!(
            request_id,
            session_id, "cancel requested for inactive request"
        );
        send_or_warn!(tx, DaemonMessage::Failed {
            request_id,
            error: "request id not active".to_string(),
        });
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Model management handlers
// ---------------------------------------------------------------------------

async fn handle_list_models(
    state: &DaemonState,
    tx: &mpsc::Sender<DaemonMessage>,
    attached_session_id: Option<u64>,
) -> io::Result<()> {
    let client = try_client!(state, tx);
    let config = client.config();
    debug!(base_url = %config.base_url, model_list_path = %config.model_list_path, responses_path = %config.responses_path, "listing configured models");
    let selected_model = match attached_session_id {
        Some(session_id) => match session_by_id(state, session_id).await {
            Some(session) => session.lock().await.selected_model.clone(),
            None => None,
        },
        None => None,
    };
    match client.validate_and_list_models().await {
        Ok(models) => {
            send_or_warn!(tx, DaemonMessage::Models {
                models,
                selected_model,
            });
        }
        Err(error) => {
            send_or_warn!(tx, DaemonMessage::ModelsFailed {
                error: format!("failed to list models: {error}"),
            });
        }
    }
    Ok(())
}

async fn handle_set_model(
    state: &DaemonState,
    tx: &mpsc::Sender<DaemonMessage>,
    _client_id: u64,
    attached_session_id: Option<u64>,
    model: String,
) -> io::Result<()> {
    let (session_id, session) = try_session!(state, attached_session_id, tx);
    let client = try_client!(state, tx);
    let config = client.config();
    debug!(base_url = %config.base_url, model_list_path = %config.model_list_path, responses_path = %config.responses_path, requested_model = %model, session_id, "setting selected model");
    match client.validate_and_list_models().await {
        Ok(models) => {
            if models.iter().any(|candidate| candidate == &model) {
                session.lock().await.selected_model = Some(model.clone());
                broadcast_to_session(
                    &session,
                    DaemonMessage::ModelSelected { model },
                    None,
                )
                .await;
            } else {
                send_or_warn!(tx, DaemonMessage::ModelSelectionFailed {
                    model: model.clone(),
                    error: format!("unknown model: {model}"),
                });
            }
        }
        Err(error) => {
            send_or_warn!(tx, DaemonMessage::ModelSelectionFailed {
                model,
                error: format!("failed to list models: {error}"),
            });
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Lock / unlock handlers
// ---------------------------------------------------------------------------

async fn handle_unlock(
    state: &DaemonState,
    tx: &mpsc::Sender<DaemonMessage>,
    passphrase: String,
) -> io::Result<()> {
    let Some(ks_path) = try_keystore_path(tx, |e| DaemonMessage::LockedError { error: e }).await?
    else {
        return Ok(());
    };
    if !ks_path.exists() {
        send_or_warn!(tx, DaemonMessage::LockedError {
            error: "keystore does not exist. run 'tai-keystore init' to create one.".to_string(),
        });
        return Ok(());
    }
    match Keystore::load(&ks_path, &passphrase) {
        Ok(ks) => {
            let keystore = Arc::new(ks);
            let mut guard = state.lock().await;
            match keystore.get_api_key("openai") {
                Some(api_key) => {
                    let service_config = match load_service_config() {
                        Ok(cfg) => cfg,
                        Err(e) => {
                            warn!(error = %e, "failed to load service config, using defaults — check config.toml");
                            Default::default()
                        }
                    };
                    match OpenAiClient::new(service_config, api_key.to_string()) {
                        Ok(client) => {
                            guard.openai_client = Some(Arc::new(client));
                            if let Some(x_creds) = keystore.get_x_credentials("twitter") {
                                x::set_x_credentials(x_creds);
                            }
                            guard.keystore = Some(keystore);
                            drop(guard);
                            send_or_warn!(tx, DaemonMessage::Unlocked);
                        }
                        Err(e) => {
                            drop(guard);
                            send_or_warn!(tx, DaemonMessage::LockedError {
                                error: format!("failed to create OpenAI client: {e}"),
                            });
                        }
                    }
                }
                None => {
                    drop(guard);
                    send_or_warn!(tx, DaemonMessage::LockedError {
                        error: "no 'openai' credential found in keystore".to_string(),
                    });
                }
            }
        }
        Err(e) => {
            send_or_warn!(tx, DaemonMessage::LockedError {
                error: format!("failed to unlock keystore: {e}"),
            });
        }
    }
    Ok(())
}

async fn handle_lock(
    state: &DaemonState,
    tx: &mpsc::Sender<DaemonMessage>,
) -> io::Result<()> {
    let mut guard = state.lock().await;
    guard.openai_client = None;
    guard.keystore = None;
    drop(guard);
    x::clear_x_credentials();
    send_or_warn!(tx, DaemonMessage::Locked);
    Ok(())
}

// ---------------------------------------------------------------------------
// Credential handlers
// ---------------------------------------------------------------------------

async fn handle_get_credential(
    state: &DaemonState,
    tx: &mpsc::Sender<DaemonMessage>,
    service: String,
) -> io::Result<()> {
    let key = {
        let guard = state.lock().await;
        guard.keystore.as_ref()
            .and_then(|ks| ks.get_api_key(&service).map(|k| k.to_string()))
    };
    send_or_warn!(tx, DaemonMessage::Credential { service, key });
    Ok(())
}

async fn handle_add_api_key(
    tx: &mpsc::Sender<DaemonMessage>,
    service: String,
    passphrase: String,
    key: String,
) -> io::Result<()> {
    let svc = service.clone();
    let Some(ks_path) = try_keystore_path(tx, |e| DaemonMessage::CredentialAddFailed {
        service: svc.clone(),
        error: e,
    })
    .await?
    else {
        return Ok(());
    };
    match Keystore::load(&ks_path, &passphrase) {
        Ok(mut keystore) => {
            keystore.add(svc.clone(), ServiceCredential::ApiKey { key });
            match keystore.save(&ks_path, &passphrase) {
                Ok(()) => {
                    send_or_warn!(tx, DaemonMessage::CredentialAdded { service: svc });
                }
                Err(e) => {
                    send_or_warn!(tx, DaemonMessage::CredentialAddFailed {
                        service: svc,
                        error: format!("failed to save keystore: {e}"),
                    });
                }
            }
        }
        Err(e) => {
            send_or_warn!(tx, DaemonMessage::CredentialAddFailed {
                service: svc,
                error: format!("failed to unlock keystore: {e}"),
            });
        }
    }
    Ok(())
}

async fn handle_add_x_credential(
    tx: &mpsc::Sender<DaemonMessage>,
    service: String,
    passphrase: String,
    api_key: String,
    api_key_secret: String,
    access_token: String,
    access_token_secret: String,
    bearer_token: Option<String>,
) -> io::Result<()> {
    let svc = service.clone();
    let Some(ks_path) = try_keystore_path(tx, |e| DaemonMessage::CredentialAddFailed {
        service: svc.clone(),
        error: e,
    })
    .await?
    else {
        return Ok(());
    };
    match Keystore::load(&ks_path, &passphrase) {
        Ok(mut keystore) => {
            keystore.add(svc.clone(), ServiceCredential::X {
                api_key,
                api_key_secret,
                access_token,
                access_token_secret,
                bearer_token,
            });
            match keystore.save(&ks_path, &passphrase) {
                Ok(()) => {
                    send_or_warn!(tx, DaemonMessage::CredentialAdded { service: svc });
                }
                Err(e) => {
                    send_or_warn!(tx, DaemonMessage::CredentialAddFailed {
                        service: svc,
                        error: format!("failed to save keystore: {e}"),
                    });
                }
            }
        }
        Err(e) => {
            send_or_warn!(tx, DaemonMessage::CredentialAddFailed {
                service: svc,
                error: format!("failed to unlock keystore: {e}"),
            });
        }
    }
    Ok(())
}

async fn handle_remove_credential(
    tx: &mpsc::Sender<DaemonMessage>,
    service: String,
    passphrase: String,
) -> io::Result<()> {
    let svc = service.clone();
    let Some(ks_path) = try_keystore_path(tx, |e| DaemonMessage::CredentialRemoveFailed {
        service: svc.clone(),
        error: e,
    })
    .await?
    else {
        return Ok(());
    };
    match Keystore::load(&ks_path, &passphrase) {
        Ok(mut keystore) => {
            if keystore.remove(&svc) {
                match keystore.save(&ks_path, &passphrase) {
                    Ok(()) => {
                        send_or_warn!(tx, DaemonMessage::CredentialRemoved { service: svc });
                    }
                    Err(e) => {
                        send_or_warn!(tx, DaemonMessage::CredentialRemoveFailed {
                            service: svc,
                            error: format!("failed to save keystore: {e}"),
                        });
                    }
                }
            } else {
                send_or_warn!(tx, DaemonMessage::CredentialRemoveFailed {
                    service: svc,
                    error: "service not found in keystore".to_string(),
                });
            }
        }
        Err(e) => {
            send_or_warn!(tx, DaemonMessage::CredentialRemoveFailed {
                service: svc,
                error: format!("failed to unlock keystore: {e}"),
            });
        }
    }
    Ok(())
}
