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

impl TokenUsage {
    /// Merge `other` into `self`, keeping the per-field maximum.
    ///
    /// Cumulative usage only ever increases, so the per-field max is the
    /// "most advanced" state without ever regressing a counter.  Shared by
    /// the daemon's mid-turn `SyncAccumulatedUsage` handler and the TUI's
    /// attach-snapshot merge so both sides apply the identical policy.
    pub fn merge_max(&mut self, other: TokenUsage) {
        self.input_tokens = self.input_tokens.max(other.input_tokens);
        self.output_tokens = self.output_tokens.max(other.output_tokens);
        self.total_tokens = self.total_tokens.max(other.total_tokens);
    }
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
    #[error("rate limited ({status}): {detail}")]
    RateLimited {
        status: u16,
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
    #[error("total request deadline exceeded while reading streaming response")]
    DeadlineExceeded,
    #[error("tool call arguments truncated by provider: {}", .discarded.iter().map(|d| d.to_string()).collect::<Vec<_>>().join(", "))]
    TruncatedToolCall { discarded: Vec<DiscardedToolCall> },
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

impl InferenceError {
    /// Map this error variant to a stable, metrics-safe label string.
    ///
    /// Labels are lowercase snake_case constants consumed by the daemon's
    /// Prometheus counters (e.g. `choreo_api_errors_total{error_type=...}`).
    /// They are part of the public metrics contract: renaming a label changes
    /// dashboards/alerts, so keep existing values stable.
    pub fn metric_label(&self) -> &'static str {
        match self {
            InferenceError::Unauthorized { .. } => "unauthorized",
            InferenceError::RateLimited { .. } => "rate_limited",
            InferenceError::ServerError { .. } => "server_error",
            InferenceError::ClientError { .. } => "client_error",
            InferenceError::EmptyResponse => "empty_response",
            InferenceError::Cancelled => "cancelled",
            InferenceError::DeadlineExceeded => "deadline_exceeded",
            InferenceError::TruncatedToolCall { .. } => "truncated_tool_call",
            InferenceError::Io(_) => "other",
        }
    }
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

/// Which OpenAI-compatible chat field carried the reasoning text, locked in
/// when the adapter captured the payload. The artifact must be re-emitted to
/// the SAME field the provider used — a provider that streams `reasoning_text`
/// must not have its payload echoed back as `reasoning_content` on the next
/// tool-loop turn (the mis-routing this tag prevents).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChatReasoningField {
    /// The `reasoning_content` field (DeepSeek/Kimi style).
    ReasoningContent,
    /// The bare `reasoning` field.
    Reasoning,
    /// The `reasoning_text` field.
    ReasoningText,
}

/// Opaque reasoning round-trip payload, captured verbatim by a provider
/// adapter and re-emitted verbatim on the next request. Only the producing
/// adapter may interpret the payload. Display text lives separately in
/// `Turn::assistant_reasoning`.
///
/// Stored as raw bytes so the proto type stays dependency-light and cannot
/// accidentally be interpreted: the producing adapter serializes its own
/// wire representation (e.g. Anthropic block JSON, Gemini signature string)
/// into `Vec<u8>` at parse time and deserializes it back at request-build
/// time. The variant tags which adapter owns the payload.
///
/// Serialized as an externally-tagged enum (`rename_all = "snake_case"`), so
/// the adapter-ownership tag is the JSON object key (e.g.
/// `{"chat_reasoning": {"field": "reasoning_content", "bytes": [104,105]}}`)
/// and the MessagePack variant name — NOT
/// `#[serde(tag = "kind", content = "payload")]`: an internally/adjacently
/// tagged layout would add nothing here, because named MessagePack (the
/// workspace wire format, see `frame.rs`) already encodes variants as
/// `{"variant_name": payload}`, keeping the ownership tag as the object key
/// just like the JSON shape.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningArtifact {
    /// OpenAI-compatible chat: the reasoning text (verbatim) plus the wire
    /// field it was captured from (see [`ChatReasoningField`]), so re-emission
    /// targets the same field the provider used.
    ChatReasoning {
        field: ChatReasoningField,
        bytes: Vec<u8>,
    },
    /// Anthropic: ordered thinking / redacted_thinking blocks, JSON as
    /// received (signatures + redacted data intact, order preserved).
    AnthropicThinking(Vec<u8>),
    /// Gemini: encrypted thought signatures to send back.
    GoogleSignatures(Vec<u8>),
    /// OpenAI/xAI Responses: opaque reasoning items (or encrypted_content).
    ResponsesItems(Vec<u8>),
}

