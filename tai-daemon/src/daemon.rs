use crate::db::{self, SessionRecord};
use crate::sessions::{ActiveSessionEntry, SessionCommand, SessionMetadata, session_main};
use std::collections::HashMap;
use std::io;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use tai_proto::{DaemonMessage, SessionStatus, SessionSummary};
use tracing::{debug, error, info};

pub struct DaemonState {
    pub next_session_id: u64,
    pub max_turns: u32,
    pub active_sessions: HashMap<u64, ActiveSessionEntry>,
    pub session_metadata: HashMap<u64, SessionMetadata>,
    pub openai_client: Option<Arc<crate::openai::OpenAiClient>>,
    pub keystore: Option<Arc<crate::Keystore>>,
    pub x_credentials: Option<tai_keystore::XCredentials>,
    pub db: Arc<redb::Database>,
    pub tool_registry: Arc<crate::tools::ToolRegistry>,
    pub daemon_tx: mpsc::Sender<DaemonCommand>,
    pub client_streams: Vec<UnixStream>,
    pub summary_subscribers: HashMap<u64, mpsc::Sender<DaemonMessage>>,
    pub model_cache: Option<(Vec<String>, std::time::Instant)>,
}

pub enum DaemonCommand {
    Shutdown,
    CreateSession {
        title: Option<String>,
        parent_session_id: Option<u64>,
        cwd: Option<PathBuf>,
        max_turns: Option<u32>,
        active_categories: Vec<String>,
        reply: std::sync::mpsc::Sender<io::Result<(u64, std::sync::mpsc::Sender<SessionCommand>)>>,
    },
    AttachSession {
        session_id: u64,
        reply: std::sync::mpsc::Sender<io::Result<std::sync::mpsc::Sender<SessionCommand>>>,
    },
    ListSessions {
        reply: std::sync::mpsc::Sender<Vec<SessionSummary>>,
    },
    GetSession {
        session_id: u64,
        reply: std::sync::mpsc::Sender<Option<SessionSummary>>,
    },
    UpdateMetadata {
        session_id: u64,
        metadata: SessionMetadata,
    },
    SessionExited {
        session_id: u64,
    },
    Unlock {
        passphrase: String,
        reply: std::sync::mpsc::Sender<Result<(), String>>,
    },
    ListModels {
        session_id: Option<u64>,
        reply: std::sync::mpsc::Sender<Result<(Vec<String>, Option<String>), String>>,
    },
    GetCredential {
        service: String,
        reply: std::sync::mpsc::Sender<Option<String>>,
    },
    RegisterSummarySubscriber {
        client_id: u64,
        writer: std::sync::mpsc::Sender<DaemonMessage>,
    },
    UnregisterSummarySubscriber {
        client_id: u64,
    },
    BroadcastSessionStatus {
        session_id: u64,
        status: SessionStatus,
    },
}

