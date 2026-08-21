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
/// into protocol messages (e.g. `SessionEvent::Failed { error }`).
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

/// Session-scoped events produced by the daemon for a specific session.
///
/// These 29 events used to be `DaemonMessage` variants that each carried
/// their own `session_id` field. They now live inside
/// [`DaemonMessage::Session`], whose envelope hoists the `session_id` so
/// every event has an origin session **by construction** — it can never be
/// forgotten, mismatched, or duplicated. Consumers that dispatch on session
/// events match the envelope's `session_id` once and then handle the inner
/// event; the wire format nests the event inside the envelope, so the origin
/// is always present on the wire too.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum SessionEvent {
    SessionCreated {
        title: Option<String>,
        parent_session_id: Option<u64>,
        working_dir: Option<String>,
        account_name: Option<String>,
        selected_model: Option<String>,
        reasoning_effort: Option<String>,
    },
    SessionAttached,
    SessionState {
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
        turn_id: u32,
        turn: Turn,
    },
    SessionStatusChanged {
        status: SessionStatus,
        /// Unix-epoch-milliseconds timestamp of this status change, so the
        /// TUI can re-sort the sessions list (most recently modified first)
        /// without waiting for a fresh ListSessions round-trip.
        last_modified: i64,
    },
    SessionFailed {
        operation: String,
        error: String,
    },
    Started {
        request_id: u32,
        turn_id: u32,
        estimated_prompt_tokens: u32,
    },
    ToolCallStarted {
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
        request_id: u32,
        call_id: String,
        tool_name: String,
    },
    ToolResultChunk {
        request_id: u32,
        call_id: String,
        data: Vec<u8>,
    },
    ToolCallFailed {
        request_id: u32,
        call_id: String,
        tool_name: String,
        error: String,
    },
    TokenUsageUpdate {
        token_usage: TokenUsage,
        last_prompt_tokens: Option<u32>,
    },
    /// Cumulative output-token estimate for the current turn, updated as
    /// each stream chunk arrives.  Used by the TUI for live token display.
    LiveOutputTokenCount {
        request_id: u32,
        output_tokens: u32,
    },
    OutputChunk {
        request_id: u32,
        stream: OutputStream,
        data: Vec<u8>,
    },
    Done {
        request_id: u32,
        /// Token usage for the completed request, if reported by the provider.
        token_usage: Option<TokenUsage>,
        /// The prompt_tokens from the most recent API response (the actual
        /// context size that was sent to the model), if available.
        #[serde(default)]
        last_prompt_tokens: Option<u32>,
    },
    Failed {
        request_id: u32,
        error: String,
    },
    Cancelled {
        request_id: u32,
    },
    ModelSelected {
        model: String,
        #[serde(default)]
        reasoning_capability: Option<ReasoningCapability>,
    },
    ModelSelectionFailed {
        model: String,
        error: String,
    },
    SessionDeleted,
    SessionDeleteFailed {
        error: String,
    },
    TurnsUndone {
        turn_ids: Vec<u32>,
    },
    TurnsRedone {
        turns: BTreeMap<u32, Turn>,
    },
    SessionAccountSet {
        account: String,
    },
    ContextWindowResolved {
        context_window: u32,
    },
    SessionWorkingDirSet {
        path: Option<String>,
    },
    SessionTitleSet {
        title: String,
    },
    ReasoningEffortSet {
        effort: String,
    },
    ReasoningEffortSetFailed {
        effort: String,
        error: String,
    },
}

