use crate::context::{LoadedSkill, SkillMeta};
use crate::daemon::DaemonCommand;
use crate::db::{self, SessionRecord, write_session_retry, write_turn_retry};
use crate::providers::{InferenceProvider, model_reasoning_capability};
use crate::requests::run_agent_loop;
use crate::tools::ToolRegistry;
use choreo_proto::{
    AssistantToolCallRecord, ContextConfig, DaemonMessage, DisplayedImageRecord, SessionStatus,
    SessionSummary, TimestampMs, TokenUsage, ToolResultRecord, Turn,
};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, mpsc};
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{debug, error, info, trace, warn};

/// Sentinel `request_id` meaning "cancel whatever is currently active, regardless of its ID".
/// Used in child-session cancellation where we don't know the child's active request ID.
pub(crate) const CANCEL_ALL: u32 = 0;

pub enum SessionCommand {
    RunInput {
        request_id: u32,
        input: Vec<u8>,
    },
    RunChildInput {
        request_id: u32,
        user_text: Option<String>,
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
        effort: String,
    },
    GetReasoningEffort {
        reply: mpsc::Sender<String>,
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
    pub reasoning_effort: Option<String>,
    pub parent_session_id: Option<u64>,
    pub working_dir: Option<String>,
    pub created_at: i64,
    pub turn_count: u32,
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
        let config = SessionConfig {
            title: record.title,
            selected_model: record.selected_model,
            reasoning_effort: record.reasoning_effort,
            parent_session_id: record.parent_session_id,
            working_dir: record.working_dir.map(PathBuf::from),
            max_turns: record.max_turns,
            created_at: record.created_at,
            status: SessionStatus::Sleeping,
            active_tool_groups: record.active_tool_groups.into_iter().collect(),
            context_config: record.context_config,
            account_name: record.account_name,
            accumulated_usage: TokenUsage::default(),
            context_window: None,
            last_prompt_tokens: None,
        };
        let mut meta = SessionMetadata::from(&config);
        meta.turn_count = record.turn_count;
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
            turn_count: meta.turn_count,
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
/// requests, turn contents, etc.) are dropped. The `PathBuf` working_dir is
/// stringified.
impl From<&SessionState> for SessionMetadata {
    fn from(state: &SessionState) -> Self {
        let mut meta = SessionMetadata::from(&state.config);
        meta.turn_count = state.turns.len() as u32;
        meta
    }
}

impl SessionMetadata {
    pub fn to_summary(&self, session_id: u64) -> SessionSummary {
        SessionSummary {
            session_id,
            title: self.title.clone(),
            selected_model: self.selected_model.clone(),
            reasoning_effort: self.reasoning_effort.clone(),
            parent_session_id: self.parent_session_id,
            working_dir: self.working_dir.clone(),
            created_at: self.created_at,
            turn_count: self.turn_count,
            max_turns: self.max_turns,
            status: self.status.clone(),
            active_tool_groups: self.active_tool_groups.clone(),
            account_name: self.account_name.clone(),
            token_usage: Some(self.accumulated_usage),
            context_window: self.context_window,
            last_prompt_tokens: self.last_prompt_tokens,
        }
    }
}

/// Persistent configuration fields for a session.
/// Bundled to avoid duplication across snapshot/restore, metadata conversion,
/// and record persistence.
#[derive(Clone, Debug)]
pub struct SessionConfig {
    pub title: Option<String>,
    pub selected_model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub parent_session_id: Option<u64>,
    pub working_dir: Option<PathBuf>,
    pub max_turns: Option<u32>,
    pub created_at: i64,
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
            reasoning_effort: config.reasoning_effort.clone(),
            parent_session_id: config.parent_session_id,
            working_dir: config.working_dir.as_ref().map(|p| p.display().to_string()),
            created_at: config.created_at,
            turn_count: 0,
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
    pub turns: BTreeMap<u32, Turn>,
    pub loaded_skill_bodies: Vec<LoadedSkill>,
    pub context_cache: Option<(u64, Arc<String>)>,
    pub discovered_skills: Option<Vec<SkillMeta>>,
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
    pub next_turn_id: u32,
    last_undo_turn_ids: Option<Vec<u32>>,
    pub turns: BTreeMap<u32, Turn>,
    subscribers: HashMap<u64, std::sync::mpsc::SyncSender<DaemonMessage>>,
    pub(crate) active_requests: HashMap<u32, ActiveRequest>,
    pub provider: Option<InferenceProvider>,
    pub loaded_skill_bodies: Vec<LoadedSkill>,
    pub context_cache: Option<(u64, Arc<String>)>,
    pub discovered_skills: Option<Vec<SkillMeta>>,
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
            broadcast(
                &mut self.subscribers,
                DaemonMessage::ContextWindowResolved {
                    session_id,
                    context_window: cw,
                },
            );
        }
    }