/// Identity of the model that produced a reasoning artifact.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReasoningProducer {
    pub provider_slug: String,
    pub model: String,
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
    /// Opaque reasoning round-trip artifact (None when never captured or
    /// when the provider exposes no reusable artifact).
    #[serde(default)]
    pub reasoning_artifact: Option<ReasoningArtifact>,
    /// Which provider+model produced `reasoning_artifact`. Set whenever the
    /// artifact is captured; used for the same-model check at build time
    /// (artifacts are model-bound and must be dropped after a model switch).
    #[serde(default)]
    pub reasoning_producer: Option<ReasoningProducer>,
}

impl Turn {
    /// Cheap, conservative estimate of this turn's serialized byte size.
    ///
    /// Used by [`DaemonMessage::approx_wire_size`] (daemon-side lag
    /// accounting). Sums the variable-size fields — text, tool-call
    /// arguments, tool-result content, and image binary data — plus a fixed
    /// per-record overhead, and deliberately over-estimates: it is a
    /// threshold gauge, so over-counting only makes the lag limit slightly
    /// conservative, while under-counting could let a genuinely lagging
    /// client escape eviction. Fixed-size scalars (timestamps, token counts,
    /// flags) are ignored.
    pub fn approx_size(&self) -> usize {
        // Fixed per-turn overhead (variant/field tags, scalars, Option
        // markers), over-estimated a little.
        let mut size = 32usize;
        if let Some(s) = &self.error {
            size += s.len();
        }
        if let Some(s) = &self.user_text {
            size += s.len();
        }
        if let Some(s) = &self.assistant_text {
            size += s.len();
        }
        if let Some(s) = &self.assistant_reasoning {
            size += s.len();
        }
        for call in &self.tool_calls {
            size += 16 + call.call_id.len() + call.name.len() + call.arguments_json.len();
        }
        for result in &self.tool_results {
            size += 32
                + result.call_id.len()
                + result.name.len()
                + result.content.len()
                + result.invocation_description.len();
        }
        for image in &self.displayed_images {
            size += 48
                + image.data.len()
                + image.metadata.mime_type.len()
                + image.metadata.alt.as_ref().map_or(0, String::len);
        }
        if let Some(artifact) = &self.reasoning_artifact {
            // Opaque round-trip payload: count the raw bytes, tagged by the
            // variant that owns them.
            let payload = match artifact {
                ReasoningArtifact::ChatReasoning { bytes, .. } => bytes.len(),
                ReasoningArtifact::AnthropicThinking(bytes)
                | ReasoningArtifact::GoogleSignatures(bytes)
                | ReasoningArtifact::ResponsesItems(bytes) => bytes.len(),
            };
            size += 16 + payload;
        }
        if let Some(producer) = &self.reasoning_producer {
            size += 16 + producer.provider_slug.len() + producer.model.len();
        }
        size
    }
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
    /// Ask the daemon to refresh the models.dev catalog from upstream: a
    /// conditional GET against the cached etag, then a catalog swap when the
    /// remote changed. `force` bypasses the etag (`Cache-Control: no-cache`).
    /// The daemon replies with `DaemonMessage::ModelsRefreshed` (or
    /// `ModelsRefreshFailed`).
    RefreshModels {
        force: bool,
    },
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
        total_timeout_secs: Option<u64>,
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

/// Outcome of a `ClientMessage::RefreshModels` request, reported in
/// `DaemonMessage::ModelsRefreshed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefreshStatus {
    /// The conditional GET returned 304 — the cached catalog is current.
    UpToDate,
    /// The remote changed and the catalog was swapped in.
    Updated,
    /// A `--force` refresh fetched and swapped in a new catalog.
    Forced,
}

