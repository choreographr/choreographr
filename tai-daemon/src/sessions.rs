use crate::context;
use crate::daemon::DaemonCommand;
use crate::db::{self, SessionRecord, write_message_retry, write_session_retry};
use crate::openai::OpenAiClient;
use crate::requests::run_agent_loop;
use crate::tools::ToolRegistry;
use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{SystemTime, UNIX_EPOCH};
use tai_proto::{DaemonMessage, SessionMessage, SessionSummary};
use tokio::sync::mpsc::UnboundedSender;
use tracing::warn;

pub enum SessionCommand {
    RunInput {
        request_id: u32,
        input: Vec<u8>,
    },
    RunChildInput {
        request_id: u32,
        input_tokens: Vec<u8>,
        reply: std::sync::mpsc::Sender<io::Result<ChildResult>>,
    },
    Cancel {
        request_id: u32,
    },
    SetModel {
        model: String,
    },
    Attach {
        client_id: u64,
        tx: std::sync::mpsc::Sender<DaemonMessage>,
    },
    Detach {
        client_id: u64,
    },
    GetSummary {
        reply: std::sync::mpsc::Sender<SessionSummary>,
    },
    AppendMessage {
        message: SessionMessage,
    },
    RequestFinished {
        request_id: u32,
        snapshot: SessionSnapshot,
    },
    Shutdown,
}

pub struct ChildResult {
    pub output: String,
    pub is_error: bool,
}

#[derive(Debug, Clone)]
pub struct SessionMetadata {
    pub title: Option<String>,
    pub selected_model: Option<String>,
    pub parent_session_id: Option<u64>,
    pub cwd: Option<String>,
    pub created_at: i64,
    pub message_count: u32,
    pub max_turns: Option<u32>,
}

#[derive(Clone)]
pub struct SessionSnapshot {
    pub title: Option<String>,
    pub selected_model: Option<String>,
    pub parent_session_id: Option<u64>,
    pub cwd: Option<PathBuf>,
    pub max_turns: Option<u32>,
    pub created_at: i64,
    pub messages: Vec<SessionMessage>,
    pub context_fingerprint: Option<u64>,
    pub context_file_paths: Vec<PathBuf>,
    pub context_message_index: Option<usize>,
}

struct ActiveRequest {
    cancel: Arc<AtomicBool>,
}

pub struct ActiveSessionEntry {
    pub cmd_tx: mpsc::Sender<SessionCommand>,
    pub handle: tokio::task::JoinHandle<()>,
}

pub struct SessionState {
    pub title: Option<String>,
    pub selected_model: Option<String>,
    pub parent_session_id: Option<u64>,
    pub cwd: Option<PathBuf>,
    pub max_turns: Option<u32>,
    pub created_at: i64,
    pub messages: Vec<SessionMessage>,
    pub subscribers: HashMap<u64, std::sync::mpsc::Sender<DaemonMessage>>,
    active_requests: HashMap<u32, ActiveRequest>,
    pub context_fingerprint: Option<u64>,
    pub context_file_paths: Vec<PathBuf>,
    pub context_message_index: Option<usize>,
}

impl SessionState {
    fn snapshot(&self) -> SessionSnapshot {
        SessionSnapshot {
            title: self.title.clone(),
            selected_model: self.selected_model.clone(),
            parent_session_id: self.parent_session_id,
            cwd: self.cwd.clone(),
            max_turns: self.max_turns,
            created_at: self.created_at,
            messages: self.messages.clone(),
            context_fingerprint: self.context_fingerprint,
            context_file_paths: self.context_file_paths.clone(),
            context_message_index: self.context_message_index,
        }
    }

    fn from_snapshot(
        snapshot: SessionSnapshot,
        subscribers: HashMap<u64, std::sync::mpsc::Sender<DaemonMessage>>,
    ) -> Self {
        Self {
            title: snapshot.title,
            selected_model: snapshot.selected_model,
            parent_session_id: snapshot.parent_session_id,
            cwd: snapshot.cwd,
            max_turns: snapshot.max_turns,
            created_at: snapshot.created_at,
            messages: snapshot.messages,
            subscribers,
            active_requests: HashMap::new(),
            context_fingerprint: snapshot.context_fingerprint,
            context_file_paths: snapshot.context_file_paths,
            context_message_index: snapshot.context_message_index,
        }
    }