    fn snapshot(&self) -> SessionSnapshot {
        SessionSnapshot {
            config: self.config.clone(),
            turns: self.turns.clone(),
            loaded_skill_bodies: self.loaded_skill_bodies.clone(),
            context_cache: self.context_cache.clone(),
            discovered_skills: self.discovered_skills.clone(),
        }
    }

    fn from_snapshot(
        snapshot: SessionSnapshot,
        subscribers: HashMap<u64, std::sync::mpsc::SyncSender<DaemonMessage>>,
    ) -> Self {
        let turn_count = snapshot.turns.len() as u32;
        Self {
            config: snapshot.config,
            next_turn_id: turn_count,
            last_undo_turn_ids: None,
            turns: snapshot.turns,
            subscribers,
            active_requests: HashMap::new(),
            provider: None,
            loaded_skill_bodies: snapshot.loaded_skill_bodies,
            context_cache: snapshot.context_cache,
            discovered_skills: snapshot.discovered_skills,
        }
    }

    /// Build a [`DaemonMessage::SessionState`] snapshot of the current session
    /// for broadcasting to connected clients.  Centralises the field mapping
    /// so that every broadcast site stays consistent when new fields are added.
    pub(crate) fn session_state_message(&self, session_id: u64) -> DaemonMessage {
        let reasoning_capability = self.config.selected_model.as_ref().and_then(|model| {
            let slug = self.provider.as_ref()?.provider_slug();
            Some(model_reasoning_capability(slug, model))
        });
        DaemonMessage::SessionState {
            session_id,
            title: self.config.title.clone(),
            selected_model: self.config.selected_model.clone(),
            parent_session_id: self.config.parent_session_id,
            working_dir: self
                .config
                .working_dir
                .as_ref()
                .map(|p| p.display().to_string()),
            max_turns: self.config.max_turns,
            turns: self.turns.clone(),
            active_tool_groups: self.config.active_tool_groups.iter().cloned().collect(),
            token_usage: Some(self.config.accumulated_usage),
            context_window: self.config.context_window,
            last_prompt_tokens: self.config.last_prompt_tokens,
            status: self.config.status.clone(),
            reasoning_effort: self.config.reasoning_effort.clone(),
            reasoning_capability,
        }
    }

    /// Start a new turn, returning its turn_id.
    /// If the turn has user text, the redo stack is cleared.
    pub fn start_turn(&mut self, user_text: Option<String>) -> (u32, Turn) {
        // New user input after an undo clears the redo opportunity.
        if user_text.is_some() {
            self.last_undo_turn_ids = None;
        }
        let turn_id = self.next_turn_id;
        self.next_turn_id += 1;
        let turn = Turn {
            created_at: TimestampMs::now(),
            undone: false,
            error: None,
            user_text,
            assistant_text: None,
            assistant_reasoning: None,
            tool_calls: Vec::new(),
            token_usage: None,
            tool_results: Vec::new(),
            displayed_images: Vec::new(),
        };
        self.turns.insert(turn_id, turn.clone());
        (turn_id, turn)
    }

    /// Set the assistant response on a turn (text or tool-use).
    pub fn set_assistant_response(
        &mut self,
        turn_id: u32,
        text: Option<String>,
        reasoning: Option<String>,
        tool_calls: Vec<AssistantToolCallRecord>,
        token_usage: Option<TokenUsage>,
    ) {
        if let Some(turn) = self.turns.get_mut(&turn_id) {
            turn.assistant_text = text;
            turn.assistant_reasoning = reasoning;
            turn.tool_calls = tool_calls;
            turn.token_usage = token_usage;
        }
    }

    /// Add a tool result to a turn.
    pub fn add_tool_result(
        &mut self,
        turn_id: u32,
        call_id: String,
        name: String,
        content: String,
        is_error: bool,
        invocation_description: String,
    ) {
        if let Some(turn) = self.turns.get_mut(&turn_id) {
            turn.tool_results.push(ToolResultRecord {
                call_id,
                name,
                content,
                is_error,
                invocation_description,
            });
        }
    }

    /// Add a displayed image to a turn.
    pub fn add_displayed_image(&mut self, turn_id: u32, record: DisplayedImageRecord) {
        if let Some(turn) = self.turns.get_mut(&turn_id) {
            turn.displayed_images.push(record);
        }
    }

    /// Set an error on a turn.
    pub fn set_turn_error(&mut self, turn_id: u32, error: String) {
        if let Some(turn) = self.turns.get_mut(&turn_id) {
            turn.error = Some(error);
        }
    }

    /// Finalize a turn and persist it to the database.
    /// Returns an error if persistence fails after all retries.
    pub fn finalize_turn(
        &mut self,
        db: &redb::Database,
        session_id: u64,
        turn_id: u32,
    ) -> io::Result<()> {
        if let Some(turn) = self.turns.get(&turn_id) {
            write_turn_retry(db, session_id, turn_id, turn)
                .map_err(|e| io::Error::other(format!("failed to persist turn {turn_id}: {e}")))?;
        }
        Ok(())
    }