/// One provider in a `DaemonMessage::CatalogUpdated` broadcast: the slug the
/// daemon's catalog is keyed by, plus the human-readable display name. A
/// plain wire pair — the TUI maps it into its own `ProviderInfo`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogProvider {
    pub slug: String,
    pub display_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[non_exhaustive]
pub enum DaemonMessage {
    SessionCreated {
        session_id: u64,
        title: Option<String>,
        parent_session_id: Option<u64>,
        working_dir: Option<String>,
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
        /// Human-readable invocation description (e.g. "Running command:
        /// `cargo build`.") so clients can render the tool's context as soon
        /// as the call starts, without waiting for the first streaming chunk
        /// or the final result.  Mirrors `ToolOutput.invocation_description`.
        invocation_description: String,
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
    /// Reply to `ClientMessage::RefreshModels`. `status` distinguishes
    /// "nothing changed" (304) from a real swap (200), and a forced swap.
    ModelsRefreshed {
        providers: usize,
        models: usize,
        status: RefreshStatus,
    },
    /// Reply to `ClientMessage::RefreshModels` when the fetch/merge failed.
    ModelsRefreshFailed {
        error: String,
    },
    /// Broadcast whenever the daemon swaps the provider catalog (startup
    /// refresh, user-overlay reload, `/refresh-models`). Carries the full
    /// provider list so clients can replace their static default picker.
    CatalogUpdated {
        providers: Vec<CatalogProvider>,
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
    /// Best-effort advisory, sent by the daemon immediately before it
    /// disconnects a client that has fallen too far behind the streaming
    /// frontier (lag eviction). Clients use it to distinguish an
    /// "evicted for lag" disconnect from a daemon crash. Best-effort: the
    /// daemon may drop the connection before this message is flushed, so
    /// clients must not treat its absence as meaningful.
    Evicted,
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
            | Self::ModelsRefreshed { .. }
            | Self::ModelsRefreshFailed { .. }
            | Self::CatalogUpdated { .. }
            | Self::ShuttingDown
            | Self::Evicted => None,
        }
    }
}

/// Byte length of an `Option<String>`'s payload (0 when None). Shared by
/// [`DaemonMessage::approx_wire_size`] so every optional string field is
/// counted the same way.
fn option_str_len(s: &Option<String>) -> usize {
    s.as_ref().map_or(0, String::len)
}

/// Byte length of a `SessionStatus` payload (a small enum; only the
/// `ToolCall(String)` and `Retrying` variants carry data).
fn session_status_size(status: &SessionStatus) -> usize {
    match status {
        SessionStatus::ToolCall(name) => 8 + name.len(),
        SessionStatus::Retrying { .. } => 16,
        SessionStatus::Sleeping | SessionStatus::Inactive | SessionStatus::Inference => 4,
    }
}

/// Byte length of a `ReasoningCapability` payload.
fn reasoning_capability_size(cap: &ReasoningCapability) -> usize {
    16 + cap
        .available_effort_levels
        .iter()
        .map(String::len)
        .sum::<usize>()
}

