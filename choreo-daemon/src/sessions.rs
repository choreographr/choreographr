use crate::broadcast::{ClientId, LagLimits, ReplyTarget, SubscriberSink, fan_out_evicting};
use crate::cache_warm::WarmPolicy;
use crate::context::{LoadedSkill, SkillMeta};
use crate::daemon::{DaemonCommand, ResolvedAccount};
use crate::db::{self, SessionRecord, write_session_retry, write_turn_retry};
use crate::mcp::{ProjectToolSet, SessionMcpOverlay};
use crate::providers::InferenceProvider;
use crate::requests::run_agent_loop;
use crate::tools::{ToolOutput, ToolRegistry};
use choreo_ai_protocols::model_reasoning_capability;
use choreo_keystore::ServiceCredential;
use choreo_proto::{
    AssistantToolCallRecord, ContextConfig, DaemonMessage, DaemonMessageType, DisplayedImageRecord,
    ImageReference, ReasoningArtifact, ReasoningProducer, SessionEvent, SessionStatus,
    SessionSummary, TimestampMs, TokenUsage, ToolResultRecord, Turn,
};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, mpsc};
use tracing::{debug, error, info, trace, warn};
use unicode_segmentation::UnicodeSegmentation;

/// Sentinel `stream_id` meaning "cancel whatever is currently active, regardless of its ID".
/// Used in child-session cancellation where we don't know the child's active request ID.
pub(crate) const CANCEL_ALL: u64 = 0;

/// Maximum length of a session title in grapheme clusters (user-perceived
/// characters), not bytes or Unicode scalar values.  Titles are user-facing
/// display strings shown in session listings and the TUI sidebar, so
/// multi-byte scripts and composed emoji (e.g. "👨‍👩‍👧‍👦" = 1 grapheme, 7
/// `char` values) are treated fairly.  Defined here as the single source
/// of truth; the tool-level validator in `set_session_title.rs` imports
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
        || {
            handle
                .borrow()
                .as_ref()
                .is_some_and(std::thread::JoinHandle::is_finished)
        },
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