/// Messages sent from the daemon to a client.
///
/// Split into two families:
/// - [`DaemonMessage::Session`] carries a session-scoped [`SessionEvent`]
///   wrapped in an envelope that supplies the origin `session_id` by
///   construction. These events are delivered to per-session subscribers and
///   the all-activity fan, and the all-activity dedup is keyed on the
///   envelope's origin session.
/// - The remaining flat variants are connection/reply/global messages
///   (`Sessions`, `Pong`, `Models`, keystore and account replies,
///   `ShuttingDown`, …) that have no session scope and stay flat.
//
// `Session` holds a `SessionEvent` by value per the agreed split design (the
// envelope hoists `session_id`, and the event must arrive flat on the wire, not
// behind indirection). Boxing it would shrink `DaemonMessage` past the
// `large_enum_variant` threshold but would also change the documented public
// shape every producer/consumer migrated to. The lint is the accepted cost of
// the honest envelope, so it stays allowed. (No `#[non_exhaustive]`: the
// variant set IS the wire contract — a new variant is a protocol bump, v5, the
// same as removing one — and without the attribute every consumer match must
// enumerate the whole set, so the compiler points at every site that needs
// revisiting when that happens.)
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum DaemonMessage {
    /// Single home for all session-scoped events: the `session_id` that used
    /// to ride on each of the 29 moved variants is hoisted onto this
    /// envelope, so every inner [`SessionEvent`] has an origin session **by
    /// construction**.
    Session {
        session_id: u64,
        event: SessionEvent,
    },
    Sessions {
        sessions: Vec<SessionSummary>,
    },
    Pong,
    Models {
        models: Vec<String>,
        selected_model: Option<String>,
    },
    ModelsFailed {
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
    ShuttingDown,
    /// Best-effort advisory, sent by the daemon immediately before it
    /// disconnects a client that has fallen too far behind the streaming
    /// frontier (lag eviction). Clients use it to distinguish an
    /// "evicted for lag" disconnect from a daemon crash. Best-effort: the
    /// daemon may drop the connection before this message is flushed, so
    /// clients must not treat its absence as meaningful.
    Evicted,
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

    /// The lag-eviction gauge must never UNDER-estimate the serialized payload,
    /// or a genuinely lagging client could escape eviction: the estimate is the
    /// threshold the daemon's lag accounting compares against the per-client
    /// cap / global budget, so under-counting directly weakens the memory
    /// bound. Every `DaemonMessage` variant is encoded with a realistic (and
    /// deliberately DENSE for the record-bearing ones) payload, and the
    /// estimate must cover the actual frame bytes. This is the property the
    /// record-size allowances in [`Turn::approx_size`] and the per-variant
    /// field counts are tuned against — a future serde/encoding change or a
    /// new variant that shrinks the margin must re-prove it here.
    #[test]
    fn approx_wire_size_never_underestimates_encoded_payload() {
        fn turn(n_calls: usize, n_results: usize, n_images: usize) -> Turn {
            Turn {
                created_at: TimestampMs(1_700_000_000_000),
                undone: false,
                error: Some("boom".into()),
                user_text: Some("hello world".into()),
                assistant_text: Some("x".repeat(100)),
                assistant_reasoning: Some("thinking".repeat(10)),
                tool_calls: (0..n_calls)
                    .map(|i| AssistantToolCallRecord {
                        call_id: format!("call_{i}"),
                        name: "sh".into(),
                        arguments_json: format!(r#"{{"command":"echo step {i}"}}"#),
                    })
                    .collect(),
                token_usage: Some(TokenUsage {
                    input_tokens: 1,
                    output_tokens: 2,
                    total_tokens: 3,
                }),
                tool_results: (0..n_results)
                    .map(|i| ToolResultRecord {
                        call_id: format!("call_{i}"),
                        name: "sh".into(),
                        content: format!("output line {i} of the tool\n"),
                        is_error: false,
                        invocation_description: format!("Running: echo step {i}"),
                    })
                    .collect(),
                displayed_images: (0..n_images)
                    .map(|i| DisplayedImageRecord {
                        metadata: ImageMetadata {
                            mime_type: "image/png".into(),
                            width: 640,
                            height: 480,
                            byte_len: 100,
                            alt: Some(format!("screenshot {i}")),
                        },
                        data: vec![0u8; 100],
                        tool_call_id: Some(format!("call_{i}")),
                    })
                    .collect(),
                reasoning_artifact: Some(ReasoningArtifact::ChatReasoning {
                    field: ChatReasoningField::ReasoningContent,
                    bytes: b"{\"type\":\"thinking\",\"signature\":\"sig_abc\"}".to_vec(),
                }),
                reasoning_producer: Some(ReasoningProducer {
                    provider_slug: "anthropic".into(),
                    model: "claude-4.6".into(),
                }),
            }
        }

        fn summary(id: u64) -> SessionSummary {
            SessionSummary {
                session_id: id,
                title: Some(format!("session {id}")),
                selected_model: Some("gpt-5.6".into()),
                reasoning_effort: Some("high".into()),
                parent_session_id: Some(3),
                working_dir: Some("/home/user/projects/demo".into()),
                created_at: 1_700_000_000_000,
                last_modified: 1_700_000_000_001,
                turn_count: 12,
                status: SessionStatus::ToolCall("sh".into()),
                active_tool_groups: vec![
                    "core".into(),
                    "git".into(),
                    "shell".into(),
                    "filesystem".into(),
                ],
                account_name: Some("default".into()),
                token_usage: Some(TokenUsage {
                    input_tokens: 100,
                    output_tokens: 50,
                    total_tokens: 150,
                }),
                context_window: Some(128_000),
                last_prompt_tokens: Some(100),
            }
        }

        let dense_turn = turn(10, 10, 3);
        let one_turn = turn(1, 1, 0);
        // A second artifact shape: bare-byte variants (Anthropic/Google/Responses)
        // have a cheaper wire form than `ChatReasoning`, but must still fit.
        let mut bare_artifact_turn = turn(10, 10, 3);
        bare_artifact_turn.reasoning_artifact = Some(ReasoningArtifact::ResponsesItems(
            b"[{\"type\":\"reasoning\",\"id\":\"re_1\"}]".to_vec(),
        ));
        let samples: Vec<(&str, DaemonMessage)> = vec![
            (
                "SessionCreated",
                DaemonMessage::Session {
                    session_id: 1,
                    event: SessionEvent::SessionCreated {
                        title: Some("t".into()),
                        parent_session_id: Some(2),
                        working_dir: Some("/tmp".into()),
                        account_name: Some("default".into()),
                        selected_model: Some("gpt-5.6".into()),
                        reasoning_effort: Some("high".into()),
                    },
                },
            ),
            (
                "Sessions",
                DaemonMessage::Sessions {
                    sessions: (0..20).map(summary).collect(),
                },
            ),
            (
                "SessionAttached",
                DaemonMessage::Session {
                    session_id: 1,
                    event: SessionEvent::SessionAttached,
                },
            ),
            (
                "SessionState",
                DaemonMessage::Session {
                    session_id: 1,
                    event: SessionEvent::SessionState {
                        title: Some("t".into()),
                        selected_model: Some("gpt-5.6".into()),
                        parent_session_id: Some(2),
                        working_dir: Some("/tmp".into()),
                        turns: std::collections::BTreeMap::from([
                            (1, dense_turn.clone()),
                            (2, one_turn.clone()),
                        ]),
                        active_tool_groups: vec!["core".into(), "shell".into(), "git".into()],
                        token_usage: Some(TokenUsage {
                            input_tokens: 100,
                            output_tokens: 50,
                            total_tokens: 150,
                        }),
                        context_window: Some(128_000),
                        last_prompt_tokens: Some(100),
                        status: SessionStatus::Inference,
                        reasoning_effort: Some("high".into()),
                        reasoning_capability: Some(ReasoningCapability {
                            available_effort_levels: vec![
                                "off".into(),
                                "low".into(),
                                "medium".into(),
                                "high".into(),
                            ],
                        }),
                    },
                },
            ),
            (
                "TurnAppended",
                DaemonMessage::Session {
                    session_id: 1,
                    event: SessionEvent::TurnAppended {
                        turn_id: 1,
                        turn: dense_turn.clone(),
                    },
                },
            ),
            (
                "TurnAppendedBareArtifact",
                DaemonMessage::Session {
                    session_id: 1,
                    event: SessionEvent::TurnAppended {
                        turn_id: 2,
                        turn: bare_artifact_turn.clone(),
                    },
                },
            ),
            (
                "SessionStatusChanged",
                DaemonMessage::Session {
                    session_id: 1,
                    event: SessionEvent::SessionStatusChanged {
                        status: SessionStatus::ToolCall("sh".into()),
                        last_modified: 0,
                    },
                },
            ),
            (
                "SessionFailed",
                DaemonMessage::Session {
                    session_id: 1,
                    event: SessionEvent::SessionFailed {
                        operation: "create_session".into(),
                        error: "some failure happened here".into(),
                    },
                },
            ),
            (
                "Started",
                DaemonMessage::Session {
                    session_id: 1,
                    event: SessionEvent::Started {
                        request_id: 1,
                        turn_id: 1,
                        estimated_prompt_tokens: 100,
                    },
                },
            ),
            (
                "ToolCallStarted",
                DaemonMessage::Session {
                    session_id: 1,
                    event: SessionEvent::ToolCallStarted {
                        request_id: 1,
                        call_id: "call_1".into(),
                        tool_name: "sh".into(),
                        arguments_json: r#"{"command":"echo hi"}"#.into(),
                        invocation_description: "Running command: `echo hi`.".into(),
                    },
                },
            ),
            (
                "ToolCallFinished",
                DaemonMessage::Session {
                    session_id: 1,
                    event: SessionEvent::ToolCallFinished {
                        request_id: 1,
                        call_id: "call_1".into(),
                        tool_name: "sh".into(),
                    },
                },
            ),
            (
                "ToolResultChunk",
                DaemonMessage::Session {
                    session_id: 1,
                    event: SessionEvent::ToolResultChunk {
                        request_id: 1,
                        call_id: "call_1".into(),
                        data: vec![b'x'; 100],
                    },
                },
            ),
            (
                "ToolCallFailed",
                DaemonMessage::Session {
                    session_id: 1,
                    event: SessionEvent::ToolCallFailed {
                        request_id: 1,
                        call_id: "call_1".into(),
                        tool_name: "sh".into(),
                        error: "command not found".into(),
                    },
                },
            ),
            (
                "TokenUsageUpdate",
                DaemonMessage::Session {
                    session_id: 1,
                    event: SessionEvent::TokenUsageUpdate {
                        token_usage: TokenUsage {
                            input_tokens: 100,
                            output_tokens: 50,
                            total_tokens: 150,
                        },
                        last_prompt_tokens: Some(100),
                    },
                },
            ),
            (
                "LiveOutputTokenCount",
                DaemonMessage::Session {
                    session_id: 1,
                    event: SessionEvent::LiveOutputTokenCount {
                        request_id: 1,
                        output_tokens: 42,
                    },
                },
            ),
            (
                "OutputChunk",
                DaemonMessage::Session {
                    session_id: 1,
                    event: SessionEvent::OutputChunk {
                        request_id: 1,
                        stream: OutputStream::Answer,
                        data: vec![b'x'; 100],
                    },
                },
            ),
            (
                "Done",
                DaemonMessage::Session {
                    session_id: 1,
                    event: SessionEvent::Done {
                        request_id: 1,
                        token_usage: Some(TokenUsage {
                            input_tokens: 100,
                            output_tokens: 50,
                            total_tokens: 150,
                        }),
                        last_prompt_tokens: Some(100),
                    },
                },
            ),
            (
                "Failed",
                DaemonMessage::Session {
                    session_id: 1,
                    event: SessionEvent::Failed {
                        request_id: 1,
                        error: "x".repeat(100),
                    },
                },
            ),
            (
                "Cancelled",
                DaemonMessage::Session {
                    session_id: 1,
                    event: SessionEvent::Cancelled { request_id: 1 },
                },
            ),
            ("Pong", DaemonMessage::Pong),
            (
                "Models",
                DaemonMessage::Models {
                    models: vec!["gpt-4".into(), "gpt-4o".into(), "gpt-5.6".into()],
                    selected_model: Some("gpt-5.6".into()),
                },
            ),
            (
                "ModelsFailed",
                DaemonMessage::ModelsFailed {
                    error: "failed to list models".into(),
                },
            ),
            (
                "ModelsRefreshed",
                DaemonMessage::ModelsRefreshed {
                    providers: 208,
                    models: 1234,
                    status: RefreshStatus::Updated,
                },
            ),
            (
                "ModelsRefreshFailed",
                DaemonMessage::ModelsRefreshFailed {
                    error: "network error".into(),
                },
            ),
            (
                "CatalogUpdated",
                DaemonMessage::CatalogUpdated {
                    providers: (0..208)
                        .map(|i| CatalogProvider {
                            slug: format!("provider-slug-{i}"),
                            display_name: format!("Provider Display Name {i}"),
                        })
                        .collect(),
                },
            ),
            (
                "ModelSelected",
                DaemonMessage::Session {
                    session_id: 1,
                    event: SessionEvent::ModelSelected {
                        model: "gpt-5.6".into(),
                        reasoning_capability: Some(ReasoningCapability {
                            available_effort_levels: vec![
                                "off".into(),
                                "low".into(),
                                "medium".into(),
                                "high".into(),
                            ],
                        }),
                    },
                },
            ),
            (
                "ModelSelectionFailed",
                DaemonMessage::Session {
                    session_id: 1,
                    event: SessionEvent::ModelSelectionFailed {
                        model: "gpt-5.6".into(),
                        error: "model not found".into(),
                    },
                },
            ),
            ("Unlocked", DaemonMessage::Unlocked),
            ("Locked", DaemonMessage::Locked),
            (
                "LockedError",
                DaemonMessage::LockedError {
                    error: "wrong password".into(),
                },
            ),
            (
                "CredentialAdded",
                DaemonMessage::CredentialAdded {
                    service: "openai".into(),
                },
            ),
            (
                "CredentialAddFailed",
                DaemonMessage::CredentialAddFailed {
                    service: "openai".into(),
                    error: "already exists".into(),
                },
            ),
            (
                "CredentialRemoved",
                DaemonMessage::CredentialRemoved {
                    service: "openai".into(),
                },
            ),
            (
                "CredentialRemoveFailed",
                DaemonMessage::CredentialRemoveFailed {
                    service: "openai".into(),
                    error: "not found".into(),
                },
            ),
            (
                "SessionDeleted",
                DaemonMessage::Session {
                    session_id: 1,
                    event: SessionEvent::SessionDeleted,
                },
            ),
            (
                "SessionDeleteFailed",
                DaemonMessage::Session {
                    session_id: 1,
                    event: SessionEvent::SessionDeleteFailed {
                        error: "db error".into(),
                    },
                },
            ),
            (
                "TurnsUndone",
                DaemonMessage::Session {
                    session_id: 1,
                    event: SessionEvent::TurnsUndone {
                        turn_ids: vec![1, 2, 3, 4, 5],
                    },
                },
            ),
            (
                "TurnsRedone",
                DaemonMessage::Session {
                    session_id: 1,
                    event: SessionEvent::TurnsRedone {
                        turns: std::collections::BTreeMap::from([(1, dense_turn), (2, one_turn)]),
                    },
                },
            ),
            (
                "Credential",
                DaemonMessage::Credential {
                    service: "openai".into(),
                    key: Some("sk-123".into()),
                },
            ),
            (
                "AccountAdded",
                DaemonMessage::AccountAdded {
                    name: "default".into(),
                },
            ),
            (
                "AccountAddFailed",
                DaemonMessage::AccountAddFailed {
                    name: "default".into(),
                    error: "invalid provider".into(),
                },
            ),
            (
                "AccountRemoved",
                DaemonMessage::AccountRemoved {
                    name: "default".into(),
                },
            ),
            (
                "AccountRemoveFailed",
                DaemonMessage::AccountRemoveFailed {
                    name: "default".into(),
                    error: "not found".into(),
                },
            ),
            (
                "Accounts",
                DaemonMessage::Accounts {
                    accounts: (0..10)
                        .map(|i| AccountInfo {
                            name: format!("account-{i}"),
                            provider: "openai".into(),
                            has_credential: i % 2 == 0,
                        })
                        .collect(),
                },
            ),
            (
                "AccountListFailed",
                DaemonMessage::AccountListFailed {
                    error: "failed to list accounts".into(),
                },
            ),
            (
                "SessionAccountSet",
                DaemonMessage::Session {
                    session_id: 1,
                    event: SessionEvent::SessionAccountSet {
                        account: "default".into(),
                    },
                },
            ),
            (
                "ContextWindowResolved",
                DaemonMessage::Session {
                    session_id: 1,
                    event: SessionEvent::ContextWindowResolved {
                        context_window: 128_000,
                    },
                },
            ),
            (
                "SessionWorkingDirSet",
                DaemonMessage::Session {
                    session_id: 1,
                    event: SessionEvent::SessionWorkingDirSet {
                        path: Some("/tmp".into()),
                    },
                },
            ),
            (
                "SessionTitleSet",
                DaemonMessage::Session {
                    session_id: 1,
                    event: SessionEvent::SessionTitleSet {
                        title: "hello".into(),
                    },
                },
            ),
            (
                "ReasoningEffortSet",
                DaemonMessage::Session {
                    session_id: 1,
                    event: SessionEvent::ReasoningEffortSet {
                        effort: "high".into(),
                    },
                },
            ),
            (
                "ReasoningEffortSetFailed",
                DaemonMessage::Session {
                    session_id: 1,
                    event: SessionEvent::ReasoningEffortSetFailed {
                        effort: "high".into(),
                        error: "model does not support it".into(),
                    },
                },
            ),
            ("ShuttingDown", DaemonMessage::ShuttingDown),
            ("Evicted", DaemonMessage::Evicted),
        ];

        let mut checked = 0usize;
        for (name, msg) in &samples {
            let frame = crate::encode_frame(msg).expect("encode");
            // The 4-byte BE length prefix precedes the payload; the estimate
            // must cover the payload itself.
            let payload = frame.len() - 4;
            let est = msg.approx_wire_size();
            assert!(
                est >= payload,
                "approx_wire_size ({est}) UNDER-estimates the {payload}-byte encoded payload by {} for {name}: {msg:?}",
                payload - est
            );
            checked += 1;
        }
        assert_eq!(checked, samples.len(), "every sample must be checked");
    }
}