    fn apply_snapshot(&mut self, snapshot: SessionSnapshot) {
        self.title = snapshot.title;
        self.selected_model = snapshot.selected_model;
        self.parent_session_id = snapshot.parent_session_id;
        self.cwd = snapshot.cwd;
        self.max_turns = snapshot.max_turns;
        self.created_at = snapshot.created_at;
        self.messages = snapshot.messages;
        self.context_fingerprint = snapshot.context_fingerprint;
        self.context_file_paths = snapshot.context_file_paths;
        self.context_message_index = snapshot.context_message_index;
    }
}

fn broadcast(
    subscribers: &HashMap<u64, std::sync::mpsc::Sender<DaemonMessage>>,
    message: DaemonMessage,
) {
    for tx in subscribers.values() {
        if let Err(e) = tx.send(message.clone()) {
            warn!("failed to broadcast, subscriber disconnected: {e}");
        }
    }
}

pub fn session_main(
    cmd_tx: mpsc::Sender<SessionCommand>,
    rx: std::sync::mpsc::Receiver<SessionCommand>,
    session_id: u64,
    db: Arc<redb::Database>,
    client: Option<Arc<OpenAiClient>>,
    tool_registry: Arc<ToolRegistry>,
    daemon_tx: UnboundedSender<DaemonCommand>,
    init_record: Option<SessionRecord>,
    max_turns_default: u32,
) {
    let mut state = SessionState {
        title: init_record.as_ref().and_then(|r| r.title.clone()),
        selected_model: init_record.as_ref().and_then(|r| r.selected_model.clone()),
        parent_session_id: init_record.as_ref().and_then(|r| r.parent_session_id),
        cwd: init_record
            .as_ref()
            .and_then(|r| r.cwd.as_ref().map(PathBuf::from)),
        max_turns: init_record.as_ref().and_then(|r| r.max_turns),
        created_at: init_record
            .as_ref()
            .map(|r| r.created_at)
            .unwrap_or_else(|| {
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_secs() as i64
            }),
        messages: Vec::new(),
        subscribers: HashMap::new(),
        active_requests: HashMap::new(),
        context_fingerprint: None,
        context_file_paths: Vec::new(),
        context_message_index: None,
    };

    match db::read_messages(&db, session_id) {
        Ok(msgs) => state.messages = msgs,
        Err(e) => warn!(session_id, error = %e, "failed to load messages from DB"),
    }

    if init_record.is_none() || state.messages.is_empty() {
        let effective_cwd = state.cwd.as_deref().unwrap_or_else(|| Path::new("."));
        let skills = context::discover_skills(effective_cwd);
        let base_prompt = context::build_base_prompt(&skills);
        state.messages.push(SessionMessage::SystemText {
            content: base_prompt,
        });
        write_message_retry(&db, session_id, 0, &state.messages[0]).ok();

        if let Ok(bundle) = context::discover_context(effective_cwd, &Default::default()) {
            let context_str = context::assemble_context(&bundle);
            if !context_str.is_empty() {
                state.messages.push(SessionMessage::SystemText {
                    content: context_str,
                });
                write_message_retry(&db, session_id, 1, &state.messages[1]).ok();
                state.context_fingerprint = Some(bundle.fingerprint);
                state.context_file_paths = bundle.files.iter().map(|f| f.path.clone()).collect();
                state.context_message_index = Some(1);
            }
        }
    }

    let _ = daemon_tx.send(DaemonCommand::UpdateMetadata {
        session_id,
        metadata: SessionMetadata {
            title: state.title.clone(),
            selected_model: state.selected_model.clone(),
            parent_session_id: state.parent_session_id,
            cwd: state.cwd.as_ref().map(|p| p.display().to_string()),
            created_at: state.created_at,
            message_count: state.messages.len() as u32,
            max_turns: state.max_turns,
        },
    });

    let mut shutdown_requested = false;
    loop {
        match rx.recv() {
            Ok(cmd) => {
                if process_command(
                    cmd,
                    &mut state,
                    session_id,
                    &db,
                    client.as_ref(),
                    &tool_registry,
                    &daemon_tx,
                    &cmd_tx,
                    &mut shutdown_requested,
                    max_turns_default,
                ) {
                    break;
                }
            }
            Err(_) => break,
        }
    }

    persist_and_exit(&state, &db, session_id, &daemon_tx);
}