    /// Undo the most recent user-initiated turns: find the most recent
    /// non-undone turn with `user_text: Some(...)`, mark it and all
    /// higher-id turns as `undone = true`, store turn_ids for redo.
    pub fn undo_turns(&mut self) -> Option<Vec<u32>> {
        let target = self
            .turns
            .iter()
            .rev()
            .find(|(_, t)| !t.undone && t.user_text.is_some())
            .map(|(&id, _)| id)?;
        let to_undo: Vec<u32> = self.turns.range(target..).map(|(&id, _)| id).collect();
        for &id in &to_undo {
            if let Some(turn) = self.turns.get_mut(&id) {
                turn.undone = true;
            }
        }
        self.last_undo_turn_ids = Some(to_undo.clone());
        Some(to_undo)
    }

    /// Redo the most recent `/undo`, restoring exactly the turns that
    /// were marked as undone.
    pub fn redo_turns(&mut self) -> Option<BTreeMap<u32, Turn>> {
        let ids = self.last_undo_turn_ids.take()?;
        let mut restored = BTreeMap::new();
        for &id in &ids {
            if let Some(turn) = self.turns.get_mut(&id) {
                turn.undone = false;
                restored.insert(id, turn.clone());
            }
        }
        Some(restored)
    }

    /// Create an empty session state.
    pub fn empty() -> Self {
        Self {
            config: SessionConfig::default(),
            next_turn_id: 0,
            last_undo_turn_ids: None,
            turns: BTreeMap::new(),
            subscribers: HashMap::new(),
            active_requests: HashMap::new(),
            provider: None,
            loaded_skill_bodies: Vec::new(),
            context_cache: None,
            discovered_skills: None,
        }
    }
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
            turn_id: 0,
            estimated_prompt_tokens: 0,
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
        reasoning_effort: init_record
            .as_ref()
            .and_then(|r| r.reasoning_effort.clone()),
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
        accumulated_usage: TokenUsage::default(),
        context_window: None,
        last_prompt_tokens: None,
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

    match db::read_turns(&ctx.db, ctx.session_id) {
        Ok(turns) => {
            for (turn_id, turn) in turns {
                state.turns.insert(turn_id, turn);
                state.next_turn_id = state.next_turn_id.max(turn_id + 1);
            }
            // Reconstruct accumulated_usage and last_prompt_tokens from
            // per-turn token_usage so both the running total and the
            // context-window display (e.g. "45k / 128k (35%)") survive
            // daemon restarts without storing them redundantly in the
            // session record.  Both are derived in a single pass over
            // turns (ordered by turn_id) — the last turn with token_usage
            // is the most recent one, giving us last_prompt_tokens.
            let mut accumulated_usage = TokenUsage::default();
            let mut last_prompt_tokens = None;
            for turn in state.turns.values() {
                if let Some(u) = turn.token_usage {
                    accumulated_usage.input_tokens += u.input_tokens;
                    accumulated_usage.output_tokens += u.output_tokens;
                    accumulated_usage.total_tokens += u.total_tokens;
                    last_prompt_tokens = Some(u.input_tokens);
                }
            }
            state.config.accumulated_usage = accumulated_usage;
            state.config.last_prompt_tokens = last_prompt_tokens;
            trace!(
                last_prompt_tokens,
                ?accumulated_usage,
                "reconstructed token state from turns after daemon restart"
            );
        }
        Err(e) => warn!(ctx.session_id, error = %e, "failed to load turns from DB"),
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
        SessionCommand::RunChildInput {
            request_id,
            user_text,
            reply,
        } => handle_run_child_input(request_id, user_text, reply, state, shutdown_requested, ctx),
        SessionCommand::Cancel { request_id } => handle_cancel(request_id, state, ctx),
        SessionCommand::SetModel { model } => handle_set_model(model, state, ctx),
        SessionCommand::StatusChanged(new_status) => handle_status_changed(new_status, state, ctx),
        SessionCommand::Attach { client_id, tx } => handle_attach(client_id, tx, state, ctx),
        SessionCommand::Detach { client_id } => {
            handle_detach(client_id, state, shutdown_requested, ctx)
        }
        SessionCommand::GetSummary { reply } => handle_get_summary(reply, state, ctx),
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

    broadcast(
        &mut state.subscribers,
        DaemonMessage::Started {
            request_id,
            turn_id: state.next_turn_id,
            estimated_prompt_tokens: 0,
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
    let user_text = Some(text);
    std::thread::spawn(move || {
        let _ = run_request_worker(
            request_id,
            provider,
            &mut worker_session,
            model,
            cancel_rx,
            ctx,
            None,
            user_text,
        );
    });
    false
}

/// Run the agent loop on a pre-populated child session and return the result.
///
/// The caller is responsible for injecting any prompt into the session — this
/// command only triggers the agent loop on whatever turns are already
/// queued. The response is delivered through the `reply` channel.
fn handle_run_child_input(
    request_id: u32,
    user_text: Option<String>,
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
            turn_id: state.next_turn_id,
            estimated_prompt_tokens: 0,
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
            user_text,
        );
        let _ = result;
    });
    false
}

