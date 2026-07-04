use crate::db::{self, SessionRecord};
use crate::sessions::{ActiveSessionEntry, SessionCommand, SessionMetadata, session_main};
use std::collections::HashMap;
use std::io;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::Arc;
use tai_proto::SessionSummary;
use tokio::sync::mpsc::UnboundedSender;
use tokio::task;

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
    pub daemon_tx: UnboundedSender<DaemonCommand>,
    pub client_streams: Vec<UnixStream>,
}

pub enum DaemonCommand {
    CreateSession {
        title: Option<String>,
        parent_session_id: Option<u64>,
        cwd: Option<PathBuf>,
        max_turns: Option<u32>,
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
}

impl DaemonState {
    pub fn handle_command(&mut self, cmd: DaemonCommand) {
        match cmd {
            DaemonCommand::CreateSession {
                title,
                parent_session_id,
                cwd,
                max_turns,
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

                let record = SessionRecord {
                    title: title.clone(),
                    selected_model: None,
                    parent_session_id,
                    cwd: cwd.as_ref().map(|p| p.display().to_string()),
                    max_turns,
                    message_count: 0,
                    created_at: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_secs() as i64,
                };

                db::write_session(&self.db, sid, &record).ok();

                let db = Arc::clone(&self.db);
                let client = self.openai_client.clone();
                let tool_registry = Arc::clone(&self.tool_registry);
                let daemon_tx = self.daemon_tx.clone();
                let max_turns_default = self.max_turns;
                let (session_tx, session_rx) = std::sync::mpsc::channel();
                let cmd_tx = session_tx.clone();
                let init_record = record.clone();

                let handle = task::spawn_blocking(move || {
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
                        cwd: cwd.map(|p| p.display().to_string()),
                        created_at: record.created_at,
                        message_count: 0,
                        max_turns,
                    },
                );

                let _ = reply.send(Ok((sid, session_tx)));
            }
            DaemonCommand::AttachSession { session_id, reply } => {
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
                            let db = Arc::clone(&self.db);
                            let client = self.openai_client.clone();
                            let tool_registry = Arc::clone(&self.tool_registry);
                            let daemon_tx = self.daemon_tx.clone();
                            let max_turns_default = self.max_turns;
                            let (session_tx, session_rx) = std::sync::mpsc::channel();
                            let cmd_tx = session_tx.clone();

                            let handle = task::spawn_blocking(move || {
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
                    })
                    .collect();

                if let Ok(all) = db::read_all_sessions(&self.db) {
                    for (id, record) in all {
                        if !self.session_metadata.contains_key(&id) {
                            summaries.push(SessionSummary {
                                session_id: id,
                                title: record.title,
                                selected_model: record.selected_model,
                                parent_session_id: record.parent_session_id,
                                cwd: record.cwd,
                                created_at: record.created_at,
                                message_count: record.message_count,
                                max_turns: record.max_turns,
                            });
                        }
                    }
                }
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
                    });
                let _ = reply.send(summary);
            }
            DaemonCommand::UpdateMetadata {
                session_id,
                metadata,
            } => {
                self.session_metadata.insert(session_id, metadata);
            }
            DaemonCommand::SessionExited { session_id } => {
                self.active_sessions.remove(&session_id);
            }
            DaemonCommand::Unlock { passphrase, reply } => {
                let result = handle_unlock_inner(self, passphrase);
                let _ = reply.send(result);
            }
            DaemonCommand::ListModels { session_id, reply } => {
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
    state: &DaemonState,
    session_id: Option<u64>,
) -> Result<(Vec<String>, Option<String>), String> {
    let client = state
        .openai_client
        .as_ref()
        .ok_or("daemon is locked".to_string())?;
    let models = client
        .validate_and_list_models()
        .map_err(|e| format!("failed to list models: {e}"))?;
    let selected_model = session_id
        .and_then(|sid| state.session_metadata.get(&sid))
        .and_then(|m| m.selected_model.clone());
    Ok((models, selected_model))
}