#[expect(
    clippy::large_enum_variant,
    reason = "a control-plane channel message enum whose variants carry large payloads (`DaemonMessage` broadcasts, per-request snapshots) by nature; boxing every payload variant would churn every construction and match site for a human-rate channel where throughput is not the bottleneck"
)]
pub enum SessionCommand {
    RunInput {
        input: Vec<u8>,
        /// The requester's reply target, when the run originated from a client
        /// request (`RunInput`/`ContinueGeneration`). The session thread sends
        /// the acceptance reply — a TARGETED `Started` on accept, a targeted
        /// `Failed` on reject — in addition to the unchanged broadcast stream.
        /// `None` for internally-requested runs (e.g. a child session's
        /// implicit continue) that have no client awaiting a reply.
        ///
        /// The run's `stream_id` is NOT carried here: the daemon assigns it on
        /// the session thread when it accepts the run, so a client never
        /// chooses one (the cross-client collision fix).
        reply: Option<ReplyTarget>,
    },
    RunChildInput {
        user_text: Option<String>,
        reply: std::sync::mpsc::Sender<io::Result<ChildResult>>,
    },
    Cancel {
        stream_id: u64,
    },
    SetModel {
        model: String,
        /// The requester's reply target: `Accepted` on success,
        /// `Failed { kind: SetModel }` on rejection. The `ModelSelected` /
        /// `ModelSelectionFailed` broadcasts still fire unchanged.
        reply: Option<ReplyTarget>,
    },
    StatusChanged(SessionStatus),
    Attach {
        client_id: ClientId,
        tx: SubscriberSink,
    },
    Detach {
        client_id: ClientId,
    },
    /// Remove a subscriber without detaching the session (used by the daemon
    /// when a client is evicted for lag or fully disconnects — the daemon
    /// knows the client's session memberships and cleans them up promptly
    /// instead of waiting for the next broadcast to notice the dead sink).
    RemoveSubscriber {
        client_id: ClientId,
    },
    GetSummary {
        reply: std::sync::mpsc::Sender<SessionSummary>,
    },
    RequestFinished {
        stream_id: u64,
        snapshot: SessionSnapshot,
    },
    /// Route a daemon message through the main session thread's subscriber
    /// map so that workers always broadcast to the live subscriber set
    /// rather than a stale clone of it.
    Broadcast(DaemonMessageType),
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
        /// The requester's reply target. Title changes are driven by the agent's
        /// `set_session_title` tool, which has no client request id, so this is
        /// `None` there; the slot exists so a client-originated title request
        /// can ack the same way as the other session mutations without another
        /// cross-crate change.
        reply: Option<ReplyTarget>,
    },
    /// Set the session working directory (authoritative state lives in the
    /// main loop, so this must be routed here rather than mutated on the
    /// request worker's throwaway copy).  Replies with the applied path once
    /// the change has been broadcast and persisted.
    SetWorkingDir {
        path: PathBuf,
        /// One-shot reply to the blocked `set_working_dir` tool caller (the
        /// tool's synchronous round-trip), carrying the applied path or the
        /// rejection reason.
        tool_reply: mpsc::Sender<Result<String, String>>,
        /// The client requester's reply target (see [`SessionCommand::SetTitle`]
        /// for why this is `None` on the current tool-driven path).
        reply: Option<ReplyTarget>,
    },
    /// Replace the session's MCP overlay: its private project/per-session tool
    /// wrappers plus the daemon-tier groups those project servers shadow. Pushed
    /// by the daemon command loop after it (re)resolves the session's project.
    /// Crucially NOT persisted into `active_tool_groups` — the overlay is
    /// recomputed on every working-directory change, so a moved directory can
    /// never strand a stale project group.
    SetMcpOverlay(Box<SessionMcpOverlay>),
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
        /// The requester's reply target: `Accepted` on success. The account's
        /// existence is verified on the connection thread BEFORE this command
        /// is sent, so the session thread only ever sees the success path.
        reply: Option<ReplyTarget>,
    },
    /// Drop the session's cached provider client so it is rebuilt lazily on
    /// the next request. Sent by the daemon command loop when the keystore
    /// locks, or a credential/account is removed or reconfigured — the
    /// cached client would otherwise keep dialing with a stale (possibly
    /// revoked) key. The session registry itself stays alive: the session
    /// keeps its own cancellable scope; only the client is discarded.
    DropProvider,
    /// Push the (non-secret) provider slug the daemon now knows for this
    /// session's account, so slug-keyed static catalog facts (context window,
    /// reasoning capability) stay exact after an external account edit without
    /// waiting for the next request to rebuild the client. `None` means the
    /// account was removed — clear the recorded slug. Sent alongside
    /// [`SessionCommand::DropProvider`] by the daemon's accounts-reload path.
    SetProviderSlug {
        slug: Option<String>,
    },
    SetReasoningEffort {
        effort: String,
        /// The requester's reply target: `Accepted` on success,
        /// `Failed { kind: SetReasoningEffort }` on rejection. The
        /// `ReasoningEffortSet` / `ReasoningEffortSetFailed` broadcasts still
        /// fire unchanged.
        reply: Option<ReplyTarget>,
    },
    GetReasoningEffort {
        reply: mpsc::Sender<String>,
    },
    /// Reply with the session's current full-state snapshot (`SessionState`) to
    /// answer a `GetSessionState` request. The requesting client does NOT attach
    /// (it just wants to look), so this is a read-only snapshot built by the
    /// authoritative session thread — the same `session_state_message` the
    /// attach push uses, so the wire shape cannot drift. The reply carries the
    /// daemon's own `io::Result` wrapper so the session thread can answer an
    /// active session directly, off the daemon command loop.
    GetState {
        reply: mpsc::Sender<io::Result<DaemonMessageType>>,
    },
    Undo {
        /// The requester's reply target: `Accepted` when turns were undone,
        /// `Failed { kind: Undo, error: "nothing to undo" }` when there was
        /// nothing to undo (closing the silent no-op gap).
        reply: Option<ReplyTarget>,
    },
    Redo {
        /// The requester's reply target: `Accepted` when turns were restored,
        /// `Failed { kind: Redo, error: "nothing to redo" }` otherwise.
        reply: Option<ReplyTarget>,
    },
    Shutdown,
}