/// Cancel an active request by sending on its cancel channel.
/// Child-session propagation is handled by the daemon when it processes
/// `DaemonCommand::CancelRequest`, so this function does not send any
/// additional messages back to the daemon.
/// Cancel one or all active requests.
///
/// When `request_id` is `0` (the `CANCEL_ALL` sentinel), every active
/// request is cancelled — this is used by child-session cancellation
/// where the parent doesn't know the child's specific request ID.
/// Otherwise only the matching request is cancelled.
fn handle_cancel(request_id: u32, state: &mut SessionState, _ctx: &RequestContext) -> bool {
    let targets: Vec<u32> = if request_id == 0 {
        state.active_requests.keys().copied().collect()
    } else {
        vec![request_id]
    };
    for rid in targets {
        if let Some(active) = state.active_requests.get(&rid) {
            let _ = active.cancel_tx.send(());
            broadcast(
                &mut state.subscribers,
                DaemonMessage::Cancelled { request_id: rid },
            );
        }
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
    if let Some(cw) = cw {
        broadcast(
            &mut state.subscribers,
            DaemonMessage::ContextWindowResolved {
                session_id: ctx.session_id,
                context_window: cw,
            },
        );
    }
    let capability = state
        .provider
        .as_ref()
        .map(|p| model_reasoning_capability(p.provider_slug(), &model));

    // Re-validate the current reasoning effort against the new model's
    // capability.  Slugs that were valid on the old model may not be
    // supported by the new one — silently reset to "off" when that happens.
    if let Some(ref cap) = capability
        && let Some(ref effort) = state.config.reasoning_effort
        && effort != "off"
        && !cap.available_effort_levels.iter().any(|l| l == effort)
    {
        warn!(
            session_id = ctx.session_id,
            old_effort = %effort,
            "reasoning effort not supported by new model, resetting to 'off'",
        );
        state.config.reasoning_effort = Some("off".to_string());
        broadcast(
            &mut state.subscribers,
            DaemonMessage::ReasoningEffortSet {
                effort: "off".to_string(),
            },
        );
    }

    debug!(
        "session {}: broadcasting ModelSelected model={}",
        ctx.session_id, model
    );
    broadcast(
        &mut state.subscribers,
        DaemonMessage::ModelSelected {
            model: model.clone(),
            reasoning_capability: capability,
        },
    );
    let _ = ctx.daemon_tx.send(DaemonCommand::UpdateMetadata {
        session_id: ctx.session_id,
        metadata: SessionMetadata::from(&*state),
    });
    // Persist the updated session record so resolved context_window
    // survives daemon restarts.
    let record = SessionRecord::from(&*state);
    if let Err(e) = write_session_retry(&ctx.db, ctx.session_id, &record) {
        warn!(error = %e, "failed to persist session record after SetModel");
    }
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
    let snapshot = state.session_state_message(ctx.session_id);
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
        reasoning_effort: state.config.reasoning_effort.clone(),
        parent_session_id: state.config.parent_session_id,
        working_dir: state
            .config
            .working_dir
            .as_ref()
            .map(|p| p.display().to_string()),
        created_at: state.config.created_at,
        turn_count: state.turns.len() as u32,
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

/// Apply the worker's snapshot (config only) and merge turn state.
fn handle_request_finished(
    request_id: u32,
    snapshot: SessionSnapshot,
    state: &mut SessionState,
    shutdown_requested: &bool,
    ctx: &RequestContext,
) -> bool {
    // Apply config changes from worker (accumulated usage, context_window, etc.)
    state.config = snapshot.config;

    // Persist the updated session config (accumulated usage, context_window, etc.)
    // so resolved values survive daemon restarts.
    let record = SessionRecord::from(&*state);
    if let Err(e) = write_session_retry(&ctx.db, ctx.session_id, &record) {
        warn!(error = %e, "failed to persist session config after request");
    }

    // Merge runtime state from the worker snapshot so that loaded skills,
    // context cache, and discovered skills survive across requests.
    state.loaded_skill_bodies = snapshot.loaded_skill_bodies;
    state.context_cache = snapshot.context_cache;
    state.discovered_skills = snapshot.discovered_skills;

    // Merge turns from the worker snapshot into the main session state.
    for (&turn_id, turn) in &snapshot.turns {
        let is_new = !state.turns.contains_key(&turn_id);
        state.turns.insert(turn_id, turn.clone());
        if is_new {
            // Persist the newly created turn. The turn was already broadcast
            // during the agent loop, so no need to re-broadcast here.
            if let Err(e) = write_turn_retry(&ctx.db, ctx.session_id, turn_id, turn) {
                tracing::warn!(turn_id, error = %e, "failed to persist turn");
            }
        } else if state.turns.get(&turn_id).is_some_and(|t| t != turn) {
            // Turn was updated — persist the latest state.
            if let Err(e) = write_turn_retry(&ctx.db, ctx.session_id, turn_id, turn) {
                tracing::warn!(turn_id, error = %e, "failed to persist updated turn");
            }
        }
    }
    // Advance next_turn_id past any turns from the snapshot.
    if let Some(max_id) = snapshot.turns.keys().max() {
        state.next_turn_id = state.next_turn_id.max(max_id + 1);
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
            if let Some(cw) = cw {
                broadcast(
                    &mut state.subscribers,
                    DaemonMessage::ContextWindowResolved {
                        session_id: ctx.session_id,
                        context_window: cw,
                    },
                );
            }
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
    // Persist the updated session record so resolved context_window
    // survives daemon restarts.
    let record = SessionRecord::from(&*state);
    if let Err(e) = write_session_retry(&ctx.db, ctx.session_id, &record) {
        warn!(error = %e, "failed to persist session record after SetAccount");
    }
    false
}

/// Set the reasoning effort for this session, validating against the model.
fn handle_set_reasoning_effort(
    effort: String,
    state: &mut SessionState,
    ctx: &RequestContext,
) -> bool {
    // Reject overly long slugs early (defense in depth).
    if effort.len() > 64 {
        let msg = format!("reasoning effort slug too long ({} bytes)", effort.len());
        warn!(session_id = ctx.session_id, error = %msg, "reasoning effort rejected");
        broadcast(
            &mut state.subscribers,
            DaemonMessage::ReasoningEffortSetFailed { effort, error: msg },
        );
        return false;
    }

    // Compute capability for the current model (if any).
    let capability = state.config.selected_model.as_ref().and_then(|model| {
        let slug = state.provider.as_ref()?.provider_slug();
        Some(model_reasoning_capability(slug, model))
    });

    // "off" is always valid — every model can disable reasoning. Otherwise
    // check the slug is in the model's capability set.
    let valid = effort == "off"
        || capability
            .as_ref()
            .map(|c| c.available_effort_levels.contains(&effort))
            .unwrap_or(false)
        // No model selected yet: accept the preference optimistically (it
        // will be validated when inference actually runs in
        // resolve_reasoning_effort).
        || capability.is_none();

    if valid {
        state.config.reasoning_effort = Some(effort.clone());
        info!(
            session_id = ctx.session_id,
            effort = %effort,
            model = ?state.config.selected_model,
            "reasoning effort set",
        );
        broadcast(
            &mut state.subscribers,
            DaemonMessage::ReasoningEffortSet { effort },
        );
        return false;
    } else {
        let model = state.config.selected_model.as_deref().unwrap_or("(none)");
        let msg = format!("model '{model}' does not support reasoning effort '{effort}'",);
        warn!(session_id = ctx.session_id, error = %msg, "reasoning effort rejected");
        broadcast(
            &mut state.subscribers,
            DaemonMessage::ReasoningEffortSetFailed { effort, error: msg },
        );
    }
    false
}

/// Return the current reasoning effort via the reply channel.
fn handle_get_reasoning_effort(
    reply: mpsc::Sender<String>,
    state: &SessionState,
    ctx: &RequestContext,
) -> bool {
    let _ = ctx;
    let current = state
        .config
        .reasoning_effort
        .clone()
        .unwrap_or_else(|| "off".to_string());
    let _ = reply.send(current);
    false
}

/// Handle Undo: mark the most recent user turn's subtree as deleted.
/// Uses a quick-reference HashMap to avoid an O(n) scan per ID.
fn handle_undo(state: &mut SessionState, ctx: &RequestContext) -> bool {
    let Some(turn_ids) = state.undo_turns() else {
        debug!(
            session_id = ctx.session_id,
            "undo requested but no user turn to undo",
        );
        return false;
    };
    info!(
        session_id = ctx.session_id,
        turn_count = turn_ids.len(),
        "undo: marked turns as undone",
    );
    // Persist the updated turns.
    for &id in &turn_ids {
        if let Some(turn) = state.turns.get(&id)
            && let Err(e) = write_turn_retry(&ctx.db, ctx.session_id, id, turn)
        {
            tracing::warn!(turn_id = id, error = %e, "failed to persist undone turn");
        }
    }
    broadcast(
        &mut state.subscribers,
        DaemonMessage::TurnsUndone { turn_ids },
    );
    false
}

/// Reinstate the turns that were hidden by the preceding undo,
/// persisting the restored state so it survives daemon restart.
fn handle_redo(state: &mut SessionState, ctx: &RequestContext) -> bool {
    let Some(turns) = state.redo_turns() else {
        debug!(
            session_id = ctx.session_id,
            "redo requested but nothing to redo (no prior undo, or new input after undo)",
        );
        return false;
    };
    info!(
        session_id = ctx.session_id,
        turn_count = turns.len(),
        "redo: restored previously-undone turns",
    );
    for (&id, turn) in &turns {
        if let Err(e) = write_turn_retry(&ctx.db, ctx.session_id, id, turn) {
            tracing::warn!(turn_id = id, error = %e, "failed to persist redone turn");
        }
    }
    broadcast(&mut state.subscribers, DaemonMessage::TurnsRedone { turns });
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
    user_text: Option<String>,
) -> io::Result<()> {
    let request_start = std::time::Instant::now();
    let initial_snapshot = session.snapshot();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run_agent_loop(
            &client, session, &model, request_id, &cancel_rx, &ctx, user_text,
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
                    .turns
                    .values()
                    .filter_map(|t| t.assistant_text.clone())
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
    use crate::server::connection::SUBSCRIBER_CHANNEL_CAPACITY;
    use crate::tools::ToolRegistry;
    use choreo_proto::SessionStatus;
    use std::collections::HashMap;
    use tempfile::tempdir;

    fn test_state() -> SessionState {
        let mut turns = BTreeMap::new();
        turns.insert(
            0,
            Turn {
                created_at: TimestampMs::now(),
                undone: false,
                error: None,
                user_text: Some("hello".into()),
                assistant_text: Some("hi".into()),
                assistant_reasoning: None,
                tool_calls: Vec::new(),
                token_usage: None,
                tool_results: Vec::new(),
                displayed_images: Vec::new(),
            },
        );
        SessionState {
            config: SessionConfig {
                title: Some("test session".into()),
                selected_model: Some("gpt-4".into()),
                reasoning_effort: None,
                parent_session_id: None,
                working_dir: Some(std::path::PathBuf::from("/tmp")),
                max_turns: Some(10),
                created_at: 1000,
                status: SessionStatus::Inactive,
                active_tool_groups: ["core".into(), "shell".into()].into(),
                context_config: ContextConfig::default(),
                account_name: None,
                accumulated_usage: TokenUsage::default(),
                context_window: None,
                last_prompt_tokens: None,
            },
            next_turn_id: 1,
            last_undo_turn_ids: None,
            turns,
            loaded_skill_bodies: Vec::new(),
            context_cache: None,
            discovered_skills: None,
            subscribers: HashMap::new(),
            active_requests: HashMap::new(),
            provider: None,
        }
    }

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
    fn session_state_round_trip_metadata() {
        let state = test_state();
        let meta: SessionMetadata = (&state).into();
        assert_eq!(meta.title, state.config.title);
        assert_eq!(meta.selected_model, state.config.selected_model);
        assert_eq!(meta.turn_count, 1);
        assert_eq!(meta.status, state.config.status);
    }

    #[test]
    fn session_state_to_record() {
        let state = test_state();
        let record: SessionRecord = (&state).into();
        assert_eq!(record.title, state.config.title);
        assert_eq!(record.selected_model, state.config.selected_model);
        assert_eq!(record.turn_count, 1);
    }

    // -- SessionCommand::Broadcast tests -----------------------------------

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
    fn broadcast_with_no_subscribers_does_not_panic() {
        let (mut state, ctx) = broadcast_setup();
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

        assert!(cancel_rx.try_recv().is_ok());
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
        assert!(cancel_rx1.try_recv().is_ok());
        assert!(cancel_rx2.try_recv().is_ok());
    }

    #[test]
    fn shutdown_with_empty_active_requests_returns_true() {
        let (mut state, ctx) = broadcast_setup();
        let mut shutdown = false;
        let should_exit =
            process_command(SessionCommand::Shutdown, &mut state, &mut shutdown, &ctx);
        assert!(shutdown);
        assert!(should_exit);
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
    fn accumulated_usage_reconstructed_from_turns() {
        let mut state = SessionState::empty();

        // Turn 0: no token_usage (should be filtered out)
        state.turns.insert(
            0,
            Turn {
                created_at: TimestampMs::now(),
                undone: false,
                error: None,
                user_text: Some("hello".into()),
                assistant_text: Some("hi".into()),
                assistant_reasoning: None,
                tool_calls: Vec::new(),
                token_usage: None,
                tool_results: Vec::new(),
                displayed_images: Vec::new(),
            },
        );

        // Turn 1: has token_usage
        state.turns.insert(
            1,
            Turn {
                created_at: TimestampMs::now(),
                undone: false,
                error: None,
                user_text: Some("turn 2".into()),
                assistant_text: Some("response 2".into()),
                assistant_reasoning: None,
                tool_calls: Vec::new(),
                token_usage: Some(TokenUsage {
                    input_tokens: 10,
                    output_tokens: 20,
                    total_tokens: 30,
                }),
                tool_results: Vec::new(),
                displayed_images: Vec::new(),
            },
        );

        // Turn 2: another with token_usage
        state.turns.insert(
            2,
            Turn {
                created_at: TimestampMs::now(),
                undone: false,
                error: None,
                user_text: Some("turn 3".into()),
                assistant_text: Some("response 3".into()),
                assistant_reasoning: None,
                tool_calls: Vec::new(),
                token_usage: Some(TokenUsage {
                    input_tokens: 100,
                    output_tokens: 50,
                    total_tokens: 150,
                }),
                tool_results: Vec::new(),
                displayed_images: Vec::new(),
            },
        );

        // Turn 3: no token_usage (should be filtered out)
        state.turns.insert(
            3,
            Turn {
                created_at: TimestampMs::now(),
                undone: false,
                error: None,
                user_text: Some("no usage".into()),
                assistant_text: None,
                assistant_reasoning: None,
                tool_calls: Vec::new(),
                token_usage: None,
                tool_results: Vec::new(),
                displayed_images: Vec::new(),
            },
        );

        // Run the same reconstruction logic from session_main
        let mut accumulated_usage = TokenUsage::default();
        let mut last_prompt_tokens = None;
        for turn in state.turns.values() {
            if let Some(u) = turn.token_usage {
                accumulated_usage.input_tokens += u.input_tokens;
                accumulated_usage.output_tokens += u.output_tokens;
                accumulated_usage.total_tokens += u.total_tokens;
                last_prompt_tokens = Some(u.input_tokens);
            }
        }
        state.config.accumulated_usage = accumulated_usage;
        state.config.last_prompt_tokens = last_prompt_tokens;

        // Expected: 10+100 = 110 input, 20+50 = 70 output, 30+150 = 180 total
        assert_eq!(state.config.accumulated_usage.input_tokens, 110);
        assert_eq!(state.config.accumulated_usage.output_tokens, 70);
        assert_eq!(state.config.accumulated_usage.total_tokens, 180);
        // last_prompt_tokens should be the most recent turn's input_tokens (turn 2 = 100)
        assert_eq!(state.config.last_prompt_tokens, Some(100));
    }

    #[test]
    fn last_prompt_tokens_from_latest_usage_turn() {
        let mut state = SessionState::empty();

        // Turn 0: no token_usage
        state.turns.insert(
            0,
            Turn {
                created_at: TimestampMs::now(),
                undone: false,
                error: None,
                user_text: Some("no usage".into()),
                assistant_text: None,
                assistant_reasoning: None,
                tool_calls: Vec::new(),
                token_usage: None,
                tool_results: Vec::new(),
                displayed_images: Vec::new(),
            },
        );

        // Turn 1: has token_usage
        state.turns.insert(
            1,
            Turn {
                created_at: TimestampMs::now(),
                undone: false,
                error: None,
                user_text: Some("first".into()),
                assistant_text: Some("response".into()),
                assistant_reasoning: None,
                tool_calls: Vec::new(),
                token_usage: Some(TokenUsage {
                    input_tokens: 5,
                    output_tokens: 10,
                    total_tokens: 15,
                }),
                tool_results: Vec::new(),
                displayed_images: Vec::new(),
            },
        );

        // Turn 2: has token_usage with larger input
        state.turns.insert(
            2,
            Turn {
                created_at: TimestampMs::now(),
                undone: false,
                error: None,
                user_text: Some("second".into()),
                assistant_text: Some("response 2".into()),
                assistant_reasoning: None,
                tool_calls: Vec::new(),
                token_usage: Some(TokenUsage {
                    input_tokens: 42,
                    output_tokens: 7,
                    total_tokens: 49,
                }),
                tool_results: Vec::new(),
                displayed_images: Vec::new(),
            },
        );

        // Reconstruct
        let mut accumulated_usage = TokenUsage::default();
        let mut last_prompt_tokens = None;
        for turn in state.turns.values() {
            if let Some(u) = turn.token_usage {
                accumulated_usage.input_tokens += u.input_tokens;
                accumulated_usage.output_tokens += u.output_tokens;
                accumulated_usage.total_tokens += u.total_tokens;
                last_prompt_tokens = Some(u.input_tokens);
            }
        }
        state.config.accumulated_usage = accumulated_usage;
        state.config.last_prompt_tokens = last_prompt_tokens;

        // total usage: 5+42 = 47 input, 10+7 = 17 output, 15+49 = 64 total
        assert_eq!(state.config.accumulated_usage.input_tokens, 47);
        assert_eq!(state.config.accumulated_usage.output_tokens, 17);
        assert_eq!(state.config.accumulated_usage.total_tokens, 64);
        // Most recent turn with usage is turn 2 → input_tokens = 42
        assert_eq!(state.config.last_prompt_tokens, Some(42));
    }

    #[test]
    fn last_prompt_tokens_none_when_no_turns_have_usage() {
        let mut state = SessionState::empty();
        state.turns.insert(
            0,
            Turn {
                created_at: TimestampMs::now(),
                undone: false,
                error: None,
                user_text: Some("no usage".into()),
                assistant_text: None,
                assistant_reasoning: None,
                tool_calls: Vec::new(),
                token_usage: None,
                tool_results: Vec::new(),
                displayed_images: Vec::new(),
            },
        );

        let mut accumulated_usage = TokenUsage::default();
        let mut last_prompt_tokens = None;
        for turn in state.turns.values() {
            if let Some(u) = turn.token_usage {
                accumulated_usage.input_tokens += u.input_tokens;
                accumulated_usage.output_tokens += u.output_tokens;
                accumulated_usage.total_tokens += u.total_tokens;
                last_prompt_tokens = Some(u.input_tokens);
            }
        }
        state.config.accumulated_usage = accumulated_usage;
        state.config.last_prompt_tokens = last_prompt_tokens;

        assert_eq!(state.config.accumulated_usage.input_tokens, 0);
        assert_eq!(state.config.last_prompt_tokens, None);
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

    // -- start_turn / undo_turns / redo_turns tests -----------------------

    #[test]
    fn start_turn_assigns_increasing_ids() {
        let mut state = SessionState::empty();
        let (id0, _) = state.start_turn(Some("first".into()));
        let (id1, _) = state.start_turn(Some("second".into()));
        assert_eq!(id0, 0);
        assert_eq!(id1, 1);
        assert_eq!(state.turns.len(), 2);
    }

    #[test]
    fn undo_turns_marks_range_and_returns_ids() {
        let mut state = SessionState::empty();
        let _ = state.start_turn(Some("user 1".into()));
        let _ = state.start_turn(Some("user 2".into()));
        assert!(state.turns.values().all(|t| !t.undone));

        let ids = state
            .undo_turns()
            .expect("undo_turns should find a user turn");
        assert_eq!(ids.len(), 1, "only the most recent user turn");
        assert!(state.turns.get(&1).unwrap().undone);
    }

    #[test]
    fn undo_turns_returns_none_when_no_user_turn() {
        let mut state = SessionState::empty();
        let _ = state.start_turn(None); // No user_text, only system
        assert!(state.undo_turns().is_none());
    }

    #[test]
    fn redo_turns_restores_undone_turns() {
        let mut state = SessionState::empty();
        let _ = state.start_turn(Some("user".into()));
        let ids = state.undo_turns().expect("undo succeeds");
        assert!(!ids.is_empty());

        let restored = state.redo_turns().expect("redo succeeds");
        assert_eq!(restored.len(), ids.len());
        assert!(state.turns.values().all(|t| !t.undone));
    }

    #[test]
    fn redo_turns_returns_none_when_nothing_to_redo() {
        let mut state = SessionState::empty();
        let _ = state.start_turn(Some("user".into()));
        assert!(state.redo_turns().is_none());
    }

    #[test]
    fn redo_turns_cleared_by_new_turn_start() {
        let mut state = SessionState::empty();
        let _ = state.start_turn(Some("first".into()));
        state.undo_turns();
        // New user turn clears redo stack
        let _ = state.start_turn(Some("second".into()));
        assert!(state.redo_turns().is_none());
    }

    // -- loaded_skill_bodies / context_cache field tests -------------------

    #[test]
    fn loaded_skill_bodies_default_is_empty() {
        let state = SessionState::empty();
        assert!(state.loaded_skill_bodies.is_empty());
    }

    #[test]
    fn context_cache_default_is_none() {
        let state = SessionState::empty();
        assert!(state.context_cache.is_none());
    }

    #[test]
    fn loaded_skill_bodies_survives_snapshot_round_trip() {
        let mut state = SessionState::empty();
        state.loaded_skill_bodies.push(LoadedSkill {
            name: "test".to_string(),
            body: "body content".to_string(),
        });

        let snap = state.snapshot();
        assert_eq!(snap.loaded_skill_bodies.len(), 1);
        assert_eq!(snap.loaded_skill_bodies[0].name, "test");

        let restored = SessionState::from_snapshot(snap, HashMap::new());
        assert_eq!(restored.loaded_skill_bodies.len(), 1);
        assert_eq!(restored.loaded_skill_bodies[0].name, "test");
        assert_eq!(restored.loaded_skill_bodies[0].body, "body content");
    }

    #[test]
    fn context_cache_survives_snapshot_round_trip() {
        let mut state = SessionState::empty();
        state.context_cache = Some((42, Arc::new("cached content".to_string())));

        let snap = state.snapshot();
        assert_eq!(
            snap.context_cache,
            Some((42, Arc::new("cached content".to_string())))
        );

        let restored = SessionState::from_snapshot(snap, HashMap::new());
        assert_eq!(
            restored.context_cache,
            Some((42, Arc::new("cached content".to_string())))
        );
    }
}
