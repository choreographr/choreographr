use crate::daemon::DaemonCommand;
use crate::sessions::SessionCommand;
use std::io::{self, BufReader, BufWriter, Write};
use std::os::unix::net::UnixStream;
use std::sync::mpsc;
use tai_proto::{ClientMessage, DaemonMessage, ProtoError, read_message_sync, write_message_sync};
use tracing::{debug, error, info, warn};

pub(crate) fn client_thread(
    stream: UnixStream,
    daemon_tx: mpsc::Sender<DaemonCommand>,
) -> io::Result<()> {
    let reader = BufReader::new(stream.try_clone()?);
    let mut writer = BufWriter::new(stream);

    let (writer_tx, writer_rx) = mpsc::channel::<DaemonMessage>();
    let writer_handle = std::thread::spawn(move || {
        for msg in writer_rx {
            if write_message_sync(&mut writer, &msg).is_err() {
                break;
            }
            let _ = writer.flush();
        }
    });

    let mut attached_session_tx: Option<mpsc::Sender<SessionCommand>> = None;
    let mut attached_session_id: Option<u64> = None;
    let client_id = rand::random::<u64>();
    info!("client connected: id={}", client_id);

    let mut reader = reader;
    loop {
        match read_message_sync::<_, ClientMessage>(&mut reader) {
            Ok(msg) => {
                match msg {
                    ClientMessage::CreateSession {
                        title,
                        parent_session_id,
                        cwd,
                        max_turns,
                    } => {
                        info!("client {}: CreateSession", client_id);
                        let cwd_str = cwd.clone();
                        let (reply, rx) = mpsc::channel();
                        let _ = daemon_tx.send(DaemonCommand::CreateSession {
                            title: title.clone(),
                            parent_session_id,
                            cwd: cwd.map(std::path::PathBuf::from),
                            max_turns,
                            active_tool_groups: Vec::new(),
                            reply,
                        });
                        match rx.recv() {
                            Ok(Ok((sid, session_tx))) => {
                                // Detach old first
                                if let Some(ref old_tx) = attached_session_tx {
                                    let _ = old_tx.send(SessionCommand::Detach { client_id });
                                }
                                let _ = session_tx.send(SessionCommand::Attach {
                                    client_id,
                                    tx: writer_tx.clone(),
                                });
                                let _ = writer_tx.send(DaemonMessage::SessionCreated {
                                    session_id: sid,
                                    title,
                                    parent_session_id,
                                    cwd: cwd_str,
                                    max_turns,
                                });
                                let _ = writer_tx
                                    .send(DaemonMessage::SessionAttached { session_id: sid });
                                attached_session_tx = Some(session_tx);
                                attached_session_id = Some(sid);
                            }
                            Ok(Err(e)) => {
                                let _ = writer_tx.send(DaemonMessage::SessionFailed {
                                    operation: "create_session".into(),
                                    error: e.to_string(),
                                });
                            }
                            Err(_) => return Ok(()),
                        }
                    }
                    ClientMessage::AttachSession { session_id } => {
                        info!("client {}: AttachSession id={}", client_id, session_id);
                        let (reply, rx) = mpsc::channel();
                        let _ = daemon_tx.send(DaemonCommand::AttachSession { session_id, reply });
                        match rx.recv() {
                            Ok(Ok(session_tx)) => {
                                // Don't detach when re-attaching to the same session —
                                // it would kill the session's only subscriber.
                                if Some(session_id) != attached_session_id {
                                    if let Some(ref old_tx) = attached_session_tx {
                                        let _ = old_tx.send(SessionCommand::Detach { client_id });
                                    }
                                }
                                let _ = session_tx.send(SessionCommand::Attach {
                                    client_id,
                                    tx: writer_tx.clone(),
                                });
                                let _ =
                                    writer_tx.send(DaemonMessage::SessionAttached { session_id });
                                attached_session_tx = Some(session_tx);
                                attached_session_id = Some(session_id);
                            }
                            Ok(Err(e)) => {
                                let _ = writer_tx.send(DaemonMessage::SessionFailed {
                                    operation: "attach_session".into(),
                                    error: e.to_string(),
                                });
                            }
                            Err(_) => return Ok(()),
                        }
                    }
                    ClientMessage::ListSessions => {
                        debug!("client {}: ListSessions", client_id);
                        let (reply, rx) = mpsc::channel();
                        let _ = daemon_tx.send(DaemonCommand::ListSessions { reply });
                        if let Ok(sessions) = rx.recv() {
                            let _ = writer_tx.send(DaemonMessage::Sessions { sessions });
                        }
                    }
                    ClientMessage::SubscribeSessionsSummary => {
                        let _ = daemon_tx.send(DaemonCommand::RegisterSummarySubscriber {
                            client_id,
                            writer: writer_tx.clone(),
                        });
                    }
                    ClientMessage::UnsubscribeSessionsSummary => {
                        let _ = daemon_tx.send(DaemonCommand::UnregisterSummarySubscriber {
                            client_id,
                        });
                    }
                    ClientMessage::RunInput { request_id, input } => {
                        debug!("client {}: RunInput id={}", client_id, request_id);
                        if let Some(ref tx) = attached_session_tx {
                            let _ = tx.send(SessionCommand::RunInput { request_id, input });
                        } else {
                            let _ = writer_tx.send(DaemonMessage::Failed {
                                request_id,
                                error: "no session attached".to_string(),
                            });
                        }
                    }
                    ClientMessage::Cancel { request_id } => {
                        debug!("client {}: Cancel id={}", client_id, request_id);
                        if let Some(ref tx) = attached_session_tx {
                            let _ = tx.send(SessionCommand::Cancel { request_id });
                        }
                    }
                    ClientMessage::Ping => {
                        debug!("client {}: Ping", client_id);
                        let _ = writer_tx.send(DaemonMessage::Pong);
                    }
                    ClientMessage::SetModel { model } => {
                        info!("client {}: SetModel model={} attached={}", client_id, model, attached_session_tx.is_some());
                        if let Some(ref tx) = attached_session_tx {
                            let _ = tx.send(SessionCommand::SetModel { model });
                        } else {
                            let _ = writer_tx.send(DaemonMessage::ModelSelectionFailed {
                                model,
                                error: "no session attached".to_string(),
                            });
                        }
                    }
                    ClientMessage::Unlock { passphrase } => {
                        info!("client {}: Unlock", client_id);
                        handle_unlock_sync(&daemon_tx, &writer_tx, passphrase);
                    }
                    ClientMessage::ListModels => {
                        debug!("client {}: ListModels", client_id);
                        handle_list_models_sync(&daemon_tx, &writer_tx, attached_session_id);
                    }
                    ClientMessage::GetCredential { service } => {
                        handle_get_credential_sync(&daemon_tx, &writer_tx, service);
                    }
                    _ => {
                        warn!(
                            "unhandled client message: {:?}",
                            std::mem::discriminant(&msg)
                        );
                    }
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

    if let Some(ref tx) = attached_session_tx {
        let _ = tx.send(SessionCommand::Detach { client_id });
    }
    let _ = daemon_tx.send(DaemonCommand::UnregisterSummarySubscriber { client_id });
    drop(writer_tx);
    let _ = writer_handle.join();
    Ok(())
}

fn handle_unlock_sync(
    daemon_tx: &mpsc::Sender<DaemonCommand>,
    writer_tx: &mpsc::Sender<DaemonMessage>,
    passphrase: String,
) {
    let (reply, rx) = mpsc::channel();
    let _ = daemon_tx.send(DaemonCommand::Unlock { passphrase, reply });
    match rx.recv() {
        Ok(Ok(())) => {
            let _ = writer_tx.send(DaemonMessage::Unlocked);
        }
        Ok(Err(e)) => {
            let _ = writer_tx.send(DaemonMessage::LockedError { error: e });
        }
        Err(_) => {}
    }
}

fn handle_list_models_sync(
    daemon_tx: &mpsc::Sender<DaemonCommand>,
    writer_tx: &mpsc::Sender<DaemonMessage>,
    attached_session_id: Option<u64>,
) {
    let (reply, rx) = mpsc::channel();
    let _ = daemon_tx.send(DaemonCommand::ListModels {
        session_id: attached_session_id,
        reply,
    });
    match rx.recv() {
        Ok(Ok((models, selected_model))) => {
            let _ = writer_tx.send(DaemonMessage::Models {
                models,
                selected_model,
            });
        }
        Ok(Err(e)) => {
            let _ = writer_tx.send(DaemonMessage::ModelsFailed { error: e });
        }
        Err(_) => {}
    }
}

fn handle_get_credential_sync(
    daemon_tx: &mpsc::Sender<DaemonCommand>,
    writer_tx: &mpsc::Sender<DaemonMessage>,
    service: String,
) {
    let (reply, rx) = mpsc::channel();
    let _ = daemon_tx.send(DaemonCommand::GetCredential {
        service: service.clone(),
        reply,
    });
    match rx.recv() {
        Ok(Some(key)) => {
            let _ = writer_tx.send(DaemonMessage::Credential {
                service,
                key: Some(key),
            });
        }
        Ok(None) => {
            let _ = writer_tx.send(DaemonMessage::Credential { service, key: None });
        }
        Err(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handle_unlock_sync_ok() {
        let (daemon_tx, daemon_rx) = mpsc::channel();
        let (writer_tx, writer_rx) = mpsc::channel();
        std::thread::spawn(move || {
            if let Ok(DaemonCommand::Unlock { reply, .. }) = daemon_rx.recv() {
                let _ = reply.send(Ok(()));
            }
        });
        handle_unlock_sync(&daemon_tx, &writer_tx, "pass".into());
        let msg = writer_rx.recv().unwrap();
        assert!(matches!(msg, DaemonMessage::Unlocked));
    }

    #[test]
    fn handle_unlock_sync_err() {
        let (daemon_tx, daemon_rx) = mpsc::channel();
        let (writer_tx, writer_rx) = mpsc::channel();
        std::thread::spawn(move || {
            if let Ok(DaemonCommand::Unlock { reply, .. }) = daemon_rx.recv() {
                let _ = reply.send(Err("wrong password".into()));
            }
        });
        handle_unlock_sync(&daemon_tx, &writer_tx, "pass".into());
        let msg = writer_rx.recv().unwrap();
        assert!(matches!(msg, DaemonMessage::LockedError { .. }));
        if let DaemonMessage::LockedError { error } = &msg {
            assert_eq!(error, "wrong password");
        }
    }

    #[test]
    fn handle_unlock_sync_disconnected() {
        let (daemon_tx, daemon_rx) = mpsc::channel::<DaemonCommand>();
        let (writer_tx, writer_rx) = mpsc::channel();
        drop(daemon_rx);
        handle_unlock_sync(&daemon_tx, &writer_tx, "pass".into());
        assert!(writer_rx.try_recv().is_err());
    }

    #[test]
    fn handle_list_models_sync_ok() {
        let (daemon_tx, daemon_rx) = mpsc::channel();
        let (writer_tx, writer_rx) = mpsc::channel();
        std::thread::spawn(move || {
            if let Ok(DaemonCommand::ListModels { reply, .. }) = daemon_rx.recv() {
                let _ = reply
                    .send(Ok((vec!["gpt-4".into(), "gpt-3.5".into()], Some("gpt-4".into()))));
            }
        });
        handle_list_models_sync(&daemon_tx, &writer_tx, None);
        let msg = writer_rx.recv().unwrap();
        assert!(matches!(msg, DaemonMessage::Models { .. }));
    }

    #[test]
    fn handle_list_models_sync_err() {
        let (daemon_tx, daemon_rx) = mpsc::channel();
        let (writer_tx, writer_rx) = mpsc::channel();
        std::thread::spawn(move || {
            if let Ok(DaemonCommand::ListModels { reply, .. }) = daemon_rx.recv() {
                let _ = reply.send(Err("daemon is locked".into()));
            }
        });
        handle_list_models_sync(&daemon_tx, &writer_tx, None);
        let msg = writer_rx.recv().unwrap();
        assert!(matches!(msg, DaemonMessage::ModelsFailed { .. }));
        if let DaemonMessage::ModelsFailed { error } = &msg {
            assert_eq!(error, "daemon is locked");
        }
    }

    #[test]
    fn handle_get_credential_sync_some() {
        let (daemon_tx, daemon_rx) = mpsc::channel();
        let (writer_tx, writer_rx) = mpsc::channel();
        std::thread::spawn(move || {
            if let Ok(DaemonCommand::GetCredential { service, reply }) = daemon_rx.recv() {
                assert_eq!(service, "openai");
                let _ = reply.send(Some("sk-123".into()));
            }
        });
        handle_get_credential_sync(&daemon_tx, &writer_tx, "openai".into());
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
        let (writer_tx, writer_rx) = mpsc::channel();
        std::thread::spawn(move || {
            if let Ok(DaemonCommand::GetCredential { service, reply }) = daemon_rx.recv() {
                assert_eq!(service, "openai");
                let _ = reply.send(None);
            }
        });
        handle_get_credential_sync(&daemon_tx, &writer_tx, "openai".into());
        let msg = writer_rx.recv().unwrap();
        assert!(matches!(msg, DaemonMessage::Credential { .. }));
        if let DaemonMessage::Credential { service, key } = &msg {
            assert_eq!(service, "openai");
            assert!(key.is_none());
        }
    }
}
