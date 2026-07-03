use crate::context;
use crate::db::{self, SessionRecord, write_message_retry, write_session_retry};
use crate::openai::OpenAiClient;
use crate::requests::run_agent_loop;
use crate::tools::ToolRegistry;
use crate::daemon::DaemonCommand;
use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::task::JoinHandle;
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

pub struct ActiveSessionEntry {
    pub cmd_tx: std::sync::mpsc::Sender<SessionCommand>,
    pub handle: JoinHandle<()>,
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
    pub context_fingerprint: Option<u64>,
    pub context_file_paths: Vec<PathBuf>,
    pub context_message_index: Option<usize>,
}

fn broadcast(subscribers: &HashMap<u64, std::sync::mpsc::Sender<DaemonMessage>>, message: DaemonMessage) {
    for tx in subscribers.values() {
        if let Err(e) = tx.send(message.clone()) {
            warn!("failed to broadcast, subscriber disconnected: {e}");
        }
    }
}

pub fn session_main(
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
        cwd: init_record.as_ref().and_then(|r| r.cwd.as_ref().map(PathBuf::from)),
        max_turns: init_record.as_ref().and_then(|r| r.max_turns),
        created_at: init_record.as_ref().map(|r| r.created_at).unwrap_or_else(|| {
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64
        }),
        messages: Vec::new(),
        subscribers: HashMap::new(),
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
        state.messages.push(SessionMessage::SystemText { content: base_prompt });
        write_message_retry(&db, session_id, 0, &state.messages[0]).ok();

        if let Ok(bundle) = context::discover_context(effective_cwd, &Default::default()) {
            let context_str = context::assemble_context(&bundle);
            if !context_str.is_empty() {
                state.messages.push(SessionMessage::SystemText { content: context_str });
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

    let cancel = AtomicBool::new(false);

    loop {
        if state.subscribers.is_empty() {
            match rx.recv() {
                Ok(cmd) => {
                    process_command(cmd, &mut state, session_id, &db, client.as_deref(), &tool_registry, &daemon_tx, &cancel, max_turns_default);
                    if state.subscribers.is_empty() {
                        persist_and_exit(&state, &db, session_id, &daemon_tx);
                        return;
                    }
                }
                Err(_) => break,
            }
        } else {
            match rx.recv() {
                Ok(cmd) => {
                    process_command(cmd, &mut state, session_id, &db, client.as_deref(), &tool_registry, &daemon_tx, &cancel, max_turns_default);
                }
                Err(_) => break,
            }
        }
    }

    persist_and_exit(&state, &db, session_id, &daemon_tx);
}

fn process_command(
    cmd: SessionCommand,
    state: &mut SessionState,
    session_id: u64,
    db: &Arc<redb::Database>,
    client: Option<&OpenAiClient>,
    tool_registry: &Arc<ToolRegistry>,
    daemon_tx: &UnboundedSender<DaemonCommand>,
    cancel: &AtomicBool,
    max_turns_default: u32,
) {
    match cmd {
        SessionCommand::RunInput { request_id, input } => {
            let text = String::from_utf8_lossy(&input).trim().to_string();
            if text.is_empty() {
                broadcast(&state.subscribers, DaemonMessage::Started { request_id });
                broadcast(&state.subscribers, DaemonMessage::Failed { request_id, error: "empty input".to_string() });
                return;
            }
            let Some(client) = client else {
                broadcast(&state.subscribers, DaemonMessage::Started { request_id });
                broadcast(&state.subscribers, DaemonMessage::Failed { request_id, error: "daemon is locked".to_string() });
                return;
            };
            let model = match &state.selected_model {
                Some(m) => m.clone(),
                None => {
                    broadcast(&state.subscribers, DaemonMessage::Started { request_id });
                    broadcast(&state.subscribers, DaemonMessage::Failed { request_id, error: "no model selected".to_string() });
                    return;
                }
            };

            let user_msg = SessionMessage::UserText { content: text.clone() };
            let msg_idx = state.messages.len() as u32;
            state.messages.push(user_msg.clone());
            write_message_retry(db, session_id, msg_idx, &user_msg).ok();
            broadcast(&state.subscribers, DaemonMessage::SessionMessageAppended { message: user_msg });

            broadcast(&state.subscribers, DaemonMessage::Started { request_id });
            cancel.store(false, Ordering::SeqCst);

            let cwd = state.cwd.clone();
            let result = run_agent_loop(
                client,
                state,
                session_id,
                db,
                &model,
                request_id,
                cwd.as_deref(),
                cancel,
                tool_registry,
                daemon_tx,
                max_turns_default,
            );

            match result {
                Ok(()) => {
                    broadcast(&state.subscribers, DaemonMessage::Done { request_id });
                }
                Err(e) => {
                    broadcast(&state.subscribers, DaemonMessage::Failed { request_id, error: e.to_string() });
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
        }
        SessionCommand::RunChildInput { request_id, input_tokens: _, reply } => {
            let Some(client) = client else {
                let _ = reply.send(Err(io::Error::new(io::ErrorKind::Other, "daemon locked")));
                return;
            };
            let model = state.selected_model.clone().unwrap_or_default();
            broadcast(&state.subscribers, DaemonMessage::Started { request_id });
            cancel.store(false, Ordering::SeqCst);
            let cwd = state.cwd.clone();
            let result = run_agent_loop(
                client, state, session_id, db, &model, request_id,
                cwd.as_deref(), cancel, tool_registry, daemon_tx, max_turns_default,
            );
            match result {
                Ok(()) => {
                    let output = state.messages.iter()
                        .filter_map(|m| match m {
                            SessionMessage::AssistantText { content } => Some(content.clone()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    broadcast(&state.subscribers, DaemonMessage::Done { request_id });
                    let _ = reply.send(Ok(ChildResult { output, is_error: false }));
                }
                Err(e) => {
                    broadcast(&state.subscribers, DaemonMessage::Failed { request_id, error: e.to_string() });
                    let _ = reply.send(Ok(ChildResult { output: e.to_string(), is_error: true }));
                }
            }
        }
        SessionCommand::Cancel { request_id: _ } => {
            cancel.store(true, Ordering::SeqCst);
            broadcast(&state.subscribers, DaemonMessage::Cancelled { request_id: 0 });
        }
        SessionCommand::SetModel { model } => {
            state.selected_model = Some(model.clone());
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
        }
        SessionCommand::Detach { client_id } => {
            state.subscribers.remove(&client_id);
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
        }
        SessionCommand::AppendMessage { message } => {
            let idx = state.messages.len() as u32;
            state.messages.push(message.clone());
            write_message_retry(db, session_id, idx, &message).ok();
        }
        SessionCommand::Shutdown => {}
    }
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
