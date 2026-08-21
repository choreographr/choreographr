use crate::broadcast::{LagLimits, SubscriberSink, fan_out_evicting};
use crate::context::{LoadedSkill, SkillMeta};
use crate::daemon::DaemonCommand;
use crate::db::{self, SessionRecord, write_session_retry, write_turn_retry};
use crate::providers::InferenceProvider;
use crate::requests::run_agent_loop;
use crate::tools::ToolRegistry;
use choreo_ai_protocols::model_reasoning_capability;
use choreo_proto::{
    AssistantToolCallRecord, ContextConfig, DaemonMessage, DisplayedImageRecord, ReasoningArtifact,
    ReasoningProducer, SessionStatus, SessionSummary, TimestampMs, TokenUsage, ToolResultRecord,
    Turn,
};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io;
use std::path::PathBuf;
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, mpsc};
use tracing::{debug, error, info, trace, warn};
use unicode_segmentation::UnicodeSegmentation;

/// Sentinel `request_id` meaning "cancel whatever is currently active, regardless of its ID".
/// Used in child-session cancellation where we don't know the child's active request ID.
pub(crate) const CANCEL_ALL: u32 = 0;

/// Maximum length of a session title in grapheme clusters (user-perceived
/// characters), not bytes or Unicode scalar values.  Titles are user-facing
/// display strings shown in session listings and the TUI sidebar, so
/// multi-byte scripts and composed emoji (e.g. "👨‍👩‍👧‍👦" = 1 grapheme, 7
/// `char` values) are treated fairly.  Defined here as the single source
/// of truth; the tool-level validator in set_session_title.rs imports
/// this constant to avoid duplication.
pub(crate) const MAX_TITLE_CHARS: usize = 200;

/// Grace period allowed for a session thread to persist and exit after
/// `Shutdown` is signalled.  If a request worker is stuck in a provider
/// read that a cancel cannot interrupt, the session thread never receives
/// `RequestFinished` and would otherwise hang the daemon's shutdown join.
/// Abandoning the join is safe: per-turn state is persisted as turns
/// finalize, and process exit (or the delete path's finalize on
/// `SessionExited`) reaps the thread.
pub(crate) const SESSION_SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

/// Join a session thread after `Shutdown`, bounded by
/// [`SESSION_SHUTDOWN_GRACE`].  Returns `true` if the thread exited within
/// the grace period, `false` if it was abandoned (the caller then relies on
/// process exit — or the delete path's finalize-on-`SessionExited` — to reap
/// the thread).  Used by the daemon's lifecycle shutdown.
pub(crate) fn join_session_shutdown(handle: std::thread::JoinHandle<()>, session_id: u64) -> bool {
    poll_join_with_grace(
        handle,
        session_id,
        SESSION_SHUTDOWN_GRACE,
        std::time::Instant::now,
        std::thread::sleep,
    )
}

/// Poll a `JoinHandle`'s exit until it finishes or `grace` elapses, then reap
/// it via `join()`.  Returns whether the thread exited within the grace period.
///
/// The clock (`now`) and sleep are injected so unit tests can exercise both
/// outcomes deterministically — no real time-based waits.
fn poll_join_with_grace<F, S>(
    handle: std::thread::JoinHandle<()>,
    session_id: u64,
    grace: std::time::Duration,
    now: F,
    sleep: S,
) -> bool
where
    F: FnMut() -> std::time::Instant,
    S: FnMut(std::time::Duration),
{
    // The handle must be reachable from both the finish-check and the reap
    // closures, so it lives in a RefCell that is created and consumed on this
    // thread only (never shared across threads).
    let handle = std::cell::RefCell::new(Some(handle));
    shutdown_join_poll(
        session_id,
        grace,
        || handle.borrow().as_ref().is_some_and(|h| h.is_finished()),
        || {
            if let Some(h) = handle.borrow_mut().take() {
                let _ = h.join();
            }
        },
        now,
        sleep,
    )
}

/// Poll a thread-exit check until it passes or `grace` elapses.
///
/// `finished` reports whether the target has exited; `reap` is invoked once
/// when it has.  The clock and sleep are injected so unit tests can exercise
/// both outcomes deterministically — no real time-based waits.
fn shutdown_join_poll(
    session_id: u64,
    grace: std::time::Duration,
    mut finished: impl FnMut() -> bool,
    mut reap: impl FnMut(),
    mut now: impl FnMut() -> std::time::Instant,
    mut sleep: impl FnMut(std::time::Duration),
) -> bool {
    let deadline = now() + grace;
    loop {
        if finished() {
            // The thread exited; reap it now so resources are released.
            reap();
            return true;
        }
        // Poll every 50 ms, but never overshoot the deadline.
        let remaining = deadline.saturating_duration_since(now());
        if remaining.is_zero() {
            tracing::warn!(
                session_id,
                grace_ms = grace.as_millis(),
                "session thread did not exit within shutdown grace period; abandoning join \
                 (process exit will reap the thread; completed turns are already persisted)",
            );
            return false;
        }
        sleep(remaining.min(std::time::Duration::from_millis(50)));
    }
}

