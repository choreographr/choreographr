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
use std::path::PathBuf;
use std::sync::{Arc, mpsc};
use std::time::{SystemTime, UNIX_EPOCH};
use tai_proto::{
    ContextConfig, DaemonMessage, SessionMessage, SessionMessageKind, SessionStatus,
    SessionSummary, ThinkingEffort, TimestampMs, TokenUsage,
};
use tracing::{debug, error, info, warn};

pub enum SessionCommand {
    RunInput {
        request_id: u32,
        input: Vec<u8>,
    },
    RunChildInput {
        request_id: u32,
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
        tx: std::sync::mpsc::SyncSender<DaemonMessage>,
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
    Undo,
    Redo,
    Shutdown,
}

/// Bundles parameters that are threaded through the session/request pipeline,
/// reducing argument count and making the dependency flow explicit.
#[derive(Clone)]
pub struct RequestContext {
    /// Channel to send SessionCommands back to the session main loop.
    pub cmd_tx: mpsc::Sender<SessionCommand>,
    /// The session ID scoping all operations.
    pub session_id: u64,
    /// Database handle for persisting state.
    pub db: Arc<redb::Database>,
    /// Registry of available tools.
    pub tool_registry: Arc<ToolRegistry>,
    /// Channel to the daemon command loop.
    pub daemon_tx: mpsc::Sender<DaemonCommand>,
    /// Default max agent loop turns when the session doesn't specify one.
    pub max_turns_default: u32,
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
    pub working_dir: Option<String>,
    pub created_at: i64,
    pub message_count: u32,
    pub max_turns: Option<u32>,
    pub status: SessionStatus,
    pub active_tool_groups: Vec<String>,
    pub account_name: Option<String>,
    pub accumulated_usage: TokenUsage,
    pub context_window: Option<u32>,
    pub last_prompt_tokens: Option<u32>,
}

/// Convert a persisted record into metadata. New sessions loaded from the
/// database are given [`SessionStatus::Sleeping`] by default; the caller can
/// override if needed (e.g. `AttachSession` sets `Inactive`).
impl From<SessionRecord> for SessionMetadata {
    fn from(record: SessionRecord) -> Self {
        // Build a SessionConfig from the record, then delegate to the
        // From<&SessionConfig> impl to avoid duplicating field mappings.
        let config = SessionConfig {
            title: record.title,
            selected_model: record.selected_model,
            reasoning_effort: record.reasoning_effort,
            parent_session_id: record.parent_session_id,
            working_dir: record.working_dir.map(PathBuf::from),
            max_turns: record.max_turns,
            created_at: record.created_at,
            context_fingerprint: None,
            context_file_paths: Vec::new(),
            context_message_index: None,
            status: SessionStatus::Sleeping,
            active_tool_groups: record.active_tool_groups.into_iter().collect(),
            context_config: record.context_config,
            account_name: record.account_name,
            accumulated_usage: record.accumulated_usage,
            context_window: record.context_window,
            last_prompt_tokens: record.last_prompt_tokens,
        };
        let mut meta = SessionMetadata::from(&config);
        // message_count is a runtime field not stored in SessionConfig,
        // so patch it from the record after the delegate conversion.
        meta.message_count = record.message_count;
        meta
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
            working_dir: meta.working_dir,
            max_turns: meta.max_turns,
            message_count: meta.message_count,
            created_at: meta.created_at,
            active_tool_groups: meta.active_tool_groups,
            context_config: ContextConfig::default(),
            account_name: meta.account_name,
            accumulated_usage: meta.accumulated_usage,
            context_window: meta.context_window,
            last_prompt_tokens: meta.last_prompt_tokens,
        }
    }
}

/// Capture a snapshot of `SessionState` as metadata for the daemon's
/// in-memory index or for sending through the command channel.
///
/// Fields that don't exist in [`SessionMetadata`] (subscribers, active
/// requests, message contents, etc.) are dropped. The `PathBuf` working_dir is
/// stringified.
impl From<&SessionState> for SessionMetadata {
    fn from(state: &SessionState) -> Self {
        // Build from SessionConfig first, then patch in the live message count.
        let mut meta = SessionMetadata::from(&state.config);
        meta.message_count = state.messages.len() as u32;
        meta
    }
}

/// Persistent configuration fields for a session.
/// Bundled to avoid duplication across snapshot/restore, metadata conversion,
/// and record persistence.
#[derive(Clone, Debug)]
pub struct SessionConfig {
    pub title: Option<String>,
    pub selected_model: Option<String>,
    pub reasoning_effort: Option<ThinkingEffort>,
    pub parent_session_id: Option<u64>,
    pub working_dir: Option<PathBuf>,
    pub max_turns: Option<u32>,
    pub created_at: i64,
    pub context_fingerprint: Option<u64>,
    pub context_file_paths: Vec<PathBuf>,
    pub context_message_index: Option<usize>,
    pub status: SessionStatus,
    pub active_tool_groups: HashSet<String>,
    pub context_config: ContextConfig,
    pub account_name: Option<String>,
    pub accumulated_usage: TokenUsage,
    pub context_window: Option<u32>,
    pub last_prompt_tokens: Option<u32>,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            title: None,
            selected_model: None,
            reasoning_effort: None,
            parent_session_id: None,
            working_dir: None,
            max_turns: None,
            created_at: 0,
            context_fingerprint: None,
            context_file_paths: Vec::new(),
            context_message_index: None,
            status: SessionStatus::Inactive,
            active_tool_groups: HashSet::new(),
            context_config: ContextConfig::default(),
            account_name: None,
            accumulated_usage: TokenUsage::default(),
            context_window: None,
            last_prompt_tokens: None,
        }
    }
}

/// Delegate conversion from [`SessionConfig`] to [`SessionMetadata`] so
/// that both `From<&SessionState>` and `From<SessionRecord>` share the
/// same field mapping.
impl From<&SessionConfig> for SessionMetadata {
    fn from(config: &SessionConfig) -> Self {
        SessionMetadata {
            title: config.title.clone(),
            selected_model: config.selected_model.clone(),
            reasoning_effort: config.reasoning_effort,
            parent_session_id: config.parent_session_id,
            working_dir: config.working_dir.as_ref().map(|p| p.display().to_string()),
            created_at: config.created_at,
            message_count: 0,
            max_turns: config.max_turns,
            status: config.status.clone(),
            active_tool_groups: config.active_tool_groups.iter().cloned().collect(),
            account_name: config.account_name.clone(),
            accumulated_usage: config.accumulated_usage,
            context_window: config.context_window,
            last_prompt_tokens: config.last_prompt_tokens,
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
        record.context_config = state.config.context_config.clone();
        record
    }
}

#[derive(Clone)]
pub struct SessionSnapshot {
    pub config: SessionConfig,
    pub messages: Vec<SessionMessage>,
}

pub(crate) struct ActiveRequest {
    pub(crate) cancel_tx: mpsc::Sender<()>,
}

pub struct ActiveSessionEntry {
    pub cmd_tx: mpsc::Sender<SessionCommand>,
    pub handle: std::thread::JoinHandle<()>,
}

pub struct SessionState {
    pub config: SessionConfig,
    pub next_message_id: u32,
    last_undo_ids: Option<Vec<u32>>,
    messages: Vec<SessionMessage>,
    subscribers: HashMap<u64, std::sync::mpsc::SyncSender<DaemonMessage>>,
    pub(crate) active_requests: HashMap<u32, ActiveRequest>,
    pub provider: Option<InferenceProvider>,
}

impl SessionState {
    /// Re-resolve context window from the catalog when the stored value
    /// is `None` (e.g. sessions created before a model was added to the
    /// catalog, or after the provider was lazily resolved on unlock).
    fn resolve_context_window_if_missing(&mut self, session_id: u64) {
        if self.config.context_window.is_some() {
            return;
        }
        let (Some(model), Some(provider)) = (&self.config.selected_model, &self.provider) else {
            return;
        };
        if let Some(cw) = provider.resolve_context_window(model) {
            debug!(
                "session {}: re-resolved context_window={} for model={}",
                session_id, cw, model
            );
            self.config.context_window = Some(cw);
        }
    }

    fn snapshot(&self) -> SessionSnapshot {
        SessionSnapshot {
            config: self.config.clone(),
            messages: self.messages.clone(),
        }
    }

