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
}
