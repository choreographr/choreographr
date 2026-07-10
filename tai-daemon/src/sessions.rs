use crate::context;
use crate::daemon::DaemonCommand;
use crate::db::{self, SessionRecord, write_message_retry, write_session_retry};
use crate::providers::{
    InferenceProvider, ReasoningSupport, effective_reasoning_support, lookup_provider,
};
use crate::requests::run_agent_loop;
use crate::tools::ToolRegistry;
use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};
use std::time::{SystemTime, UNIX_EPOCH};
use tai_proto::{
    ContextConfig, DaemonMessage, SessionMessage, SessionStatus, SessionSummary, ThinkingEffort,
};
use tracing::{debug, error, info, warn};

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
    StatusChanged(SessionStatus),
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
    /// Route a daemon message through the main session thread's subscriber
    /// map so that workers always broadcast to the live subscriber set
    /// rather than a stale clone of it.
    Broadcast(DaemonMessage),
    SetAccount {
        name: String,
    },
    SetReasoningEffort {
        effort: ThinkingEffort,
    },
    GetReasoningEffort {
        reply: mpsc::Sender<ThinkingEffort>,
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
    pub reasoning_effort: Option<ThinkingEffort>,
    pub parent_session_id: Option<u64>,
    pub cwd: Option<String>,
    pub created_at: i64,
    pub message_count: u32,
    pub max_turns: Option<u32>,
    pub status: SessionStatus,
    pub active_tool_groups: Vec<String>,
    pub account_name: Option<String>,
}

/// Convert a persisted record into metadata. New sessions loaded from the
/// database are given [`SessionStatus::Sleeping`] by default; the caller can
/// override if needed (e.g. `AttachSession` sets `Inactive`).
impl From<SessionRecord> for SessionMetadata {
    fn from(record: SessionRecord) -> Self {
        SessionMetadata {
            title: record.title,
            selected_model: record.selected_model,
            reasoning_effort: record.reasoning_effort,
            parent_session_id: record.parent_session_id,
            cwd: record.cwd,
            created_at: record.created_at,
            message_count: record.message_count,
            max_turns: record.max_turns,
            status: SessionStatus::Sleeping,
            active_tool_groups: record.active_tool_groups,
            account_name: record.account_name,
        }
    }
}

/// Convert metadata back to a record for storage (drops runtime-only fields).
impl From<SessionMetadata> for SessionRecord {
    fn from(meta: SessionMetadata) -> Self {
        SessionRecord {
            title: meta.title,
            selected_model: meta.selected_model,
            reasoning_effort: meta.reasoning_effort,
            parent_session_id: meta.parent_session_id,
            cwd: meta.cwd,
            max_turns: meta.max_turns,
            message_count: meta.message_count,
            created_at: meta.created_at,
            active_tool_groups: meta.active_tool_groups,
            context_config: ContextConfig::default(),
            account_name: meta.account_name,
        }
    }
}

/// Capture a snapshot of `SessionState` as metadata for the daemon's
/// in-memory index or for sending through the command channel.
///
/// Fields that don't exist in [`SessionMetadata`] (subscribers, active
/// requests, message contents, etc.) are dropped. The `PathBuf` CWD is
/// stringified.
impl From<&SessionState> for SessionMetadata {
    fn from(state: &SessionState) -> Self {
        SessionMetadata {
            title: state.title.clone(),
            selected_model: state.selected_model.clone(),
            reasoning_effort: state.reasoning_effort,
            parent_session_id: state.parent_session_id,
            cwd: state.cwd.as_ref().map(|p| p.display().to_string()),
            created_at: state.created_at,
            message_count: state.messages.len() as u32,
            max_turns: state.max_turns,
            status: state.status.clone(),
            active_tool_groups: state.active_tool_groups.iter().cloned().collect(),
            account_name: state.account_name.clone(),
        }
    }
}

/// Convert session state to a persistable record.
///
/// Delegates through [`SessionMetadata`] so that the field-level mapping
/// lives in one place.
impl From<&SessionState> for SessionRecord {
    fn from(state: &SessionState) -> Self {
        let meta: SessionMetadata = state.into();
        let mut record: SessionRecord = meta.into();
        record.context_config = state.context_config.clone();
        record
    }
}

#[derive(Clone)]
pub struct SessionSnapshot {
    pub title: Option<String>,
    pub selected_model: Option<String>,
    pub reasoning_effort: Option<ThinkingEffort>,
    pub parent_session_id: Option<u64>,
    pub cwd: Option<PathBuf>,
    pub max_turns: Option<u32>,
    pub created_at: i64,
    pub messages: Vec<SessionMessage>,
    pub context_fingerprint: Option<u64>,
    pub context_file_paths: Vec<PathBuf>,
    pub context_message_index: Option<usize>,
    pub status: SessionStatus,
    pub active_tool_groups: std::collections::HashSet<String>,
    pub context_config: ContextConfig,
    pub account_name: Option<String>,
}

pub(crate) struct ActiveRequest {
    pub(crate) cancel_tx: mpsc::Sender<()>,
}

