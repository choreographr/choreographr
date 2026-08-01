use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use tracing::warn;

/// Per-model reasoning capability information.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReasoningCapability {
    /// The effort level slugs this model supports (e.g. "off", "low",
    /// "medium", "high", "on", "xhigh", "max").
    /// Empty means reasoning is not supported.
    pub available_effort_levels: Vec<String>,
}

impl ReasoningCapability {
    /// Cycle from `current` to the next slug, wrapping around.
    /// Logs a warning if `current` is not found — indicates a desync
    /// between the caller's state and this capability set.
    pub fn cycle_from(&self, current: &str) -> Option<String> {
        if self.available_effort_levels.is_empty() {
            return None;
        }
        let pos = match self
            .available_effort_levels
            .iter()
            .position(|e| e == current)
        {
            Some(p) => p,
            None => {
                warn!(
                    "ReasoningCapability::cycle_from: current slug {current} not in available set {:?}, starting from 0",
                    self.available_effort_levels,
                );
                0
            }
        };
        let next = (pos + 1) % self.available_effort_levels.len();
        Some(self.available_effort_levels[next].clone())
    }
}

/// ContextConfig — controls file discovery for session context.
/// Moved here from choreographr so proto messages can carry it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextConfig {
    #[serde(default = "default_context_file_names")]
    pub context_file_names: Vec<String>,
    #[serde(default = "default_context_file_max_bytes")]
    pub context_file_max_bytes: usize,
    #[serde(default)]
    pub disable_claude_code_prompt: bool,
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            context_file_names: default_context_file_names(),
            context_file_max_bytes: default_context_file_max_bytes(),
            disable_claude_code_prompt: false,
        }
    }
}

fn default_context_file_names() -> Vec<String> {
    vec!["AGENTS.md".to_string(), "CLAUDE.md".to_string()]
}

fn default_context_file_max_bytes() -> usize {
    32 * 1024
}

/// Token usage for a single LLM turn or accumulated for a session.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct TokenUsage {
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub total_tokens: u32,
}

/// Unix-epoch-milliseconds timestamp.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimestampMs(i64);

impl TimestampMs {
    /// Sentinel value for when the real timestamp is unavailable (e.g. corrupt
    /// DB entries).
    pub const ZERO: Self = Self(0);

    pub fn now() -> Self {
        Self(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or_else(|_| {
                    tracing::warn!("system clock before UNIX_EPOCH, using 0");
                    0
                }),
        )
    }

    pub fn as_millis(&self) -> i64 {
        self.0
    }
}

/// A tool call that was discarded because the provider sent truncated or
/// otherwise invalid (non-JSON) arguments.
///
/// The `arguments_json` field holds the partial/cropped payload the provider
/// actually returned, making it easier to diagnose what went wrong without
/// digging through raw network logs.
#[derive(Debug, Clone, PartialEq)]
pub struct DiscardedToolCall {
    pub name: String,
    pub arguments_json: String,
}

impl std::fmt::Display for DiscardedToolCall {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {:?}", self.name, self.arguments_json)
    }
}

