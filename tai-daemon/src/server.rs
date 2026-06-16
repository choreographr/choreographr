use crate::openai::{AuthConfig, OpenAiClient, RequestFormat};
use crate::requests::{emit_demo_image, execute_chat_tool_request, execute_plain_request};
use crate::sessions::{
    ActiveRequest, DaemonState, SessionState, broadcast_message_appended, broadcast_to_session,
    default_session_id, list_sessions, new_daemon_state, require_attached_session, session_by_id,
    session_snapshot, update_subscription,
};
use std::{collections::HashMap, io, path::Path, sync::Arc};
use tai_proto::{ClientMessage, DaemonMessage, SessionMessage, read_message, write_message};
use tokio::{
    io::AsyncWriteExt,
    net::{UnixListener, UnixStream},
    signal::unix::{signal, SignalKind},
    sync::{Mutex, mpsc},
};
use tracing::{debug, error, info, warn};

async fn wait_for_shutdown() {
    let mut sigint = signal(SignalKind::interrupt()).expect("failed to install SIGINT handler");
    let mut sigterm = signal(SignalKind::terminate()).expect("failed to install SIGTERM handler");
    tokio::select! {
        _ = sigint.recv() => info!("received SIGINT, shutting down tai-daemon"),
        _ = sigterm.recv() => info!("received SIGTERM, shutting down tai-daemon"),
    }
}

pub async fn run_server(socket_path: &str, auth_config: AuthConfig) -> io::Result<()> {
    if Path::new(socket_path).exists() {
        info!(%socket_path, "removing stale socket");
        std::fs::remove_file(socket_path)?;
    }

    let listener = UnixListener::bind(socket_path)?;
    let client = Arc::new(OpenAiClient::new(auth_config)?);
    let state = new_daemon_state();
    info!(%socket_path, "tai-daemon listening");

    let result = loop {
        tokio::select! {
            accept_result = listener.accept() => {
                let (stream, _) = accept_result?;
                debug!("accepted client connection");
                let client = Arc::clone(&client);
                let state = Arc::clone(&state);
                tokio::spawn(async move {
                    if let Err(error) = handle_client(stream, client, state).await {
                        error!(error = %error, "client error");
                    }
                });
            }
            _ = wait_for_shutdown() => break Ok(()),
        }
    };

    if Path::new(socket_path).exists() {
        info!(%socket_path, "removing socket");
        std::fs::remove_file(socket_path)?;
    }

    result
}