pub struct ActiveSessionEntry {
    pub cmd_tx: mpsc::Sender<SessionCommand>,
    pub handle: std::thread::JoinHandle<()>,
}

pub struct SessionState {
    pub title: Option<String>,
    pub selected_model: Option<String>,
    pub reasoning_effort: Option<ThinkingEffort>,
    pub parent_session_id: Option<u64>,
    pub cwd: Option<PathBuf>,
    pub max_turns: Option<u32>,
    pub created_at: i64,
    messages: Vec<SessionMessage>,
    subscribers: HashMap<u64, std::sync::mpsc::Sender<DaemonMessage>>,
    pub(crate) active_requests: HashMap<u32, ActiveRequest>,
    pub context_fingerprint: Option<u64>,
    pub context_file_paths: Vec<PathBuf>,
    pub context_message_index: Option<usize>,
    pub status: SessionStatus,
    pub active_tool_groups: std::collections::HashSet<String>,
    pub context_config: ContextConfig,
    pub account_name: Option<String>,
    pub provider: Option<InferenceProvider>,
}

impl SessionState {
    fn snapshot(&self) -> SessionSnapshot {
        SessionSnapshot {
            title: self.title.clone(),
            selected_model: self.selected_model.clone(),
            reasoning_effort: self.reasoning_effort,
            parent_session_id: self.parent_session_id,
            cwd: self.cwd.clone(),
            max_turns: self.max_turns,
            created_at: self.created_at,
            messages: self.messages.clone(),
            context_fingerprint: self.context_fingerprint,
            context_file_paths: self.context_file_paths.clone(),
            context_message_index: self.context_message_index,
            status: self.status.clone(),
            active_tool_groups: self.active_tool_groups.clone(),
            context_config: self.context_config.clone(),
            account_name: self.account_name.clone(),
        }
    }

    fn from_snapshot(
        snapshot: SessionSnapshot,
        subscribers: HashMap<u64, std::sync::mpsc::Sender<DaemonMessage>>,
    ) -> Self {
        Self {
            title: snapshot.title,
            selected_model: snapshot.selected_model,
            reasoning_effort: snapshot.reasoning_effort,
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
            status: snapshot.status,
            active_tool_groups: snapshot.active_tool_groups,
            context_config: snapshot.context_config,
            account_name: snapshot.account_name,
            provider: None,
        }
    }

    fn apply_snapshot(&mut self, snapshot: SessionSnapshot) {
        self.title = snapshot.title;
        self.selected_model = snapshot.selected_model;
        self.reasoning_effort = snapshot.reasoning_effort;
        self.parent_session_id = snapshot.parent_session_id;
        self.cwd = snapshot.cwd;
        self.max_turns = snapshot.max_turns;
        self.created_at = snapshot.created_at;
        self.messages = snapshot.messages;
        self.context_fingerprint = snapshot.context_fingerprint;
        self.context_file_paths = snapshot.context_file_paths;
        self.context_message_index = snapshot.context_message_index;
        self.status = snapshot.status;
        self.active_tool_groups = snapshot.active_tool_groups;
        self.context_config = snapshot.context_config;
        self.account_name = snapshot.account_name;
    }

    /// Read-only access to messages.
    pub fn messages(&self) -> &[SessionMessage] {
        &self.messages
    }

    /// Number of messages (convenience).
    pub fn num_messages(&self) -> usize {
        self.messages.len()
    }

    /// Append a message and return its index.
    pub fn push_message(&mut self, msg: SessionMessage) -> u32 {
        let idx = self.messages.len() as u32;
        self.messages.push(msg);
        idx
    }

    /// Replace a message at a given index (used for context refresh).
    pub fn set_message(&mut self, idx: usize, msg: SessionMessage) {
        self.messages[idx] = msg;
    }

    /// Create an empty session state.
    pub fn empty() -> Self {
        Self {
            title: None,
            selected_model: None,
            reasoning_effort: None,
            parent_session_id: None,
            cwd: None,
            max_turns: None,
            created_at: 0,
            messages: Vec::new(),
            subscribers: HashMap::new(),
            active_requests: HashMap::new(),
            context_fingerprint: None,
            context_file_paths: Vec::new(),
            context_message_index: None,
            status: SessionStatus::Inactive,
            active_tool_groups: HashSet::new(),
            context_config: ContextConfig::default(),
            account_name: None,
            provider: None,
        }
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

fn fail_request(
    subscribers: &HashMap<u64, std::sync::mpsc::Sender<DaemonMessage>>,
    request_id: u32,
    error: impl Into<String>,
) -> bool {
    broadcast(subscribers, DaemonMessage::Started { request_id });
    broadcast(
        subscribers,
        DaemonMessage::Failed {
            request_id,
            error: error.into(),
        },
    );
    false
}

#[allow(clippy::too_many_arguments)]
pub fn session_main(
    cmd_tx: mpsc::Sender<SessionCommand>,
    rx: std::sync::mpsc::Receiver<SessionCommand>,
    session_id: u64,
    db: Arc<redb::Database>,
    provider: Option<InferenceProvider>,
    account_name: Option<String>,
    tool_registry: Arc<ToolRegistry>,
    daemon_tx: mpsc::Sender<DaemonCommand>,
    init_record: Option<SessionRecord>,
    max_turns_default: u32,
) {
    let mut state = SessionState {
        title: init_record.as_ref().and_then(|r| r.title.clone()),
        selected_model: init_record.as_ref().and_then(|r| r.selected_model.clone()),
        reasoning_effort: init_record.as_ref().and_then(|r| r.reasoning_effort),
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
                    .unwrap_or_default()
                    .as_secs() as i64
            }),
        messages: Vec::new(),
        subscribers: HashMap::new(),
        active_requests: HashMap::new(),
        context_fingerprint: None,
        context_file_paths: Vec::new(),
        context_message_index: None,
        status: SessionStatus::Inactive,
        active_tool_groups: init_record
            .as_ref()
            .map(|r| r.active_tool_groups.iter().cloned().collect())
            .filter(|cats: &HashSet<String>| !cats.is_empty())
            .unwrap_or_else(|| {
                HashSet::from(["core".to_string(), "git".to_string(), "shell".to_string()])
            }),
        context_config: init_record
            .as_ref()
            .map(|r| r.context_config.clone())
            .unwrap_or_default(),
        account_name,
        provider,
    };

