//! Session configuration, snapshot, and metadata types with their
//! persistence conversions.
//!
//! [`SessionConfig`] holds a session's persistent fields; [`SessionSnapshot`]
//! is the worker-facing capture of the runtime state; [`SessionMetadata`] is
//! the daemon-facing summary. The `From` impls here are the single place the
//! field-level mapping between a [`SessionState`] / [`SessionRecord`] and the
//! metadata lives.

use super::{
    Arc, BTreeMap, ContextConfig, HashSet, LoadedSkill, PathBuf, ProjectToolSet, ReasoningProducer,
    SessionRecord, SessionState, SessionStatus, SessionSummary, SkillMeta, TokenUsage, Turn,
    WarmPolicy,
};

/// A snapshot of a session's state for the daemon's in-memory index, session
/// listings, and the `UpdateMetadata` command.
///
/// Carries only the fields the daemon needs to describe a session; runtime-only
/// state (subscribers, in-flight requests, turn contents) is dropped, and the
/// `PathBuf` working directory is stringified for the wire.
#[derive(Debug, Clone)]
pub struct SessionMetadata {
    /// Display name, or `None` for an untitled session.
    pub title: Option<String>,
    /// The selected model name, or `None` before one is chosen.
    pub selected_model: Option<String>,
    /// The per-session reasoning effort slug, or `None` when unset.
    pub reasoning_effort: Option<String>,
    /// The parent session's id for a sub-session, or `None` for a root session.
    pub parent_session_id: Option<u64>,
    /// Working directory for filesystem tools, stringified for the wire.
    pub working_dir: Option<String>,
    /// Unix-epoch-milliseconds creation timestamp.
    pub created_at: i64,
    /// Unix-epoch-milliseconds of the last modification; drives the
    /// newest-first session-list ordering.
    pub last_modified: i64,
    /// Number of turns in the session.
    pub turn_count: u32,
    /// The session's current status.
    pub status: SessionStatus,
    /// The tool groups active for this session.
    pub active_tool_groups: Vec<String>,
    /// The bound inference account's name, or `None` when unbound.
    pub account_name: Option<String>,
    /// The session-level accumulated token counter.
    pub accumulated_usage: TokenUsage,
    /// The model's context window size, once resolved.
    pub context_window: Option<u32>,
    /// `input_tokens` from the most recent provider response, used for the
    /// context-window progress display.
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
    /// Build the wire [`SessionSummary`] for this session, stamping the given
    /// `session_id` onto it.
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
    /// (accumulated usage, context window, last prompt tokens, and the
    /// `last_response_id` chain with its producer) are copied from the
    /// snapshot.  All other configuration (title, account, model, working
    /// dir, etc.) is preserved so that mid-request mutations are not
    /// silently clobbered.
    pub(in crate::sessions) fn apply_worker_snapshot(&mut self, snapshot: &SessionConfig) {
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