pub async fn handle_client(
    stream: UnixStream,
    client: Arc<OpenAiClient>,
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
                match message {
                    ClientMessage::CreateSession { title } => {
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
                            &state,
                            client_id,
                            attached_session_id,
                            Some(session_id),
                            &tx,
                        )
                        .await;
                        attached_session_id = Some(session_id);
                        let _ = tx
                            .send(DaemonMessage::SessionCreated {
                                session_id,
                                title: title.clone(),
                            })
                            .await;
                        let _ = tx.send(DaemonMessage::SessionAttached { session_id }).await;
                        let snapshot = session_snapshot(session_id, &session).await;
                        let _ = tx.send(snapshot).await;
                    }
                    ClientMessage::ListSessions => {
                        let sessions = list_sessions(&state).await;
                        let _ = tx.send(DaemonMessage::Sessions { sessions }).await;
                    }
                    ClientMessage::AttachSession { session_id } => {
                        let Some(session) = session_by_id(&state, session_id).await else {
                            let _ = tx
                                .send(DaemonMessage::SessionFailed {
                                    operation: "attach_session".to_string(),
                                    error: format!("unknown session: {session_id}"),
                                })
                                .await;
                            continue;
                        };
                        update_subscription(
                            &state,
                            client_id,
                            attached_session_id,
                            Some(session_id),
                            &tx,
                        )
                        .await;
                        attached_session_id = Some(session_id);
                        let _ = tx.send(DaemonMessage::SessionAttached { session_id }).await;
                        let snapshot = session_snapshot(session_id, &session).await;
                        let _ = tx.send(snapshot).await;
                    }
                    ClientMessage::GetSessionState { session_id } => {
                        let Some(session) = session_by_id(&state, session_id).await else {
                            let _ = tx
                                .send(DaemonMessage::SessionFailed {
                                    operation: "get_session_state".to_string(),
                                    error: format!("unknown session: {session_id}"),
                                })
                                .await;
                            continue;
                        };
                        let snapshot = session_snapshot(session_id, &session).await;
                        let _ = tx.send(snapshot).await;
                    }
                    ClientMessage::RunInput { request_id, input } => {
                        let Some((session_id, session)) =
                            require_attached_session(&state, attached_session_id, &tx).await?
                        else {
                            continue;
                        };

                        let text = String::from_utf8_lossy(&input).trim().to_string();
                        if text.is_empty() {
                            warn!(request_id, "request failed: empty input");
                            let _ = tx.send(DaemonMessage::Started { request_id }).await;
                            let _ = tx
                                .send(DaemonMessage::Failed {
                                    request_id,
                                    error: "empty input".to_string(),
                                })
                                .await;
                            continue;
                        }

                        let model = {
                            let mut guard = session.lock().await;
                            if guard.active_requests.contains_key(&request_id) {
                                warn!(request_id, session_id, "duplicate request id rejected");
                                let _ = tx
                                    .send(DaemonMessage::Failed {
                                        request_id,
                                        error: "request id already active".to_string(),
                                    })
                                    .await;
                                continue;
                            }
                            let Some(model) = guard.selected_model.clone() else {
                                warn!(request_id, session_id, "request failed: no model selected");
                                let _ = tx.send(DaemonMessage::Started { request_id }).await;
                                let _ = tx
                                    .send(DaemonMessage::Failed {
                                        request_id,
                                        error: "no model selected".to_string(),
                                    })
                                    .await;
                                continue;
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
                        let client_clone = Arc::clone(&client);
                        let session_clone = Arc::clone(&session);
                        let handle = tokio::spawn(async move {
                            broadcast_to_session(
                                &session_clone,
                                DaemonMessage::Started { request_id },
                                None,
                            )
                            .await;

                            let result = match request_format {
                                RequestFormat::Responses => {
                                    execute_plain_request(
                                        &client_clone,
                                        &session_clone,
                                        &model,
                                        request_id,
                                    )
                                    .await
                                }
                                RequestFormat::ChatCompletions => {
                                    execute_chat_tool_request(
                                        &client_clone,
                                        &session_clone,
                                        &model,
                                        request_id,
                                    )
                                    .await
                                }
                            };

                            match result {
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
                            session_clone
                                .lock()
                                .await
                                .active_requests
                                .remove(&request_id);
                        });

                        session
                            .lock()
                            .await
                            .active_requests
                            .insert(request_id, ActiveRequest { handle });
                    }
                    ClientMessage::TestImage { request_id } => {
                        let Some((_session_id, _session)) =
                            require_attached_session(&state, attached_session_id, &tx).await?
                        else {
                            continue;
                        };
                        info!(request_id, "sending demo image");
                        let _ = tx.send(DaemonMessage::Started { request_id }).await;
                        match emit_demo_image(&tx, request_id, 1).await {
                            Ok(()) => {
                                let _ = tx.send(DaemonMessage::Done { request_id }).await;
                            }
                            Err(_) => {
                                warn!(
                                    request_id,
                                    "client disconnected before image could be delivered"
                                );
                            }
                        }
                    }
                    ClientMessage::Cancel { request_id } => {
                        let Some((session_id, session)) =
                            require_attached_session(&state, attached_session_id, &tx).await?
                        else {
                            continue;
                        };
                        if let Some(active_request) =
                            session.lock().await.active_requests.remove(&request_id)
                        {
                            info!(request_id, session_id, "cancelling active request");
                            active_request.handle.abort();
                            let _ = tx.send(DaemonMessage::Cancelled { request_id }).await;
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
                            let _ = tx
                                .send(DaemonMessage::Failed {
                                    request_id,
                                    error: "request id not active".to_string(),
                                })
                                .await;
                        }
                    }
                    ClientMessage::Ping => {
                        debug!("responding to ping");
                        let _ = tx.send(DaemonMessage::Pong).await;
                    }
                    ClientMessage::ListModels => {
                        let config = client.config();
                        debug!(base_url = %config.base_url, model_list_path = %config.model_list_path, responses_path = %config.responses_path, "listing configured models");
                        let selected_model = match attached_session_id {
                            Some(session_id) => match session_by_id(&state, session_id).await {
                                Some(session) => session.lock().await.selected_model.clone(),
                                None => None,
                            },
                            None => None,
                        };
                        match client.validate_and_list_models().await {
                            Ok(models) => {
                                let _ = tx
                                    .send(DaemonMessage::Models {
                                        models,
                                        selected_model,
                                    })
                                    .await;
                            }
                            Err(error) => {
                                let _ = tx
                                    .send(DaemonMessage::ModelsFailed {
                                        error: format!("failed to list models: {error}"),
                                    })
                                    .await;
                            }
                        }
                    }
                    ClientMessage::SetModel { model } => {
                        let Some((session_id, session)) =
                            require_attached_session(&state, attached_session_id, &tx).await?
                        else {
                            continue;
                        };
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
                                    let _ = tx
                                        .send(DaemonMessage::ModelSelectionFailed {
                                            model: model.clone(),
                                            error: format!("unknown model: {model}"),
                                        })
                                        .await;
                                }
                            }
                            Err(error) => {
                                let _ = tx
                                    .send(DaemonMessage::ModelSelectionFailed {
                                        model,
                                        error: format!("failed to list models: {error}"),
                                    })
                                    .await;
                            }
                        }
                    }
                }
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