    match db::read_messages(&db, session_id) {
        Ok(msgs) => state.messages = msgs,
        Err(e) => warn!(session_id, error = %e, "failed to load messages from DB"),
    }

    if init_record.is_none() || state.messages.is_empty() {
        let effective_cwd = state.cwd.as_deref().unwrap_or_else(|| Path::new("."));
        let skills = context::discover_skills(effective_cwd);
        let base_prompt = context::build_base_prompt(&skills, tool_registry.groups());
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
        metadata: SessionMetadata::from(&state),
    });

    info!("session {} started", session_id);

    let mut shutdown_requested = false;
    while let Ok(cmd) = rx.recv() {
        if process_command(
            cmd,
            &mut state,
            session_id,
            &db,
            &tool_registry,
            &daemon_tx,
            &cmd_tx,
            &mut shutdown_requested,
            max_turns_default,
        ) {
            break;
        }
    }

    info!("session {} exiting", session_id);
    persist_and_exit(&state, &db, session_id, &daemon_tx);
}

#[allow(clippy::too_many_arguments)]
fn process_command(
    cmd: SessionCommand,
    state: &mut SessionState,
    session_id: u64,
    db: &Arc<redb::Database>,
    tool_registry: &Arc<ToolRegistry>,
    daemon_tx: &mpsc::Sender<DaemonCommand>,
    cmd_tx: &mpsc::Sender<SessionCommand>,
    shutdown_requested: &mut bool,
    max_turns_default: u32,
) -> bool {
    match cmd {
        SessionCommand::RunInput { request_id, input } => {
            debug!("session {}: RunInput id={}", session_id, request_id);
            let text = String::from_utf8_lossy(&input).trim().to_string();
            info!(
                session_id,
                input_len = text.len(),
                input_preview = %text.chars().take(120).collect::<String>(),
                "session received input",
            );
            if text.is_empty() {
                return fail_request(&state.subscribers, request_id, "empty input");
            }
            let provider = if let Some(p) = state.provider.as_ref() {
                p
            } else if let Some(ref name) = state.account_name {
                // No cached provider yet — try lazy resolution via the daemon.
                let (reply, rx) = mpsc::channel();
                let _ = daemon_tx.send(DaemonCommand::ResolveProviderCmd {
                    account: name.clone(),
                    reply,
                });
                match rx.recv() {
                    Ok(Some(provider)) => {
                        state.provider = Some(provider);
                        let Some(p) = state.provider.as_ref() else {
                            return fail_request(
                                &state.subscribers,
                                request_id,
                                "internal error: provider not set after resolution".to_string(),
                            );
                        };
                        p
                    }
                    _ => {
                        return fail_request(
                            &state.subscribers,
                            request_id,
                            format!(
                                "no credential stored for account '{name}' — add one via the AI Providers page or /add-key"
                            ),
                        );
                    }
                }
            } else {
                return fail_request(
                    &state.subscribers,
                    request_id,
                    "no account configured on this session — use /account <name> to set one",
                );
            };
            let model = match &state.selected_model {
                Some(m) => m.clone(),
                None => {
                    return fail_request(&state.subscribers, request_id, "no model selected");
                }
            };
            if *shutdown_requested {
                return fail_request(&state.subscribers, request_id, "session is shutting down");
            }
            if !state.active_requests.is_empty() {
                return fail_request(
                    &state.subscribers,
                    request_id,
                    "session already has an active request",
                );
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
            let (cancel_tx, cancel_rx) = mpsc::channel::<()>();
            state
                .active_requests
                .insert(request_id, ActiveRequest { cancel_tx });

            let cwd = state.cwd.clone();
            // Workers don't need their own subscriber map — all broadcasts
            // are routed through SessionCommand::Broadcast to this main
            // session thread which holds the live subscriber set.
            let mut worker_session = SessionState::from_snapshot(state.snapshot(), HashMap::new());
            let db = Arc::clone(db);
            let provider = provider.clone();
            let tool_registry = Arc::clone(tool_registry);
            let daemon_tx = daemon_tx.clone();
            let cmd_tx = cmd_tx.clone();
            std::thread::spawn(move || {
                let _ = run_request_worker(
                    session_id,
                    request_id,
                    provider,
                    &mut worker_session,
                    db,
                    model,
                    cwd,
                    cancel_rx,
                    tool_registry,
                    daemon_tx,
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
            let Some(provider) = state.provider.as_ref() else {
                let _ = reply.send(Err(io::Error::other("daemon locked")));
                return false;
            };
            let model = state.selected_model.clone().unwrap_or_default();
            if *shutdown_requested {
                let _ = reply.send(Err(io::Error::other("session is shutting down")));
                return false;
            }
            if !state.active_requests.is_empty() {
                let _ = reply.send(Err(io::Error::other(
                    "session already has an active request",
                )));
                return false;
            }
            broadcast(&state.subscribers, DaemonMessage::Started { request_id });
            let cwd = state.cwd.clone();
            let (cancel_tx, cancel_rx) = mpsc::channel::<()>();
            state
                .active_requests
                .insert(request_id, ActiveRequest { cancel_tx });
            let mut worker_session = SessionState::from_snapshot(state.snapshot(), HashMap::new());
            let db = Arc::clone(db);
            let provider = provider.clone();
            let tool_registry = Arc::clone(tool_registry);
            let daemon_tx = daemon_tx.clone();
            let cmd_tx = cmd_tx.clone();
            std::thread::spawn(move || {
                let result = run_request_worker(
                    session_id,
                    request_id,
                    provider,
                    &mut worker_session,
                    db,
                    model,
                    cwd,
                    cancel_rx,
                    tool_registry,
                    daemon_tx,
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
                let _ = active.cancel_tx.send(());
                broadcast(&state.subscribers, DaemonMessage::Cancelled { request_id });
            }
            false
        }
        SessionCommand::SetModel { model } => {
            info!("session {}: SetModel model={}", session_id, model);
            state.selected_model = Some(model.clone());
            debug!(
                "session {}: broadcasting ModelSelected model={}",
                session_id, model
            );
            broadcast(
                &state.subscribers,
                DaemonMessage::ModelSelected {
                    model: model.clone(),
                },
            );
            let _ = daemon_tx.send(DaemonCommand::UpdateMetadata {
                session_id,
                metadata: SessionMetadata::from(&*state),
            });
            false
        }
        SessionCommand::StatusChanged(new_status) => {
            state.status = new_status.clone();
            broadcast(
                &state.subscribers,
                DaemonMessage::SessionStatusChanged {
                    session_id,
                    status: new_status.clone(),
                },
            );
            let _ = daemon_tx.send(DaemonCommand::BroadcastSessionStatus {
                session_id,
                status: new_status,
            });
            false
        }
        SessionCommand::Attach { client_id, tx } => {
            info!("session {}: client {} attached", session_id, client_id);
            state.subscribers.insert(client_id, tx);
            let snapshot = DaemonMessage::SessionState {
                session_id,
                title: state.title.clone(),
                selected_model: state.selected_model.clone(),
                parent_session_id: state.parent_session_id,
                cwd: state.cwd.as_ref().map(|p| p.display().to_string()),
                max_turns: state.max_turns,
                messages: state.messages.clone(),
                active_tool_groups: state.active_tool_groups.iter().cloned().collect(),
            };
            if let Some(tx) = state.subscribers.get(&client_id) {
                let _ = tx.send(snapshot);
            }
            false
        }
        SessionCommand::Detach { client_id } => {
            info!("session {}: client {} detached", session_id, client_id);
            state.subscribers.remove(&client_id);
            state.active_requests.is_empty()
                && (state.subscribers.is_empty() || *shutdown_requested)
        }
        SessionCommand::GetSummary { reply } => {
            let _ = reply.send(SessionSummary {
                session_id,
                title: state.title.clone(),
                selected_model: state.selected_model.clone(),
                reasoning_effort: state.reasoning_effort,
                parent_session_id: state.parent_session_id,
                cwd: state.cwd.as_ref().map(|p| p.display().to_string()),
                created_at: state.created_at,
                message_count: state.messages.len() as u32,
                max_turns: state.max_turns,
                status: state.status.clone(),
                active_tool_groups: state.active_tool_groups.iter().cloned().collect(),
                account_name: state.account_name.clone(),
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
            state.status = SessionStatus::Inactive;
            let _ = daemon_tx.send(DaemonCommand::UpdateMetadata {
                session_id,
                metadata: SessionMetadata::from(&*state),
            });
            broadcast(
                &state.subscribers,
                DaemonMessage::SessionStatusChanged {
                    session_id,
                    status: SessionStatus::Inactive,
                },
            );
            let _ = daemon_tx.send(DaemonCommand::BroadcastSessionStatus {
                session_id,
                status: SessionStatus::Inactive,
            });
            state.active_requests.is_empty()
                && (state.subscribers.is_empty() || *shutdown_requested)
        }
        SessionCommand::Broadcast(message) => {
            // Broadcast through the main session thread's live subscriber
            // map so that in-flight worker broadcasts respect detach.
            broadcast(&state.subscribers, message);
            false
        }
        SessionCommand::SetAccount { name } => {
            info!("session {}: SetAccount account={}", session_id, name);
            // Try to resolve the provider from the daemon by name.
            let (reply, rx) = mpsc::channel();
            let _ = daemon_tx.send(DaemonCommand::ResolveProviderCmd {
                account: name.clone(),
                reply,
            });
            if let Ok(Some(provider)) = rx.recv() {
                state.provider = Some(provider);
            }
            // Always store the account name on the session, even if the
            // provider wasn't resolvable yet (e.g. no credential stored,
            // or daemon hasn't unlocked).  The provider can be resolved
            // lazily when RunInput is called.  This way the user can set
            // an account on a session before unlocking.
            state.account_name = Some(name.clone());
            broadcast(
                &state.subscribers,
                DaemonMessage::SessionAccountSet { account: name },
            );
            let _ = daemon_tx.send(DaemonCommand::UpdateMetadata {
                session_id,
                metadata: SessionMetadata::from(&*state),
            });
            false
        }
        SessionCommand::SetReasoningEffort { effort } => {
            info!(
                session_id,
                effort = %effort.as_label(),
                "setting reasoning effort"
            );

            // Check if the current model supports reasoning
            let supported = if let (Some(model), Some(provider)) =
                (state.selected_model.as_ref(), state.provider.as_ref())
            {
                // Get the provider slug
                let slug = provider.provider_slug();
                let catalog_entry = lookup_provider(slug);
                let reasoning_support = catalog_entry
                    .map(|e| e.reasoning)
                    .unwrap_or(ReasoningSupport::None);
                let effective = effective_reasoning_support(model, reasoning_support);
                effective != ReasoningSupport::None
            } else {
                // If no model or provider is set yet, accept the preference
                // (it will be validated when inference actually runs)
                true
            };

            if supported || effort == ThinkingEffort::Off {
                state.reasoning_effort = Some(effort);
                debug!(
                    session_id,
                    effort = %effort.as_label(),
                    "reasoning effort stored"
                );
                broadcast(
                    &state.subscribers,
                    DaemonMessage::reasoning_effort_set(effort),
                );
            } else {
                let model = state.selected_model.as_deref().unwrap_or("(none)");
                let msg = format!(
                    "model '{}' does not support reasoning effort '{}'",
                    model,
                    effort.as_label(),
                );
                warn!(session_id, error = %msg, "reasoning effort rejected");
                broadcast(
                    &state.subscribers,
                    DaemonMessage::reasoning_effort_set_failed(effort.as_label(), msg),
                );
            }
            false
        }
        SessionCommand::GetReasoningEffort { reply } => {
            let current = state.reasoning_effort.unwrap_or(ThinkingEffort::Off);
            let _ = reply.send(current);
            false
        }
        SessionCommand::Shutdown => {
            *shutdown_requested = true;
            for (&request_id, active) in &state.active_requests {
                let _ = active.cancel_tx.send(());
                broadcast(&state.subscribers, DaemonMessage::Cancelled { request_id });
            }
            state.active_requests.is_empty()
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn run_request_worker(
    session_id: u64,
    request_id: u32,
    client: InferenceProvider,
    session: &mut SessionState,
    db: Arc<redb::Database>,
    model: String,
    cwd: Option<PathBuf>,
    cancel_rx: mpsc::Receiver<()>,
    tool_registry: Arc<ToolRegistry>,
    daemon_tx: mpsc::Sender<DaemonCommand>,
    max_turns_default: u32,
    cmd_tx: mpsc::Sender<SessionCommand>,
    child_reply: Option<mpsc::Sender<io::Result<ChildResult>>>,
) -> io::Result<()> {
    let request_start = std::time::Instant::now();
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
            &cancel_rx,
            &tool_registry,
            &daemon_tx,
            max_turns_default,
            &cmd_tx,
        )
    }));

    let (outcome, snapshot) = match result {
        Ok(Ok(true)) => (RequestOutcome::Cancelled, session.snapshot()),
        Ok(Ok(false)) => (RequestOutcome::Done, session.snapshot()),
        Ok(Err(e)) => (RequestOutcome::Failed(e), session.snapshot()),
        Err(_) => (
            RequestOutcome::Failed(io::Error::other("request worker panicked")),
            initial_snapshot,
        ),
    };

    let req_status = match &outcome {
        RequestOutcome::Done => "done",
        RequestOutcome::Failed(_) => "failed",
        RequestOutcome::Cancelled => "cancelled",
    };
    crate::metrics::record_request_total(req_status);
    crate::metrics::record_request_duration(req_status, request_start.elapsed().as_secs_f64());

    match &outcome {
        RequestOutcome::Done => {
            info!(session_id, request_id, "request completed");
            // Route through the main session thread so detach is respected.
            let _ = cmd_tx.send(SessionCommand::Broadcast(DaemonMessage::Done {
                request_id,
            }));
        }
        RequestOutcome::Failed(error) => {
            info!(session_id, request_id, error = %error, "request failed");
            // Route through the main session thread so detach is respected.
            let _ = cmd_tx.send(SessionCommand::Broadcast(DaemonMessage::Failed {
                request_id,
                error: error.to_string(),
            }));
        }
        RequestOutcome::Cancelled => {
            info!(session_id, request_id, "request cancelled");
        }
    }

    if let Some(reply) = child_reply {
        let child_result = match &outcome {
            RequestOutcome::Done => {
                let output = session
                    .messages
                    .iter()
                    .filter_map(|m| match m {
                        SessionMessage::AssistantText { content, .. } => Some(content.clone()),
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
    daemon_tx: &mpsc::Sender<DaemonCommand>,
) {
    let record: SessionRecord = SessionRecord::from(state);
    if let Err(e) = write_session_retry(db, session_id, &record) {
        error!(
            "persist_and_exit: failed to persist session {}: {e}",
            session_id
        );
    }
    let _ = daemon_tx.send(DaemonCommand::SessionExited { session_id });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::SessionRecord;
    use crate::tools::ToolRegistry;
    use std::collections::HashMap;
    use tai_proto::{OutputStream, SessionMessage, SessionStatus};
    use tempfile::tempdir;

    fn test_record() -> SessionRecord {
        SessionRecord {
            title: Some("test session".into()),
            selected_model: Some("gpt-4".into()),
            reasoning_effort: None,
            parent_session_id: None,
            cwd: Some("/tmp".into()),
            max_turns: Some(10),
            message_count: 3,
            created_at: 1000,
            active_tool_groups: vec!["core".into(), "shell".into()],
            context_config: ContextConfig::default(),
            account_name: None,
        }
    }

    fn test_state() -> SessionState {
        SessionState {
            title: Some("test session".into()),
            selected_model: Some("gpt-4".into()),
            reasoning_effort: None,
            parent_session_id: None,
            cwd: Some(std::path::PathBuf::from("/tmp")),
            max_turns: Some(10),
            created_at: 1000,
            messages: vec![
                SessionMessage::SystemText {
                    content: "prompt".into(),
                },
                SessionMessage::UserText {
                    content: "hello".into(),
                },
                SessionMessage::AssistantText {
                    content: "hi".into(),
                    reasoning: None,
                },
            ],
            subscribers: HashMap::new(),
            active_requests: HashMap::new(),
            context_fingerprint: None,
            context_file_paths: Vec::new(),
            context_message_index: None,
            status: SessionStatus::Inactive,
            active_tool_groups: ["core".into(), "shell".into()].into(),
            context_config: ContextConfig::default(),
            account_name: None,
            provider: None,
        }
    }

    #[test]
    fn session_record_to_metadata() {
        let record = test_record();
        let meta: SessionMetadata = record.clone().into();
        // Default status should be Sleeping
        assert_eq!(meta.status, SessionStatus::Sleeping);
        assert_eq!(meta.title, record.title);
        assert_eq!(meta.selected_model, record.selected_model);
        assert_eq!(meta.cwd, record.cwd);
        assert_eq!(meta.message_count, record.message_count);
        assert_eq!(meta.active_tool_groups, record.active_tool_groups);
    }

    #[test]
    fn session_metadata_to_record() {
        let meta = SessionMetadata {
            title: Some("meta title".into()),
            selected_model: Some("claude-3".into()),
            reasoning_effort: None,
            parent_session_id: Some(42),
            cwd: Some("/home".into()),
            created_at: 2000,
            message_count: 7,
            max_turns: Some(20),
            status: SessionStatus::Inactive,
            active_tool_groups: vec!["git".into()],
            account_name: None,
        };
        let record: SessionRecord = meta.clone().into();
        // Status field does not exist in record
        assert_eq!(record.title, meta.title);
        assert_eq!(record.selected_model, meta.selected_model);
        assert_eq!(record.active_tool_groups, meta.active_tool_groups);
    }

    #[test]
    fn session_record_round_trip() {
        let record = test_record();
        let meta: SessionMetadata = record.clone().into();
        let record2: SessionRecord = meta.into();
        assert_eq!(record.title, record2.title);
        assert_eq!(record.selected_model, record2.selected_model);
        assert_eq!(record.parent_session_id, record2.parent_session_id);
        assert_eq!(record.cwd, record2.cwd);
        assert_eq!(record.max_turns, record2.max_turns);
        assert_eq!(record.message_count, record2.message_count);
        assert_eq!(record.created_at, record2.created_at);
        assert_eq!(record.active_tool_groups, record2.active_tool_groups);
    }

    #[test]
    fn session_state_to_metadata() {
        let state = test_state();
        let meta: SessionMetadata = (&state).into();
        assert_eq!(meta.title, state.title);
        assert_eq!(meta.selected_model, state.selected_model);
        assert_eq!(meta.message_count, 3);
        assert_eq!(meta.status, state.status);
        assert_eq!(meta.cwd, Some("/tmp".into()));
        assert_eq!(meta.parent_session_id, state.parent_session_id);
    }

    #[test]
    fn session_state_to_record() {
        let state = test_state();
        let record: SessionRecord = (&state).into();
        assert_eq!(record.title, state.title);
        assert_eq!(record.selected_model, state.selected_model);
        assert_eq!(record.message_count, 3);
        assert_eq!(record.cwd, Some("/tmp".into()));
    }

    #[test]
    fn record_round_trip_preserves_active_tool_groups() {
        let record = test_record();
        let meta: SessionMetadata = record.clone().into();
        let record2: SessionRecord = meta.into();
        assert_eq!(record.active_tool_groups, record2.active_tool_groups);
        assert_eq!(record2.active_tool_groups.len(), 2);
    }

    // -- SessionCommand::Broadcast tests -----------------------------------

    /// Build minimal stubs needed to call `process_command` with a Broadcast.
    fn broadcast_setup() -> (
        SessionState,
        Arc<redb::Database>,
        Arc<ToolRegistry>,
        mpsc::Sender<DaemonCommand>,
        mpsc::Sender<SessionCommand>,
    ) {
        let dir = tempdir().unwrap();
        let db = Arc::new(redb::Database::create(dir.path().join("test.redb")).unwrap());
        let tool_registry = ToolRegistry::new().build();
        let (daemon_tx, _) = mpsc::channel();
        let (cmd_tx, _) = mpsc::channel();
        (test_state(), db, tool_registry, daemon_tx, cmd_tx)
    }

    #[test]
    fn broadcast_delivers_message_to_all_subscribers() {
        let (tx1, rx1) = mpsc::channel();
        let (tx2, rx2) = mpsc::channel();
        let (mut state, db, tool_registry, daemon_tx, cmd_tx) = broadcast_setup();
        state.subscribers.insert(10, tx1);
        state.subscribers.insert(20, tx2);

        let mut shutdown = false;
        process_command(
            SessionCommand::Broadcast(DaemonMessage::Done { request_id: 5 }),
            &mut state,
            1,
            &db,
            &tool_registry,
            &daemon_tx,
            &cmd_tx,
            &mut shutdown,
            25,
        );

        assert_eq!(rx1.recv().unwrap(), DaemonMessage::Done { request_id: 5 });
        assert_eq!(rx2.recv().unwrap(), DaemonMessage::Done { request_id: 5 });
        assert!(!shutdown);
    }

    #[test]
    fn broadcast_does_not_deliver_to_detached_client() {
        let (tx1, rx1) = mpsc::channel();
        let (tx2, rx2) = mpsc::channel();
        let (mut state, db, tool_registry, daemon_tx, cmd_tx) = broadcast_setup();
        state.subscribers.insert(10, tx1);
        state.subscribers.insert(20, tx2);

        // Detach client 10
        state.subscribers.remove(&10);

        let mut shutdown = false;
        process_command(
            SessionCommand::Broadcast(DaemonMessage::OutputChunk {
                request_id: 1,
                stream: OutputStream::Answer,
                data: b"hello".to_vec(),
            }),
            &mut state,
            1,
            &db,
            &tool_registry,
            &daemon_tx,
            &cmd_tx,
            &mut shutdown,
            25,
        );

        // Client 20 (still attached) receives the message.
        assert_eq!(
            rx2.recv().unwrap(),
            DaemonMessage::OutputChunk {
                request_id: 1,
                stream: OutputStream::Answer,
                data: b"hello".to_vec(),
            },
        );

        // Client 10 (detached) does not — the sender was removed and dropped,
        // so the channel is disconnected.
        match rx1.recv() {
            Err(_) => {} // expected
            Ok(msg) => panic!("detached client received: {msg:?}"),
        }
    }

    #[test]
    fn broadcast_with_no_subscribers_does_not_panic() {
        let (mut state, db, tool_registry, daemon_tx, cmd_tx) = broadcast_setup();
        // subscribers is already empty

        let mut shutdown = false;
        process_command(
            SessionCommand::Broadcast(DaemonMessage::Done { request_id: 0 }),
            &mut state,
            1,
            &db,
            &tool_registry,
            &daemon_tx,
            &cmd_tx,
            &mut shutdown,
            25,
        );

        assert!(!shutdown);
    }

    #[test]
    fn broadcast_handles_disconnected_subscriber_gracefully() {
        // A sender whose receiver has been dropped (simulating a client that
        // disconnected without properly detaching) should not panic or crash.
        let (tx, _rx) = mpsc::channel();
        drop(_rx);
        let (mut state, db, tool_registry, daemon_tx, cmd_tx) = broadcast_setup();
        state.subscribers.insert(99, tx);

        let mut shutdown = false;
        process_command(
            SessionCommand::Broadcast(DaemonMessage::Pong),
            &mut state,
            1,
            &db,
            &tool_registry,
            &daemon_tx,
            &cmd_tx,
            &mut shutdown,
            25,
        );

        assert!(!shutdown);
    }

    // -- Cancel / Shutdown tests -------------------------------------------

    #[test]
    fn cancel_sends_through_channel() {
        let (cancel_tx, cancel_rx) = mpsc::channel::<()>();
        let (mut state, db, tool_registry, daemon_tx, cmd_tx) = broadcast_setup();
        state.active_requests.insert(1, ActiveRequest { cancel_tx });

        let mut shutdown = false;
        process_command(
            SessionCommand::Cancel { request_id: 1 },
            &mut state,
            1,
            &db,
            &tool_registry,
            &daemon_tx,
            &cmd_tx,
            &mut shutdown,
            25,
        );

        // The cancellation signal should be delivered on the channel.
        assert!(cancel_rx.try_recv().is_ok());
        assert!(!shutdown);
    }

    #[test]
    fn cancel_broadcasts_cancelled_to_subscribers() {
        let (cancel_tx, _cancel_rx) = mpsc::channel::<()>();
        let (mut state, db, tool_registry, daemon_tx, cmd_tx) = broadcast_setup();
        state.active_requests.insert(1, ActiveRequest { cancel_tx });

        let (sub_tx, sub_rx) = mpsc::channel();
        state.subscribers.insert(42, sub_tx);

        let mut shutdown = false;
        process_command(
            SessionCommand::Cancel { request_id: 1 },
            &mut state,
            1,
            &db,
            &tool_registry,
            &daemon_tx,
            &cmd_tx,
            &mut shutdown,
            25,
        );

        assert_eq!(
            sub_rx.recv().unwrap(),
            DaemonMessage::Cancelled { request_id: 1 },
        );
    }

    #[test]
    fn cancel_unknown_request_id_is_noop() {
        let (mut state, db, tool_registry, daemon_tx, cmd_tx) = broadcast_setup();
        // No active requests — cancel on a non-existent ID should not fail.

        let mut shutdown = false;
        process_command(
            SessionCommand::Cancel { request_id: 99 },
            &mut state,
            1,
            &db,
            &tool_registry,
            &daemon_tx,
            &cmd_tx,
            &mut shutdown,
            25,
        );

        assert!(!shutdown);
    }

    #[test]
    fn shutdown_cancels_all_active_requests() {
        let (cancel_tx1, cancel_rx1) = mpsc::channel::<()>();
        let (cancel_tx2, cancel_rx2) = mpsc::channel::<()>();
        let (mut state, db, tool_registry, daemon_tx, cmd_tx) = broadcast_setup();
        state.active_requests.insert(
            1,
            ActiveRequest {
                cancel_tx: cancel_tx1,
            },
        );
        state.active_requests.insert(
            2,
            ActiveRequest {
                cancel_tx: cancel_tx2,
            },
        );

        let mut shutdown = false;
        process_command(
            SessionCommand::Shutdown,
            &mut state,
            1,
            &db,
            &tool_registry,
            &daemon_tx,
            &cmd_tx,
            &mut shutdown,
            25,
        );

        assert!(shutdown);
        // Both requests should have received cancellation signals.
        assert!(cancel_rx1.try_recv().is_ok());
        assert!(cancel_rx2.try_recv().is_ok());
    }

    #[test]
    fn shutdown_with_empty_active_requests_returns_true() {
        // When there are no active requests, Shutdown should signal
        // that the session loop can exit (return true).
        let (mut state, db, tool_registry, daemon_tx, cmd_tx) = broadcast_setup();
        // active_requests is already empty.

        let mut shutdown = false;
        let should_exit = process_command(
            SessionCommand::Shutdown,
            &mut state,
            1,
            &db,
            &tool_registry,
            &daemon_tx,
            &cmd_tx,
            &mut shutdown,
            25,
        );

        assert!(shutdown);
        assert!(should_exit);
    }

    #[test]
    fn shutdown_broadcasts_cancelled_for_each_active_request() {
        let (cancel_tx1, _) = mpsc::channel::<()>();
        let (cancel_tx2, _) = mpsc::channel::<()>();
        let (mut state, db, tool_registry, daemon_tx, cmd_tx) = broadcast_setup();
        state.active_requests.insert(
            1,
            ActiveRequest {
                cancel_tx: cancel_tx1,
            },
        );
        state.active_requests.insert(
            2,
            ActiveRequest {
                cancel_tx: cancel_tx2,
            },
        );

        let (sub_tx, sub_rx) = mpsc::channel();
        state.subscribers.insert(10, sub_tx);

        let mut shutdown = false;
        process_command(
            SessionCommand::Shutdown,
            &mut state,
            1,
            &db,
            &tool_registry,
            &daemon_tx,
            &cmd_tx,
            &mut shutdown,
            25,
        );

        // Should receive two Cancelled broadcasts (order not guaranteed).
        let msgs: Vec<DaemonMessage> = (0..2).map(|_| sub_rx.recv().unwrap()).collect();
        assert!(msgs.contains(&DaemonMessage::Cancelled { request_id: 1 }));
        assert!(msgs.contains(&DaemonMessage::Cancelled { request_id: 2 }));
    }
}