    fn from_snapshot(
        snapshot: SessionSnapshot,
        subscribers: HashMap<u64, std::sync::mpsc::SyncSender<DaemonMessage>>,
    ) -> Self {
        let msg_count = snapshot.messages.len() as u32;
        Self {
            config: snapshot.config,
            next_message_id: msg_count,
            last_undo_ids: None,
            messages: snapshot.messages,
            subscribers,
            active_requests: HashMap::new(),
            provider: None,
        }
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

    /// Find the parent message that a new message of this `kind` should link
    /// to.  The parent is the most recent non-deleted ancestor in the
    /// conversation tree: UserText for assistant messages, the originating
    /// AssistantToolUse for tool results and displayed images.
    fn resolve_parent_id(&self, kind: &SessionMessageKind) -> Option<u32> {
        match kind {
            SessionMessageKind::SystemText { .. } | SessionMessageKind::UserText { .. } => None,
            SessionMessageKind::AssistantText { .. }
            | SessionMessageKind::AssistantToolUse { .. } => {
                self.messages.iter().rev().find_map(|m| {
                    if !m.deleted && matches!(m.kind, SessionMessageKind::UserText { .. }) {
                        Some(m.message_id)
                    } else {
                        None
                    }
                })
            }
            SessionMessageKind::ToolResult { call_id, .. } => {
                self.messages.iter().rev().find_map(|m| {
                    if !m.deleted
                        && let SessionMessageKind::AssistantToolUse { tool_calls, .. } = &m.kind
                        && tool_calls.iter().any(|tc| tc.call_id == *call_id)
                    {
                        Some(m.message_id)
                    } else {
                        None
                    }
                })
            }
            SessionMessageKind::DisplayedImage(record) => {
                if let Some(call_id) = &record.tool_call_id {
                    self.messages.iter().rev().find_map(|m| {
                        if !m.deleted
                            && let SessionMessageKind::AssistantToolUse { tool_calls, .. } = &m.kind
                            && tool_calls.iter().any(|tc| tc.call_id == *call_id)
                        {
                            Some(m.message_id)
                        } else {
                            None
                        }
                    })
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    /// Push a message with a monotonically increasing id and resolved parent.
    /// Returns the assigned `message_id` and the message so callers can
    /// persist and broadcast without re-indexing into `self.messages`.
    ///
    /// Appending a `UserText` resets the redo stack — new user input after
    /// an undo starts a fresh editing session.
    pub fn append_message(&mut self, kind: SessionMessageKind) -> (u32, SessionMessage) {
        // New user input after an undo clears the redo opportunity.
        if matches!(&kind, SessionMessageKind::UserText { .. }) {
            self.last_undo_ids = None;
        }
        let parent_id = self.resolve_parent_id(&kind);
        let id = self.next_message_id;
        self.next_message_id += 1;
        let msg = SessionMessage {
            message_id: id,
            parent_id,
            created_at: TimestampMs::now(),
            kind,
            deleted: false,
        };
        self.messages.push(msg.clone());
        (id, msg)
    }

    /// Push a message at a caller-specified id, used to keep ToolResult ids in
    /// sync with the `ToolCallStarted` predictions that clients already received.
    /// Returns the message so callers can persist without re-indexing.
    pub fn append_message_with_id(&mut self, kind: SessionMessageKind, id: u32) -> SessionMessage {
        let parent_id = self.resolve_parent_id(&kind);
        let msg = SessionMessage {
            message_id: id,
            parent_id,
            created_at: TimestampMs::now(),
            kind,
            deleted: false,
        };
        self.messages.push(msg.clone());
        self.next_message_id = self.next_message_id.max(id + 1);
        msg
    }

    /// Undo the most recent user turn: mark the subtree as deleted and record
    /// which message IDs were removed so that `/redo` can restore exactly them.
    /// Uses a HashSet for O(1) membership checks instead of O(n) per ID.
    pub fn undo(&mut self) -> Option<Vec<u32>> {
        let user_idx = self
            .messages
            .iter()
            .rposition(|m| !m.deleted && matches!(m.kind, SessionMessageKind::UserText { .. }))?;
        let root_id = self.messages[user_idx].message_id;
        // Build the children map once and reuse across traversal so we don't
        // scan all messages again inside collect_subtree.
        let children = build_children_map(&self.messages);
        let ids = collect_subtree(&children, root_id);
        // One pass through messages + O(1) set lookups beats O(n) per ID.
        let delete_set: HashSet<u32> = ids.iter().copied().collect();
        for msg in self.messages.iter_mut() {
            if delete_set.contains(&msg.message_id) {
                msg.deleted = true;
            }
        }
        self.last_undo_ids = Some(ids.clone());
        Some(ids)
    }

    /// Redo the most recent `/undo`, restoring exactly the message IDs that
    /// were removed.  Returns `None` if there is nothing to redo (either no
    /// prior undo, or a new UserText was appended after the undo).
    pub fn redo(&mut self) -> Option<Vec<SessionMessage>> {
        let ids = self.last_undo_ids.take()?;
        // One pass through messages + O(1) set lookups beats O(n) per ID.
        let redo_set: HashSet<u32> = ids.iter().copied().collect();
        let mut restored = Vec::new();
        for msg in self.messages.iter_mut() {
            if redo_set.contains(&msg.message_id) {
                msg.deleted = false;
                restored.push(msg.clone());
            }
        }
        Some(restored)
    }

    /// Create an empty session state.
    pub fn empty() -> Self {
        Self {
            config: SessionConfig::default(),
            next_message_id: 0,
            last_undo_ids: None,
            messages: Vec::new(),
            subscribers: HashMap::new(),
            active_requests: HashMap::new(),
            provider: None,
        }
    }
}

/// Index the message list into a parent→children map so that
/// subtree traversal (used by undo) is O(n) rather than O(n²).
fn build_children_map(messages: &[SessionMessage]) -> HashMap<u32, Vec<u32>> {
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for msg in messages {
        if let Some(pid) = msg.parent_id {
            children.entry(pid).or_default().push(msg.message_id);
        }
    }
    children
}

/// Walk the message tree breadth-first from `root_id` returning every
/// descendant's message ID.  The caller supplies a pre-built adjacency
/// map (from `build_children_map`) so that each edge is followed exactly
/// once — O(n) overall.
fn collect_subtree(children: &HashMap<u32, Vec<u32>>, root_id: u32) -> Vec<u32> {
    let mut result = vec![root_id];
    let mut i = 0;
    while i < result.len() {
        let pid = result[i];
        if let Some(kids) = children.get(&pid) {
            result.extend(kids);
        }
        i += 1;
    }
    result
}

fn broadcast(
    subscribers: &mut HashMap<u64, std::sync::mpsc::SyncSender<DaemonMessage>>,
    message: DaemonMessage,
) {
    subscribers.retain(|client_id, tx| {
        match tx.try_send(message.clone()) {
            Ok(()) => true,
            Err(mpsc::TrySendError::Full(_)) => {
                // Subscriber is too slow to keep up — drop this message.
                // For streaming chunks this is acceptable since the final
                // ToolCallFinished + SessionMessageAppended(ToolResult)
                // deliver the complete content.  Other broadcast messages
                // (status, metadata) are also not critical enough to block
                // the session thread.
                debug!("broadcast dropped message for subscriber {client_id}: buffer full");
                true // keep the subscriber, it may catch up
            }
            Err(mpsc::TrySendError::Disconnected(_)) => {
                warn!("removing disconnected subscriber {client_id}");
                false
            }
        }
    });
}

fn fail_request(
    subscribers: &mut HashMap<u64, std::sync::mpsc::SyncSender<DaemonMessage>>,
    request_id: u32,
    error: impl Into<String>,
) -> bool {
    broadcast(
        subscribers,
        DaemonMessage::Started {
            request_id,
            message_id: 0,
        },
    );
    broadcast(
        subscribers,
        DaemonMessage::Failed {
            request_id,
            error: error.into(),
        },
    );
    false
}

pub fn session_main(
    rx: std::sync::mpsc::Receiver<SessionCommand>,
    provider: Option<InferenceProvider>,
    account_name: Option<String>,
    init_record: Option<SessionRecord>,
    ctx: RequestContext,
) {
    let config = SessionConfig {
        title: init_record.as_ref().and_then(|r| r.title.clone()),
        selected_model: init_record.as_ref().and_then(|r| r.selected_model.clone()),
        reasoning_effort: init_record.as_ref().and_then(|r| r.reasoning_effort),
        parent_session_id: init_record.as_ref().and_then(|r| r.parent_session_id),
        working_dir: init_record
            .as_ref()
            .and_then(|r| r.working_dir.as_ref().map(PathBuf::from)),
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
        accumulated_usage: init_record
            .as_ref()
            .map(|r| r.accumulated_usage)
            .unwrap_or_default(),
        context_window: init_record.as_ref().and_then(|r| r.context_window),
        last_prompt_tokens: init_record.as_ref().and_then(|r| r.last_prompt_tokens),
    };
    let mut state = SessionState {
        config,
        provider,
        ..SessionState::empty()
    };

    // Re-resolve context window from the catalog when loading an existing
    // session whose stored context_window is None (e.g. sessions created
    // before a model was added to the catalog).
    state.resolve_context_window_if_missing(ctx.session_id);

    match db::read_messages(&ctx.db, ctx.session_id) {
        Ok(msgs) => {
            state.messages = msgs;
            state.next_message_id = state.messages.len() as u32;
        }
        Err(e) => warn!(ctx.session_id, error = %e, "failed to load messages from DB"),
    }

    if init_record.is_none() || state.messages.is_empty() {
        let effective_working_dir = state
            .config
            .working_dir
            .clone()
            .unwrap_or_else(|| PathBuf::from("."));
        let skills = context::discover_skills(&effective_working_dir);
        let base_prompt = context::build_base_prompt(&skills, &ctx.tool_registry.groups());
        let (id0, msg0) = state.append_message(SessionMessageKind::SystemText {
            content: base_prompt,
        });
        write_message_retry(&ctx.db, ctx.session_id, id0, &msg0).ok();

        if let Ok(bundle) = context::discover_context(&effective_working_dir, &Default::default()) {
            let context_str = context::assemble_context(&bundle);
            if !context_str.is_empty() {
                let (id1, msg1) = state.append_message(SessionMessageKind::SystemText {
                    content: context_str,
                });
                write_message_retry(&ctx.db, ctx.session_id, id1, &msg1).ok();
                state.config.context_fingerprint = Some(bundle.fingerprint);
                state.config.context_file_paths =
                    bundle.files.iter().map(|f| f.path.clone()).collect();
                state.config.context_message_index = Some(1);
            }
        }
    }

    let _ = ctx.daemon_tx.send(DaemonCommand::UpdateMetadata {
        session_id: ctx.session_id,
        metadata: SessionMetadata::from(&state),
    });

    info!("session {} started", ctx.session_id);

    let mut shutdown_requested = false;
    while let Ok(cmd) = rx.recv() {
        if process_command(cmd, &mut state, &mut shutdown_requested, &ctx) {
            break;
        }
    }

    info!("session {} exiting", ctx.session_id);
    persist_and_exit(&state, &ctx.db, ctx.session_id, &ctx.daemon_tx);
}

fn process_command(
    cmd: SessionCommand,
    state: &mut SessionState,
    shutdown_requested: &mut bool,
    ctx: &RequestContext,
) -> bool {
    match cmd {
        SessionCommand::RunInput { request_id, input } => {
            handle_run_input(request_id, input, state, shutdown_requested, ctx)
        }
        SessionCommand::RunChildInput { request_id, reply } => {
            handle_run_child_input(request_id, reply, state, shutdown_requested, ctx)
        }
        SessionCommand::Cancel { request_id } => handle_cancel(request_id, state, ctx),
        SessionCommand::SetModel { model } => handle_set_model(model, state, ctx),
        SessionCommand::StatusChanged(new_status) => handle_status_changed(new_status, state, ctx),
        SessionCommand::Attach { client_id, tx } => handle_attach(client_id, tx, state, ctx),
        SessionCommand::Detach { client_id } => {
            handle_detach(client_id, state, shutdown_requested, ctx)
        }
        SessionCommand::GetSummary { reply } => handle_get_summary(reply, state, ctx),
        SessionCommand::AppendMessage { message } => handle_append_message(message, state, ctx),
        SessionCommand::RequestFinished {
            request_id,
            snapshot,
        } => handle_request_finished(request_id, snapshot, state, shutdown_requested, ctx),
        SessionCommand::Broadcast(message) => handle_broadcast(message, state, ctx),
        SessionCommand::SetAccount { name } => handle_set_account(name, state, ctx),
        SessionCommand::SetReasoningEffort { effort } => {
            handle_set_reasoning_effort(effort, state, ctx)
        }
        SessionCommand::GetReasoningEffort { reply } => {
            handle_get_reasoning_effort(reply, state, ctx)
        }
        SessionCommand::Undo => handle_undo(state, ctx),
        SessionCommand::Redo => handle_redo(state, ctx),
        SessionCommand::Shutdown => handle_shutdown(state, shutdown_requested),
    }
}

// ── SessionCommand handler functions ─────────────────────────────────────────

/// Process a user input: validate, resolve provider, spawn a request worker.
fn handle_run_input(
    request_id: u32,
    input: Vec<u8>,
    state: &mut SessionState,
    shutdown_requested: &mut bool,
    ctx: &RequestContext,
) -> bool {
    debug!("session {}: RunInput id={}", ctx.session_id, request_id);
    let text = String::from_utf8_lossy(&input).trim().to_string();
    info!(
        session_id = ctx.session_id,
        input_len = text.len(),
        input_preview = %text.chars().take(120).collect::<String>(),
        "session received input",
    );
    if text.is_empty() {
        return fail_request(&mut state.subscribers, request_id, "empty input");
    }
    let provider = if let Some(p) = state.provider.as_ref() {
        p.clone()
    } else if let Some(ref name) = state.config.account_name {
        // No cached provider yet — try lazy resolution via the daemon.
        let (reply, rx) = mpsc::channel();
        let _ = ctx.daemon_tx.send(DaemonCommand::ResolveProviderCmd {
            account: name.clone(),
            reply,
        });
        match rx.recv() {
            Ok(Some(provider)) => {
                state.provider = Some(provider);
                // Re-resolve context window now that the provider is
                // available (e.g. after unlocking the daemon).
                state.resolve_context_window_if_missing(ctx.session_id);
                let Some(p) = state.provider.as_ref() else {
                    return fail_request(
                        &mut state.subscribers,
                        request_id,
                        "internal error: provider not set after resolution".to_string(),
                    );
                };
                p.clone()
            }
            _ => {
                return fail_request(
                    &mut state.subscribers,
                    request_id,
                    format!(
                        "no credential stored for account '{name}' — add one via the AI Providers page or /add-key"
                    ),
                );
            }
        }
    } else {
        return fail_request(
            &mut state.subscribers,
            request_id,
            "no account configured on this session — use /account <name> to set one",
        );
    };
    let model = match &state.config.selected_model {
        Some(m) => m.clone(),
        None => {
            return fail_request(&mut state.subscribers, request_id, "no model selected");
        }
    };
    if *shutdown_requested {
        return fail_request(
            &mut state.subscribers,
            request_id,
            "session is shutting down",
        );
    }
    if !state.active_requests.is_empty() {
        return fail_request(
            &mut state.subscribers,
            request_id,
            "session already has an active request",
        );
    }

    let (msg_id, user_msg) = state.append_message(SessionMessageKind::UserText {
        content: text.clone(),
    });
    write_message_retry(&ctx.db, ctx.session_id, msg_id, &user_msg).ok();
    broadcast(
        &mut state.subscribers,
        DaemonMessage::SessionMessageAppended { message: user_msg },
    );

    broadcast(
        &mut state.subscribers,
        DaemonMessage::Started {
            request_id,
            message_id: state.next_message_id,
        },
    );
    let (cancel_tx, cancel_rx) = mpsc::channel::<()>();
    state
        .active_requests
        .insert(request_id, ActiveRequest { cancel_tx });

    // Workers don't need their own subscriber map — all broadcasts
    // are routed through SessionCommand::Broadcast to this main
    // session thread which holds the live subscriber set.
    let mut worker_session = SessionState::from_snapshot(state.snapshot(), HashMap::new());
    let ctx = ctx.clone();
    std::thread::spawn(move || {
        let _ = run_request_worker(
            request_id,
            provider,
            &mut worker_session,
            model,
            cancel_rx,
            ctx,
            None,
        );
    });
    false
}

/// Run the agent loop on a pre-populated child session and return the result.
///
/// The caller is responsible for injecting any prompt into the session via
/// [`SessionCommand::AppendMessage`] before sending this command — this
/// command only triggers the agent loop on whatever messages are already
/// queued. The response is delivered through the `reply` channel.
fn handle_run_child_input(
    request_id: u32,
    reply: std::sync::mpsc::Sender<io::Result<ChildResult>>,
    state: &mut SessionState,
    shutdown_requested: &mut bool,
    ctx: &RequestContext,
) -> bool {
    let Some(provider) = state.provider.as_ref() else {
        let _ = reply.send(Err(io::Error::other("daemon locked")));
        return false;
    };
    let model = state.config.selected_model.clone().unwrap_or_default();
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
    broadcast(
        &mut state.subscribers,
        DaemonMessage::Started {
            request_id,
            message_id: state.next_message_id,
        },
    );
    let (cancel_tx, cancel_rx) = mpsc::channel::<()>();
    state
        .active_requests
        .insert(request_id, ActiveRequest { cancel_tx });
    let mut worker_session = SessionState::from_snapshot(state.snapshot(), HashMap::new());
    let ctx = ctx.clone();
    let provider = provider.clone();
    std::thread::spawn(move || {
        let result = run_request_worker(
            request_id,
            provider,
            &mut worker_session,
            model,
            cancel_rx,
            ctx,
            Some(reply),
        );
        let _ = result;
    });
    false
}

/// Cancel an active request by sending on its cancel channel.
fn handle_cancel(request_id: u32, state: &mut SessionState, ctx: &RequestContext) -> bool {
    let _ = ctx;
    if let Some(active) = state.active_requests.get(&request_id) {
        let _ = active.cancel_tx.send(());
        broadcast(
            &mut state.subscribers,
            DaemonMessage::Cancelled { request_id },
        );
    }
    false
}

/// Set the model for this session and broadcast the change.
/// Rejects invalid model names by broadcasting `ModelSelectionFailed`
/// instead of mutating state.
fn handle_set_model(model: String, state: &mut SessionState, ctx: &RequestContext) -> bool {
    info!("session {}: SetModel model={}", ctx.session_id, model);

    // Validate the model against the provider's model list before accepting it.
    if let Err(msg) = validate_model_via_daemon(&model, ctx) {
        warn!(
            "session {}: model '{model}' rejected: {msg}",
            ctx.session_id
        );
        broadcast(
            &mut state.subscribers,
            DaemonMessage::ModelSelectionFailed { model, error: msg },
        );
        return false;
    }

    state.config.selected_model = Some(model.clone());
    let cw = state
        .provider
        .as_ref()
        .and_then(|p| p.resolve_context_window(&model));
    debug!(
        "session {}: resolved context_window={:?} for model={}",
        ctx.session_id, cw, model
    );
    state.config.context_window = cw;
    debug!(
        "session {}: broadcasting ModelSelected model={}",
        ctx.session_id, model
    );
    broadcast(
        &mut state.subscribers,
        DaemonMessage::ModelSelected {
            model: model.clone(),
        },
    );
    let _ = ctx.daemon_tx.send(DaemonCommand::UpdateMetadata {
        session_id: ctx.session_id,
        metadata: SessionMetadata::from(&*state),
    });
    false
}

/// Ask the daemon whether `model` is valid for this session's account.
/// Returns `Ok(())` if valid, `Err(reason)` if invalid.
/// If the daemon is unreachable or the model list is unavailable the
/// model is allowed through (`Ok(())`).
fn validate_model_via_daemon(model: &str, ctx: &RequestContext) -> Result<(), String> {
    let (reply, rx) = mpsc::channel();
    if ctx
        .daemon_tx
        .send(DaemonCommand::ValidateModel {
            session_id: ctx.session_id,
            model: model.to_string(),
            reply,
        })
        .is_err()
    {
        warn!(
            "session {}: daemon disconnected during model validation for '{model}'",
            ctx.session_id
        );
        return Ok(());
    }

    match rx.recv() {
        Ok(Ok(())) => Ok(()),
        Ok(Err(msg)) => Err(msg),
        Err(_) => {
            warn!(
                "session {}: daemon disconnected while waiting for model validation \
                 of '{model}', allowing through",
                ctx.session_id
            );
            Ok(())
        }
    }
}

/// Update the session status and broadcast to subscribers and daemon.
fn handle_status_changed(
    new_status: SessionStatus,
    state: &mut SessionState,
    ctx: &RequestContext,
) -> bool {
    state.config.status = new_status.clone();
    broadcast(
        &mut state.subscribers,
        DaemonMessage::SessionStatusChanged {
            session_id: ctx.session_id,
            status: new_status.clone(),
        },
    );
    let _ = ctx.daemon_tx.send(DaemonCommand::BroadcastSessionStatus {
        session_id: ctx.session_id,
        status: new_status,
    });
    false
}

/// Attach a client to this session, sending the full session state snapshot.
fn handle_attach(
    client_id: u64,
    tx: std::sync::mpsc::SyncSender<DaemonMessage>,
    state: &mut SessionState,
    ctx: &RequestContext,
) -> bool {
    info!("session {}: client {} attached", ctx.session_id, client_id);
    state.subscribers.insert(client_id, tx);
    let snapshot = DaemonMessage::SessionState {
        session_id: ctx.session_id,
        title: state.config.title.clone(),
        selected_model: state.config.selected_model.clone(),
        parent_session_id: state.config.parent_session_id,
        working_dir: state
            .config
            .working_dir
            .as_ref()
            .map(|p| p.display().to_string()),
        max_turns: state.config.max_turns,
        messages: state.messages.clone(),
        active_tool_groups: state.config.active_tool_groups.iter().cloned().collect(),
        token_usage: Some(state.config.accumulated_usage),
        context_window: state.config.context_window,
        last_prompt_tokens: state.config.last_prompt_tokens,
        status: state.config.status.clone(),
    };
    if let Some(tx) = state.subscribers.get(&client_id) {
        let _ = tx.send(snapshot);
    }
    false
}

/// Detach a client from this session.
fn handle_detach(
    client_id: u64,
    state: &mut SessionState,
    shutdown_requested: &bool,
    ctx: &RequestContext,
) -> bool {
    info!("session {}: client {} detached", ctx.session_id, client_id);
    let _ = ctx;
    state.subscribers.remove(&client_id);
    state.active_requests.is_empty() && (state.subscribers.is_empty() || *shutdown_requested)
}

/// Return a SessionSummary for this session via the reply channel.
fn handle_get_summary(
    reply: std::sync::mpsc::Sender<SessionSummary>,
    state: &SessionState,
    ctx: &RequestContext,
) -> bool {
    let _ = reply.send(SessionSummary {
        session_id: ctx.session_id,
        title: state.config.title.clone(),
        selected_model: state.config.selected_model.clone(),
        reasoning_effort: state.config.reasoning_effort,
        parent_session_id: state.config.parent_session_id,
        working_dir: state
            .config
            .working_dir
            .as_ref()
            .map(|p| p.display().to_string()),
        created_at: state.config.created_at,
        message_count: state.messages.len() as u32,
        max_turns: state.config.max_turns,
        status: state.config.status.clone(),
        active_tool_groups: state.config.active_tool_groups.iter().cloned().collect(),
        account_name: state.config.account_name.clone(),
        token_usage: Some(state.config.accumulated_usage),
        context_window: state.config.context_window,
        last_prompt_tokens: state.config.last_prompt_tokens,
    });
    false
}

/// Persist an incoming message.
///
/// When the caller supplies a fully-formed `SessionMessage` (message_id != 0)
/// its metadata — timestamps and ids from the ACP layer — are kept intact and
/// only the parent_id is re-resolved into the live tree so that undo can
/// navigate the parent→child chain.  Otherwise a fresh id is assigned.
fn handle_append_message(
    message: SessionMessage,
    state: &mut SessionState,
    ctx: &RequestContext,
) -> bool {
    // Use the caller-provided message_id if non-zero, otherwise assign one.
    // This preserves caller metadata (e.g. timestamps from ACP) while still
    // integrating with the daemon's message_id ordering scheme.
    let msg = if message.message_id != 0 {
        // Re-parent the caller-supplied message into the live tree so that
        // undo can navigate the parent → child chain correctly.
        let parent_id = state.resolve_parent_id(&message.kind);
        let msg = SessionMessage {
            parent_id,
            ..message
        };
        state.messages.push(msg.clone());
        state.next_message_id = state.next_message_id.max(msg.message_id + 1);
        msg
    } else {
        let (_id, msg) = state.append_message(message.kind);
        msg
    };
    write_message_retry(&ctx.db, ctx.session_id, msg.message_id, &msg).ok();
    false
}

/// Apply the worker's snapshot (config only) and append any new messages.
fn handle_request_finished(
    request_id: u32,
    snapshot: SessionSnapshot,
    state: &mut SessionState,
    shutdown_requested: &bool,
    ctx: &RequestContext,
) -> bool {
    let pre_len = state.messages.len();

    // Apply config changes from worker (accumulated usage, etc.)
    state.config = snapshot.config;

    // Refresh pre-existing messages that may have been updated by the worker
    // (e.g. context refresh replacing a system message in-place)
    for (idx, snap_msg) in snapshot.messages.iter().enumerate().take(pre_len) {
        if idx < state.messages.len() && state.messages[idx] != *snap_msg {
            state.messages[idx] = snap_msg.clone();
        }
    }

    // Append new messages preserving the worker's message_ids so that
    // the client's message_id-based ordering remains consistent with the
    // ToolCallStarted predictions that were already broadcast.
    let snapshot_msg_count = snapshot.messages.len();
    if snapshot_msg_count < pre_len {
        warn!(
            session_id = ctx.session_id,
            pre_len,
            snapshot_msg_count,
            "handle_request_finished: snapshot had fewer messages than expected",
        );
    }
    for i in pre_len..snapshot_msg_count {
        let msg = snapshot.messages[i].clone();
        let id = msg.message_id;
        if !matches!(msg.kind, SessionMessageKind::AssistantToolUse { .. }) {
            write_message_retry(&ctx.db, ctx.session_id, id, &msg).ok();
        }
        state.messages.push(msg);
        state.next_message_id = state.next_message_id.max(id + 1);
    }

    // Broadcast new messages to subscribers (skip DisplayedImage and
    // AssistantToolUse — already broadcast mid-turn).
    for i in pre_len..snapshot_msg_count {
        if i >= state.messages.len() {
            break;
        }
        let msg = &state.messages[i];
        if matches!(
            msg.kind,
            SessionMessageKind::DisplayedImage(_) | SessionMessageKind::AssistantToolUse { .. }
        ) {
            continue;
        }
        broadcast(
            &mut state.subscribers,
            DaemonMessage::SessionMessageAppended {
                message: msg.clone(),
            },
        );
    }

    state.active_requests.remove(&request_id);
    state.config.status = SessionStatus::Inactive;
    let _ = ctx.daemon_tx.send(DaemonCommand::UpdateMetadata {
        session_id: ctx.session_id,
        metadata: SessionMetadata::from(&*state),
    });
    broadcast(
        &mut state.subscribers,
        DaemonMessage::SessionStatusChanged {
            session_id: ctx.session_id,
            status: SessionStatus::Inactive,
        },
    );
    let _ = ctx.daemon_tx.send(DaemonCommand::BroadcastSessionStatus {
        session_id: ctx.session_id,
        status: SessionStatus::Inactive,
    });
    state.active_requests.is_empty() && (state.subscribers.is_empty() || *shutdown_requested)
}

/// Broadcast a message through the live subscriber map.
fn handle_broadcast(
    message: DaemonMessage,
    state: &mut SessionState,
    ctx: &RequestContext,
) -> bool {
    let _ = ctx;
    // Broadcast through the main session thread's live subscriber
    // map so that in-flight worker broadcasts respect detach.
    broadcast(&mut state.subscribers, message);
    false
}

/// Set the account for this session and try to resolve its provider.
fn handle_set_account(name: String, state: &mut SessionState, ctx: &RequestContext) -> bool {
    info!("session {}: SetAccount account={}", ctx.session_id, name);
    // Try to resolve the provider from the daemon by name.
    let (reply, rx) = mpsc::channel();
    let _ = ctx.daemon_tx.send(DaemonCommand::ResolveProviderCmd {
        account: name.clone(),
        reply,
    });
    if let Ok(Some(provider)) = rx.recv() {
        // Re-resolve context window if a model is already selected.
        if let Some(ref model) = state.config.selected_model {
            let cw = provider.resolve_context_window(model);
            debug!(
                "session {}: re-resolved context_window={:?} after account change for model={}",
                ctx.session_id, cw, model
            );
            state.config.context_window = cw;
        }
        state.provider = Some(provider);
    }
    // Always store the account name on the session, even if the
    // provider wasn't resolvable yet (e.g. no credential stored,
    // or daemon hasn't unlocked).  The provider can be resolved
    // lazily when RunInput is called.  This way the user can set
    // an account on a session before unlocking.
    state.config.account_name = Some(name.clone());
    broadcast(
        &mut state.subscribers,
        DaemonMessage::SessionAccountSet { account: name },
    );
    let _ = ctx.daemon_tx.send(DaemonCommand::UpdateMetadata {
        session_id: ctx.session_id,
        metadata: SessionMetadata::from(&*state),
    });
    false
}

/// Set the reasoning effort for this session, validating against the model.
fn handle_set_reasoning_effort(
    effort: ThinkingEffort,
    state: &mut SessionState,
    ctx: &RequestContext,
) -> bool {
    info!(
        session_id = ctx.session_id,
        effort = %effort.as_label(),
        "setting reasoning effort"
    );

    // Check if the current model supports reasoning
    let supported = if let (Some(model), Some(provider)) = (
        state.config.selected_model.as_ref(),
        state.provider.as_ref(),
    ) {
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
        state.config.reasoning_effort = Some(effort);
        debug!(
            session_id = ctx.session_id,
            effort = %effort.as_label(),
            "reasoning effort stored"
        );
        broadcast(
            &mut state.subscribers,
            DaemonMessage::ReasoningEffortSet { effort },
        );
    } else {
        let model = state.config.selected_model.as_deref().unwrap_or("(none)");
        let msg = format!(
            "model '{}' does not support reasoning effort '{}'",
            model,
            effort.as_label(),
        );
        warn!(session_id = ctx.session_id, error = %msg, "reasoning effort rejected");
        broadcast(
            &mut state.subscribers,
            DaemonMessage::ReasoningEffortSetFailed {
                effort: effort.as_label().to_string(),
                error: msg,
            },
        );
    }
    false
}

/// Return the current reasoning effort via the reply channel.
fn handle_get_reasoning_effort(
    reply: mpsc::Sender<ThinkingEffort>,
    state: &SessionState,
    ctx: &RequestContext,
) -> bool {
    let _ = ctx;
    let current = state.config.reasoning_effort.unwrap_or(ThinkingEffort::Off);
    let _ = reply.send(current);
    false
}

/// Handle Undo: mark the most recent user turn's subtree as deleted.
/// Uses a quick-reference HashMap to avoid an O(n) scan per ID.
fn handle_undo(state: &mut SessionState, ctx: &RequestContext) -> bool {
    let Some(message_ids) = state.undo() else {
        debug!(
            session_id = ctx.session_id,
            "undo requested but no user turn to undo",
        );
        return false;
    };
    info!(
        session_id = ctx.session_id,
        msg_count = message_ids.len(),
        "undo: marked subtree as deleted",
    );
    // Build a single-pass index so persistence lookups are O(1) per ID.
    let msg_map: HashMap<u32, &SessionMessage> =
        state.messages.iter().map(|m| (m.message_id, m)).collect();
    for &id in &message_ids {
        if let Some(msg) = msg_map.get(&id) {
            write_message_retry(&ctx.db, ctx.session_id, id, msg).ok();
        }
    }
    broadcast(
        &mut state.subscribers,
        DaemonMessage::SessionMessagesUndone { message_ids },
    );
    false
}

/// Reinstate the subtree that was hidden by the preceding undo,
/// persisting the restored state so it survives daemon restart.
fn handle_redo(state: &mut SessionState, ctx: &RequestContext) -> bool {
    let Some(messages) = state.redo() else {
        debug!(
            session_id = ctx.session_id,
            "redo requested but nothing to redo (no prior undo, or new input after undo)",
        );
        return false;
    };
    info!(
        session_id = ctx.session_id,
        msg_count = messages.len(),
        "redo: restored previously-undone messages",
    );
    for msg in &messages {
        write_message_retry(&ctx.db, ctx.session_id, msg.message_id, msg).ok();
    }
    broadcast(
        &mut state.subscribers,
        DaemonMessage::SessionMessagesRedone { messages },
    );
    false
}

/// Signal shutdown: cancel all active requests and check if the loop should exit.
fn handle_shutdown(state: &mut SessionState, shutdown_requested: &mut bool) -> bool {
    *shutdown_requested = true;
    for (&request_id, active) in &state.active_requests {
        let _ = active.cancel_tx.send(());
        broadcast(
            &mut state.subscribers,
            DaemonMessage::Cancelled { request_id },
        );
    }
    state.active_requests.is_empty()
}

#[allow(clippy::too_many_arguments)]
fn run_request_worker(
    request_id: u32,
    client: InferenceProvider,
    session: &mut SessionState,
    model: String,
    cancel_rx: mpsc::Receiver<()>,
    ctx: RequestContext,
    child_reply: Option<mpsc::Sender<io::Result<ChildResult>>>,
) -> io::Result<()> {
    let request_start = std::time::Instant::now();
    let initial_snapshot = session.snapshot();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run_agent_loop(&client, session, &model, request_id, &cancel_rx, &ctx)
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
            info!(session_id = ctx.session_id, request_id, "request completed");
            // Route through the main session thread so detach is respected.
            // Include the worker's accumulated token usage so subscribers
            // (e.g. the TUI) can show per-request token counts.
            let usage = &session.config.accumulated_usage;
            debug!(
                session_id = ctx.session_id,
                request_id,
                input_tokens = usage.input_tokens,
                output_tokens = usage.output_tokens,
                total_tokens = usage.total_tokens,
                "broadcasting Done with accumulated token usage"
            );
            let _ = ctx
                .cmd_tx
                .send(SessionCommand::Broadcast(DaemonMessage::Done {
                    request_id,
                    token_usage: Some(*usage),
                    last_prompt_tokens: session.config.last_prompt_tokens,
                }));
        }
        RequestOutcome::Failed(error) => {
            info!(session_id = ctx.session_id, request_id, error = %error, "request failed");
            // Route through the main session thread so detach is respected.
            let _ = ctx
                .cmd_tx
                .send(SessionCommand::Broadcast(DaemonMessage::Failed {
                    request_id,
                    error: error.to_string(),
                }));
        }
        RequestOutcome::Cancelled => {
            info!(session_id = ctx.session_id, request_id, "request cancelled");
        }
    }

    if let Some(reply) = child_reply {
        let child_result = match &outcome {
            RequestOutcome::Done => {
                let output = session
                    .messages
                    .iter()
                    .filter_map(|m| match &m.kind {
                        SessionMessageKind::AssistantText { content, .. } => Some(content.clone()),
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

    let _ = ctx.cmd_tx.send(SessionCommand::RequestFinished {
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
    use crate::server::connection::SUBSCRIBER_CHANNEL_CAPACITY;
    use crate::tools::ToolRegistry;
    use std::collections::HashMap;
    use tai_proto::{
        AssistantToolCallRecord, DisplayedImageRecord, ImageMetadata, OutputStream, SessionMessage,
        SessionStatus,
    };
    use tempfile::tempdir;

    fn test_record() -> SessionRecord {
        SessionRecord {
            title: Some("test session".into()),
            selected_model: Some("gpt-4".into()),
            reasoning_effort: None,
            parent_session_id: None,
            working_dir: Some("/tmp".into()),
            max_turns: Some(10),
            message_count: 3,
            created_at: 1000,
            active_tool_groups: vec!["core".into(), "shell".into()],
            context_config: ContextConfig::default(),
            account_name: None,
            accumulated_usage: TokenUsage::default(),
            context_window: None,
            last_prompt_tokens: None,
        }
    }

    fn test_state() -> SessionState {
        SessionState {
            config: SessionConfig {
                title: Some("test session".into()),
                selected_model: Some("gpt-4".into()),
                reasoning_effort: None,
                parent_session_id: None,
                working_dir: Some(std::path::PathBuf::from("/tmp")),
                max_turns: Some(10),
                created_at: 1000,
                context_fingerprint: None,
                context_file_paths: Vec::new(),
                context_message_index: None,
                status: SessionStatus::Inactive,
                active_tool_groups: ["core".into(), "shell".into()].into(),
                context_config: ContextConfig::default(),
                account_name: None,
                accumulated_usage: TokenUsage::default(),
                context_window: None,
                last_prompt_tokens: None,
            },
            next_message_id: 3,
            messages: vec![
                SessionMessage::now(SessionMessageKind::SystemText {
                    content: "prompt".into(),
                }),
                SessionMessage::now(SessionMessageKind::UserText {
                    content: "hello".into(),
                }),
                SessionMessage::now(SessionMessageKind::AssistantText {
                    content: "hi".into(),
                    reasoning: None,
                    token_usage: None,
                }),
            ],
            subscribers: HashMap::new(),
            last_undo_ids: None,
            active_requests: HashMap::new(),
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
        assert_eq!(meta.working_dir, record.working_dir);
        assert_eq!(meta.message_count, record.message_count);
        let mut expected = record.active_tool_groups.clone();
        let mut actual = meta.active_tool_groups.clone();
        expected.sort();
        actual.sort();
        assert_eq!(expected, actual);
    }

    #[test]
    fn session_metadata_to_record() {
        let meta = SessionMetadata {
            title: Some("meta title".into()),
            selected_model: Some("claude-3".into()),
            reasoning_effort: None,
            parent_session_id: Some(42),
            working_dir: Some("/home".into()),
            created_at: 2000,
            message_count: 7,
            max_turns: Some(20),
            status: SessionStatus::Inactive,
            active_tool_groups: vec!["git".into()],
            account_name: None,
            accumulated_usage: TokenUsage::default(),
            context_window: None,
            last_prompt_tokens: None,
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
        assert_eq!(record.working_dir, record2.working_dir);
        assert_eq!(record.max_turns, record2.max_turns);
        assert_eq!(record.message_count, record2.message_count);
        assert_eq!(record.created_at, record2.created_at);
        let mut expected = record.active_tool_groups.clone();
        let mut actual = record2.active_tool_groups.clone();
        expected.sort();
        actual.sort();
        assert_eq!(expected, actual);
    }

    #[test]
    fn session_state_to_metadata() {
        let state = test_state();
        let meta: SessionMetadata = (&state).into();
        assert_eq!(meta.title, state.config.title);
        assert_eq!(meta.selected_model, state.config.selected_model);
        assert_eq!(meta.message_count, 3);
        assert_eq!(meta.status, state.config.status);
        assert_eq!(meta.working_dir, Some("/tmp".into()));
        assert_eq!(meta.parent_session_id, state.config.parent_session_id);
    }

    #[test]
    fn session_state_to_record() {
        let state = test_state();
        let record: SessionRecord = (&state).into();
        assert_eq!(record.title, state.config.title);
        assert_eq!(record.selected_model, state.config.selected_model);
        assert_eq!(record.message_count, 3);
        assert_eq!(record.working_dir, Some("/tmp".into()));
    }

    #[test]
    fn record_round_trip_preserves_active_tool_groups() {
        let record = test_record();
        let meta: SessionMetadata = record.clone().into();
        let record2: SessionRecord = meta.into();
        let mut expected = record.active_tool_groups.clone();
        let mut actual = record2.active_tool_groups.clone();
        expected.sort();
        actual.sort();
        assert_eq!(expected, actual);
        assert_eq!(record2.active_tool_groups.len(), 2);
    }

    // -- SessionCommand::Broadcast tests -----------------------------------

    /// Build minimal stubs needed to call `process_command` with a Broadcast.
    fn broadcast_setup() -> (SessionState, RequestContext) {
        let dir = tempdir().unwrap();
        let db = Arc::new(redb::Database::create(dir.path().join("test.redb")).unwrap());
        let tool_registry = ToolRegistry::new().build();
        let (daemon_tx, _) = mpsc::channel();
        let (cmd_tx, _) = mpsc::channel();
        let ctx = RequestContext {
            cmd_tx,
            session_id: 1,
            db,
            tool_registry,
            daemon_tx,
            max_turns_default: 25,
        };
        (test_state(), ctx)
    }

    #[test]
    fn broadcast_delivers_message_to_all_subscribers() {
        let (tx1, rx1) = mpsc::sync_channel(SUBSCRIBER_CHANNEL_CAPACITY);
        let (tx2, rx2) = mpsc::sync_channel(SUBSCRIBER_CHANNEL_CAPACITY);
        let (mut state, ctx) = broadcast_setup();
        state.subscribers.insert(10, tx1);
        state.subscribers.insert(20, tx2);

        let mut shutdown = false;
        process_command(
            SessionCommand::Broadcast(DaemonMessage::Done {
                request_id: 5,
                token_usage: None,
                last_prompt_tokens: None,
            }),
            &mut state,
            &mut shutdown,
            &ctx,
        );

        assert_eq!(
            rx1.recv().unwrap(),
            DaemonMessage::Done {
                request_id: 5,
                token_usage: None,
                last_prompt_tokens: None,
            }
        );
        assert_eq!(
            rx2.recv().unwrap(),
            DaemonMessage::Done {
                request_id: 5,
                token_usage: None,
                last_prompt_tokens: None,
            }
        );
        assert!(!shutdown);
    }

    #[test]
    fn broadcast_does_not_deliver_to_detached_client() {
        let (tx1, rx1) = mpsc::sync_channel(SUBSCRIBER_CHANNEL_CAPACITY);
        let (tx2, rx2) = mpsc::sync_channel(SUBSCRIBER_CHANNEL_CAPACITY);
        let (mut state, ctx) = broadcast_setup();
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
            &mut shutdown,
            &ctx,
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
        let (mut state, ctx) = broadcast_setup();
        // subscribers is already empty

        let mut shutdown = false;
        process_command(
            SessionCommand::Broadcast(DaemonMessage::Done {
                request_id: 0,
                token_usage: None,
                last_prompt_tokens: None,
            }),
            &mut state,
            &mut shutdown,
            &ctx,
        );

        assert!(!shutdown);
    }

    #[test]
    fn broadcast_handles_disconnected_subscriber_gracefully() {
        // A sender whose receiver has been dropped (simulating a client that
        // disconnected without properly detaching) should not panic or crash.
        let (tx, _rx) = mpsc::sync_channel(SUBSCRIBER_CHANNEL_CAPACITY);
        drop(_rx);
        let (mut state, ctx) = broadcast_setup();
        state.subscribers.insert(99, tx);

        let mut shutdown = false;
        process_command(
            SessionCommand::Broadcast(DaemonMessage::Pong),
            &mut state,
            &mut shutdown,
            &ctx,
        );

        assert!(!shutdown);
    }

    // -- handle_request_finished tests ------------------------------------

    #[test]
    fn request_finished_broadcasts_new_messages_skipping_displayed_images() {
        let (sub_tx, sub_rx) = mpsc::sync_channel(SUBSCRIBER_CHANNEL_CAPACITY);
        let (mut state, ctx) = broadcast_setup();
        state.subscribers.insert(10, sub_tx);

        // Snapshot with additional messages: a DisplayedImage (should be
        // skipped — already broadcast mid-turn) and a ToolResult (should
        // be delivered via SessionMessageAppended).
        let mut snap_msgs = state.messages.clone();
        snap_msgs.push(SessionMessage::now(SessionMessageKind::DisplayedImage(
            DisplayedImageRecord {
                metadata: ImageMetadata {
                    image_id: 1,
                    mime_type: "image/png".into(),
                    width: 100,
                    height: 50,
                    byte_len: 64,
                    alt: None,
                },
                data: vec![0u8; 64],
                tool_call_id: None,
            },
        )));
        snap_msgs.push(SessionMessage::now(SessionMessageKind::ToolResult {
            call_id: "call_1".into(),
            name: "echo".into(),
            content: "hello".into(),
            is_error: false,
        }));
        let snapshot = SessionSnapshot {
            config: state.config.clone(),
            messages: snap_msgs,
        };

        let shutdown = false;
        handle_request_finished(1, snapshot, &mut state, &shutdown, &ctx);

        // Collect all broadcasts from the subscriber.
        let mut saw_tool_result = false;
        let mut saw_displayed_image = false;
        while let Ok(msg) = sub_rx.try_recv() {
            if let DaemonMessage::SessionMessageAppended { message } = &msg {
                match &message.kind {
                    SessionMessageKind::DisplayedImage(_) => saw_displayed_image = true,
                    SessionMessageKind::ToolResult { .. } => saw_tool_result = true,
                    _ => {}
                }
            }
        }

        assert!(saw_tool_result, "ToolResult should have been broadcast");
        assert!(
            !saw_displayed_image,
            "DisplayedImage should have been skipped (broadcast mid-turn already)"
        );
    }

    // -- Cancel / Shutdown tests -------------------------------------------

    #[test]
    fn cancel_sends_through_channel() {
        let (cancel_tx, cancel_rx) = mpsc::channel::<()>();
        let (mut state, ctx) = broadcast_setup();
        state.active_requests.insert(1, ActiveRequest { cancel_tx });

        let mut shutdown = false;
        process_command(
            SessionCommand::Cancel { request_id: 1 },
            &mut state,
            &mut shutdown,
            &ctx,
        );

        // The cancellation signal should be delivered on the channel.
        assert!(cancel_rx.try_recv().is_ok());
        assert!(!shutdown);
    }

    #[test]
    fn cancel_broadcasts_cancelled_to_subscribers() {
        let (cancel_tx, _cancel_rx) = mpsc::channel::<()>();
        let (mut state, ctx) = broadcast_setup();
        state.active_requests.insert(1, ActiveRequest { cancel_tx });

        let (sub_tx, sub_rx) = mpsc::sync_channel(SUBSCRIBER_CHANNEL_CAPACITY);
        state.subscribers.insert(42, sub_tx);

        let mut shutdown = false;
        process_command(
            SessionCommand::Cancel { request_id: 1 },
            &mut state,
            &mut shutdown,
            &ctx,
        );

        assert_eq!(
            sub_rx.recv().unwrap(),
            DaemonMessage::Cancelled { request_id: 1 },
        );
    }

    #[test]
    fn cancel_unknown_request_id_is_noop() {
        let (mut state, ctx) = broadcast_setup();
        // No active requests — cancel on a non-existent ID should not fail.

        let mut shutdown = false;
        process_command(
            SessionCommand::Cancel { request_id: 99 },
            &mut state,
            &mut shutdown,
            &ctx,
        );

        assert!(!shutdown);
    }

    #[test]
    fn shutdown_cancels_all_active_requests() {
        let (cancel_tx1, cancel_rx1) = mpsc::channel::<()>();
        let (cancel_tx2, cancel_rx2) = mpsc::channel::<()>();
        let (mut state, ctx) = broadcast_setup();
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
        process_command(SessionCommand::Shutdown, &mut state, &mut shutdown, &ctx);

        assert!(shutdown);
        // Both requests should have received cancellation signals.
        assert!(cancel_rx1.try_recv().is_ok());
        assert!(cancel_rx2.try_recv().is_ok());
    }

    #[test]
    fn shutdown_with_empty_active_requests_returns_true() {
        // When there are no active requests, Shutdown should signal
        // that the session loop can exit (return true).
        let (mut state, ctx) = broadcast_setup();
        // active_requests is already empty.

        let mut shutdown = false;
        let should_exit =
            process_command(SessionCommand::Shutdown, &mut state, &mut shutdown, &ctx);

        assert!(shutdown);
        assert!(should_exit);
    }

    #[test]
    fn shutdown_broadcasts_cancelled_for_each_active_request() {
        let (cancel_tx1, _) = mpsc::channel::<()>();
        let (cancel_tx2, _) = mpsc::channel::<()>();
        let (mut state, ctx) = broadcast_setup();
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

        let (sub_tx, sub_rx) = mpsc::sync_channel(SUBSCRIBER_CHANNEL_CAPACITY);
        state.subscribers.insert(10, sub_tx);

        let mut shutdown = false;
        process_command(SessionCommand::Shutdown, &mut state, &mut shutdown, &ctx);

        // Should receive two Cancelled broadcasts (order not guaranteed).
        let msgs: Vec<DaemonMessage> = (0..2).map(|_| sub_rx.recv().unwrap()).collect();
        assert!(msgs.contains(&DaemonMessage::Cancelled { request_id: 1 }));
        assert!(msgs.contains(&DaemonMessage::Cancelled { request_id: 2 }));
    }

    // ── Token accumulation tests ──────────────────────────────────────────

    #[test]
    fn accumulated_usage_starts_at_zero() {
        let state = SessionState::empty();
        assert_eq!(state.config.accumulated_usage.input_tokens, 0);
        assert_eq!(state.config.accumulated_usage.output_tokens, 0);
        assert_eq!(state.config.accumulated_usage.total_tokens, 0);
    }

    #[test]
    fn accumulated_usage_persists_through_session_record_round_trip() {
        let meta = SessionMetadata {
            title: Some("test".into()),
            selected_model: None,
            reasoning_effort: None,
            parent_session_id: None,
            working_dir: Some("/tmp".into()),
            max_turns: None,
            created_at: 1000,
            message_count: 0,
            status: SessionStatus::Sleeping,
            active_tool_groups: vec!["core".into()],
            account_name: None,
            accumulated_usage: TokenUsage {
                input_tokens: 200,
                output_tokens: 100,
                total_tokens: 300,
            },
            context_window: None,
            last_prompt_tokens: None,
        };
        // Round-trip through SessionRecord (persisted form)
        let record: SessionRecord = meta.clone().into();
        let restored: SessionMetadata = record.into();
        assert_eq!(
            restored.accumulated_usage.input_tokens, 200,
            "input_tokens should survive round-trip"
        );
        assert_eq!(
            restored.accumulated_usage.output_tokens, 100,
            "output_tokens should survive round-trip"
        );
        assert_eq!(
            restored.accumulated_usage.total_tokens, 300,
            "total_tokens should survive round-trip"
        );
    }

    #[test]
    fn accumulated_usage_in_snapshot() {
        let mut state = SessionState::empty();
        state.config.accumulated_usage = TokenUsage {
            input_tokens: 50,
            output_tokens: 25,
            total_tokens: 75,
        };
        let snap = state.snapshot();
        assert_eq!(snap.config.accumulated_usage.input_tokens, 50);
        assert_eq!(snap.config.accumulated_usage.output_tokens, 25);
        assert_eq!(snap.config.accumulated_usage.total_tokens, 75);
    }

    #[test]
    fn accumulated_usage_in_session_summary() {
        // The SessionSummary sent via GetSummary includes the accumulated
        // token usage.
        let (mut state, ctx) = broadcast_setup();
        state.config.accumulated_usage = TokenUsage {
            input_tokens: 80,
            output_tokens: 40,
            total_tokens: 120,
        };

        let (reply, rx) = mpsc::channel();
        let mut shutdown = false;
        process_command(
            SessionCommand::GetSummary { reply },
            &mut state,
            &mut shutdown,
            &ctx,
        );

        let summary: SessionSummary = rx.recv().unwrap();
        let summary_usage = summary
            .token_usage
            .expect("token_usage should be present in SessionSummary");
        assert_eq!(summary_usage.input_tokens, 80);
        assert_eq!(summary_usage.output_tokens, 40);
        assert_eq!(summary_usage.total_tokens, 120);
    }

    #[test]
    fn accumulated_usage_in_attach_snapshot() {
        // When a client attaches, it receives a SessionState message that
        // includes accumulated token usage.
        let (mut state, ctx) = broadcast_setup();
        state.config.accumulated_usage = TokenUsage {
            input_tokens: 30,
            output_tokens: 15,
            total_tokens: 45,
        };

        let (sub_tx, sub_rx) = mpsc::sync_channel(SUBSCRIBER_CHANNEL_CAPACITY);
        let mut shutdown = false;
        process_command(
            SessionCommand::Attach {
                client_id: 42,
                tx: sub_tx,
            },
            &mut state,
            &mut shutdown,
            &ctx,
        );

        let msg = sub_rx.recv().unwrap();
        match msg {
            DaemonMessage::SessionState { token_usage, .. } => {
                let usage = token_usage.expect("token_usage in SessionState");
                assert_eq!(usage.input_tokens, 30);
                assert_eq!(usage.output_tokens, 15);
                assert_eq!(usage.total_tokens, 45);
            }
            other => panic!("expected SessionState, got {other:?}"),
        }
    }

    // -- SetModel validation tests ----------------------------------------

    /// Spawn a daemon handler that replies to ValidateModel with either
    /// Ok(()) or an error, then drains the rest of the channel.
    fn spawn_daemon_handler(
        daemon_rx: mpsc::Receiver<DaemonCommand>,
        accept: bool,
    ) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            if let Ok(DaemonCommand::ValidateModel { reply, .. }) = daemon_rx.recv() {
                if accept {
                    let _ = reply.send(Ok(()));
                } else {
                    let _ = reply.send(Err("not available".into()));
                }
            }
            // Drain remaining commands so the sender doesn't get
            // disconnected errors (UpdateMetadata etc.).
            while daemon_rx.recv().is_ok() {}
        })
    }

    #[test]
    fn handle_set_model_rejected_by_daemon_broadcasts_failure() {
        let dir = tempdir().unwrap();
        let db = Arc::new(redb::Database::create(dir.path().join("test.redb")).unwrap());
        let tool_registry = ToolRegistry::new().build();
        let (daemon_tx, daemon_rx) = mpsc::channel();
        let (cmd_tx, _cmd_rx) = mpsc::channel::<SessionCommand>();
        let daemon = spawn_daemon_handler(daemon_rx, false);

        let ctx = RequestContext {
            cmd_tx,
            session_id: 1,
            db,
            tool_registry,
            daemon_tx,
            max_turns_default: 25,
        };

        let (sub_tx, sub_rx) = mpsc::sync_channel(SUBSCRIBER_CHANNEL_CAPACITY);
        let mut state = test_state();
        state.subscribers.insert(10, sub_tx);

        handle_set_model("invalid-model".into(), &mut state, &ctx);

        // Should broadcast ModelSelectionFailed
        let msg = sub_rx.recv().unwrap();
        match msg {
            DaemonMessage::ModelSelectionFailed { model, error } => {
                assert_eq!(model, "invalid-model");
                assert_eq!(error, "not available");
            }
            other => panic!("expected ModelSelectionFailed, got {other:?}"),
        }

        // Model should NOT be changed from original
        assert_eq!(state.config.selected_model.as_deref(), Some("gpt-4"));

        drop(ctx);
        daemon.join().unwrap();
    }

    #[test]
    fn handle_set_model_accepted_by_daemon_updates_model() {
        let dir = tempdir().unwrap();
        let db = Arc::new(redb::Database::create(dir.path().join("test.redb")).unwrap());
        let tool_registry = ToolRegistry::new().build();
        let (daemon_tx, daemon_rx) = mpsc::channel();
        let (cmd_tx, _cmd_rx) = mpsc::channel::<SessionCommand>();
        let daemon = spawn_daemon_handler(daemon_rx, true);

        let ctx = RequestContext {
            cmd_tx,
            session_id: 1,
            db,
            tool_registry,
            daemon_tx,
            max_turns_default: 25,
        };

        let (sub_tx, sub_rx) = mpsc::sync_channel(SUBSCRIBER_CHANNEL_CAPACITY);
        let mut state = test_state();
        state.subscribers.insert(10, sub_tx);

        handle_set_model("gpt-5".into(), &mut state, &ctx);

        // Should broadcast ModelSelected
        let msg = sub_rx.recv().unwrap();
        match msg {
            DaemonMessage::ModelSelected { model } => {
                assert_eq!(model, "gpt-5");
            }
            other => panic!("expected ModelSelected, got {other:?}"),
        }

        // Model should be updated
        assert_eq!(state.config.selected_model.as_deref(), Some("gpt-5"));

        drop(ctx);
        daemon.join().unwrap();
    }

    // ── Undo / Redo tests ────────────────────────────────────────────────

    #[test]
    fn append_message_assigns_increasing_ids() {
        let mut state = SessionState::empty();
        let (id0, _) = state.append_message(SessionMessageKind::UserText {
            content: "first".into(),
        });
        let (id1, _) = state.append_message(SessionMessageKind::UserText {
            content: "second".into(),
        });
        assert_eq!(id0, 0);
        assert_eq!(id1, 1);
        assert_eq!(state.messages.len(), 2);
    }

    #[test]
    fn append_message_with_id_uses_explicit_id_and_bumps_next() {
        let mut state = SessionState::empty();
        state.append_message_with_id(
            SessionMessageKind::UserText {
                content: "skip".into(),
            },
            42,
        );
        assert_eq!(state.messages.len(), 1);
        assert_eq!(state.messages[0].message_id, 42);
        // next_message_id should advance past the explicit id
        let (id2, _) = state.append_message(SessionMessageKind::UserText {
            content: "after".into(),
        });
        assert_eq!(id2, 43, "next_message_id advanced past explicit id");
    }

    #[test]
    fn append_message_resolves_parent_id_for_assistant_messages() {
        let mut state = SessionState::empty();
        let (uid, _) = state.append_message(SessionMessageKind::UserText {
            content: "hi".into(),
        });
        let (aid, msg) = state.append_message(SessionMessageKind::AssistantText {
            content: "hello".into(),
            reasoning: None,
            token_usage: None,
        });
        assert_eq!(
            msg.parent_id,
            Some(uid),
            "assistant text gets user as parent"
        );
        assert!(aid > uid);
        let (aid2, msg2) = state.append_message(SessionMessageKind::AssistantText {
            content: "reply 2".into(),
            reasoning: None,
            token_usage: None,
        });
        // Second assistant message should also parent to the last non-deleted user
        assert_eq!(msg2.parent_id, Some(uid));
        assert!(aid2 > aid);
    }

    #[test]
    fn append_message_resolves_parent_id_for_tool_result() {
        let mut state = SessionState::empty();
        let (_uid, _) = state.append_message(SessionMessageKind::UserText {
            content: "run tool".into(),
        });
        let (atu_id, atu) = state.append_message(SessionMessageKind::AssistantToolUse {
            content: None,
            tool_calls: vec![AssistantToolCallRecord {
                call_id: "call_1".into(),
                name: "echo".into(),
                arguments_json: "{}".into(),
            }],
            reasoning: None,
            token_usage: None,
        });
        assert_eq!(atu.parent_id, Some(_uid));
        let (_tr_id, tr) = state.append_message(SessionMessageKind::ToolResult {
            call_id: "call_1".into(),
            name: "echo".into(),
            content: "done".into(),
            is_error: false,
        });
        assert_eq!(
            tr.parent_id,
            Some(atu_id),
            "tool result parent is the assistant tool use message"
        );
    }

    #[test]
    fn undo_marks_subtree_deleted_and_returns_ids() {
        let mut state = SessionState::empty();
        let (uid, _) = state.append_message(SessionMessageKind::UserText {
            content: "draw a cat".into(),
        });
        let (atu_id, _) = state.append_message(SessionMessageKind::AssistantToolUse {
            content: None,
            tool_calls: vec![AssistantToolCallRecord {
                call_id: "c1".into(),
                name: "svg".into(),
                arguments_json: "{}".into(),
            }],
            reasoning: None,
            token_usage: None,
        });
        let (tr_id, _) = state.append_message(SessionMessageKind::ToolResult {
            call_id: "c1".into(),
            name: "svg".into(),
            content: "<svg/>".into(),
            is_error: false,
        });
        let (_aid, _) = state.append_message(SessionMessageKind::AssistantText {
            content: "done".into(),
            reasoning: None,
            token_usage: None,
        });

        assert!(
            state.messages.iter().all(|m| !m.deleted),
            "no messages deleted before undo",
        );

        let ids = state.undo().expect("undo should find a user turn");
        // Should include the user message and its entire subtree.
        assert!(ids.contains(&uid), "user message id in undo set");
        assert!(ids.contains(&atu_id), "assistant tool use id in undo set");
        assert!(ids.contains(&tr_id), "tool result id in undo set");

        assert!(
            state.messages.iter().all(|m| m.deleted),
            "all messages marked deleted after undo",
        );
    }

    #[test]
    fn undo_returns_none_when_no_user_turn() {
        let mut state = SessionState::empty();
        // Only system messages — no user turn to undo.
        state.append_message(SessionMessageKind::SystemText {
            content: "prompt".into(),
        });
        assert!(state.undo().is_none(), "no user turn to undo");
    }

    #[test]
    fn undo_skips_already_deleted_messages() {
        let mut state = SessionState::empty();
        // Two user turns, each with an assistant reply.
        let (u0, _) = state.append_message(SessionMessageKind::UserText {
            content: "first".into(),
        });
        let (a0, _) = state.append_message(SessionMessageKind::AssistantText {
            content: "reply1".into(),
            reasoning: None,
            token_usage: None,
        });
        let (u1, _) = state.append_message(SessionMessageKind::UserText {
            content: "second".into(),
        });
        let (a1, _) = state.append_message(SessionMessageKind::AssistantText {
            content: "reply2".into(),
            reasoning: None,
            token_usage: None,
        });

        // Undo the most recent user turn (the "second" one and its subtree).
        let ids = state.undo().expect("undo should find a user turn");
        assert!(ids.contains(&u1), "second user turn in undo set");
        assert!(ids.contains(&a1), "assistant reply in undo set");
        // First turn messages should remain non-deleted.
        assert!(
            !state
                .messages
                .iter()
                .find(|m| m.message_id == u0)
                .unwrap()
                .deleted
        );
        assert!(
            !state
                .messages
                .iter()
                .find(|m| m.message_id == a0)
                .unwrap()
                .deleted
        );
        assert!(
            state
                .messages
                .iter()
                .find(|m| m.message_id == u1)
                .unwrap()
                .deleted
        );
        assert!(
            state
                .messages
                .iter()
                .find(|m| m.message_id == a1)
                .unwrap()
                .deleted
        );

        // Undo again — there's still the first user turn.
        let ids2 = state.undo().expect("undo second turn");
        assert!(ids2.contains(&u0), "first user turn in undo set");
    }

    #[test]
    fn redo_restores_undo_subtree() {
        let mut state = SessionState::empty();
        state.append_message(SessionMessageKind::UserText {
            content: "hi".into(),
        });
        let ids = state.undo().expect("undo succeeds");
        assert!(!ids.is_empty());

        let restored = state.redo().expect("redo succeeds");
        assert_eq!(restored.len(), ids.len());
        assert!(
            state.messages.iter().all(|m| !m.deleted),
            "all messages restored after redo",
        );
    }

    #[test]
    fn redo_returns_none_when_nothing_to_redo() {
        let mut state = SessionState::empty();
        state.append_message(SessionMessageKind::UserText {
            content: "hi".into(),
        });
        // No undo was performed — nothing to redo.
        assert!(state.redo().is_none());
    }

    #[test]
    fn redo_returns_none_after_new_user_input() {
        let mut state = SessionState::empty();
        state.append_message(SessionMessageKind::UserText {
            content: "first".into(),
        });
        state.undo();
        // New user input should clear the redo stack.
        state.append_message(SessionMessageKind::UserText {
            content: "second".into(),
        });
        assert!(state.redo().is_none(), "redo cleared after new user input");
    }

    #[test]
    fn collect_subtree_gathers_all_descendants() {
        // Manually construct a tree: 0 (root) → 1, 2 (children), 1 → 3 (grandchild)
        let messages = vec![
            SessionMessage {
                message_id: 0,
                parent_id: None,
                created_at: TimestampMs::now(),
                kind: SessionMessageKind::UserText {
                    content: "root".into(),
                },
                deleted: false,
            },
            SessionMessage {
                message_id: 1,
                parent_id: Some(0),
                created_at: TimestampMs::now(),
                kind: SessionMessageKind::AssistantText {
                    content: "child1".into(),
                    reasoning: None,
                    token_usage: None,
                },
                deleted: false,
            },
            SessionMessage {
                message_id: 2,
                parent_id: Some(0),
                created_at: TimestampMs::now(),
                kind: SessionMessageKind::AssistantText {
                    content: "child2".into(),
                    reasoning: None,
                    token_usage: None,
                },
                deleted: false,
            },
            SessionMessage {
                message_id: 3,
                parent_id: Some(1),
                created_at: TimestampMs::now(),
                kind: SessionMessageKind::AssistantText {
                    content: "grandchild".into(),
                    reasoning: None,
                    token_usage: None,
                },
                deleted: false,
            },
        ];
        let children = build_children_map(&messages);
        let ids = collect_subtree(&children, 0);
        assert_eq!(ids.len(), 4, "all four messages in subtree");
        assert!(ids.contains(&0));
        assert!(ids.contains(&1));
        assert!(ids.contains(&2));
        assert!(ids.contains(&3));
    }
}