/// Unified error type for all inference providers.
/// NOTE: does NOT derive Serialize/Deserialize — this error type is never
/// sent over the wire.  Provider errors are stringified before being placed
/// into protocol messages (e.g. `DaemonMessage::Failed { error }`).
#[derive(Debug, thiserror::Error)]
pub enum InferenceError {
    #[error("unauthorized ({status}): {detail}")]
    Unauthorized { status: u16, detail: String },
    #[error("rate limited: {detail}")]
    RateLimited {
        retry_after_secs: Option<u64>,
        detail: String,
    },
    #[error("server error ({status}): {detail}")]
    ServerError { status: u16, detail: String },
    #[error("client error ({status}): {detail}")]
    ClientError { status: u16, detail: String },
    #[error("provider returned an empty response")]
    EmptyResponse,
    #[error("request cancelled during retry backoff")]
    Cancelled,
    #[error("tool call arguments truncated by provider: {}", .discarded.iter().map(|d| d.to_string()).collect::<Vec<_>>().join(", "))]
    TruncatedToolCall { discarded: Vec<DiscardedToolCall> },
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

impl From<InferenceError> for std::io::Error {
    fn from(e: InferenceError) -> Self {
        match e {
            InferenceError::Io(io) => io,
            other => std::io::Error::other(other.to_string()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AccountInfo {
    pub name: String,
    pub provider: String,
    pub has_credential: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DisplayedImageRecord {
    pub metadata: ImageMetadata,
    pub data: Vec<u8>,
    pub tool_call_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AssistantToolCallRecord {
    pub call_id: String,
    pub name: String,
    pub arguments_json: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolResultRecord {
    pub call_id: String,
    pub name: String,
    pub content: String,
    pub is_error: bool,
    pub invocation_description: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Turn {
    pub created_at: TimestampMs,
    pub undone: bool,
    pub error: Option<String>,
    pub user_text: Option<String>,
    pub assistant_text: Option<String>,
    pub assistant_reasoning: Option<String>,
    pub tool_calls: Vec<AssistantToolCallRecord>,
    pub token_usage: Option<TokenUsage>,
    pub tool_results: Vec<ToolResultRecord>,
    pub displayed_images: Vec<DisplayedImageRecord>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[non_exhaustive]
pub enum SessionStatus {
    Sleeping,
    /// Default initial state — session is loaded and ready but not processing.
    #[default]
    Inactive,
    Inference,
    ToolCall(String),
    /// The daemon received a retryable HTTP error (429/5xx/connection) and is
    /// waiting before the next attempt.  Displayed in the TUI so the user
    /// knows the model call hasn't stalled and can choose to cancel.
    Retrying {
        attempt: u32,
        max_attempts: u32,
        delay_ms: u64,
    },
}

impl SessionStatus {
    /// Returns `true` when the session is actively processing (inference,
    /// tool call, or retrying).  Returns `false` for idle states (inactive,
    /// sleeping).
    pub fn is_active(&self) -> bool {
        matches!(
            self,
            SessionStatus::Inference | SessionStatus::ToolCall(_) | SessionStatus::Retrying { .. }
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionSummary {
    pub session_id: u64,
    pub title: Option<String>,
    pub selected_model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub parent_session_id: Option<u64>,
    pub working_dir: Option<String>,
    /// Session creation time, Unix-epoch-milliseconds.
    pub created_at: i64,
    /// Most recent modification time, Unix-epoch-milliseconds.  Bumped by the
    /// daemon whenever the session's status, title, model, or turn count
    /// changes, and used to order the sessions list (newest first).
    pub last_modified: i64,
    pub turn_count: u32,
    pub max_turns: Option<u32>,
    pub status: SessionStatus,
    pub active_tool_groups: Vec<String>,
    /// The AI provider account name associated with this session, if any.
    pub account_name: Option<String>,
    /// Total token usage accumulated across all turns in this session.
    #[serde(default)]
    pub token_usage: Option<TokenUsage>,
    /// Model context window size for this session, if known.
    #[serde(default)]
    pub context_window: Option<u32>,
    /// The prompt_tokens from the most recent API response (the actual
    /// context size being sent to the model), if available.
    #[serde(default)]
    pub last_prompt_tokens: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[non_exhaustive]
pub enum ClientMessage {
    CreateSession {
        title: Option<String>,
        parent_session_id: Option<u64>,
        working_dir: Option<String>,
        max_turns: Option<u32>,
        context_config: Option<ContextConfig>,
        account_name: Option<String>,
        selected_model: Option<String>,
        reasoning_effort: Option<String>,
    },
    ListSessions,
    SubscribeSessionsSummary,
    UnsubscribeSessionsSummary,
    AttachSession {
        session_id: u64,
    },
    GetSessionState {
        session_id: u64,
    },
    RunInput {
        request_id: u32,
        input: Vec<u8>,
    },
    Cancel {
        request_id: u32,
    },
    Ping,
    GetCredential {
        service: String,
    },
    ListModels,
    SetModel {
        model: String,
    },
    Unlock {
        private_key: Vec<u8>,
    },
    Lock,
    AddCredential {
        service: String,
        encrypted_payload: Vec<u8>,
        unlock_key: Option<Vec<u8>>,
    },
    RemoveCredential {
        service: String,
    },
    DeleteSession {
        session_id: u64,
    },
    AddAccount {
        name: String,
        provider: String,
        base_url: Option<String>,
        streaming: Option<bool>,
        retry_max_attempts: Option<u32>,
        connect_timeout_secs: Option<u64>,
        request_timeout_secs: Option<u64>,
    },
    RemoveAccount {
        name: String,
    },
    ListAccounts,
    SetSessionAccount {
        name: String,
    },
    SetReasoningEffort {
        effort: String,
    },
    GetReasoningEffort,
    Undo,
    Redo,
    /// Create a new turn with the text "Continue." and run the agent loop.
    /// Semantically distinct from RunInput — the daemon controls the prompt text.
    ContinueGeneration {
        request_id: u32,
    },
    SubscribeAllActivity,
    UnsubscribeAllActivity,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[non_exhaustive]
pub enum OutputStream {
    Answer,
    Reasoning,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ImageMetadata {
    pub mime_type: String,
    pub width: u32,
    pub height: u32,
    pub byte_len: u64,
    pub alt: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[non_exhaustive]
pub enum DaemonMessage {
    SessionCreated {
        session_id: u64,
        title: Option<String>,
        parent_session_id: Option<u64>,
        working_dir: Option<String>,
        max_turns: Option<u32>,
        account_name: Option<String>,
        selected_model: Option<String>,
        reasoning_effort: Option<String>,
    },
    Sessions {
        sessions: Vec<SessionSummary>,
    },
    SessionAttached {
        session_id: u64,
    },
    SessionState {
        session_id: u64,
        title: Option<String>,
        selected_model: Option<String>,
        parent_session_id: Option<u64>,
        working_dir: Option<String>,
        max_turns: Option<u32>,
        turns: BTreeMap<u32, Turn>,
        active_tool_groups: Vec<String>,
        /// Accumulated token usage for this session, if available.
        #[serde(default)]
        token_usage: Option<TokenUsage>,
        /// Model context window size for this session, if known.
        #[serde(default)]
        context_window: Option<u32>,
        /// The prompt_tokens from the most recent API response (the actual
        /// context size being sent to the model), if available.
        #[serde(default)]
        last_prompt_tokens: Option<u32>,
        /// Current session status (Inactive, Inference, ToolCall, etc.).
        #[serde(default)]
        status: SessionStatus,
        #[serde(default)]
        reasoning_effort: Option<String>,
        #[serde(default)]
        reasoning_capability: Option<ReasoningCapability>,
    },
    TurnAppended {
        session_id: u64,
        turn_id: u32,
        turn: Turn,
    },
    TurnFinalized {
        session_id: u64,
        turn_id: u32,
        turn: Turn,
    },
    SessionStatusChanged {
        session_id: u64,
        status: SessionStatus,
        /// Unix-epoch-milliseconds timestamp of this status change, so the
        /// TUI can re-sort the sessions list (most recently modified first)
        /// without waiting for a fresh ListSessions round-trip.
        last_modified: i64,
    },
    SessionFailed {
        session_id: u64,
        operation: String,
        error: String,
    },
    Started {
        session_id: u64,
        request_id: u32,
        turn_id: u32,
        estimated_prompt_tokens: u32,
    },
    ToolCallStarted {
        session_id: u64,
        request_id: u32,
        call_id: String,
        tool_name: String,
        arguments_json: String,
    },
    ToolCallFinished {
        session_id: u64,
        request_id: u32,
        call_id: String,
        tool_name: String,
    },
    ToolResultChunk {
        session_id: u64,
        request_id: u32,
        call_id: String,
        data: Vec<u8>,
    },
    ToolCallFailed {
        session_id: u64,
        request_id: u32,
        call_id: String,
        tool_name: String,
        error: String,
    },
    TokenUsageUpdate {
        session_id: u64,
        token_usage: TokenUsage,
        last_prompt_tokens: Option<u32>,
    },
    /// Cumulative output-token estimate for the current turn, updated as
    /// each stream chunk arrives.  Used by the TUI for live token display.
    LiveOutputTokenCount {
        session_id: u64,
        request_id: u32,
        output_tokens: u32,
    },
    OutputChunk {
        session_id: u64,
        request_id: u32,
        stream: OutputStream,
        data: Vec<u8>,
    },
    Done {
        session_id: u64,
        request_id: u32,
        /// Token usage for the completed request, if reported by the provider.
        token_usage: Option<TokenUsage>,
        /// The prompt_tokens from the most recent API response (the actual
        /// context size that was sent to the model), if available.
        #[serde(default)]
        last_prompt_tokens: Option<u32>,
    },
    Failed {
        session_id: u64,
        request_id: u32,
        error: String,
    },
    Cancelled {
        session_id: u64,
        request_id: u32,
    },
    Pong,
    Models {
        models: Vec<String>,
        selected_model: Option<String>,
    },
    ModelsFailed {
        error: String,
    },
    ModelSelected {
        session_id: u64,
        model: String,
        #[serde(default)]
        reasoning_capability: Option<ReasoningCapability>,
    },
    ModelSelectionFailed {
        session_id: u64,
        model: String,
        error: String,
    },
    Unlocked,
    Locked,
    LockedError {
        error: String,
    },
    CredentialAdded {
        service: String,
    },
    CredentialAddFailed {
        service: String,
        error: String,
    },
    CredentialRemoved {
        service: String,
    },
    CredentialRemoveFailed {
        service: String,
        error: String,
    },
    SessionDeleted {
        session_id: u64,
    },
    SessionDeleteFailed {
        session_id: u64,
        error: String,
    },
    TurnsUndone {
        session_id: u64,
        turn_ids: Vec<u32>,
    },
    TurnsRedone {
        session_id: u64,
        turns: BTreeMap<u32, Turn>,
    },
    Credential {
        service: String,
        key: Option<String>,
    },
    AccountAdded {
        name: String,
    },
    AccountAddFailed {
        name: String,
        error: String,
    },
    AccountRemoved {
        name: String,
    },
    AccountRemoveFailed {
        name: String,
        error: String,
    },
    Accounts {
        accounts: Vec<AccountInfo>,
    },
    AccountListFailed {
        error: String,
    },
    SessionAccountSet {
        session_id: u64,
        account: String,
    },
    ContextWindowResolved {
        session_id: u64,
        context_window: u32,
    },
    SessionWorkingDirSet {
        session_id: u64,
        path: Option<String>,
    },
    SessionTitleSet {
        session_id: u64,
        title: String,
    },
    ReasoningEffortSet {
        session_id: u64,
        effort: String,
    },
    ReasoningEffortSetFailed {
        session_id: u64,
        effort: String,
        error: String,
    },
    ShuttingDown,
}

impl DaemonMessage {
    /// Return the `session_id` field from this message, if the variant carries one.
    ///
    /// Used by the daemon's `handle_broadcast_activity` to determine the origin
    /// session of a message for duplicate-suppression purposes.
    pub fn session_id(&self) -> Option<u64> {
        match self {
            Self::SessionCreated { session_id, .. }
            | Self::SessionAttached { session_id }
            | Self::SessionState { session_id, .. }
            | Self::SessionStatusChanged { session_id, .. }
            | Self::SessionFailed { session_id, .. }
            | Self::SessionDeleted { session_id }
            | Self::SessionDeleteFailed { session_id, .. }
            | Self::TurnAppended { session_id, .. }
            | Self::TurnFinalized { session_id, .. }
            | Self::TurnsUndone { session_id, .. }
            | Self::TurnsRedone { session_id, .. }
            | Self::Started { session_id, .. }
            | Self::OutputChunk { session_id, .. }
            | Self::ToolCallStarted { session_id, .. }
            | Self::ToolCallFinished { session_id, .. }
            | Self::ToolCallFailed { session_id, .. }
            | Self::ToolResultChunk { session_id, .. }
            | Self::Done { session_id, .. }
            | Self::Failed { session_id, .. }
            | Self::Cancelled { session_id, .. }
            | Self::ModelSelected { session_id, .. }
            | Self::ModelSelectionFailed { session_id, .. }
            | Self::TokenUsageUpdate { session_id, .. }
            | Self::LiveOutputTokenCount { session_id, .. }
            | Self::SessionAccountSet { session_id, .. }
            | Self::ContextWindowResolved { session_id, .. }
            | Self::SessionWorkingDirSet { session_id, .. }
            | Self::SessionTitleSet { session_id, .. }
            | Self::ReasoningEffortSet { session_id, .. }
            | Self::ReasoningEffortSetFailed { session_id, .. } => Some(*session_id),
            // Variants that do not carry a session_id.
            Self::Sessions { .. }
            | Self::Pong
            | Self::Models { .. }
            | Self::ModelsFailed { .. }
            | Self::Unlocked
            | Self::Locked
            | Self::LockedError { .. }
            | Self::CredentialAdded { .. }
            | Self::CredentialAddFailed { .. }
            | Self::CredentialRemoved { .. }
            | Self::CredentialRemoveFailed { .. }
            | Self::Credential { .. }
            | Self::AccountAdded { .. }
            | Self::AccountAddFailed { .. }
            | Self::AccountRemoved { .. }
            | Self::AccountRemoveFailed { .. }
            | Self::Accounts { .. }
            | Self::AccountListFailed { .. }
            | Self::ShuttingDown => None,
        }
    }
}