/// Test-only seam: bounded join with a caller-supplied grace period so
/// integration tests can exercise both outcomes in a few hundred ms instead
/// of waiting out the production 5s grace.  Uses the real clock and sleep.
///
/// Only compiled under the `test-utils` feature, which the crate's own
/// dev-dependency enables for test builds, so it never leaks into the
/// published public API.
#[cfg(feature = "test-utils")]
#[doc(hidden)]
pub fn join_session_shutdown_with_grace_for_test(
    handle: std::thread::JoinHandle<()>,
    session_id: u64,
    grace: std::time::Duration,
) -> bool {
    poll_join_with_grace(
        handle,
        session_id,
        grace,
        std::time::Instant::now,
        std::thread::sleep,
    )
}

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
        tx: SubscriberSink,
    },
    Detach {
        client_id: u64,
    },
    /// Remove a subscriber without detaching the session (used by the daemon
    /// when a client is evicted for lag or fully disconnects — the daemon
    /// knows the client's session memberships and cleans them up promptly
    /// instead of waiting for the next broadcast to notice the dead sink).
    RemoveSubscriber {
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
    /// Mid-turn token-usage sync from the request worker.  The worker owns
    /// the live accumulation (its private session clone), so the main
    /// thread's `config.accumulated_usage` would otherwise stay at the
    /// pre-request value until `RequestFinished` — leaking stale totals into
    /// attach snapshots and session summaries for the whole turn.  Applying
    /// the worker's cumulative total here (and re-broadcasting the update
    /// from the authoritative state) keeps every consumer fresh mid-turn.
    SyncAccumulatedUsage {
        token_usage: TokenUsage,
        last_prompt_tokens: Option<u32>,
    },
    SetTitle {
        title: String,
    },
    /// Set the session working directory (authoritative state lives in the
    /// main loop, so this must be routed here rather than mutated on the
    /// request worker's throwaway copy).  Replies with the applied path once
    /// the change has been broadcast and persisted.
    SetWorkingDir {
        path: PathBuf,
        reply: mpsc::Sender<Result<String, String>>,
    },
    /// Activate tool groups on the authoritative active-group set, then
    /// reply to the caller with a summary of what changed.
    LoadTools {
        groups: Vec<String>,
        reply: mpsc::Sender<Result<String, String>>,
    },
    /// Deactivate tool groups on the authoritative active-group set, then
    /// reply to the caller with a summary of what changed.
    UnloadTools {
        groups: Vec<String>,
        reply: mpsc::Sender<Result<String, String>>,
    },
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
    /// Daemon-wide cap on agent tool-loop iterations per request (0 = unlimited).
    pub max_turns: u32,
    /// Lag thresholds for the lossless broadcast fan-out (see `crate::broadcast`).
    /// Session threads are producers in that fan-out, so they enforce the same
    /// per-client cap and global budget as the daemon command loop.
    pub lag_limits: LagLimits,
    /// Daemon-wide backlog counter, shared with every session thread and the
    /// daemon command loop (the 6th sanctioned shared-state exception).
    pub global_lag: Arc<AtomicUsize>,
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
    pub last_modified: i64,
    pub turn_count: u32,
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
            created_at: record.created_at,
            last_modified: record.last_modified,
            status: SessionStatus::Sleeping,
            active_tool_groups: record.active_tool_groups.into_iter().collect(),
            context_config: record.context_config,
            account_name: record.account_name,
            accumulated_usage: TokenUsage::default(),
            context_window: None,
            last_prompt_tokens: None,
            last_response_id: record.last_response_id,
            last_response_id_producer: record.last_response_id_producer,
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
            turn_count: meta.turn_count,
            created_at: meta.created_at,
            last_modified: meta.last_modified,
            active_tool_groups: meta.active_tool_groups,
            context_config: ContextConfig::default(),
            account_name: meta.account_name,
            // `SessionMetadata` deliberately does not carry response ids; the
            // state→record conversion below overrides this from the config.
            last_response_id: None,
            last_response_id_producer: None,
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
            last_modified: self.last_modified,
            turn_count: self.turn_count,
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
    pub created_at: i64,
    pub last_modified: i64,
    pub status: SessionStatus,
    pub active_tool_groups: HashSet<String>,
    pub context_config: ContextConfig,
    pub account_name: Option<String>,
    pub accumulated_usage: TokenUsage,
    pub context_window: Option<u32>,
    pub last_prompt_tokens: Option<u32>,
    /// Last provider response id, persisted so ResponseId-policy models
    /// (OpenAI/xAI Responses) can chain `previous_response_id` across user
    /// turns (phase 4c). Meaningless for other policies; set after every model
    /// call in the agent loop and restored only under the ResponseId policy.
    pub last_response_id: Option<String>,
    /// Which provider+model produced `last_response_id`. The builder restores
    /// the id only when the current provider+model matches (same provenance
    /// rule as reasoning artifacts) — a stale id persisted under a different
    /// provider must never be replayed into a service that does not recognize
    /// it.
    pub last_response_id_producer: Option<ReasoningProducer>,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            title: None,
            selected_model: None,
            reasoning_effort: None,
            parent_session_id: None,
            working_dir: None,
            created_at: 0,
            last_modified: 0,
            status: SessionStatus::Inactive,
            active_tool_groups: HashSet::new(),
            context_config: ContextConfig::default(),
            account_name: None,
            accumulated_usage: TokenUsage::default(),
            context_window: None,
            last_prompt_tokens: None,
            last_response_id: None,
            last_response_id_producer: None,
        }
    }
}