fn process_command(
    cmd: SessionCommand,
    state: &mut SessionState,
    session_id: u64,
    db: &Arc<redb::Database>,
    client: Option<&Arc<OpenAiClient>>,
    tool_registry: &Arc<ToolRegistry>,
    daemon_tx: &UnboundedSender<DaemonCommand>,
    cmd_tx: &mpsc::Sender<SessionCommand>,
    shutdown_requested: &mut bool,
    max_turns_default: u32,
) -> bool {
    match cmd {
        SessionCommand::RunInput { request_id, input } => {
            let text = String::from_utf8_lossy(&input).trim().to_string();
            if text.is_empty() {
                broadcast(&state.subscribers, DaemonMessage::Started { request_id });
                broadcast(
                    &state.subscribers,
                    DaemonMessage::Failed {
                        request_id,
                        error: "empty input".to_string(),
                    },
                );
                return false;
            }
            let Some(client) = client else {
                broadcast(&state.subscribers, DaemonMessage::Started { request_id });
                broadcast(
                    &state.subscribers,
                    DaemonMessage::Failed {
                        request_id,
                        error: "daemon is locked".to_string(),
                    },
                );
                return false;
            };
            let model = match &state.selected_model {
                Some(m) => m.clone(),
                None => {
                    broadcast(&state.subscribers, DaemonMessage::Started { request_id });
                    broadcast(
                        &state.subscribers,
                        DaemonMessage::Failed {
                            request_id,
                            error: "no model selected".to_string(),
                        },
                    );
                    return false;
                }
            };
            if *shutdown_requested {
                broadcast(&state.subscribers, DaemonMessage::Started { request_id });
                broadcast(
                    &state.subscribers,
                    DaemonMessage::Failed {
                        request_id,
                        error: "session is shutting down".to_string(),
                    },
                );
                return false;
            }
            if !state.active_requests.is_empty() {
                broadcast(&state.subscribers, DaemonMessage::Started { request_id });
                broadcast(
                    &state.subscribers,
                    DaemonMessage::Failed {
                        request_id,
                        error: "session already has an active request".to_string(),
                    },
                );
                return false;
            }

            let user_msg = SessionMessage::UserText {
                content: text.clone(),
            };
            let msg_idx = state.messages.len() as u32;
            state.messages.push(user_msg.clone());
            write_message_retry(db, session_id, msg_idx, &user_msg).ok();
            broadcast(
                &state.subscribers,
                DaemonMessage::SessionMessageAppended { message: user_msg },
            );

            broadcast(&state.subscribers, DaemonMessage::Started { request_id });
            let cancel = Arc::new(AtomicBool::new(false));
            state.active_requests.insert(
                request_id,
                ActiveRequest {
                    cancel: Arc::clone(&cancel),
                },
            );

            let cwd = state.cwd.clone();
            let worker_subscribers = state.subscribers.clone();
            let mut worker_session =
                SessionState::from_snapshot(state.snapshot(), worker_subscribers.clone());
            let db = Arc::clone(db);
            let client = Arc::clone(client);
            let tool_registry = Arc::clone(tool_registry);
            let daemon_tx = daemon_tx.clone();
            let cmd_tx = cmd_tx.clone();
            std::thread::spawn(move || {
                let _ = run_request_worker(
                    session_id,
                    request_id,
                    client,
                    &mut worker_session,
                    db,
                    model,
                    cwd,
                    cancel,
                    tool_registry,
                    daemon_tx,
                    worker_subscribers,
                    max_turns_default,
                    cmd_tx,
                    None,
                );
            });
            false
        }
        SessionCommand::RunChildInput {
            request_id,
            input_tokens: _,
            reply,
        } => {
            let Some(client) = client else {
                let _ = reply.send(Err(io::Error::new(io::ErrorKind::Other, "daemon locked")));
                return false;
            };
            let model = state.selected_model.clone().unwrap_or_default();
            if *shutdown_requested {
                let _ = reply.send(Err(io::Error::new(
                    io::ErrorKind::Other,
                    "session is shutting down",
                )));
                return false;
            }
            if !state.active_requests.is_empty() {
                let _ = reply.send(Err(io::Error::new(
                    io::ErrorKind::Other,
                    "session already has an active request",
                )));
                return false;
            }
            broadcast(&state.subscribers, DaemonMessage::Started { request_id });
            let cwd = state.cwd.clone();
            let cancel = Arc::new(AtomicBool::new(false));
            state.active_requests.insert(
                request_id,
                ActiveRequest {
                    cancel: Arc::clone(&cancel),
                },
            );
            let worker_subscribers = state.subscribers.clone();
            let mut worker_session =
                SessionState::from_snapshot(state.snapshot(), worker_subscribers.clone());
            let db = Arc::clone(db);
            let client = Arc::clone(client);
            let tool_registry = Arc::clone(tool_registry);
            let daemon_tx = daemon_tx.clone();
            let cmd_tx = cmd_tx.clone();
            std::thread::spawn(move || {
                let result = run_request_worker(
                    session_id,
                    request_id,
                    client,
                    &mut worker_session,
                    db,
                    model,
                    cwd,
                    cancel,
                    tool_registry,
                    daemon_tx,
                    worker_subscribers,
                    max_turns_default,
                    cmd_tx,
                    Some(reply),
                );
                let _ = result;
            });
            false
        }
        SessionCommand::Cancel { request_id } => {
            if let Some(active) = state.active_requests.get(&request_id) {
                active.cancel.store(true, Ordering::SeqCst);
                broadcast(&state.subscribers, DaemonMessage::Cancelled { request_id });
            }
            false
        }
        SessionCommand::SetModel { model } => {
            state.selected_model = Some(model.clone());
            false
        }
        SessionCommand::Attach { client_id, tx } => {
            state.subscribers.insert(client_id, tx);
            let snapshot = DaemonMessage::SessionState {
                session_id,
                title: state.title.clone(),
                selected_model: state.selected_model.clone(),
                parent_session_id: state.parent_session_id,
                cwd: state.cwd.as_ref().map(|p| p.display().to_string()),
                max_turns: state.max_turns,
                messages: state.messages.clone(),
            };
            if let Some(tx) = state.subscribers.get(&client_id) {
                let _ = tx.send(snapshot);
            }
            false
        }
        SessionCommand::Detach { client_id } => {
            state.subscribers.remove(&client_id);
            state.active_requests.is_empty()
                && (state.subscribers.is_empty() || *shutdown_requested)
        }
        SessionCommand::GetSummary { reply } => {
            let _ = reply.send(SessionSummary {
                session_id,
                title: state.title.clone(),
                selected_model: state.selected_model.clone(),
                parent_session_id: state.parent_session_id,
                cwd: state.cwd.as_ref().map(|p| p.display().to_string()),
                created_at: state.created_at,
                message_count: state.messages.len() as u32,
                max_turns: state.max_turns,
            });
            false
        }
        SessionCommand::AppendMessage { message } => {
            let idx = state.messages.len() as u32;
            state.messages.push(message.clone());
            write_message_retry(db, session_id, idx, &message).ok();
            false
        }
        SessionCommand::RequestFinished {
            request_id,
            snapshot,
        } => {
            state.apply_snapshot(snapshot);
            state.active_requests.remove(&request_id);
            let _ = daemon_tx.send(DaemonCommand::UpdateMetadata {
                session_id,
                metadata: SessionMetadata {
                    title: state.title.clone(),
                    selected_model: state.selected_model.clone(),
                    parent_session_id: state.parent_session_id,
                    cwd: state.cwd.as_ref().map(|p| p.display().to_string()),
                    created_at: state.created_at,
                    message_count: state.messages.len() as u32,
                    max_turns: state.max_turns,
                },
            });
            state.active_requests.is_empty()
                && (state.subscribers.is_empty() || *shutdown_requested)
        }
        SessionCommand::Shutdown => {
            *shutdown_requested = true;
            for (&request_id, active) in &state.active_requests {
                active.cancel.store(true, Ordering::SeqCst);
                broadcast(&state.subscribers, DaemonMessage::Cancelled { request_id });
            }
            state.active_requests.is_empty()
        }
    }
}

