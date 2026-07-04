use crate::daemon::DaemonCommand;
use crate::sessions::SessionCommand;
use std::io::{self, BufReader, BufWriter, Write};
use std::os::unix::net::UnixStream;
use std::sync::mpsc;
use tai_proto::{ClientMessage, DaemonMessage, ProtoError, read_message_sync, write_message_sync};
use tokio::sync::mpsc::UnboundedSender;
use tracing::{debug, error};

pub(crate) fn client_thread(
    stream: UnixStream,
    daemon_tx: UnboundedSender<DaemonCommand>,
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
                        let cwd_str = cwd.clone();
                        let (reply, rx) = mpsc::channel();
                        let _ = daemon_tx.send(DaemonCommand::CreateSession {
                            title: title.clone(),
                            parent_session_id,
                            cwd: cwd.map(std::path::PathBuf::from),
                            max_turns,
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
                        let (reply, rx) = mpsc::channel();
                        let _ = daemon_tx.send(DaemonCommand::AttachSession { session_id, reply });
                        match rx.recv() {
                            Ok(Ok(session_tx)) => {
                                if let Some(ref old_tx) = attached_session_tx {
                                    let _ = old_tx.send(SessionCommand::Detach { client_id });
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
                        let (reply, rx) = mpsc::channel();
                        let _ = daemon_tx.send(DaemonCommand::ListSessions { reply });
                        if let Ok(sessions) = rx.recv() {
                            let _ = writer_tx.send(DaemonMessage::Sessions { sessions });
                        }
                    }
                    ClientMessage::RunInput { request_id, input } => {
                        if let Some(ref tx) = attached_session_tx {
                            let _ = tx.send(SessionCommand::RunInput { request_id, input });
                        }
                    }
                    ClientMessage::Cancel { request_id } => {
                        if let Some(ref tx) = attached_session_tx {
                            let _ = tx.send(SessionCommand::Cancel { request_id });
                        }
                    }
                    ClientMessage::Ping => {
                        let _ = writer_tx.send(DaemonMessage::Pong);
                    }
                    ClientMessage::SetModel { model } => {
                        if let Some(ref tx) = attached_session_tx {
                            let _ = tx.send(SessionCommand::SetModel { model });
                        }
                    }
                    ClientMessage::Unlock { passphrase } => {
                        handle_unlock_sync(&daemon_tx, &writer_tx, passphrase);
                    }
                    ClientMessage::Lock => {
                        handle_lock_sync(&daemon_tx, &writer_tx);
                    }
                    ClientMessage::ListModels => {
                        handle_list_models_sync(&daemon_tx, &writer_tx, attached_session_id);
                    }
                    ClientMessage::GetCredential { service } => {
                        handle_get_credential_sync(&daemon_tx, &writer_tx, service);
                    }
                    _ => {
                        debug!(
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
    drop(writer_tx);
    let _ = writer_handle.join();
    Ok(())
}

fn handle_unlock_sync(
    daemon_tx: &UnboundedSender<DaemonCommand>,
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

fn handle_lock_sync(
    daemon_tx: &UnboundedSender<DaemonCommand>,
    writer_tx: &mpsc::Sender<DaemonMessage>,
) {
    let _ = daemon_tx.send(DaemonCommand::Lock);
    let _ = writer_tx.send(DaemonMessage::Locked);
}

fn handle_list_models_sync(
    daemon_tx: &UnboundedSender<DaemonCommand>,
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
    daemon_tx: &UnboundedSender<DaemonCommand>,
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