impl SessionConfig {
    /// Apply fields from a worker snapshot, preserving any fields
    /// that may have been mutated mid-request through direct
    /// `SessionCommand` calls (SetTitle, SetAccount, SetReasoningEffort)
    /// that the worker snapshot wouldn't know about.
    ///
    /// This is an allowlist — only fields the worker actually owns
    /// (accumulated usage, context window, last prompt tokens) are
    /// copied from the snapshot.  All other configuration (title,
    /// account, model, working dir, etc.) is preserved so that
    /// mid-request mutations are not silently clobbered.
    fn apply_worker_snapshot(&mut self, snapshot: &SessionConfig) {
        self.accumulated_usage = snapshot.accumulated_usage;
        self.context_window = snapshot.context_window;
        self.last_prompt_tokens = snapshot.last_prompt_tokens;
        // The worker writes last_response_id (+ its producer) after each model
        // call; they must survive the request boundary so ResponseId-policy
        // chaining works across user turns (phase 4c).
        self.last_response_id = snapshot.last_response_id.clone();
        self.last_response_id_producer = snapshot.last_response_id_producer.clone();
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
            last_modified: config.last_modified,
            turn_count: 0,
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
        // last_response_id (+ producer) is worker-owned runtime state that must
        // survive the record round-trip: it chains ResponseId-policy models
        // across user turns AND daemon restarts (phase 4c).
        record.last_response_id = state.config.last_response_id.clone();
        record.last_response_id_producer = state.config.last_response_id_producer.clone();
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
    /// Cancellation channel for this request. The sender is held here and
    /// dropped only when the request is torn down (`RequestFinished`), so it
    /// outlives every worker wait that `select!`s on it: a firing cancel arm
    /// in `recv_sse_event`/the concurrent collector always means a real
    /// cancel message, never a disconnect. `sleep_or_cancel` (retry backoff)
    /// is the one deliberate exception — it treats a (theoretically
    /// unreachable) disconnect as "proceed without cancellation" rather than
    /// aborting a retry loop.
    pub(crate) cancel_tx: crossbeam_channel::Sender<()>,
    /// The turn_id associated with this request, so that late-joining
    /// subscribers can route streaming chunks to the correct turn.
    pub(crate) turn_id: u32,
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
    subscribers: HashMap<u64, SubscriberSink>,
    pub(crate) active_requests: BTreeMap<u32, ActiveRequest>,
    pub provider: Option<InferenceProvider>,
    pub loaded_skill_bodies: Vec<LoadedSkill>,
    pub context_cache: Option<(u64, Arc<String>)>,
    pub discovered_skills: Option<Vec<SkillMeta>>,
}

/// The assistant response recorded onto a turn by the agent loop: display
/// text + reasoning, tool calls, token usage, and the opaque reasoning
/// round-trip artifact with its producing model (see `ReasoningArtifact`).
///
/// Bundled into one value so the artifact + producer travel as a unit and
/// call sites stay readable instead of threading eight positional arguments
/// through [`SessionState::set_assistant_response`].
#[derive(Debug, Clone, Default)]
pub struct AssistantResponse {
    pub text: Option<String>,
    pub reasoning: Option<String>,
    pub tool_calls: Vec<AssistantToolCallRecord>,
    pub token_usage: Option<TokenUsage>,
    pub reasoning_artifact: Option<ReasoningArtifact>,
    pub reasoning_producer: Option<ReasoningProducer>,
}

impl SessionState {
    /// Re-resolve context window from the catalog when the stored value
    /// is `None` (e.g. sessions created before a model was added to the
    /// catalog, or after the provider was lazily resolved on unlock).
    fn resolve_context_window_if_missing(&mut self, ctx: &RequestContext) {
        if self.config.context_window.is_some() {
            return;
        }
        let (Some(model), Some(provider)) = (&self.config.selected_model, &self.provider) else {
            return;
        };
        if let Some(cw) = provider.resolve_context_window(model) {
            debug!(
                "session {}: re-resolved context_window={} for model={}",
                ctx.session_id, cw, model
            );
            self.config.context_window = Some(cw);
            broadcast(
                &mut self.subscribers,
                ctx,
                DaemonMessage::ContextWindowResolved {
                    session_id: ctx.session_id,
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

    fn from_snapshot(snapshot: SessionSnapshot, subscribers: HashMap<u64, SubscriberSink>) -> Self {
        let turn_count = snapshot.turns.len() as u32;
        Self {
            config: snapshot.config,
            next_turn_id: turn_count,
            last_undo_turn_ids: None,
            turns: snapshot.turns,
            subscribers,
            active_requests: BTreeMap::new(),
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
            turns: self
                .turns
                .iter()
                .map(|(&turn_id, turn)| (turn_id, turn_for_client(turn)))
                .collect(),
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
            reasoning_artifact: None,
            reasoning_producer: None,
        };
        self.turns.insert(turn_id, turn.clone());
        (turn_id, turn)
    }

    /// Set the assistant response on a turn (text or tool-use).
    ///
    /// The response's `reasoning_artifact`/`reasoning_producer` record the
    /// opaque reasoning round-trip payload and the model that produced it
    /// (phase 4b/4c). The producer is set whenever the model completes a
    /// response — even when the artifact is None (no reusable payload) — so
    /// the builder's same-model provenance check is well-defined for every
    /// turn. See [`AssistantResponse`].
    pub fn set_assistant_response(&mut self, turn_id: u32, response: AssistantResponse) {
        if let Some(turn) = self.turns.get_mut(&turn_id) {
            turn.assistant_text = response.text;
            turn.assistant_reasoning = response.reasoning;
            turn.tool_calls = response.tool_calls;
            turn.token_usage = response.token_usage;
            turn.reasoning_artifact = response.reasoning_artifact;
            turn.reasoning_producer = response.reasoning_producer;
        }
    }

    /// Seed placeholder tool results for every tool call, in call order, so
    /// the transcript always renders tool results in the order the model
    /// issued them — even while the tools are still running. Each placeholder
    /// is filled in place by [`Self::update_tool_result`] the moment its tool
    /// streams or finishes, so the rendered order never changes.
    ///
    /// `invocation_descriptions` runs parallel to `tool_calls` (same length,
    /// same order — both are derived from the same model response): seeding
    /// the description onto each placeholder means clients render the tool's
    /// context (e.g. "Running command: `…`.") from the moment the turn is
    /// broadcast, matching the final record's header exactly.
    pub fn seed_tool_results(
        &mut self,
        turn_id: u32,
        tool_calls: &[AssistantToolCallRecord],
        invocation_descriptions: &[String],
    ) {
        if let Some(turn) = self.turns.get_mut(&turn_id) {
            turn.tool_results = tool_calls
                .iter()
                .zip(invocation_descriptions.iter())
                .map(|(tc, desc)| ToolResultRecord {
                    call_id: tc.call_id.clone(),
                    name: tc.name.clone(),
                    content: String::new(),
                    is_error: false,
                    invocation_description: desc.clone(),
                })
                .collect();
        }
    }

    /// Set (or replace) a single tool result in place, matched by `call_id`,
    /// so the result keeps its position in the model's call order regardless
    /// of when the tool actually finished. Requires the turn to have been
    /// seeded via [`Self::seed_tool_results`]; otherwise it is a no-op.
    pub fn update_tool_result(
        &mut self,
        turn_id: u32,
        call_id: &str,
        name: String,
        content: String,
        is_error: bool,
        invocation_description: String,
    ) {
        if let Some(turn) = self.turns.get_mut(&turn_id)
            && let Some(record) = turn.tool_results.iter_mut().find(|r| r.call_id == call_id)
        {
            record.name = name;
            record.content = content;
            record.is_error = is_error;
            record.invocation_description = invocation_description;
        }
    }

    /// Mark the tool results whose outcome was never recorded because the
    /// request was cancelled.
    ///
    /// Placeholders are seeded for every call before any tool executes, so a
    /// request cancelled mid-execution (e.g. Escape during the serial phase)
    /// leaves empty slots for calls that never ran *and* for calls that were
    /// dispatched but still running when the request stopped. Fill them with
    /// an explicit marker so the transcript shows what happened and the next
    /// provider request does not carry empty tool messages for calls whose
    /// outcome is unknown. `executed` holds the call_ids whose results were
    /// actually recorded; every other placeholder is marked.
    pub fn mark_unexecuted_tool_results(&mut self, turn_id: u32, executed: &HashSet<String>) {
        if let Some(turn) = self.turns.get_mut(&turn_id) {
            for record in &mut turn.tool_results {
                if !executed.contains(&record.call_id) {
                    record.content = "[cancelled — result not recorded]".to_string();
                    record.is_error = true;
                }
            }
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
            active_requests: BTreeMap::new(),
            provider: None,
            loaded_skill_bodies: Vec::new(),
            context_cache: None,
            discovered_skills: None,
        }
    }
}

/// Client-bound copy of a turn with the opaque reasoning round-trip payload
/// stripped: only the daemon consumes `reasoning_artifact`/`reasoning_producer`
/// (it rebuilds the next provider request from them); clients render
/// `assistant_reasoning` and never need the artifact bytes.  Stripping here
/// keeps the artifact off every `DaemonMessage` payload (bandwidth + privacy:
/// thinking-block JSON and encrypted provider blobs never leave the daemon
/// process), while the authoritative `Turn` in `SessionState` and the DB keeps
/// the full payload for the next request's builder.
pub(crate) fn turn_for_client(turn: &Turn) -> Turn {
    let mut clone = turn.clone();
    clone.reasoning_artifact = None;
    clone.reasoning_producer = None;
    clone
}

fn broadcast(
    subscribers: &mut HashMap<u64, SubscriberSink>,
    ctx: &RequestContext,
    message: DaemonMessage,
) {
    // Forward to daemon-level activity subscribers so clients subscribed
    // to all session activity (e.g. the TUI after SubscribeAllActivity)
    // receive every session-scoped event without having to attach to every
    // session individually. The session thread KNOWS its own id, so the
    // origin is carried explicitly on the command for the daemon's
    // duplicate-suppression (it no longer re-derives the origin from the
    // message shape).
    let _ = ctx.daemon_tx.send(DaemonCommand::BroadcastActivity {
        session_id: Some(ctx.session_id),
        msg: message.clone(),
    });

    // Lossless + lag-eviction via the ONE shared policy — the same
    // [`crate::broadcast::fan_out_evicting`] the daemon's summary/activity
    // broadcasts use, so the three fan-outs cannot drift. Every message is
    // enqueued into each subscriber's UNBOUNDED queue (never dropped, never
    // stalling this session thread), and a subscriber whose queue crossed
    // the lag limits is evicted. Eviction is signalled to the daemon (which
    // owns the connection) rather than done here: this thread holds only the
    // sink, so it sends `EvictClient`/`EvictLargestLagging` commands and the
    // daemon tears the connection down.
    let (evict_clients, evict_largest) = fan_out_evicting(
        subscribers,
        &message,
        &ctx.lag_limits,
        &ctx.global_lag,
        |_| false, // session subscribers are never duplicate-suppressed
    );
    for client_id in evict_clients {
        let _ = ctx.daemon_tx.send(DaemonCommand::EvictClient { client_id });
    }
    if evict_largest {
        let _ = ctx.daemon_tx.send(DaemonCommand::EvictLargestLagging);
    }
}

fn fail_request(
    subscribers: &mut HashMap<u64, SubscriberSink>,
    ctx: &RequestContext,
    session_id: u64,
    request_id: u32,
    error: impl Into<String>,
) -> bool {
    broadcast(
        subscribers,
        ctx,
        DaemonMessage::Started {
            session_id,
            request_id,
            turn_id: 0,
            estimated_prompt_tokens: 0,
        },
    );
    broadcast(
        subscribers,
        ctx,
        DaemonMessage::Failed {
            session_id,
            request_id,
            error: error.into(),
        },
    );
    false
}

/// Notify the daemon of updated metadata and persist the session record
/// to the database.  Shared boilerplate used by session mutation handlers
/// (SetTitle, SetAccount, SetModel, etc.) so that changes are reflected
/// in session listings immediately and survive daemon restarts.
fn persist_session_metadata(state: &mut SessionState, ctx: &RequestContext, label: &str) {
    // Any metadata mutation is a modification: bump the timestamp so the
    // sessions list reorders (newest first) the moment the daemon index and
    // the persisted record are updated.
    let now = TimestampMs::now().as_millis();
    state.config.last_modified = state.config.last_modified.max(now);
    let _ = ctx.daemon_tx.send(DaemonCommand::UpdateMetadata {
        session_id: ctx.session_id,
        metadata: SessionMetadata::from(&*state),
    });
    let record = SessionRecord::from(&*state);
    if let Err(e) = write_session_retry(&ctx.db, ctx.session_id, &record) {
        warn!(error = %e, "failed to persist session record after {label}");
    }
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
        created_at: init_record
            .as_ref()
            .map(|r| r.created_at)
            .unwrap_or_else(|| TimestampMs::now().as_millis()),
        last_modified: init_record
            .as_ref()
            .map(|r| r.last_modified)
            .unwrap_or_else(|| TimestampMs::now().as_millis()),
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
        last_response_id: init_record
            .as_ref()
            .and_then(|r| r.last_response_id.clone()),
        last_response_id_producer: init_record
            .as_ref()
            .and_then(|r| r.last_response_id_producer.clone()),
    };
    let mut state = SessionState {
        config,
        provider,
        ..SessionState::empty()
    };

    // Re-resolve context window from the catalog when loading an existing
    // session whose stored context_window is None (e.g. sessions created
    // before a model was added to the catalog).
    state.resolve_context_window_if_missing(&ctx);

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
        SessionCommand::RemoveSubscriber { client_id } => {
            handle_remove_subscriber(client_id, state, shutdown_requested, ctx)
        }
        SessionCommand::GetSummary { reply } => handle_get_summary(reply, state, ctx),
        SessionCommand::RequestFinished {
            request_id,
            snapshot,
        } => handle_request_finished(request_id, snapshot, state, shutdown_requested, ctx),
        SessionCommand::Broadcast(message) => handle_broadcast(message, state, ctx),
        SessionCommand::SyncAccumulatedUsage {
            token_usage,
            last_prompt_tokens,
        } => handle_sync_accumulated_usage(token_usage, last_prompt_tokens, state, ctx),
        SessionCommand::SetTitle { title } => handle_set_title(title, state, ctx),
        SessionCommand::SetWorkingDir { path, reply } => {
            handle_set_working_dir(path, reply, state, ctx)
        }
        SessionCommand::LoadTools { groups, reply } => handle_load_tools(groups, reply, state, ctx),
        SessionCommand::UnloadTools { groups, reply } => {
            handle_unload_tools(groups, reply, state, ctx)
        }
        SessionCommand::SetAccount { name } => handle_set_account(name, state, ctx),
        SessionCommand::SetReasoningEffort { effort } => {
            handle_set_reasoning_effort(effort, state, ctx)
        }
        SessionCommand::GetReasoningEffort { reply } => {
            handle_get_reasoning_effort(reply, state, ctx)
        }
        SessionCommand::Undo => handle_undo(state, ctx),
        SessionCommand::Redo => handle_redo(state, ctx),
        SessionCommand::Shutdown => handle_shutdown(state, shutdown_requested, ctx),
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
        return fail_request(
            &mut state.subscribers,
            ctx,
            ctx.session_id,
            request_id,
            "empty input",
        );
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
                state.resolve_context_window_if_missing(ctx);
                let Some(p) = state.provider.as_ref() else {
                    return fail_request(
                        &mut state.subscribers,
                        ctx,
                        ctx.session_id,
                        request_id,
                        "internal error: provider not set after resolution".to_string(),
                    );
                };
                p.clone()
            }
            _ => {
                return fail_request(
                    &mut state.subscribers,
                    ctx,
                    ctx.session_id,
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
            ctx,
            ctx.session_id,
            request_id,
            "no account configured on this session — use /account <name> to set one",
        );
    };
    let model = match &state.config.selected_model {
        Some(m) => m.clone(),
        None => {
            return fail_request(
                &mut state.subscribers,
                ctx,
                ctx.session_id,
                request_id,
                "no model selected",
            );
        }
    };
    if *shutdown_requested {
        return fail_request(
            &mut state.subscribers,
            ctx,
            ctx.session_id,
            request_id,
            "session is shutting down",
        );
    }
    if !state.active_requests.is_empty() {
        return fail_request(
            &mut state.subscribers,
            ctx,
            ctx.session_id,
            request_id,
            "session already has an active request",
        );
    }

    broadcast(
        &mut state.subscribers,
        ctx,
        DaemonMessage::Started {
            session_id: ctx.session_id,
            request_id,
            turn_id: state.next_turn_id,
            estimated_prompt_tokens: 0,
        },
    );
    let (cancel_tx, cancel_rx) = crossbeam_channel::unbounded::<()>();
    state.active_requests.insert(
        request_id,
        ActiveRequest {
            cancel_tx,
            turn_id: state.next_turn_id,
        },
    );

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
        ctx,
        DaemonMessage::Started {
            session_id: ctx.session_id,
            request_id,
            turn_id: state.next_turn_id,
            estimated_prompt_tokens: 0,
        },
    );
    let (cancel_tx, cancel_rx) = crossbeam_channel::unbounded::<()>();
    state.active_requests.insert(
        request_id,
        ActiveRequest {
            cancel_tx,
            turn_id: state.next_turn_id,
        },
    );
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
fn handle_cancel(request_id: u32, state: &mut SessionState, ctx: &RequestContext) -> bool {
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
                ctx,
                DaemonMessage::Cancelled {
                    session_id: ctx.session_id,
                    request_id: rid,
                },
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
            ctx,
            DaemonMessage::ModelSelectionFailed {
                session_id: ctx.session_id,
                model,
                error: msg,
            },
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
            ctx,
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
            ctx,
            DaemonMessage::ReasoningEffortSet {
                session_id: ctx.session_id,
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
        ctx,
        DaemonMessage::ModelSelected {
            session_id: ctx.session_id,
            model: model.clone(),
            reasoning_capability: capability,
        },
    );
    persist_session_metadata(state, ctx, "SetModel");
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
///
/// Status transitions (Inference, ToolCall, Retrying) are internal pipeline
/// churn, NOT user-visible modifications — the status is refreshed everywhere
/// but `last_modified` is deliberately left untouched so the sessions list
/// does not re-sort on every tool call mid-request.  Only completed requests
/// (`handle_request_finished`) and explicit metadata edits
/// (`persist_session_metadata`) bump the timestamp.  The message carries the
/// session's *current* `last_modified`, so clients' monotonic `max()` guards
/// keep the value stable.
fn handle_status_changed(
    new_status: SessionStatus,
    state: &mut SessionState,
    ctx: &RequestContext,
) -> bool {
    state.config.status = new_status.clone();
    let last_modified = state.config.last_modified;
    broadcast(
        &mut state.subscribers,
        ctx,
        DaemonMessage::SessionStatusChanged {
            session_id: ctx.session_id,
            status: new_status.clone(),
            last_modified,
        },
    );
    let _ = ctx.daemon_tx.send(DaemonCommand::BroadcastSessionStatus {
        session_id: ctx.session_id,
        status: new_status,
    });
    false
}

/// Attach a client to this session, sending the full session state snapshot.
///
/// If the session has active requests when the client attaches (i.e. the new
/// client is joining mid-stream), synthetic `Started` messages are sent first
/// so the client can populate its `request_id → turn_id` mapping and route
/// subsequent streaming chunks (`OutputChunk`, `ToolResultChunk`, etc.) to
/// the correct turn — without this, those chunks would be silently dropped.
fn handle_attach(
    client_id: u64,
    tx: SubscriberSink,
    state: &mut SessionState,
    ctx: &RequestContext,
) -> bool {
    info!("session {}: client {} attached", ctx.session_id, client_id);
    state.subscribers.insert(client_id, tx);

    // Notify the daemon so it can filter duplicate delivery through the
    // activity subscriber path for this client/session pair.
    let _ = ctx.daemon_tx.send(DaemonCommand::TrackSessionSubscription {
        client_id,
        session_id: ctx.session_id,
    });

    // Send synthetic Started messages for every active request so the
    // new subscriber can route in-flight streaming chunks to the correct
    // turn.  The subscriber will then populate its request_to_turn map
    // and begin accumulating streaming content from this point forward.
    //
    // These are sent directly to the joining client only — the message
    // is not forwarded through broadcast_activity because the client is
    // already a subscriber of this session via the per-session path, and
    // the broadcast_activity filter in the daemon won't see this message
    // anyway (it's sent directly, not through session's broadcast()).
    //
    // Both these and the snapshot below go through `send_unchecked`:
    // with lossless unbounded channels they are GUARANTEED to arrive, but
    // a freshly-attached client's one-shot snapshot must not trip the lag
    // cap (a large snapshot is not evidence of a lagging client), and the
    // byte accounting still keeps the writer thread's per-dequeue
    // decrement balanced.
    if !state.active_requests.is_empty()
        && let Some(tx) = state.subscribers.get(&client_id)
    {
        for (&request_id, active) in &state.active_requests {
            tx.send_unchecked(
                &DaemonMessage::Started {
                    session_id: ctx.session_id,
                    request_id,
                    turn_id: active.turn_id,
                    estimated_prompt_tokens: 0,
                },
                &ctx.global_lag,
            );
        }
    }

    // The snapshot is the new client's only complete view of accumulated
    // content, so lossless delivery matters more here than anywhere else —
    // and the unbounded channel makes it guaranteed (the old code could
    // drop it on a full 128-slot buffer).
    let snapshot = state.session_state_message(ctx.session_id);
    if let Some(tx) = state.subscribers.get(&client_id) {
        tx.send_unchecked(&snapshot, &ctx.global_lag);
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
    state.subscribers.remove(&client_id);

    // Notify the daemon so it can stop filtering duplicates through
    // the activity subscriber path for this client/session pair.
    let _ = ctx
        .daemon_tx
        .send(DaemonCommand::UntrackSessionSubscription {
            client_id,
            session_id: ctx.session_id,
        });
    state.active_requests.is_empty() && (state.subscribers.is_empty() || *shutdown_requested)
}

/// Remove a subscriber at the daemon's request (client evicted for lag or
/// fully disconnected). Mirrors [`handle_detach`] but does NOT send
/// `UntrackSessionSubscription` — the daemon already removed the client from
/// its own tracking in `client_subscribed_sessions` when it initiated the
/// eviction/cleanup, and sending the untrack here would race the daemon's own
/// removal. The exit predicate is the same as detach: a session with no
/// subscribers and no active requests (and not mid-shutdown) can exit.
fn handle_remove_subscriber(
    client_id: u64,
    state: &mut SessionState,
    shutdown_requested: &bool,
    ctx: &RequestContext,
) -> bool {
    debug!(
        "session {}: removing subscriber {}",
        ctx.session_id, client_id
    );
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
        last_modified: state.config.last_modified,
        turn_count: state.turns.len() as u32,
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
    mut snapshot: SessionSnapshot,
    state: &mut SessionState,
    shutdown_requested: &bool,
    ctx: &RequestContext,
) -> bool {
    // An undo processed while the request worker was in flight leaves the
    // worker's snapshot stale in two ways: its turns carry no `undone` flags
    // (the child session never saw the undo), and its `last_response_id`
    // points at a response whose conversation includes the very turns being
    // undone. Detect that race by comparing undone-ness: any turn the live
    // state marks undone but the snapshot does not means the undo landed
    // after the request started. Preserve the undo by dropping the stale
    // response id before the snapshot's config is applied below (the undo
    // already persisted the cleared record), and by refusing to overwrite
    // those turns with the worker's pre-undo copies in the merge loop.
    let undo_during_request = snapshot.turns.iter().any(|(&turn_id, snap_turn)| {
        state
            .turns
            .get(&turn_id)
            .is_some_and(|state_turn| state_turn.undone && !snap_turn.undone)
    });
    if undo_during_request {
        debug!(
            session_id = ctx.session_id,
            request_id,
            "undo landed while request was in flight; dropping stale response-id chain from worker snapshot",
        );
        snapshot.config.last_response_id = None;
        snapshot.config.last_response_id_producer = None;
    }
    // Apply config changes from the worker snapshot using the allowlist
    // on `SessionConfig` so that fields mutated mid-request through direct
    // SessionCommand calls (SetTitle, SetAccount, SetReasoningEffort) are
    // preserved without needing an explicit save/restore list.
    state.config.apply_worker_snapshot(&snapshot.config);

    // A completed request is a modification: bump the timestamp exactly once,
    // BEFORE persisting the record and refreshing the daemon index, so the
    // on-disk record, the daemon index, and the broadcast all agree on the
    // same value.  `.max()` keeps the bump monotonic — a future-dated value
    // (clock skew) can never regress.
    let last_modified = TimestampMs::now().as_millis();
    state.config.last_modified = state.config.last_modified.max(last_modified);

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
        // Preserve an undo that landed mid-request: the worker's copy of a
        // turn the user hid carries no `undone` flag, so overwriting would
        // resurrect the hidden turn — and re-persisting it would resurrect
        // it on disk too. Keep the undone state instead (its content is
        // skipped by the message builder anyway).
        if !is_new
            && let Some(state_turn) = state.turns.get(&turn_id)
            && state_turn.undone
            && !turn.undone
        {
            continue;
        }
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
        ctx,
        DaemonMessage::SessionStatusChanged {
            session_id: ctx.session_id,
            status: SessionStatus::Inactive,
            last_modified,
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
    // Broadcast through the main session thread's live subscriber
    // map so that in-flight worker broadcasts respect detach.
    broadcast(&mut state.subscribers, ctx, message);
    false
}

/// Apply the request worker's mid-turn cumulative token usage to the
/// authoritative session config, then broadcast the update.
///
/// The agent loop accumulates usage on its private worker clone and only
/// merges it back at `RequestFinished`; without this sync the main
/// thread's `accumulated_usage` stays at the pre-request value for the
/// whole turn, leaking stale totals into attach snapshots and session
/// summaries.  `apply_worker_snapshot` at `RequestFinished` still applies
/// the final value, which is >= this one — the two paths are idempotent.
fn handle_sync_accumulated_usage(
    token_usage: TokenUsage,
    last_prompt_tokens: Option<u32>,
    state: &mut SessionState,
    ctx: &RequestContext,
) -> bool {
    // Merge, never overwrite: today a single FIFO worker per session makes a
    // blind assignment safe (the accumulated total only grows), but a
    // per-field max keeps the counter monotonic without resting on that
    // invariant — an out-of-order or overlapping sync can never regress a
    // total a client already saw.  [`TokenUsage::merge_max`] implements the
    // same policy the TUI applies to attach snapshots.
    state.config.accumulated_usage.merge_max(token_usage);
    if let Some(tokens) = last_prompt_tokens {
        state.config.last_prompt_tokens = Some(tokens);
    }
    // Broadcast through the live subscriber map AFTER the state write so
    // a client can never be ahead of the snapshot it receives on attach.
    broadcast(
        &mut state.subscribers,
        ctx,
        DaemonMessage::TokenUsageUpdate {
            session_id: ctx.session_id,
            token_usage: state.config.accumulated_usage,
            last_prompt_tokens: state.config.last_prompt_tokens,
        },
    );
    // Refresh the daemon's session-metadata index (no last_modified bump:
    // this is a data refresh, not a modification) so session-list / detail
    // token totals are accurate mid-turn on the next ListSessions.
    let _ = ctx.daemon_tx.send(DaemonCommand::UpdateMetadata {
        session_id: ctx.session_id,
        metadata: SessionMetadata::from(&*state),
    });
    false
}

/// Set the session title, broadcasting the change to subscribers and
/// notifying the daemon so session listings reflect the new title
/// immediately.
fn handle_set_title(title: String, state: &mut SessionState, ctx: &RequestContext) -> bool {
    // Defense-in-depth: cap title length by grapheme clusters so
    // multi-byte scripts and composed emoji are treated as single
    // user-perceived characters.  The tool-level validation in
    // set_session_title.rs catches this first; the session handler
    // is the second line of defence against any code path that sends
    // SetTitle directly (e.g. future internal commands).
    if title.graphemes(true).count() > MAX_TITLE_CHARS {
        warn!(
            session_id = ctx.session_id,
            length = title.graphemes(true).count(),
            max = MAX_TITLE_CHARS,
            "rejecting SetTitle: title too long (defense-in-depth)",
        );
        return false;
    }

    info!(
        session_id = ctx.session_id,
        old_title = ?state.config.title,
        new_title = %title,
        "session title changed",
    );
    state.config.title = Some(title.clone());

    // Broadcast to session subscribers (e.g. TUI) so they reflect the
    // new title immediately, without waiting for the next persist cycle.
    broadcast(
        &mut state.subscribers,
        ctx,
        DaemonMessage::SessionTitleSet {
            session_id: ctx.session_id,
            title: title.clone(),
        },
    );

    persist_session_metadata(state, ctx, "SetTitle");

    false
}

/// Set the session working directory, broadcasting the change to
/// subscribers and notifying the daemon so session listings reflect it
/// immediately.
///
/// This runs in the session's main loop, where the authoritative
/// `SessionConfig` lives — so the change survives the request and is picked
/// up by the next turn's snapshot.  (The pre-refactor implementation
/// mutated the request worker's throwaway copy, which was discarded at
/// request end, silently reverting the change.)  Replies with the canonical
/// path that was applied so the calling tool knows the round-trip succeeded.
fn handle_set_working_dir(
    path: PathBuf,
    reply: mpsc::Sender<Result<String, String>>,
    state: &mut SessionState,
    ctx: &RequestContext,
) -> bool {
    info!(
        session_id = ctx.session_id,
        old_path = ?state.config.working_dir,
        new_path = %path.display(),
        "session working directory changed",
    );
    state.config.working_dir = Some(path.clone());
    // Skills are discovered relative to the working directory — invalidate
    // the cache so they are re-discovered from the new location on the next
    // agent-loop turn.  (The system-prompt context cache is fingerprint-keyed
    // and self-invalidates when the working directory changes.)
    state.discovered_skills = None;

    // Broadcast to session subscribers (e.g. TUI) so they reflect the new
    // path immediately, without waiting for the next persist cycle.
    broadcast(
        &mut state.subscribers,
        ctx,
        DaemonMessage::SessionWorkingDirSet {
            session_id: ctx.session_id,
            path: Some(path.to_string_lossy().into_owned()),
        },
    );

    persist_session_metadata(state, ctx, "SetWorkingDir");

    let _ = reply.send(Ok(path.to_string_lossy().into_owned()));

    false
}

/// Activate tool groups on the authoritative session state, broadcast the
/// updated group set, persist, and reply to the calling tool with a summary
/// of what changed.
fn handle_load_tools(
    groups: Vec<String>,
    reply: mpsc::Sender<Result<String, String>>,
    state: &mut SessionState,
    ctx: &RequestContext,
) -> bool {
    info!(session_id = ctx.session_id, groups = ?groups, "session load_tools");

    // Defense-in-depth: the tool validates group names against the live
    // registry before sending, so unknown names normally never reach the
    // handler.  Re-validate here so a directly-sent command can never
    // persist a typo'd group into the authoritative active set.
    let known = ctx.tool_registry.known_group_names();
    if let Some(unknown) = crate::tools::unknown_group_names(&groups, &known) {
        let _ = reply.send(Err(format!(
            "Unknown tool group(s): {}",
            unknown.join(", ")
        )));
        return false;
    }

    let result =
        crate::tools::load_tools::apply_load_tools(&mut state.config.active_tool_groups, &groups);

    // Broadcast updated session state so the client (e.g. TUI status bar)
    // picks up the new active_tool_groups immediately.
    let session_state = state.session_state_message(ctx.session_id);
    broadcast(&mut state.subscribers, ctx, session_state);
    persist_session_metadata(state, ctx, "LoadTools");
    let _ = reply.send(Ok(result));

    false
}

/// Deactivate tool groups on the authoritative session state, broadcast the
/// updated group set, persist, and reply to the calling tool with a summary
/// of what changed.
fn handle_unload_tools(
    groups: Vec<String>,
    reply: mpsc::Sender<Result<String, String>>,
    state: &mut SessionState,
    ctx: &RequestContext,
) -> bool {
    info!(session_id = ctx.session_id, groups = ?groups, "session unload_tools");

    // Defense-in-depth: reject unknown group names (same rationale as
    // handle_load_tools).  "core" is known and handled below as protected.
    let known = ctx.tool_registry.known_group_names();
    if let Some(unknown) = crate::tools::unknown_group_names(&groups, &known) {
        let _ = reply.send(Err(format!(
            "Unknown tool group(s): {}",
            unknown.join(", ")
        )));
        return false;
    }

    let result = crate::tools::unload_tools::apply_unload_tools(
        &mut state.config.active_tool_groups,
        &groups,
    );

    // Broadcast updated session state so the client picks up the new
    // active_tool_groups immediately.
    let session_state = state.session_state_message(ctx.session_id);
    broadcast(&mut state.subscribers, ctx, session_state);
    persist_session_metadata(state, ctx, "UnloadTools");
    let _ = reply.send(Ok(result));

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
                    ctx,
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
        ctx,
        DaemonMessage::SessionAccountSet {
            session_id: ctx.session_id,
            account: name,
        },
    );
    persist_session_metadata(state, ctx, "SetAccount");
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
            ctx,
            DaemonMessage::ReasoningEffortSetFailed {
                session_id: ctx.session_id,
                effort,
                error: msg,
            },
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
            ctx,
            DaemonMessage::ReasoningEffortSet {
                session_id: ctx.session_id,
                effort,
            },
        );
        return false;
    } else {
        let model = state.config.selected_model.as_deref().unwrap_or("(none)");
        let msg = format!("model '{model}' does not support reasoning effort '{effort}'");
        warn!(session_id = ctx.session_id, error = %msg, "reasoning effort rejected");
        broadcast(
            &mut state.subscribers,
            ctx,
            DaemonMessage::ReasoningEffortSetFailed {
                session_id: ctx.session_id,
                effort,
                error: msg,
            },
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
    // Undoing turns invalidates the server-side response chain: the persisted
    // `previous_response_id` points at a response whose conversation includes
    // the turns being undone, so restoring it on the next request would leak
    // that context back into the model (the builder skips undone turns, but
    // the chain does not). Clear it (and its provenance) so the next request
    // falls back to a non-chained one carrying only the visible turns. Redo
    // deliberately does NOT restore the id — the turns come back, but the
    // chain is reset; a stateless request is always safe, and the old id was
    // already discarded.
    if state.config.last_response_id.is_some() {
        state.config.last_response_id = None;
        state.config.last_response_id_producer = None;
        // Persist the cleared id so a daemon restart cannot resurrect the
        // stale chain from the on-disk record. Writing the record directly
        // (rather than `persist_session_metadata`) keeps undo's observable
        // behavior unchanged: `last_modified` is not bumped, so the sessions
        // list does not reorder.
        let record = SessionRecord::from(&*state);
        if let Err(e) = write_session_retry(&ctx.db, ctx.session_id, &record) {
            tracing::warn!(error = %e, "failed to persist session record after Undo");
        }
    }
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
        ctx,
        DaemonMessage::TurnsUndone {
            session_id: ctx.session_id,
            turn_ids,
        },
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
    broadcast(
        &mut state.subscribers,
        ctx,
        DaemonMessage::TurnsRedone {
            session_id: ctx.session_id,
            turns: turns
                .iter()
                .map(|(&turn_id, turn)| (turn_id, turn_for_client(turn)))
                .collect(),
        },
    );
    false
}

/// Signal shutdown: cancel all active requests and check if the loop should exit.
fn handle_shutdown(
    state: &mut SessionState,
    shutdown_requested: &mut bool,
    ctx: &RequestContext,
) -> bool {
    *shutdown_requested = true;
    for (&request_id, active) in &state.active_requests {
        let _ = active.cancel_tx.send(());
        broadcast(
            &mut state.subscribers,
            ctx,
            DaemonMessage::Cancelled {
                session_id: ctx.session_id,
                request_id,
            },
        );
    }
    state.active_requests.is_empty()
}

#[expect(clippy::too_many_arguments)]
fn run_request_worker(
    request_id: u32,
    client: InferenceProvider,
    session: &mut SessionState,
    model: String,
    cancel_rx: crossbeam_channel::Receiver<()>,
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
                    session_id: ctx.session_id,
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
                    session_id: ctx.session_id,
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
mod tests;