impl DaemonState {
    pub fn handle_command(&mut self, cmd: DaemonCommand) {
        match cmd {
            DaemonCommand::CreateSession {
                title,
                parent_session_id,
                cwd,
                max_turns,
                active_categories,
                reply,
            } => {
                if self.openai_client.is_none() {
                    let _ = reply.send(Err(io::Error::new(
                        io::ErrorKind::Other,
                        "daemon is locked",
                    )));
                    return;
                }
                let sid = self.next_session_id;
                self.next_session_id += 1;
                info!("CreateSession: id={}, title={:?}", sid, title);

                let cwd_str = cwd.as_ref().map(|p| p.display().to_string());
                let active_cats = if active_categories.is_empty() {
                    vec!["core".into(), "git".into(), "shell".into()]
                } else {
                    active_categories.clone()
                };
                let record = SessionRecord {
                    title: title.clone(),
                    selected_model: None,
                    parent_session_id,
                    cwd: cwd_str.clone(),
                    max_turns,
                    message_count: 0,
                    created_at: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_secs() as i64,
                    active_categories: active_cats.clone(),
                };

                if let Err(e) = db::write_session(&self.db, sid, &record) {
                    error!("CreateSession: failed to persist session {}: {e}", sid);
                }

                let db = Arc::clone(&self.db);
                let client = self.openai_client.clone();
                let tool_registry = Arc::clone(&self.tool_registry);
                let daemon_tx = self.daemon_tx.clone();
                let max_turns_default = self.max_turns;
                let (session_tx, session_rx) = std::sync::mpsc::channel();
                let cmd_tx = session_tx.clone();
                let init_record = record.clone();

                let handle = thread::spawn(move || {
                    session_main(
                        cmd_tx,
                        session_rx,
                        sid,
                        db,
                        client,
                        tool_registry,
                        daemon_tx,
                        Some(init_record),
                        max_turns_default,
                    );
                });

                self.active_sessions.insert(
                    sid,
                    ActiveSessionEntry {
                        cmd_tx: session_tx.clone(),
                        handle,
                    },
                );
                self.session_metadata.insert(
                    sid,
                    SessionMetadata {
                        title: title.clone(),
                        selected_model: None,
                        parent_session_id,
                        cwd: cwd_str.clone(),
                        created_at: record.created_at,
                        message_count: 0,
                        max_turns,
                        status: SessionStatus::Inactive,
                        active_categories: active_cats.clone(),
                    },
                );

                let _ = reply.send(Ok((sid, session_tx)));
                let created_msg = DaemonMessage::SessionCreated {
                    session_id: sid,
                    title,
                    parent_session_id,
                    cwd: cwd_str,
                    max_turns,
                };
                let status_msg = DaemonMessage::SessionStatusChanged {
                    session_id: sid,
                    status: SessionStatus::Inactive,
                };
                self.summary_subscribers.retain(|_id, tx| {
                    tx.send(created_msg.clone()).is_ok() && tx.send(status_msg.clone()).is_ok()
                });
            }
            DaemonCommand::AttachSession { session_id, reply } => {
                debug!("AttachSession: id={}", session_id);
                if self.openai_client.is_none() {
                    let _ = reply.send(Err(io::Error::new(
                        io::ErrorKind::Other,
                        "daemon is locked",
                    )));
                    return;
                }
                match self.active_sessions.get(&session_id) {
                    Some(entry) => {
                        let _ = reply.send(Ok(entry.cmd_tx.clone()));
                    }
                    None => match db::read_session(&self.db, session_id) {
                        Ok(Some(record)) => {
                            let mut metadata: SessionMetadata = record.clone().into();
                            metadata.status = SessionStatus::Inactive;
                            let db = Arc::clone(&self.db);
                            let client = self.openai_client.clone();
                            let tool_registry = Arc::clone(&self.tool_registry);
                            let daemon_tx = self.daemon_tx.clone();
                            let max_turns_default = self.max_turns;
                            let (session_tx, session_rx) = std::sync::mpsc::channel();
                            let cmd_tx = session_tx.clone();

                            let handle = thread::spawn(move || {
                                session_main(
                                    cmd_tx,
                                    session_rx,
                                    session_id,
                                    db,
                                    client,
                                    tool_registry,
                                    daemon_tx,
                                    Some(record),
                                    max_turns_default,
                                );
                            });

                            self.active_sessions.insert(
                                session_id,
                                ActiveSessionEntry {
                                    cmd_tx: session_tx.clone(),
                                    handle,
                                },
                            );
                            self.session_metadata.insert(session_id, metadata);
                            info!("AttachSession: loaded session {} from db", session_id);
                            let _ = reply.send(Ok(session_tx));
                        }
                        Ok(None) => {
                            let _ = reply.send(Err(io::Error::new(
                                io::ErrorKind::NotFound,
                                "session not found",
                            )));
                        }
                        Err(e) => {
                            let _ = reply.send(Err(e));
                        }
                    },
                }
            }
            DaemonCommand::ListSessions { reply } => {
                let mut summaries: Vec<SessionSummary> = self
                    .session_metadata
                    .iter()
                    .map(|(id, meta)| SessionSummary {
                        session_id: *id,
                        title: meta.title.clone(),
                        selected_model: meta.selected_model.clone(),
                        parent_session_id: meta.parent_session_id,
                        cwd: meta.cwd.clone(),
                        created_at: meta.created_at,
                        message_count: meta.message_count,
                        max_turns: meta.max_turns,
                        status: meta.status.clone(),
                        active_categories: meta.active_categories.clone(),
                    })
                    .collect();

                summaries.sort_by_key(|s| s.session_id);
                let _ = reply.send(summaries);
            }
            DaemonCommand::GetSession { session_id, reply } => {
                let summary = self
                    .session_metadata
                    .get(&session_id)
                    .map(|meta| SessionSummary {
                        session_id,
                        title: meta.title.clone(),
                        selected_model: meta.selected_model.clone(),
                        parent_session_id: meta.parent_session_id,
                        cwd: meta.cwd.clone(),
                        created_at: meta.created_at,
                        message_count: meta.message_count,
                        max_turns: meta.max_turns,
                        status: meta.status.clone(),
                        active_categories: meta.active_categories.clone(),
                    });
                let _ = reply.send(summary);
            }
            DaemonCommand::UpdateMetadata {
                session_id,
                metadata,
            } => {
                debug!("UpdateMetadata: id={}, model={:?}", session_id, metadata.selected_model);
                self.session_metadata.insert(session_id, metadata);
            }
            DaemonCommand::SessionExited { session_id } => {
                info!("SessionExited: id={}", session_id);
                self.active_sessions.remove(&session_id);
                if let Some(meta) = self.session_metadata.get_mut(&session_id) {
                    meta.status = SessionStatus::Sleeping;
                }
                let msg = DaemonMessage::SessionStatusChanged {
                    session_id,
                    status: SessionStatus::Sleeping,
                };
                self.summary_subscribers.retain(|_id, tx| {
                    tx.send(msg.clone()).is_ok()
                });
            }
            DaemonCommand::Unlock { passphrase, reply } => {
                info!("Unlock attempt");
                let result = handle_unlock_inner(self, passphrase);
                info!("Unlock result: success={}", result.is_ok());
                let _ = reply.send(result);
            }
            DaemonCommand::ListModels { session_id, reply } => {
                debug!("ListModels: session_id={:?}", session_id);
                let result = handle_list_models_inner(self, session_id);
                let _ = reply.send(result);
            }
            DaemonCommand::GetCredential { service, reply } => {
                let key = self
                    .keystore
                    .as_ref()
                    .and_then(|ks| ks.get_api_key(&service).map(|k| k.to_string()));
                let _ = reply.send(key);
            }
            DaemonCommand::RegisterSummarySubscriber { client_id, writer } => {
                self.summary_subscribers.insert(client_id, writer);
            }
            DaemonCommand::UnregisterSummarySubscriber { client_id } => {
                self.summary_subscribers.remove(&client_id);
            }
            DaemonCommand::BroadcastSessionStatus { session_id, status } => {
                let msg = DaemonMessage::SessionStatusChanged { session_id, status };
                self.summary_subscribers.retain(|_id, tx| {
                    tx.send(msg.clone()).is_ok()
                });
            }
            DaemonCommand::Shutdown => unreachable!("handled by command loop"),
        }
    }
}