fn run_request_worker(
    session_id: u64,
    request_id: u32,
    client: Arc<OpenAiClient>,
    session: &mut SessionState,
    db: Arc<redb::Database>,
    model: String,
    cwd: Option<PathBuf>,
    cancel: Arc<AtomicBool>,
    tool_registry: Arc<ToolRegistry>,
    daemon_tx: UnboundedSender<DaemonCommand>,
    subscribers: HashMap<u64, std::sync::mpsc::Sender<DaemonMessage>>,
    max_turns_default: u32,
    cmd_tx: mpsc::Sender<SessionCommand>,
    child_reply: Option<mpsc::Sender<io::Result<ChildResult>>>,
) -> io::Result<()> {
    session.subscribers = subscribers;
    let initial_snapshot = session.snapshot();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run_agent_loop(
            &client,
            session,
            session_id,
            &db,
            &model,
            request_id,
            cwd.as_deref(),
            &cancel,
            &tool_registry,
            &daemon_tx,
            max_turns_default,
        )
    }));

    let (outcome, snapshot) = match result {
        Ok(Ok(())) if cancel.load(Ordering::SeqCst) => {
            (RequestOutcome::Cancelled, session.snapshot())
        }
        Ok(Ok(())) => (RequestOutcome::Done, session.snapshot()),
        Ok(Err(_)) if cancel.load(Ordering::SeqCst) => {
            (RequestOutcome::Cancelled, session.snapshot())
        }
        Ok(Err(e)) => (RequestOutcome::Failed(e), session.snapshot()),
        Err(_) => (
            RequestOutcome::Failed(io::Error::new(
                io::ErrorKind::Other,
                "request worker panicked",
            )),
            initial_snapshot,
        ),
    };

    match &outcome {
        RequestOutcome::Done => {
            broadcast(&session.subscribers, DaemonMessage::Done { request_id });
        }
        RequestOutcome::Failed(error) => {
            broadcast(
                &session.subscribers,
                DaemonMessage::Failed {
                    request_id,
                    error: error.to_string(),
                },
            );
        }
        RequestOutcome::Cancelled => {}
    }

    if let Some(reply) = child_reply {
        let child_result = match &outcome {
            RequestOutcome::Done => {
                let output = session
                    .messages
                    .iter()
                    .filter_map(|m| match m {
                        SessionMessage::AssistantText { content } => Some(content.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                Ok(ChildResult {
                    output,
                    is_error: false,
                })
            }
            RequestOutcome::Failed(error) => Ok(ChildResult {
                output: error.to_string(),
                is_error: true,
            }),
            RequestOutcome::Cancelled => Ok(ChildResult {
                output: "request cancelled".to_string(),
                is_error: true,
            }),
        };
        let _ = reply.send(child_result);
    }

    let _ = cmd_tx.send(SessionCommand::RequestFinished {
        request_id,
        snapshot,
    });
    Ok(())
}

enum RequestOutcome {
    Done,
    Failed(io::Error),
    Cancelled,
}

fn persist_and_exit(
    state: &SessionState,
    db: &redb::Database,
    session_id: u64,
    daemon_tx: &UnboundedSender<DaemonCommand>,
) {
    let record = SessionRecord {
        title: state.title.clone(),
        selected_model: state.selected_model.clone(),
        parent_session_id: state.parent_session_id,
        cwd: state.cwd.as_ref().map(|p| p.display().to_string()),
        max_turns: state.max_turns,
        message_count: state.messages.len() as u32,
        created_at: state.created_at,
    };
    write_session_retry(db, session_id, &record).ok();
    let _ = daemon_tx.send(DaemonCommand::SessionExited { session_id });
}