/// Bundles parameters that are threaded through the session/request pipeline,
/// reducing argument count and making the dependency flow explicit.
#[derive(Clone)]
pub struct RequestContext {
    /// Channel to send `SessionCommands` back to the session main loop.
    pub cmd_tx: crossbeam_channel::Sender<SessionCommand>,
    /// The session ID scoping all operations.
    pub session_id: u64,
    /// Database handle for persisting state.
    pub db: Arc<redb::Database>,
    /// The daemon's live tool catalogue. Shared as the SAME
    /// `Arc<ArcSwap<…>>` across every session and request worker, so a
    /// list-changed rebuild on the command loop (the sole writer) is visible to
    /// a live session on its next load — no restart, no session respawn.
    pub tool_registry: Arc<arc_swap::ArcSwap<ToolRegistry>>,
    /// Channel to the daemon command loop.
    pub daemon_tx: crossbeam_channel::Sender<DaemonCommand>,
    /// Daemon-wide cap on agent tool-loop iterations per request (0 = unlimited).
    pub max_turns: u32,
    /// Lag thresholds for the lossless broadcast fan-out (see `crate::broadcast`).
    /// Session threads are producers in that fan-out, so they enforce the same
    /// per-client cap and global budget as the daemon command loop.
    pub lag_limits: LagLimits,
    /// Daemon-wide backlog counter, shared with every session thread and the
    /// daemon command loop (the 6th sanctioned shared-state exception).
    pub global_lag: Arc<AtomicUsize>,
    /// The daemon's Substrate credential, plumbed to the request worker so the
    /// `content` write tools can build a signing [`ChainAccount`].
    ///
    /// // TEMPORARY: this rides the Tool trait's single `x_credentials` slot
    /// (the same slot the X tools use), so only ONE credential can be active
    /// per session at a time. This is a stopgap until a proper tool→keystore
    /// credential-access system replaces it. Only populated when the
    /// `content` feature is compiled in (see daemon.rs `spawn_session`).
    pub substrate_credential: Option<ServiceCredential>,
    /// The resolved cache-warming policy for this session's account (see
    /// [`crate::cache_warm::WarmPolicy`]). Resolved once in `spawn_session`
    /// from the daemon's loaded `[cache_warming]` config plus the account's
    /// `meter`/`cache_warming`/`prompt_cache`, so no request re-reads or
    /// re-parses config. The agent loop spawns a warmer only when `mode ==
    /// Streaming`, so the default (off) policy costs nothing.
    pub warm_policy: WarmPolicy,
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
    /// Whether this session is pinned. The DAEMON is the sole authority for
    /// this flag (the session thread's `SessionConfig` does not carry it —
    /// `handle_update_metadata` preserves the daemon's value across the
    /// session thread's snapshots).
    pub pinned: bool,
    /// When this session was archived (Unix-epoch-milliseconds), or `None`.
    /// Daemon-owned, same authority/preserve contract as `pinned`.
    pub archived_at: Option<i64>,
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
        // The flags live on the record (daemon-owned), not on `SessionConfig`,
        // so restore them from the persisted record rather than the defaulted
        // `false`/`None` that `From<&SessionConfig>` produced.
        meta.pinned = record.pinned;
        meta.archived_at = record.archived_at;
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
            // Carry the daemon-owned flags so a record built from metadata
            // keeps whatever the daemon last set.
            pinned: meta.pinned,
            archived_at: meta.archived_at,
        }
    }
}

/// Capture a snapshot of `SessionState` as metadata for the daemon's
/// in-memory index or for sending through the command channel.
///
/// Fields that don't exist in [`SessionMetadata`] (subscribers, active
/// requests, turn contents, etc.) are dropped. The `PathBuf` `working_dir` is
/// stringified.
impl From<&SessionState> for SessionMetadata {
    fn from(state: &SessionState) -> Self {
        let mut meta = SessionMetadata::from(&state.config);
        // usize→u32 turn count: a session with 4 billion turns is impossible
        // in practice (each turn is a full provider round-trip).
        #[expect(clippy::cast_possible_truncation)]
        {
            meta.turn_count = state.turns.len() as u32;
        }
        meta
    }
}

impl SessionMetadata {
    #[must_use]
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
            pinned: self.pinned,
            archived_at: self.archived_at,
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
    /// call in the agent loop and restored only under the `ResponseId` policy.
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
    /// `SessionCommand` calls (`SetTitle`, `SetAccount`, `SetReasoningEffort`)
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
        self.last_response_id.clone_from(&snapshot.last_response_id);
        self.last_response_id_producer
            .clone_from(&snapshot.last_response_id_producer);
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
            // SessionConfig does NOT carry the daemon-owned flags (the daemon
            // is their authority — see `handle_update_metadata`'s preserve and
            // `From<SessionRecord>`); default them here.
            pinned: false,
            archived_at: None,
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
        record
            .last_response_id
            .clone_from(&state.config.last_response_id);
        record
            .last_response_id_producer
            .clone_from(&state.config.last_response_id_producer);
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
    /// The session's private MCP tool set (see `SessionState::project_tools`),
    /// carried across the worker snapshot so a request worker resolves and
    /// executes project tools exactly as the main loop does.
    pub project_tools: Arc<ProjectToolSet>,
    /// The shadowed daemon-tier MCP groups (see
    /// `SessionState::project_shadowed_groups`), carried across the snapshot.
    pub project_shadowed_groups: HashSet<String>,
    /// The recorded provider slug (see `SessionState::provider_slug`) —
    /// restored so slug-keyed catalog lookups survive the worker swap.
    pub provider_slug: Option<String>,
    /// The session's cache-warming policy (runtime, not persisted). Carried
    /// across the worker snapshot so the worker's agent loop reads the same
    /// policy the session resolved.
    pub warm_policy: WarmPolicy,
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
    /// The `turn_id` associated with this request, so that late-joining
    /// subscribers can route streaming chunks to the correct turn.
    pub(crate) turn_id: u32,
}

pub struct ActiveSessionEntry {
    pub cmd_tx: crossbeam_channel::Sender<SessionCommand>,
    pub handle: std::thread::JoinHandle<()>,
}