impl DaemonMessage {
    /// Cheap, conservative estimate of this message's serialized byte size.
    ///
    /// Used by the daemon's lag accounting (a later phase) to gauge how many
    /// bytes a slow client has fallen behind the streaming frontier. It
    /// deliberately over-estimates rather than serializes: the accounting is
    /// a threshold gauge, so a slightly-too-big estimate can only trigger
    /// eviction a little early — never let a genuinely lagging client slip
    /// past the limit. O(1) in message count — it sums the variable-size
    /// fields (strings, byte buffers, vecs, turn maps) plus a fixed
    /// per-variant envelope overhead, and ignores fixed-size scalars.
    pub fn approx_wire_size(&self) -> usize {
        // Fixed envelope overhead per variant: the variant tag, MessagePack
        // field-name keys, and length prefixes. Over-estimating by a fixed
        // amount is fine — it only makes the eviction threshold slightly
        // conservative.
        const OVERHEAD: usize = 48;
        match self {
            Self::SessionCreated {
                title,
                working_dir,
                account_name,
                selected_model,
                reasoning_effort,
                ..
            } => {
                OVERHEAD
                    + 8
                    + option_str_len(title)
                    + option_str_len(working_dir)
                    + option_str_len(account_name)
                    + option_str_len(selected_model)
                    + option_str_len(reasoning_effort)
            }
            Self::Sessions { sessions } => {
                OVERHEAD
                    + sessions
                        .iter()
                        .map(|s| {
                            // Fixed per-summary overhead (scalars, status
                            // tag, flags) + the variable-size fields.
                            64 + option_str_len(&s.title)
                                + option_str_len(&s.selected_model)
                                + option_str_len(&s.reasoning_effort)
                                + 8 // parent_session_id: Option<u64>
                                + option_str_len(&s.working_dir)
                                + option_str_len(&s.account_name)
                                + s.active_tool_groups.iter().map(String::len).sum::<usize>()
                        })
                        .sum::<usize>()
            }
            Self::SessionAttached { .. } => OVERHEAD + 8,
            Self::SessionState {
                title,
                selected_model,
                working_dir,
                turns,
                active_tool_groups,
                reasoning_effort,
                status,
                reasoning_capability,
                ..
            } => {
                OVERHEAD
                    + 8
                    + option_str_len(title)
                    + option_str_len(selected_model)
                    + 8 // parent_session_id: Option<u64>
                    + option_str_len(working_dir)
                    + option_str_len(reasoning_effort)
                    + active_tool_groups.iter().map(String::len).sum::<usize>()
                    + turns
                        .iter()
                        .map(|(id, turn)| 8 + *id as usize + turn.approx_size())
                        .sum::<usize>()
                    + session_status_size(status)
                    + reasoning_capability
                        .as_ref()
                        .map_or(0, reasoning_capability_size)
            }
            Self::TurnAppended { turn, .. } => OVERHEAD + 12 + turn.approx_size(),
            Self::SessionStatusChanged { status, .. } => {
                OVERHEAD + 12 + session_status_size(status)
            }
            Self::SessionFailed {
                operation, error, ..
            } => OVERHEAD + 8 + operation.len() + error.len(),
            Self::Started { .. } => OVERHEAD + 16,
            Self::ToolCallStarted {
                call_id,
                tool_name,
                arguments_json,
                invocation_description,
                ..
            } => {
                OVERHEAD
                    + 16
                    + call_id.len()
                    + tool_name.len()
                    + arguments_json.len()
                    + invocation_description.len()
            }
            Self::ToolCallFinished {
                call_id, tool_name, ..
            } => OVERHEAD + 16 + call_id.len() + tool_name.len(),
            Self::ToolResultChunk { data, .. } => OVERHEAD + 16 + data.len(),
            Self::ToolCallFailed {
                call_id,
                tool_name,
                error,
                ..
            } => OVERHEAD + 24 + call_id.len() + tool_name.len() + error.len(),
            Self::TokenUsageUpdate { .. } => OVERHEAD + 16,
            Self::LiveOutputTokenCount { .. } => OVERHEAD + 16,
            Self::OutputChunk { stream, data, .. } => {
                let stream_len = match stream {
                    OutputStream::Answer | OutputStream::Reasoning => 4,
                };
                OVERHEAD + 16 + stream_len + data.len()
            }
            Self::Done { .. } => OVERHEAD + 16,
            Self::Failed { error, .. } => OVERHEAD + 12 + error.len(),
            Self::Cancelled { .. } => OVERHEAD + 12,
            Self::Pong => OVERHEAD,
            Self::Models {
                models,
                selected_model,
            } => {
                OVERHEAD
                    + models.iter().map(String::len).sum::<usize>()
                    + option_str_len(selected_model)
            }
            Self::ModelsFailed { error } => OVERHEAD + error.len(),
            Self::ModelsRefreshed { .. } => OVERHEAD + 16,
            Self::ModelsRefreshFailed { error } => OVERHEAD + error.len(),
            Self::CatalogUpdated { providers } => {
                OVERHEAD
                    + providers
                        .iter()
                        .map(|p| 16 + p.slug.len() + p.display_name.len())
                        .sum::<usize>()
            }
            Self::ModelSelected { model, .. } => OVERHEAD + 8 + model.len(),
            Self::ModelSelectionFailed { model, error, .. } => {
                OVERHEAD + 16 + model.len() + error.len()
            }
            Self::Unlocked | Self::Locked | Self::ShuttingDown | Self::Evicted => OVERHEAD,
            Self::LockedError { error } => OVERHEAD + error.len(),
            Self::CredentialAdded { service } => OVERHEAD + service.len(),
            Self::CredentialAddFailed { service, error } => {
                OVERHEAD + 16 + service.len() + error.len()
            }
            Self::CredentialRemoved { service } => OVERHEAD + service.len(),
            Self::CredentialRemoveFailed { service, error } => {
                OVERHEAD + 16 + service.len() + error.len()
            }
            Self::SessionDeleted { .. } => OVERHEAD + 8,
            Self::SessionDeleteFailed { error, .. } => OVERHEAD + 16 + error.len(),
            Self::TurnsUndone { turn_ids, .. } => OVERHEAD + 8 + turn_ids.len() * 4,
            Self::TurnsRedone { turns, .. } => {
                OVERHEAD
                    + 8
                    + turns
                        .iter()
                        .map(|(id, turn)| 8 + *id as usize + turn.approx_size())
                        .sum::<usize>()
            }
            Self::Credential { service, key } => OVERHEAD + service.len() + option_str_len(key),
            Self::AccountAdded { name } => OVERHEAD + name.len(),
            Self::AccountAddFailed { name, error } => OVERHEAD + 16 + name.len() + error.len(),
            Self::AccountRemoved { name } => OVERHEAD + name.len(),
            Self::AccountRemoveFailed { name, error } => OVERHEAD + 16 + name.len() + error.len(),
            Self::Accounts { accounts } => {
                OVERHEAD
                    + accounts
                        .iter()
                        .map(|a| 32 + a.name.len() + a.provider.len())
                        .sum::<usize>()
            }
            Self::AccountListFailed { error } => OVERHEAD + error.len(),
            Self::SessionAccountSet { account, .. } => OVERHEAD + 8 + account.len(),
            Self::ContextWindowResolved { .. } => OVERHEAD + 12,
            Self::SessionWorkingDirSet { path, .. } => OVERHEAD + 8 + option_str_len(path),
            Self::SessionTitleSet { title, .. } => OVERHEAD + 8 + title.len(),
            Self::ReasoningEffortSet { effort, .. } => OVERHEAD + 8 + effort.len(),
            Self::ReasoningEffortSetFailed { effort, error, .. } => {
                OVERHEAD + 16 + effort.len() + error.len()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// All four artifact variants, with realistic payload bytes.
    fn all_artifacts() -> Vec<ReasoningArtifact> {
        vec![
            ReasoningArtifact::ChatReasoning {
                field: ChatReasoningField::ReasoningContent,
                bytes: b"deep think step-by-step".to_vec(),
            },
            ReasoningArtifact::AnthropicThinking(
                br#"[{"type":"thinking","thinking":"...","signature":"sig_abc"},{"type":"redacted_thinking","data":"eJxT"}]"#
                    .to_vec(),
            ),
            ReasoningArtifact::GoogleSignatures(b"encrypted-sig-1\nencrypted-sig-2".to_vec()),
            ReasoningArtifact::ResponsesItems(b"[{\"type\":\"reasoning\",\"id\":\"re_1\"}]".to_vec()),
        ]
    }

    #[test]
    fn reasoning_artifact_variants_round_trip_msgpack() {
        // Named MessagePack is the workspace wire format (see frame.rs) —
        // persistence and the client socket both round-trip through it, so
        // every variant must survive byte-for-byte.
        for artifact in all_artifacts() {
            let bytes = rmp_serde::to_vec_named(&artifact).expect("encode");
            let decoded: ReasoningArtifact = rmp_serde::from_slice(&bytes).expect("decode");
            assert_eq!(decoded, artifact);
        }
    }

    #[test]
    fn reasoning_artifact_variants_round_trip_json() {
        // serde_json is a dev-dependency already; the externally-tagged serde
        // layout (variant name as the object key, payload as its value) must
        // round-trip too.
        for artifact in all_artifacts() {
            let json = serde_json::to_string(&artifact).expect("serialize");
            let decoded: ReasoningArtifact = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(decoded, artifact);
        }
    }

    #[test]
    fn reasoning_artifact_json_uses_kind_tag() {
        // The variant name is the adapter-ownership contract: the producing
        // adapter's identity must be visible on the wire (as the JSON object
        // key) without interpreting the payload bytes. Pin the exact shape so
        // a serde refactor cannot silently change it.
        let artifact = ReasoningArtifact::ChatReasoning {
            field: ChatReasoningField::ReasoningContent,
            bytes: b"hi".to_vec(),
        };
        let json = serde_json::to_value(&artifact).expect("serialize");
        assert_eq!(
            json["chat_reasoning"]["field"],
            serde_json::json!("reasoning_content")
        );
        assert_eq!(
            json["chat_reasoning"]["bytes"],
            serde_json::json!([104, 105])
        );
        // Exactly one key — the ownership tag — and nothing else.
        let keys: Vec<_> = json.as_object().expect("object").keys().collect();
        assert_eq!(keys, vec!["chat_reasoning"]);
    }

    #[test]
    fn chat_reasoning_struct_variant_round_trips_msgpack_and_json() {
        // The struct-variant ChatReasoning (field + bytes) is the one variant
        // whose payload is not a bare byte array — pin both wire formats so a
        // serde/codec refactor cannot silently drop the field identity
        // (re-emission would then mis-route to the default reasoning_content).
        for artifact in [
            ReasoningArtifact::ChatReasoning {
                field: ChatReasoningField::ReasoningContent,
                bytes: b"deep think step-by-step".to_vec(),
            },
            ReasoningArtifact::ChatReasoning {
                field: ChatReasoningField::Reasoning,
                bytes: b"bare reasoning".to_vec(),
            },
            ReasoningArtifact::ChatReasoning {
                field: ChatReasoningField::ReasoningText,
                bytes: b"text reasoning".to_vec(),
            },
        ] {
            let bytes = rmp_serde::to_vec_named(&artifact).expect("encode");
            let decoded: ReasoningArtifact = rmp_serde::from_slice(&bytes).expect("decode");
            assert_eq!(
                decoded, artifact,
                "MessagePack round-trip must keep field + bytes"
            );

            let json = serde_json::to_string(&artifact).expect("serialize");
            let decoded: ReasoningArtifact = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(decoded, artifact, "JSON round-trip must keep field + bytes");
        }
    }

    #[test]
    fn reasoning_producer_round_trip_msgpack() {
        let producer = ReasoningProducer {
            provider_slug: "openai".to_string(),
            model: "gpt-5.6".to_string(),
        };
        let bytes = rmp_serde::to_vec_named(&producer).expect("encode");
        let decoded: ReasoningProducer = rmp_serde::from_slice(&bytes).expect("decode");
        assert_eq!(decoded, producer);
    }

    /// A fully-populated Turn used by the round-trip tests below.
    fn sample_turn(
        reasoning_artifact: Option<ReasoningArtifact>,
        reasoning_producer: Option<ReasoningProducer>,
    ) -> Turn {
        Turn {
            created_at: TimestampMs(1_700_000_000_000),
            undone: false,
            error: None,
            user_text: Some("list files".to_string()),
            assistant_text: None,
            assistant_reasoning: Some("thinking…".to_string()),
            tool_calls: vec![AssistantToolCallRecord {
                call_id: "call_1".to_string(),
                name: "ls".to_string(),
                arguments_json: "{}".to_string(),
            }],
            token_usage: Some(TokenUsage {
                input_tokens: 10,
                output_tokens: 20,
                total_tokens: 30,
            }),
            tool_results: vec![ToolResultRecord {
                call_id: "call_1".to_string(),
                name: "ls".to_string(),
                content: "file.txt".to_string(),
                is_error: false,
                invocation_description: String::new(),
            }],
            displayed_images: vec![DisplayedImageRecord {
                metadata: ImageMetadata {
                    mime_type: "image/png".to_string(),
                    width: 640,
                    height: 480,
                    byte_len: 100,
                    alt: None,
                },
                data: vec![0u8; 100],
                tool_call_id: None,
            }],
            reasoning_artifact,
            reasoning_producer,
        }
    }

    #[test]
    fn turn_with_artifact_and_producer_round_trips_msgpack() {
        let turn = sample_turn(
            Some(ReasoningArtifact::AnthropicThinking(
                b"{\"sig\":\"x\"}".to_vec(),
            )),
            Some(ReasoningProducer {
                provider_slug: "anthropic".to_string(),
                model: "claude-4.6".to_string(),
            }),
        );
        let bytes = rmp_serde::to_vec_named(&turn).expect("encode");
        let decoded: Turn = rmp_serde::from_slice(&bytes).expect("decode");
        assert_eq!(decoded, turn);
    }

    #[test]
    fn turn_without_artifact_round_trips_msgpack() {
        // Legacy/placeholder turns (and providers that expose no reusable
        // artifact) must round-trip with both new fields as None.
        let turn = sample_turn(None, None);
        let bytes = rmp_serde::to_vec_named(&turn).expect("encode");
        let decoded: Turn = rmp_serde::from_slice(&bytes).expect("decode");
        assert_eq!(decoded, turn);
        assert!(decoded.reasoning_artifact.is_none());
        assert!(decoded.reasoning_producer.is_none());
    }

    #[test]
    fn token_usage_merge_max_keeps_most_advanced_field_per_field() {
        // Cumulative usage only ever increases, so the merge is a per-field
        // max — never regressing any counter even when one side trails the
        // other on a subset of fields (an attach snapshot built before the
        // mid-turn sync landed).
        let mut usage = TokenUsage {
            input_tokens: 30,
            output_tokens: 5,
            total_tokens: 35,
        };
        usage.merge_max(TokenUsage {
            input_tokens: 10,
            output_tokens: 15,
            total_tokens: 25,
        });
        assert_eq!(usage.input_tokens, 30);
        assert_eq!(usage.output_tokens, 15);
        assert_eq!(usage.total_tokens, 35);

        // An identical or trailing value is a no-op.
        usage.merge_max(TokenUsage {
            input_tokens: 30,
            output_tokens: 15,
            total_tokens: 35,
        });
        assert_eq!(
            usage,
            TokenUsage {
                input_tokens: 30,
                output_tokens: 15,
                total_tokens: 35,
            }
        );
    }
}