fn handle_unlock_inner(state: &mut DaemonState, passphrase: String) -> Result<(), String> {
    let ks_path = tai_keystore::keystore_path()
        .map_err(|e| format!("failed to determine keystore path: {e}"))?;
    if !ks_path.exists() {
        return Err("keystore does not exist. run 'tai-keystore init' to create one.".to_string());
    }
    let ks = tai_keystore::Keystore::load(&ks_path, &passphrase)
        .map_err(|e| format!("failed to unlock keystore: {e}"))?;
    let keystore = Arc::new(ks);
    match keystore.get_api_key("openai") {
        Some(api_key) => {
            let service_config = crate::openai::load_service_config().unwrap_or_default();
            let client = crate::openai::OpenAiClient::new(service_config, api_key.to_string())
                .map_err(|e| format!("failed to create OpenAI client: {e}"))?;
            state.openai_client = Some(Arc::new(client));
            if let Some(x_creds) = keystore.get_x_credentials("twitter") {
                state.x_credentials = Some(x_creds);
            }
            state.keystore = Some(keystore);
            Ok(())
        }
        None => Err("no 'openai' credential found in keystore".to_string()),
    }
}

fn handle_list_models_inner(
    state: &mut DaemonState,
    session_id: Option<u64>,
) -> Result<(Vec<String>, Option<String>), String> {
    let now = std::time::Instant::now();
    let five_minutes = std::time::Duration::from_secs(300);

    let models = match &state.model_cache {
        Some((cached_models, cached_at)) if now.duration_since(*cached_at) < five_minutes => {
            debug!("model cache hit");
            cached_models.clone()
        }
        _ => {
            debug!("model cache miss");
            let client = state
                .openai_client
                .as_ref()
                .ok_or("daemon is locked".to_string())?;
            let models = client
                .validate_and_list_models()
                .map_err(|e| format!("failed to list models: {e}"))?;
            state.model_cache = Some((models.clone(), now));
            models
        }
    };

    let selected_model = session_id
        .and_then(|sid| state.session_metadata.get(&sid))
        .and_then(|m| m.selected_model.clone());
    Ok((models, selected_model))
}