pub struct SessionState {
    pub config: SessionConfig,
    pub next_turn_id: u32,
    /// The next `stream_id` this session will assign to an accepted run. It is a
    /// per-session, monotonic counter owned by the session thread (never chosen
    /// by a client), so two clients' runs on the same session can never collide
    /// on a stream id. Starts at 1 so a real stream id is never the
    /// `CANCEL_ALL` sentinel (`0`).
    pub next_stream_id: u64,
    last_undo_turn_ids: Option<Vec<u32>>,
    pub turns: BTreeMap<u32, Turn>,
    subscribers: HashMap<ClientId, SubscriberSink>,
    pub(crate) active_requests: BTreeMap<u64, ActiveRequest>,
    pub provider: Option<InferenceProvider>,
    /// The account's **provider slug** (catalog key, e.g. "opencode-go"),
    /// recorded as soon as the account config resolves — at spawn time (the
    /// daemon command loop knows it from `AccountManager` with no credential
    /// involved), in [`SessionState::resolve_provider`], and on
    /// `SessionCommand::SetAccount` even before the keystore unlocks.
    ///
    /// Rationale: a model's static catalog facts (context window, reasoning
    /// capability) are pure catalog lookups keyed by the slug; they must NOT
    /// wait for the credential-bound `InferenceProvider` client to exist. The
    /// client is only built after unlock/first request, so resolving facts
    /// through it made the context window and effort cycling blink in and out
    /// of availability around keystore transitions.
    provider_slug: Option<String>,
    /// This session's cache-warming policy — resolved by the daemon per account
    /// (the global `[cache_warming]` merged with the account's `meter`/
    /// `cache_warming`/`prompt_cache`) and refreshed whenever the session
    /// (re-)resolves its account (lazy first resolve, account switch, accounts
    /// reload). The agent loop reads it to decide whether to spawn a warmer; see
    /// [`crate::cache_warm`].
    pub warm_policy: WarmPolicy,
    /// This session's provider-socket registry. The provider client built for
    /// this session registers every dialed socket here, so closing the
    /// registry (cancel / suspend / keystore-lock) force-closes THIS
    /// session's connections and nothing else's — cancellation granularity
    /// is exactly the session.
    ///
    /// Design note: the registry deliberately travels together with
    /// `provider` (and, in future, the connection pool) as one per-session
    /// triple — agent + pool + registry. Future async tool calls will reuse
    /// this same triple so their sockets land in the same cancellable scope.
    pub registry: choreo_ai_protocols::SocketRegistry,
    pub loaded_skill_bodies: Vec<LoadedSkill>,
    pub context_cache: Option<(u64, Arc<String>)>,
    pub discovered_skills: Option<Vec<SkillMeta>>,
    /// This session's private MCP tool set (project servers plus any
    /// `shared = false` per-session servers). Merged on top of the shared
    /// registry's definitions and consulted first on the execution path.
    pub project_tools: Arc<ProjectToolSet>,
    /// The daemon-tier `mcp/<slug>` groups this session's project shadows
    /// (removed from the shared registry's contribution to this session's tool
    /// list — a project server replaces the daemon-tier one by group).
    pub project_shadowed_groups: HashSet<String>,
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
    /// The effective provider slug for static catalog-fact lookups (context
    /// window, reasoning capability). Prefers the live provider client's slug;
    /// falls back to the `provider_slug` field recorded with the account config,
    /// which is available BEFORE the keystore unlocks (no credential needed).
    /// `None` only while no account is bound to the session.
    ///
    /// Named distinctly from the `provider_slug` field so a call site reads
    /// clearly: the field is the recorded fact, this resolves the one to use
    /// right now.
    fn effective_provider_slug(&self) -> Option<&str> {
        self.provider
            .as_ref()
            .map_or(self.provider_slug.as_deref(), |p| Some(p.provider_slug()))
    }

    /// Resolve the context window for `model`, keyed by provider slug rather
    /// than the credential-bound client, so the fact is available on a locked
    /// daemon / pre-first-request session. (Other static facts — e.g. the
    /// reasoning capability — resolve from [`Self::effective_provider_slug`].)
    ///
    /// With a live client this defers entirely to
    /// [`InferenceProvider::resolve_context_window`] (client-config override
    /// first, then the catalog through the client's slug). With no client — the
    /// locked/pre-first-request case — it is a pure catalog read keyed by the
    /// recorded slug, which is the whole reason the slug is stored.
    fn resolve_context_window_for_model(&self, model: &str) -> Option<u32> {
        match &self.provider {
            Some(provider) => provider.resolve_context_window(model),
            None => self
                .provider_slug
                .as_deref()
                .and_then(|slug| choreo_ai_protocols::lookup_context_window(slug, model)),
        }
    }

