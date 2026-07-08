use crate::db::{self, SessionRecord};
use crate::sessions::{ActiveSessionEntry, SessionCommand, SessionMetadata, session_main};
use std::collections::HashMap;
use std::io;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc;
use std::thread;
use tai_proto::{DaemonMessage, SessionStatus, SessionSummary};
use tracing::{debug, error, info};

/// Reply type for the ListModels command.
pub(super) type ListModelsReply =
    std::sync::mpsc::Sender<Result<(Vec<String>, Option<String>), String>>;

pub struct DaemonState {
    pub next_session_id: u64,
    pub max_turns: u32,
    pub active_sessions: HashMap<u64, ActiveSessionEntry>,
    pub session_metadata: HashMap<u64, SessionMetadata>,
    pub openai_client: Option<Arc<crate::openai::OpenAiClient>>,
    pub keystore: Option<Arc<crate::Keystore>>,
    pub x_credentials: Option<tai_keystore::ServiceCredential>,
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
        active_tool_groups: Vec<String>,
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
        reply: ListModelsReply,
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
    DeleteSession {
        session_id: u64,
        reply: std::sync::mpsc::Sender<io::Result<()>>,
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
                active_tool_groups,
                reply,
            } => {
                if let Err(e) = self.ensure_unlocked() {
                    let _ = reply.send(Err(e));
                    return;
                }
                let sid = self.next_session_id;
                self.next_session_id += 1;
                info!("CreateSession: id={}, title={:?}", sid, title);

                let cwd_str = cwd.as_ref().map(|p| p.display().to_string());
                let active_cats = if active_tool_groups.is_empty() {
                    vec!["core".into(), "git".into(), "shell".into()]
                } else {
                    active_tool_groups.clone()
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
                        .unwrap_or_default()
                        .as_secs() as i64,
                    active_tool_groups: active_cats.clone(),
                };

                if let Err(e) = db::write_session(&self.db, sid, &record) {
                    error!("CreateSession: failed to persist session {}: {e}", sid);
                }

                let metadata = SessionMetadata {
                    title: title.clone(),
                    selected_model: None,
                    parent_session_id,
                    cwd: cwd_str.clone(),
                    created_at: record.created_at,
                    message_count: 0,
                    max_turns,
                    status: SessionStatus::Inactive,
                    active_tool_groups: active_cats.clone(),
                };
                let session_tx = self.spawn_session(sid, record, metadata);

                let _ = reply.send(Ok((sid, session_tx)));
                crate::metrics::record_session_created();
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
                self.broadcast(created_msg);
                self.broadcast(status_msg);
            }
            DaemonCommand::AttachSession { session_id, reply } => {
                debug!("AttachSession: id={}", session_id);
                if let Err(e) = self.ensure_unlocked() {
                    let _ = reply.send(Err(e));
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
                            let session_tx = self.spawn_session(session_id, record, metadata);
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
                        active_tool_groups: meta.active_tool_groups.clone(),
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
                        active_tool_groups: meta.active_tool_groups.clone(),
                    });
                let _ = reply.send(summary);
            }
            DaemonCommand::UpdateMetadata {
                session_id,
                metadata,
            } => {
                debug!(
                    "UpdateMetadata: id={}, model={:?}",
                    session_id, metadata.selected_model
                );
                self.session_metadata.insert(session_id, metadata);
            }
            DaemonCommand::SessionExited { session_id } => {
                info!("SessionExited: id={}", session_id);
                crate::metrics::record_session_exited();
                self.active_sessions.remove(&session_id);
                if let Some(meta) = self.session_metadata.get_mut(&session_id) {
                    meta.status = SessionStatus::Sleeping;
                }
                let msg = DaemonMessage::SessionStatusChanged {
                    session_id,
                    status: SessionStatus::Sleeping,
                };
                self.broadcast(msg);
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
                self.broadcast(msg);
            }
            DaemonCommand::DeleteSession { session_id, reply } => {
                info!("DeleteSession: id={}", session_id);
                if let Err(e) = self.ensure_unlocked() {
                    let _ = reply.send(Err(e));
                    return;
                }
                // Gracefully shut down the session thread so it can persist its
                // final state before we delete from the DB — otherwise the
                // session's persist_and_exit would re-write the session
                // back to the DB after we delete it.
                if let Some(entry) = self.active_sessions.remove(&session_id) {
                    let _ = entry.cmd_tx.send(SessionCommand::Shutdown);
                    let _ = entry.handle.join();
                }
                // Remove from in-memory metadata
                self.session_metadata.remove(&session_id);
                // Remove from database
                if let Err(e) = db::delete_session(&self.db, session_id) {
                    error!(
                        "DeleteSession: failed to delete session {} from db: {e}",
                        session_id
                    );
                    let _ = reply.send(Err(e));
                    return;
                }
                // Broadcast deletion to subscribers
                self.broadcast(DaemonMessage::SessionDeleted { session_id });
                let _ = reply.send(Ok(()));
            }
            DaemonCommand::Shutdown => unreachable!("handled by command loop"),
        }
    }

    /// Returns an error if the daemon hasn't been unlocked yet.
    fn ensure_unlocked(&self) -> io::Result<()> {
        if self.openai_client.is_none() {
            Err(io::Error::other("daemon is locked"))
        } else {
            Ok(())
        }
    }

    fn spawn_session(
        &mut self,
        session_id: u64,
        record: SessionRecord,
        metadata: SessionMetadata,
    ) -> mpsc::Sender<SessionCommand> {
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
        session_tx
    }

    /// Send a message to all summary subscribers, removing dead ones.
    fn broadcast(&mut self, msg: DaemonMessage) {
        self.summary_subscribers
            .retain(|_id, tx| tx.send(msg.clone()).is_ok());
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
            if let Some(c) = keystore.get("twitter")
                && matches!(c, tai_keystore::ServiceCredential::X { .. })
            {
                state.x_credentials = Some(c.clone());
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sessions::SessionMetadata;
    use std::collections::HashMap;
    use std::sync::mpsc;
    use tai_proto::{DaemonMessage, SessionStatus};
    use tempfile::tempdir;

    fn make_daemon_state() -> (DaemonState, mpsc::Receiver<DaemonCommand>) {
        let (daemon_tx, daemon_rx) = mpsc::channel();
        let dir = tempdir().unwrap();
        let db = Arc::new(redb::Database::create(dir.path().join("test.redb")).unwrap());
        let tool_registry = crate::tools::ToolRegistry::new().build();
        let state = DaemonState {
            next_session_id: 1,
            max_turns: 10,
            active_sessions: HashMap::new(),
            session_metadata: HashMap::new(),
            openai_client: None,
            keystore: None,
            x_credentials: None,
            db,
            tool_registry,
            daemon_tx,
            client_streams: Vec::new(),
            summary_subscribers: HashMap::new(),
            model_cache: None,
        };
        (state, daemon_rx)
    }

    #[test]
    fn handle_list_sessions_empty() {
        let (mut state, _rx) = make_daemon_state();
        let (reply, rx) = mpsc::channel();
        state.handle_command(DaemonCommand::ListSessions { reply });
        let sessions = rx.recv().unwrap();
        assert!(sessions.is_empty());
    }

    #[test]
    fn handle_list_sessions_with_metadata() {
        let (mut state, _rx) = make_daemon_state();
        state.session_metadata.insert(
            1,
            SessionMetadata {
                title: Some("test".into()),
                selected_model: None,
                parent_session_id: None,
                cwd: None,
                created_at: 1000,
                message_count: 3,
                max_turns: None,
                status: SessionStatus::Inactive,
                active_tool_groups: vec!["core".into()],
            },
        );
        let (reply, rx) = mpsc::channel();
        state.handle_command(DaemonCommand::ListSessions { reply });
        let sessions = rx.recv().unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, 1);
        assert_eq!(sessions[0].title.as_deref(), Some("test"));
    }

    #[test]
    fn handle_get_session_missing() {
        let (mut state, _rx) = make_daemon_state();
        let (reply, rx) = mpsc::channel();
        state.handle_command(DaemonCommand::GetSession {
            session_id: 1,
            reply,
        });
        let result = rx.recv().unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn handle_update_metadata() {
        let (mut state, _rx) = make_daemon_state();
        state.session_metadata.insert(
            1,
            SessionMetadata {
                title: Some("original".into()),
                selected_model: None,
                parent_session_id: None,
                cwd: None,
                created_at: 1000,
                message_count: 0,
                max_turns: None,
                status: SessionStatus::Inactive,
                active_tool_groups: vec!["core".into()],
            },
        );
        let new_meta = SessionMetadata {
            title: Some("updated".into()),
            selected_model: Some("gpt-4".into()),
            parent_session_id: None,
            cwd: None,
            created_at: 2000,
            message_count: 5,
            max_turns: None,
            status: SessionStatus::Inference,
            active_tool_groups: vec!["core".into(), "git".into()],
        };
        state.handle_command(DaemonCommand::UpdateMetadata {
            session_id: 1,
            metadata: new_meta.clone(),
        });
        let stored = state.session_metadata.get(&1).unwrap();
        assert_eq!(stored.title.as_deref(), Some("updated"));
        assert_eq!(stored.selected_model.as_deref(), Some("gpt-4"));
        assert_eq!(stored.message_count, 5);
        assert_eq!(stored.status, SessionStatus::Inference);
    }

    #[test]
    fn handle_session_exited_nonexistent() {
        let (mut state, _rx) = make_daemon_state();
        state.handle_command(DaemonCommand::SessionExited { session_id: 999 });
        assert!(state.session_metadata.get(&999).is_none());
    }

    #[test]
    fn handle_get_credential_locked() {
        let (mut state, _rx) = make_daemon_state();
        let (reply, rx) = mpsc::channel();
        state.handle_command(DaemonCommand::GetCredential {
            service: "openai".into(),
            reply,
        });
        let key = rx.recv().unwrap();
        assert!(key.is_none());
    }

    #[test]
    fn handle_register_unregister_subscriber() {
        let (mut state, _rx) = make_daemon_state();
        let (tx, _rx_sub) = mpsc::channel();
        assert!(!state.summary_subscribers.contains_key(&42));
        state.handle_command(DaemonCommand::RegisterSummarySubscriber {
            client_id: 42,
            writer: tx,
        });
        assert!(state.summary_subscribers.contains_key(&42));
        state.handle_command(DaemonCommand::UnregisterSummarySubscriber { client_id: 42 });
        assert!(!state.summary_subscribers.contains_key(&42));
    }

    #[test]
    fn handle_broadcast_session_status() {
        let (mut state, _rx) = make_daemon_state();
        let (tx, rx) = mpsc::channel();
        state.handle_command(DaemonCommand::RegisterSummarySubscriber {
            client_id: 1,
            writer: tx,
        });
        state.handle_command(DaemonCommand::BroadcastSessionStatus {
            session_id: 42,
            status: SessionStatus::Inference,
        });
        let msg = rx.recv().unwrap();
        assert!(matches!(
            msg,
            DaemonMessage::SessionStatusChanged {
                session_id: 42,
                status: SessionStatus::Inference
            }
        ));
    }

    #[test]
    fn handle_create_session_locked() {
        let (mut state, _rx) = make_daemon_state();
        let (reply, rx) = mpsc::channel();
        state.handle_command(DaemonCommand::CreateSession {
            title: None,
            parent_session_id: None,
            cwd: None,
            max_turns: None,
            active_tool_groups: Vec::new(),
            reply,
        });
        let result = rx.recv().unwrap();
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().to_string(), "daemon is locked");
    }

    #[test]
    fn ensure_unlocked_returns_err_when_locked() {
        let (state, _rx) = make_daemon_state();
        let result = state.ensure_unlocked();
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().to_string(), "daemon is locked");
    }

    #[test]
    fn handle_delete_session_locked() {
        let (mut state, _rx) = make_daemon_state();
        let (reply, rx) = mpsc::channel();
        state.handle_command(DaemonCommand::DeleteSession {
            session_id: 1,
            reply,
        });
        let result = rx.recv().unwrap();
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().to_string(), "daemon is locked");
    }

    #[test]
    fn broadcast_sends_to_subscriber() {
        let (mut state, _rx) = make_daemon_state();
        let (tx, rx) = mpsc::channel();
        state.summary_subscribers.insert(1, tx);
        let msg = DaemonMessage::SessionDeleted { session_id: 42 };
        state.broadcast(msg.clone());
        let received = rx.recv().unwrap();
        assert_eq!(received, msg);
        // Subscriber should still be registered
        assert!(state.summary_subscribers.contains_key(&1));
    }

    #[test]
    fn broadcast_removes_disconnected_subscriber() {
        let (mut state, _rx) = make_daemon_state();
        let (tx, rx) = mpsc::channel::<DaemonMessage>();
        state.summary_subscribers.insert(1, tx);
        drop(rx); // Disconnect the receiver
        state.broadcast(DaemonMessage::SessionDeleted { session_id: 42 });
        // Dead subscriber should be removed
        assert!(!state.summary_subscribers.contains_key(&1));
    }
}