    /// Re-resolve context window from the catalog when the stored value
    /// is `None` (e.g. sessions created before a model was added to the
    /// catalog, or after the provider was lazily resolved on unlock).
    fn resolve_context_window_if_missing(&mut self, ctx: &RequestContext) {
        if self.config.context_window.is_some() {
            return;
        }
        let Some(model) = &self.config.selected_model else {
            return;
        };
        if let Some(cw) = self.resolve_context_window_for_model(model) {
            debug!(
                "session {}: re-resolved context_window={} for model={}",
                ctx.session_id, cw, model
            );
            self.config.context_window = Some(cw);
            broadcast(
                &mut self.subscribers,
                ctx,
                &DaemonMessageType::Session {
                    session_id: Some(ctx.session_id),
                    event: SessionEvent::ContextWindowResolved { context_window: cw },
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
            project_tools: Arc::clone(&self.project_tools),
            project_shadowed_groups: self.project_shadowed_groups.clone(),
            provider_slug: self.provider_slug.clone(),
            warm_policy: self.warm_policy,
        }
    }

    fn from_snapshot(
        snapshot: SessionSnapshot,
        subscribers: HashMap<ClientId, SubscriberSink>,
    ) -> Self {
        // usize→u32 turn count: a session with 4 billion turns is impossible
        // in practice (each turn is a full provider round-trip).
        #[expect(clippy::cast_possible_truncation)]
        let turn_count = snapshot.turns.len() as u32;
        Self {
            config: snapshot.config,
            next_turn_id: turn_count,
            // A restored worker snapshot never accepts runs itself (the main
            // session thread assigns stream ids), so the counter only needs a
            // sentinel-safe starting value here.
            next_stream_id: 1,
            last_undo_turn_ids: None,
            turns: snapshot.turns,
            subscribers,
            active_requests: BTreeMap::new(),
            provider: None,
            provider_slug: snapshot.provider_slug,
            warm_policy: snapshot.warm_policy,
            // Restored snapshots never carry a live provider client; the next
            // request rebuilds one lazily against this fresh registry.
            registry: choreo_ai_protocols::SocketRegistry::default(),
            loaded_skill_bodies: snapshot.loaded_skill_bodies,
            context_cache: snapshot.context_cache,
            discovered_skills: snapshot.discovered_skills,
            project_tools: snapshot.project_tools,
            project_shadowed_groups: snapshot.project_shadowed_groups,
        }
    }

    /// Build a [`SessionEvent::SessionState`] snapshot of the current session
    /// for broadcasting to connected clients.  Centralises the field mapping
    /// so that every broadcast site stays consistent when new fields are added.
    pub(crate) fn session_state_message(&self, session_id: u64) -> DaemonMessageType {
        let reasoning_capability = self.config.selected_model.as_ref().and_then(|model| {
            // Slug-keyed lookup (not the provider instance): the capability is
            // a static catalog fact and must be reported even while the
            // keystore is locked, so Alt+R works on an attached session
            // before any client has been built.
            let slug = self.effective_provider_slug()?;
            Some(model_reasoning_capability(slug, model))
        });
        DaemonMessageType::Session {
            session_id: Some(session_id),
            event: SessionEvent::SessionState {
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
            },
        }
    }

    /// Start a new turn, returning its `turn_id`.
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
                    image: None,
                })
                .collect();
        }
    }

    /// Set (or replace) a single tool result in place, matched by `call_id`,
    /// so the result keeps its position in the model's call order regardless
    /// of when the tool actually finished. Requires the turn to have been
    /// seeded via [`Self::seed_tool_results`]; otherwise it is a no-op.
    ///
    /// The five per-record fields (content, `is_error`, `invocation_description`,
    /// image) are collapsed into the `output` value so the signature stays
    /// under clippy's `too_many_arguments` threshold; `name` stays explicit
    /// because `ToolOutput` does not carry it (it comes from the tool call).
    pub fn update_tool_result(
        &mut self,
        turn_id: u32,
        call_id: &str,
        name: String,
        output: &ToolOutput,
    ) {
        if let Some(turn) = self.turns.get_mut(&turn_id)
            && let Some(record) = turn.tool_results.iter_mut().find(|r| r.call_id == call_id)
        {
            record.name = name;
            record.content.clone_from(&output.content);
            record.is_error = output.is_error;
            record
                .invocation_description
                .clone_from(&output.invocation_description);
            record.image.clone_from(&output.image_ref);
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
    /// outcome is unknown. `executed` holds the `call_ids` whose results were
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
    ///
    /// # Errors
    ///
    /// Returns Err if the turn does not exist, the session write fails
    /// after retries, or encoding/persisting any displayed image fails.
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
    /// higher-id turns as `undone = true`, store `turn_ids` for redo.
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
    #[must_use]
    pub fn empty() -> Self {
        Self {
            config: SessionConfig::default(),
            next_turn_id: 0,
            // Stream ids start at 1: 0 is the `CANCEL_ALL` sentinel, so a real
            // stream id must never be 0.
            next_stream_id: 1,
            last_undo_turn_ids: None,
            turns: BTreeMap::new(),
            subscribers: HashMap::new(),
            active_requests: BTreeMap::new(),
            provider: None,
            provider_slug: None,
            warm_policy: WarmPolicy::default(),
            // A fresh empty registry: any provider client built for this
            // state registers its sockets here, so cancelling this session
            // (or dropping it) never touches another session's connections.
            registry: choreo_ai_protocols::SocketRegistry::default(),
            loaded_skill_bodies: Vec::new(),
            context_cache: None,
            discovered_skills: None,
            project_tools: Arc::new(ProjectToolSet::empty()),
            project_shadowed_groups: HashSet::new(),
        }
    }

    /// Lazily resolve (and cache) this session's inference provider against
    /// THIS session's socket registry.
    ///
    /// Sessions can be created while the keystore is locked, so no provider
    /// exists at creation time; instead the first request asks the daemon
    /// command loop for the account's config + API key (the daemon is the
    /// sole owner of the credential map — the cleartext key never leaves it)
    /// and builds the client HERE, on the session thread, so every socket it
    /// dials lands in `self.registry`.
    ///
    /// Error strings are the SAME user-facing messages the old daemon-side
    /// cache produced, so clients see identical guidance.
    pub(crate) fn resolve_provider(
        &mut self,
        ctx: &RequestContext,
    ) -> Result<InferenceProvider, String> {
        if let Some(p) = &self.provider {
            return Ok(p.clone());
        }
        let Some(name) = self.config.account_name.clone() else {
            return Err(
                "no account configured on this session — use /account <name> to set one"
                    .to_string(),
            );
        };
        let (reply, rx) = crossbeam_channel::unbounded();
        let _ = ctx.daemon_tx.send(DaemonCommand::ResolveAccountCmd {
            account: name.clone(),
            reply,
        });
        // `None` from the daemon covers every failure the old provider cache
        // hid behind the same reply shape: unknown account, keystore locked,
        // or no credential stored. Keep the exact wording those paths used.
        // (Crossbeam per AGENTS.md: this reply may outlive the quick path —
        // e.g. a dropped channel when the session dies mid-request — and
        // Zeroizing wipes any unconsumed key from the queue on drop.)
        let Some(ResolvedAccount {
            config,
            api_key,
            warm_policy,
        }) = rx.recv().ok().flatten()
        else {
            // Unknown/locked account: reset to the conservative policy so a
            // removed account cannot keep warming on a stale meter.
            self.warm_policy = WarmPolicy::default();
            return Err(format!(
                "no credential stored for account '{name}' — add one via the AI Providers page or /add-key"
            ));
        };
        // The warm policy and provider slug are non-secret facts the daemon
        // resolved for this account; record them BEFORE the key check so
        // warming and static catalog lookups (context window, reasoning
        // capability) stay correct even on this failed resolution.
        self.warm_policy = warm_policy;
        self.provider_slug = Some(config.provider.clone());
        let Some(api_key) = api_key else {
            return Err(format!(
                "no credential stored for account '{name}' — add one via the AI Providers page or /add-key"
            ));
        };
        // The client constructor retains the key inside its HTTP config
        // anyway, so unwrapping the Zeroizing here is the ownership
        // transfer into the client's own zeroize-free storage — the wipe
        // protected the in-transit copy.
        let provider = InferenceProvider::from_account_config(
            &config,
            Some((*api_key).clone()),
            &self.registry,
        )
            .map_err(|e| {
                tracing::warn!(
                    session = ctx.session_id,
                    account = %name,
                    error = %e,
                    "failed to build provider client for session"
                );
                // Same user-facing message as the old unresolvable-cache path.
                format!(
                    "no credential stored for account '{name}' — add one via the AI Providers page or /add-key"
                )
            })?;
        debug!(session = ctx.session_id, account = %name, "resolved session provider lazily");
        self.provider = Some(provider.clone());
        Ok(provider)
    }
}

/// Client-bound copy of a turn with the opaque reasoning round-trip payload
/// and the image **bytes** stripped: only the daemon consumes
/// `reasoning_artifact`/`reasoning_producer` (it rebuilds the next provider
/// request from them), the vision image **bytes** in `ToolResultRecord.image`
/// (the request builder reads them from the authoritative daemon-side
/// `SessionState`/DB), and `DisplayedImageRecord.data` (the bytes live in the
/// DB; clients fetch them on demand via `ClientMessageType::GetImage`).
///
/// Image **metadata** stays on the client view for BOTH kinds: each
/// `DisplayedImageRecord` keeps its `metadata` (dimensions, mime, `byte_len`,
/// alt), and a `ToolResultRecord.image` keeps its `ImageReference` with `data`
/// emptied (path, mime, dimensions) — all the clients need to lay out a
/// placeholder and know there is an attachment to fetch, so a long session's
/// history no longer ships every image up front. Stripping the bytes keeps the
/// artifact and every image's bytes off each `DaemonMessage` payload
/// (bandwidth + privacy: thinking-block JSON, encrypted provider blobs, and
/// raw image bytes never ride the snapshots/broadcasts), while the
/// authoritative `Turn` in `SessionState` and the DB keeps the full payload for
/// the request builder and the image store.
pub(crate) fn turn_for_client(turn: &Turn) -> Turn {
    // Reconstruct the client turn FIELD BY FIELD instead of `turn.clone()`:
    // a deep clone would COPY every image payload (display + vision bytes, up
    // to megabytes) into the clone only for the lines below to throw it away.
    // This is the hot path — `session_state_message` runs it over every turn
    // of a session on open, and `broadcast_turn_appended` runs it after every
    // streamed tool result / image emit — so the redundant payload copy would
    // make an N-image turn O(N²) in server-side memory traffic (the wire is
    // already O(N) after stripping). Building the stripped records directly
    // never allocates the image buffers at all. (A new `Turn` field forces a
    // compile error here, which is deliberate: it must be triaged into the
    // client view rather than silently deep-cloned.)
    Turn {
        created_at: turn.created_at,
        undone: turn.undone,
        error: turn.error.clone(),
        user_text: turn.user_text.clone(),
        assistant_text: turn.assistant_text.clone(),
        assistant_reasoning: turn.assistant_reasoning.clone(),
        tool_calls: turn.tool_calls.clone(),
        token_usage: turn.token_usage,
        // Vision image BYTES are daemon/model-only: the request builder
        // consumes them from the authoritative daemon-side turn. But the client
        // still learns the image EXISTS (and its dimensions/mime) so it can
        // render a placeholder and fetch the bytes on demand — the reference
        // rides the client view with `data` emptied, exactly like a
        // `DisplayedImageRecord`'s bytes are stripped but its metadata kept.
        tool_results: turn
            .tool_results
            .iter()
            .map(|r| ToolResultRecord {
                call_id: r.call_id.clone(),
                name: r.name.clone(),
                content: r.content.clone(),
                is_error: r.is_error,
                invocation_description: r.invocation_description.clone(),
                image: r.image.as_ref().map(|img| ImageReference {
                    path: img.path.clone(),
                    mime_type: img.mime_type.clone(),
                    width: img.width,
                    height: img.height,
                    data: Vec::new(),
                }),
            })
            .collect(),
        // Displayed-image bytes are fetched on demand
        // (ClientMessageType::GetImage); the metadata (with `byte_len`) stays so
        // the client can size the placeholder and knows whether there is
        // anything to fetch.
        displayed_images: turn
            .displayed_images
            .iter()
            .map(|img| DisplayedImageRecord {
                metadata: img.metadata.clone(),
                data: Vec::new(),
                tool_call_id: img.tool_call_id.clone(),
            })
            .collect(),
        // Opaque reasoning round-trip payloads are daemon-only.
        reasoning_artifact: None,
        reasoning_producer: None,
    }
}

fn broadcast(
    subscribers: &mut HashMap<ClientId, SubscriberSink>,
    ctx: &RequestContext,
    message: &DaemonMessageType,
) {
    // Wrap the payload as a broadcast (`id: None`): session events fan out to
    // EVERY subscriber (and the all-activity bus), not to a single requester.
    let framed = DaemonMessage::broadcast(message.clone());
    // Forward to daemon-level activity subscribers so clients subscribed
    // to all session activity (e.g. the TUI after SubscribeAllActivity)
    // receive every session-scoped event without having to attach to every
    // session individually. The session thread KNOWS its own id, so the
    // origin is carried explicitly on the command for the daemon's
    // duplicate-suppression (it no longer re-derives the origin from the
    // message shape).
    let _ = ctx.daemon_tx.send(DaemonCommand::BroadcastActivity {
        session_id: Some(ctx.session_id),
        msg: framed.clone(),
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
        &framed,
        &ctx.lag_limits,
        &ctx.global_lag,
        |_id, _| false, // session subscribers are never duplicate-suppressed
    );
    for client_id in evict_clients {
        let _ = ctx.daemon_tx.send(DaemonCommand::EvictClient { client_id });
    }
    if evict_largest {
        let _ = ctx.daemon_tx.send(DaemonCommand::EvictLargestLagging);
    }
}

fn fail_request(
    subscribers: &mut HashMap<ClientId, SubscriberSink>,
    ctx: &RequestContext,
    session_id: u64,
    stream_id: u64,
    reply: Option<ReplyTarget>,
    error: impl Into<String>,
) -> bool {
    let error = error.into();
    // Targeted terminal failure to the requester: the request's one `id: Some`
    // reply. Sent BEFORE the broadcast below — the two ride the same per-client
    // writer queue, so the requester sees its rejection, then the broadcast
    // `Failed`.
    if let Some(target) = reply {
        target.fail(error.clone());
    }
    // Only the `Failed` broadcast: a REJECTED run never started, so there is no
    // stream to open. Broadcasting a `Started` here (the old `turn_id: 0`
    // shape) would register a phantom live stream on every subscriber until the
    // follow-up `Failed` cleared it; the `Failed` alone is the whole event.
    broadcast(
        subscribers,
        ctx,
        &DaemonMessageType::Session {
            session_id: Some(session_id),
            event: SessionEvent::Failed { stream_id, error },
        },
    );
    false
}

/// Notify the daemon of updated metadata and persist the session record
/// to the database.  Shared boilerplate used by session mutation handlers
/// (`SetTitle`, `SetAccount`, `SetModel`, etc.) so that changes are reflected
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

/// The default active tool-group set for a session that persisted none.
///
/// Mirrors the always-on groups (`core`, `git`, `shell`); the Coordination
/// Platform group is included only when the `content` feature is compiled in —
/// the group (and its tools) simply don't exist in a plain build, and
/// `load_tools`/`unload_tools` validation would reject it as unknown.
///
/// Note on stale persisted names: an existing session whose stored
/// `active_tool_groups` still contains the pre-rename `"coord"` (or any group
/// whose feature is off) keeps that name in its set — stale names are
/// *silently ignored*: a group with no registered tools contributes nothing to
/// `available_definitions`, so nothing is exposed to the model. Only explicit
/// `load_tools`/`unload_tools` requests validate names against the live
/// registry, so stale names can never be re-activated.
fn default_active_tool_groups() -> HashSet<String> {
    // `mut` is only needed when the `content` feature inserts its group.
    #[cfg_attr(not(feature = "content"), expect(unused_mut))]
    let mut groups = HashSet::from(["core".to_string(), "git".to_string(), "shell".to_string()]);
    #[cfg(feature = "content")]
    groups.insert("content".to_string());
    // The iOS group is PROTECTED (register_platform_tools) and unioned into
    // the active set at definition time regardless — listing it here is
    // belt-and-suspenders for display honesty (same rationale as the
    // CreateSession default list in daemon.rs).
    #[cfg(target_os = "ios")]
    groups.insert("ios".to_string());
    groups
}

pub fn session_main(
    rx: &crossbeam_channel::Receiver<SessionCommand>,
    initial_provider: Option<InferenceProvider>,
    registry: choreo_ai_protocols::SocketRegistry,
    account_name: Option<String>,
    // The account's provider slug (catalog key), resolved by the daemon
    // command loop from `AccountManager` WITHOUT touching credentials — a
    // non-secret fact supplied at spawn so session-thread catalog lookups
    // (context window, reasoning capability) work before the keystore
    // unlocks.
    provider_slug: Option<String>,
    // `init_record` is only read (title/model/effort/etc. are cloned out of
    // it); the caller in daemon.rs still owns the record it built.
    init_record: Option<&SessionRecord>,
    // Borrowed only: the context is read throughout the loop (the session
    // thread never outlives the caller's closure, which owns it).
    ctx: &RequestContext,
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
            .map_or_else(|| TimestampMs::now().as_millis(), |r| r.created_at),
        last_modified: init_record
            .as_ref()
            .map_or_else(|| TimestampMs::now().as_millis(), |r| r.last_modified),
        status: SessionStatus::Inactive,
        active_tool_groups: init_record
            .as_ref()
            .map(|r| r.active_tool_groups.iter().cloned().collect())
            .filter(|cats: &HashSet<String>| !cats.is_empty())
            .unwrap_or_else(default_active_tool_groups),
        context_config: init_record
            .as_ref()
            .map_or_default(|r| r.context_config.clone()),
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
        // `initial_provider` is `None` in production (the daemon always
        // resolves lazily on the session thread); tests may seed a provider
        // directly to avoid driving the daemon resolution round-trip.
        provider: initial_provider,
        provider_slug,
        registry,
        warm_policy: ctx.warm_policy,
        ..SessionState::empty()
    };

    // Re-resolve context window from the catalog when loading an existing
    // session whose stored context_window is None (e.g. sessions created
    // before a model was added to the catalog).
    state.resolve_context_window_if_missing(ctx);

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
        if process_command(cmd, &mut state, &mut shutdown_requested, ctx) {
            break;
        }
    }

    info!("session {} exiting", ctx.session_id);
    persist_and_exit(&state, &ctx.db, ctx.session_id, &ctx.daemon_tx);
}

mod handlers;
mod worker;

use handlers::process_command;
use worker::persist_and_exit;

#[cfg(test)]
mod tests;
