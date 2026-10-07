//! The wire protocol types: the two message envelopes and every payload they
//! carry.
//!
//! # The uniform correlation frame
//!
//! Both directions use one shaped envelope. A [`ClientMessage`] is
//! `{ id: u64, inner: ClientMessageType }`; a [`DaemonMessage`] is
//! `{ id: Option<u64>, inner: DaemonMessageType }`. A client allocates `id` as a
//! monotonic per-connection counter and the daemon MUST answer every request
//! with exactly one `DaemonMessage { id: Some(the same id), .. }` — the terminal
//! reply, success or failure. A broadcast is `DaemonMessage { id: None, .. }` and
//! never resolves a request.
//!
//! # Two orthogonal axes
//!
//! The **reply axis** (`id`) is per-connection and one-shot: one request, one
//! reply. The **stream axis** (`stream_id`, carried in the payload of the
//! streaming [`SessionEvent`]s a run fans out) is per-session and many-shot: a
//! single `RunInput`/`ContinueGeneration` fans many events out to EVERY session
//! subscriber, including mid-stream joiners. They are never merged, because two
//! clients each use their own request id 0, while a stream needs an id unique
//! across a namespace all subscribers share.
//!
//! The **daemon owns stream-id assignment**: a client does not choose a
//! `stream_id`. The session thread allocates one (per-session, monotonic) when it
//! accepts a `RunInput`/`ContinueGeneration` and reports it on the acceptance
//! reply and the `Started` broadcast; the client learns it from there (to key its
//! `stream_id → turn_id` map and to address a later `Cancel`).
//!
//! # Reply/broadcast overlap
//!
//! Reply-ness is a property of the SEND, not the payload type. The same `inner`
//! (e.g. `SessionState`, `ReasoningEffortSet`) may be emitted with `id: Some`
//! (a reply) or `id: None` (a broadcast); a client applies the payload's state
//! effect either way and resolves a pending slot only when `id` is `Some`. A
//! variant is split into a dedicated type only when the reply carries
//! requester-relative intent no broadcast may carry (the
//! `SessionCreatedForRequester` precedent).

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
    #[must_use]
    pub fn cycle_from(&self, current: &str) -> Option<String> {
        if self.available_effort_levels.is_empty() {
            return None;
        }
        let pos = self.available_effort_levels.iter().position(|e| e == current).unwrap_or_else(|| {
            warn!(
                "ReasoningCapability::cycle_from: current slug {current} not in available set {:?}, starting from 0",
                self.available_effort_levels,
            );
            0
        });
        let next = (pos + 1) % self.available_effort_levels.len();
        // The modulo guarantees `next` is in bounds; .get keeps the lint total.
        self.available_effort_levels.get(next).cloned()
    }
}

/// `ContextConfig` — controls file discovery for session context.
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
    /// Prompt tokens served from the provider's prompt cache — the cache
    /// **read/hit** count (Anthropic's `usage.cache_read_input_tokens`, z.ai's
    /// `usage.prompt_tokens_details.cached_tokens`). Cached input is priced
    /// differently, so cost/usage reporting tracks it separately. 0 when the
    /// provider does not report it; `#[serde(default)]` keeps old payloads and
    /// providers that omit the details object deserializing cleanly.
    #[serde(default)]
    pub cached_tokens: u32,
    /// Prompt tokens written to the provider's prompt cache (e.g. Anthropic's
    /// `usage.cache_creation_input_tokens`). Priced differently from a read
    /// (`cached_tokens`), so cost reporting tracks it separately. 0 when the
    /// provider does not report it.
    #[serde(default)]
    pub cache_write_tokens: u32,
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
        self.cached_tokens = self.cached_tokens.max(other.cached_tokens);
        self.cache_write_tokens = self.cache_write_tokens.max(other.cache_write_tokens);
    }
}

/// Unix-epoch-milliseconds timestamp.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimestampMs(i64);

impl TimestampMs {
    /// Sentinel value for when the real timestamp is unavailable (e.g. corrupt
    /// DB entries).
    pub const ZERO: Self = Self(0);

    /// Current wall-clock time as `TimestampMs`.
    ///
    /// The u128→`i64` narrowing only truncates for clock readings past the
    /// year 29247 — practically never, so keep the original `as i64` behavior
    /// here rather than introducing an error path.
    #[must_use]
    #[expect(clippy::cast_possible_truncation)]
    pub fn now() -> Self {
        Self(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or_else(
                    |_| {
                        tracing::warn!("system clock before UNIX_EPOCH, using 0");
                        0
                    },
                    |d| d.as_millis() as i64,
                ),
        )
    }

    #[must_use]
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
        // Bound the cropped payload: a length-truncated `write_file` (the exact
        // case this variant exists for) can carry tens of kilobytes of
        // half-written arguments, and this string is embedded verbatim in the
        // error message the daemon logs and shows. A short preview plus the
        // total character count keeps the message diagnosable without dumping
        // the whole blob into logs or the transcript.
        const PREVIEW_CHARS: usize = 200;
        let args = &self.arguments_json;
        let total = args.chars().count();
        if total <= PREVIEW_CHARS {
            write!(f, "{}: {}", self.name, sanitize_control_chars(args))
        } else {
            let preview: String = args.chars().take(PREVIEW_CHARS).collect();
            write!(
                f,
                "{} ({total} chars): {}…",
                self.name,
                sanitize_control_chars(&preview)
            )
        }
    }
}

/// Replace any control character with a space so a (possibly truncated) tool
/// payload can never inject a newline or other control byte into a single-line
/// log line or transcript entry. Borrows when there is nothing to sanitize (the
/// common case — arguments are JSON, whose newlines are already backslash-
/// escaped on the wire).
fn sanitize_control_chars(s: &str) -> std::borrow::Cow<'_, str> {
    if s.chars().any(char::is_control) {
        std::borrow::Cow::Owned(
            s.chars()
                .map(|c| if c.is_control() { ' ' } else { c })
                .collect(),
        )
    } else {
        std::borrow::Cow::Borrowed(s)
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
    /// The provider accepted the request but the referenced artifact (e.g. a
    /// generated image on its CDN) is not available yet — a propagation race,
    /// not an HTTP error and not an empty response. Retryable by callers that
    /// know how to wait; distinct from [`InferenceError::EmptyResponse`] so a
    /// genuinely empty body is never mistaken for "not published yet".
    #[error("provider artifact not ready yet: {detail}")]
    NotReady { detail: String },
    /// The provider's content filter blocked the generation. No HTTP status
    /// accompanied the response (it came back with a success code), so this
    /// variant carries none — policy denial is honest as-is, and resending
    /// the same prompt can never clear it.
    #[error("provider content filter blocked the generation: {detail}")]
    ContentFiltered { detail: String },
    /// The prompt exceeded the model's declared context window (e.g. z.ai's
    /// `finish_reason: "model_context_window_exceeded"`). Terminal and never
    /// retryable: resending the same prompt cannot shrink it, and surfacing a
    /// distinct variant (instead of a generic 4xx) makes it a compaction-bug
    /// signal rather than an opaque provider failure.
    #[error("prompt exceeded the model's context window: {detail}")]
    ContextWindowExceeded { detail: String },
    #[error("request cancelled during retry backoff")]
    Cancelled,
    #[error("total request deadline exceeded while reading streaming response")]
    DeadlineExceeded,
    #[error("tool call arguments truncated by provider: {}", .discarded.iter().map(ToString::to_string).collect::<Vec<_>>().join(", "))]
    TruncatedToolCall { discarded: Vec<DiscardedToolCall> },
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

impl InferenceError {
    /// Map this error variant to a stable, metrics-safe label string.
    ///
    /// Labels are lowercase `snake_case` constants consumed by the daemon's
    /// Prometheus counters (e.g. `choreo_api_errors_total{error_type=...}`).
    /// They are part of the public metrics contract: renaming a label changes
    /// dashboards/alerts, so keep existing values stable.
    #[must_use]
    pub fn metric_label(&self) -> &'static str {
        match self {
            InferenceError::Unauthorized { .. } => "unauthorized",
            InferenceError::RateLimited { .. } => "rate_limited",
            InferenceError::ServerError { .. } => "server_error",
            InferenceError::ClientError { .. } => "client_error",
            InferenceError::EmptyResponse => "empty_response",
            InferenceError::NotReady { .. } => "not_ready",
            InferenceError::ContentFiltered { .. } => "content_filtered",
            InferenceError::ContextWindowExceeded { .. } => "context_window_exceeded",
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
    /// A vision image this tool result produced, stored as a *reference* that
    /// carries the normalized image **bytes** (`ImageReference::data`), so the
    /// request builder attaches them directly without re-reading the source
    /// file — an additive, `#[serde(default)]` field so old persisted turns
    /// deserialize with `None`. `None` for text-only tool results.
    ///
    /// On the client-facing view (`turn_for_client`) the reference is KEPT but
    /// its `data` is emptied, so a client learns the image exists (path, mime,
    /// dimensions) and fetches the bytes on demand under
    /// `ImageKey::ToolResult { call_id }`.
    #[serde(default)]
    pub image: Option<ImageReference>,
}

/// A reference to a vision image produced by a tool (e.g. `read_image`),
/// carrying the normalized image **bytes** (PNG/JPEG) durably so the request
/// builder can attach them directly to the model request without re-reading
/// the source file.
///
/// The `path`/`mime_type`/`width`/`height` metadata also tells a client that a
/// vision image exists so it can render a placeholder and fetch the bytes on
/// demand. `data` alone is daemon/model-only: it feeds the request builder
/// directly, is persisted in the `session_attachments` DB table at write time
/// (kept out of the zstd turn blob) and re-attached on read, and is emptied on
/// the client-facing view by `turn_for_client` (the client fetches the bytes
/// with `ClientMessageType::GetImage` under `ImageKey::ToolResult { call_id }`).
/// `#[serde(default)]` keeps old persisted records backward compatible — they
/// deserialize with an empty `data`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ImageReference {
    /// Source path the image was read from.
    pub path: String,
    /// Image MIME type as produced by normalization (e.g. `image/png`).
    pub mime_type: String,
    /// Width in pixels after normalization.
    pub width: u32,
    /// Height in pixels after normalization.
    pub height: u32,
    /// The normalized image bytes (PNG/JPEG), durable and daemon/model-only.
    /// Empty on old persisted records (the `#[serde(default)]` backward-compat
    /// path) and on non-vision gate placeholders.
    #[serde(default)]
    pub data: Vec<u8>,
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
/// and the `MessagePack` variant name — NOT
/// `#[serde(tag = "kind", content = "payload")]`: an internally/adjacently
/// tagged layout would add nothing here, because named `MessagePack` (the
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
    /// Anthropic: ordered thinking / `redacted_thinking` blocks, JSON as
    /// received (signatures + redacted data intact, order preserved).
    AnthropicThinking(Vec<u8>),
    /// Gemini: encrypted thought signatures to send back.
    GoogleSignatures(Vec<u8>),
    /// OpenAI/xAI Responses: opaque reasoning items (or `encrypted_content`).
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
    #[must_use]
    pub fn is_active(&self) -> bool {
        matches!(
            self,
            SessionStatus::Inference | SessionStatus::ToolCall(_) | SessionStatus::Retrying { .. }
        )
    }

    /// Returns `true` when the session is idle and ready to begin a new turn —
    /// i.e. exactly [`SessionStatus::Inactive`].  `Sleeping` is deliberately
    /// NOT idle: it is the session-thread exit marker (only an `AttachSession`
    /// reloads the session), so a prompt submitted to a sleeping session cannot
    /// start a turn either.
    ///
    /// Note this is not the negation of [`Self::is_active`], which is `false`
    /// for both `Inactive` and `Sleeping`.
    #[must_use]
    pub fn is_idle(&self) -> bool {
        matches!(self, SessionStatus::Inactive)
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
    /// The `prompt_tokens` from the most recent API response (the actual
    /// context size being sent to the model), if available.
    #[serde(default)]
    pub last_prompt_tokens: Option<u32>,
    /// Whether this session is pinned (pinned sessions float to the top of
    /// the sessions list). A daemon-owned per-session flag;
    /// `#[serde(default)]` keeps older payloads deserializing as `false`.
    #[serde(default)]
    pub pinned: bool,
    /// When this session was archived, as Unix-epoch-milliseconds (the same
    /// time family as `created_at`/`last_modified`), or `None` when it is
    /// not archived. A daemon-owned per-session flag; `#[serde(default)]`
    /// keeps older payloads deserializing as `None`.
    #[serde(default)]
    pub archived_at: Option<i64>,
}

impl SessionSummary {
    /// The session-LIST ordering, shared by BOTH the daemon (`ListSessions`)
    /// and every client that renders the list (the TUI's session manager).
    /// Pinned sessions sort first, then newest `last_modified`, then highest
    /// `session_id` as a deterministic tiebreak (so equal timestamps never
    /// jitter between refreshes).
    ///
    /// Centralising the comparator here is deliberate: the daemon and the
    /// clients both order the same rows, and two hand-kept copies of this key
    /// would silently drift (a client re-sort that disagrees with the daemon's
    /// order), so there is exactly one definition of "list order".
    #[must_use]
    pub fn cmp_for_list(&self, other: &Self) -> std::cmp::Ordering {
        other
            .pinned
            .cmp(&self.pinned)
            .then_with(|| other.last_modified.cmp(&self.last_modified))
            .then_with(|| other.session_id.cmp(&self.session_id))
    }
}

/// A client→server wire frame: a per-connection correlation `id` plus the
/// request payload in [`ClientMessageType`].
///
/// `id` is allocated by the client as a monotonic per-connection counter
/// (starting at 0, never reused for the connection's life), and the daemon
/// MUST answer with exactly one `DaemonMessage { id: Some(self.id), .. }` —
/// the terminal reply, success or failure. There is no sentinel and no
/// uncorrelated send.
///
/// The reply `id` and the stream `stream_id` are two ORTHOGONAL axes and are
/// never merged: `id` is per-connection and one-shot (exactly one reply per
/// request), while `stream_id` is per-session and drives the many streaming
/// events a single `RunInput`/`ContinueGeneration` fans out to every session
/// subscriber. Two clients may each use their own request id 0, but a stream
/// fanned to all subscribers needs an id unique in a namespace they share —
/// which is why the two cannot be the same key. The daemon, not the client,
/// assigns `stream_id`; the client learns it from the run's acceptance reply
/// (`SessionEvent::Started`) or, for a pre-acceptance cancel, uses the
/// `CANCEL_ALL` sentinel.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ClientMessage {
    /// The per-connection request id the daemon echoes onto its reply.
    pub id: u64,
    /// The request payload.
    pub inner: ClientMessageType,
}

impl ClientMessage {
    /// Build a request frame: `inner` tagged with correlation `id`.
    #[must_use]
    pub fn request(id: u64, inner: ClientMessageType) -> Self {
        Self { id, inner }
    }
}

/// The request payloads carried inside a [`ClientMessage`].
///
/// The variants are exactly the former `ClientMessage` variants, unchanged;
/// only the envelope around them moved to the [`ClientMessage`] struct's
/// `inner` field. Every variant is a correlated request that receives exactly
/// one terminal reply.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[non_exhaustive]
pub enum ClientMessageType {
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
    /// Submit user input to the attached session. The daemon allocates the
    /// run's `stream_id` when it accepts the request and reports it in the
    /// targeted acceptance reply and the broadcast `SessionEvent::Started`; the
    /// client does not choose it. While the run is pending, `Cancel` with the
    /// `CANCEL_ALL` sentinel (`stream_id = 0`) stops whatever is active on the
    /// attached session; once `Started` arrives the client cancels by the
    /// learned `stream_id`.
    RunInput {
        input: Vec<u8>,
    },
    Cancel {
        stream_id: u64,
    },
    Ping,
    GetCredential {
        service: String,
    },
    ListModels,
    /// Ask the daemon to refresh the models.dev catalog from upstream: a
    /// conditional GET against the cached etag, then a catalog swap when the
    /// remote changed. `force` bypasses the etag (`Cache-Control: no-cache`).
    /// The daemon replies with `DaemonMessageType::ModelsRefreshed` (or
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
    /// Establish (TOFU-bind) the daemon's keystore binding with the 32-byte
    /// X25519 private unlock key. This is the ONLY wire path that can create
    /// the binding: on an unbound keystore the daemon adopts the key (loud
    /// `KEYSTORE BOUND` log), runs the shared unlock tail (same code path as
    /// `AddCredential`'s implicit unlock), and replies
    /// [`DaemonMessageType::Bound`]. On an ALREADY-bound keystore the key is
    /// verified against the binding — a mismatch is rejected with the usual
    /// wrong-key semantics (no unlock, no overwrite). Unlock and `AddCredential`
    /// are strictly VERIFY-ONLY and cannot create a binding.
    BindKeystore {
        key: Vec<u8>,
    },
    AddCredential {
        service: String,
        encrypted_payload: Vec<u8>,
        // Required (not Option): the credential blob is encrypted with the
        // unlock key client-side, so the daemon must be able to
        // test-decrypt + implicitly unlock on receipt (TOFU per-daemon
        // keystore binding). An omitted key would leave an undecryptable
        // blob persisted, breaking the whole keystore.
        unlock_key: Vec<u8>,
    },
    RemoveCredential {
        service: String,
    },
    /// Enroll a NEW client in the daemon's ACL (base64 of the client's
    /// 32-byte transport public key). LOCAL connections only — the daemon
    /// rejects this from TCP clients, because the person physically at the
    /// machine (or an ssh session into it) is the right approver for a
    /// trust decision, and an already-remote client must not be able to
    /// mint new trust. The daemon appends the key to
    /// `authorized_clients.toml` under a file lock; the ACL hot-reload
    /// chain makes it authoritative immediately, and an `AclUpdated`
    /// broadcast informs every connected client.
    AclAdd {
        pubkey: String,
    },
    DeleteSession {
        session_id: u64,
    },
    /// Set (or clear) the session's `pinned` flag. The daemon is the
    /// authority: it updates its metadata index, persists the flag via a
    /// read-modify-write that touches ONLY the two flag columns, and
    /// broadcasts [`SessionEvent::SessionFlagsChanged`] to every subscriber
    /// (the requesting connection included) — that broadcast is the state
    /// update, not the acknowledgement. The requester ALSO gets exactly one
    /// targeted terminal reply: a [`DaemonMessageType::Accepted`] on success,
    /// or a session-scoped [`SessionEvent::SessionFailed`] on failure.
    SetSessionPinned {
        session_id: u64,
        pinned: bool,
    },
    /// Set (or clear) the session's `archived` state. Archiving stamps the
    /// current time into the summary's `archived_at`; unarchiving clears it.
    /// Same daemon-authoritative update/broadcast/ack contract as
    /// [`ClientMessageType::SetSessionPinned`].
    SetSessionArchived {
        session_id: u64,
        archived: bool,
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
    /// Semantically distinct from `RunInput` — the daemon controls the prompt
    /// text. Like `RunInput`, the daemon assigns the run's `stream_id`.
    ContinueGeneration,
    /// Request the raw bytes of one turn attachment in `session_id` — either a
    /// displayed image or a tool-result vision image — selected by [`ImageKey`].
    ///
    /// Attachment bytes are deliberately kept OFF the session-scoped snapshots
    /// (`SessionState`, `TurnAppended`, `TurnsRedone`): displayed images carry
    /// only `ImageMetadata`, and a tool-result vision image carries only a
    /// byte-less `ImageReference` (path + mime + dimensions). A long session's
    /// history therefore ships no image bytes up front; the client fetches each
    /// image on demand — typically when it scrolls into view — and the daemon
    /// replies with a targeted [`DaemonMessageType::Image`].
    ///
    /// The bytes are served from the daemon's durable `session_attachments`
    /// store, keyed exactly like the wire request: (`session_id`, `turn_id`,
    /// slot), where the [`ImageKey`] maps to the slot (`d{index}` for a displayed
    /// image, `r{call_id}` for a tool-result vision image). Both attachments are
    /// append-only within a turn (a new displayed image lands at the next index;
    /// a vision image is pinned to its tool call's id), so the key is a stable
    /// identifier and matches the DB slot the turn was persisted under.
    GetImage {
        session_id: u64,
        turn_id: u32,
        key: ImageKey,
    },
    /// Request the state of every configured MCP server. The daemon replies
    /// with [`DaemonMessageType::McpStatus`].
    McpStatusRequest,
    /// Reconnect one configured MCP server (rebuild its connection and refresh
    /// the tool catalogue), identified by its slug. On success the daemon
    /// replies with a refreshed [`DaemonMessageType::McpStatus`]; on failure with
    /// [`DaemonMessageType::McpReconnectFailed`].
    McpReconnect {
        slug: String,
    },
    /// Reload the MCP server configuration: reconcile the active session's
    /// project `.mcp.json` (its daemon-tier `mcp.json` and `trust.toml` are
    /// hot-reloaded by the daemon's config watcher), rebuilding the tool
    /// catalogue — without restarting the daemon. On success the daemon
    /// replies with [`DaemonMessageType::McpReloaded`]; when the config cannot be
    /// read or parsed, with [`DaemonMessageType::McpReloadFailed`].
    McpReload,
    /// Trust the ACTIVE session's project MCP root: the directory containing
    /// the nearest `.mcp.json` found by walking up from the session's working
    /// directory to the git root. Trust is whole-project (one decision covers
    /// every server that root's `.mcp.json` declares) and content-agnostic (a
    /// root with no `.mcp.json` yet may be trusted). The daemon replies with
    /// [`DaemonMessageType::McpTrustUpdated`].
    McpTrust,
    /// Revoke trust for the ACTIVE session's project MCP root (the same root
    /// [`ClientMessageType::McpTrust`] would trust). Replies with
    /// [`DaemonMessageType::McpTrustUpdated`].
    McpUntrust,
    /// Request the list of currently trusted project MCP roots. Replies with
    /// [`DaemonMessageType::McpTrustList`].
    McpTrustList,
    SubscribeAllActivity,
    UnsubscribeAllActivity,
}

/// A coarse tag naming a request kind, one variant per [`ClientMessageType`].
///
/// The daemon surfaces it in [`DaemonMessageType::Accepted`] /
/// [`DaemonMessageType::Failed`] and clients use it as the log/timeout key, so
/// every request/reply exchange self-identifies without the receiver having to
/// reconstruct which request a bare success reply answered. It is `Copy`
/// because it is a pure tag carried by value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MessageKind {
    CreateSession,
    ListSessions,
    SubscribeSessionsSummary,
    UnsubscribeSessionsSummary,
    AttachSession,
    GetSessionState,
    RunInput,
    Cancel,
    Ping,
    GetCredential,
    ListModels,
    RefreshModels,
    SetModel,
    Unlock,
    Lock,
    BindKeystore,
    AddCredential,
    RemoveCredential,
    AclAdd,
    DeleteSession,
    SetSessionPinned,
    SetSessionArchived,
    AddAccount,
    RemoveAccount,
    ListAccounts,
    SetSessionAccount,
    SetReasoningEffort,
    GetReasoningEffort,
    Undo,
    Redo,
    ContinueGeneration,
    GetImage,
    McpStatusRequest,
    McpReconnect,
    McpReload,
    McpTrust,
    McpUntrust,
    McpTrustList,
    SubscribeAllActivity,
    UnsubscribeAllActivity,
}

impl ClientMessageType {
    /// The [`MessageKind`] tag naming this request's kind.
    #[must_use]
    pub fn kind(&self) -> MessageKind {
        MessageKind::from(self)
    }
}

impl From<&ClientMessageType> for MessageKind {
    fn from(message: &ClientMessageType) -> Self {
        match message {
            ClientMessageType::CreateSession { .. } => Self::CreateSession,
            ClientMessageType::ListSessions => Self::ListSessions,
            ClientMessageType::SubscribeSessionsSummary => Self::SubscribeSessionsSummary,
            ClientMessageType::UnsubscribeSessionsSummary => Self::UnsubscribeSessionsSummary,
            ClientMessageType::AttachSession { .. } => Self::AttachSession,
            ClientMessageType::GetSessionState { .. } => Self::GetSessionState,
            ClientMessageType::RunInput { .. } => Self::RunInput,
            ClientMessageType::Cancel { .. } => Self::Cancel,
            ClientMessageType::Ping => Self::Ping,
            ClientMessageType::GetCredential { .. } => Self::GetCredential,
            ClientMessageType::ListModels => Self::ListModels,
            ClientMessageType::RefreshModels { .. } => Self::RefreshModels,
            ClientMessageType::SetModel { .. } => Self::SetModel,
            ClientMessageType::Unlock { .. } => Self::Unlock,
            ClientMessageType::Lock => Self::Lock,
            ClientMessageType::BindKeystore { .. } => Self::BindKeystore,
            ClientMessageType::AddCredential { .. } => Self::AddCredential,
            ClientMessageType::RemoveCredential { .. } => Self::RemoveCredential,
            ClientMessageType::AclAdd { .. } => Self::AclAdd,
            ClientMessageType::DeleteSession { .. } => Self::DeleteSession,
            ClientMessageType::SetSessionPinned { .. } => Self::SetSessionPinned,
            ClientMessageType::SetSessionArchived { .. } => Self::SetSessionArchived,
            ClientMessageType::AddAccount { .. } => Self::AddAccount,
            ClientMessageType::RemoveAccount { .. } => Self::RemoveAccount,
            ClientMessageType::ListAccounts => Self::ListAccounts,
            ClientMessageType::SetSessionAccount { .. } => Self::SetSessionAccount,
            ClientMessageType::SetReasoningEffort { .. } => Self::SetReasoningEffort,
            ClientMessageType::GetReasoningEffort => Self::GetReasoningEffort,
            ClientMessageType::Undo => Self::Undo,
            ClientMessageType::Redo => Self::Redo,
            ClientMessageType::ContinueGeneration => Self::ContinueGeneration,
            ClientMessageType::GetImage { .. } => Self::GetImage,
            ClientMessageType::McpStatusRequest => Self::McpStatusRequest,
            ClientMessageType::McpReconnect { .. } => Self::McpReconnect,
            ClientMessageType::McpReload => Self::McpReload,
            ClientMessageType::McpTrust => Self::McpTrust,
            ClientMessageType::McpUntrust => Self::McpUntrust,
            ClientMessageType::McpTrustList => Self::McpTrustList,
            ClientMessageType::SubscribeAllActivity => Self::SubscribeAllActivity,
            ClientMessageType::UnsubscribeAllActivity => Self::UnsubscribeAllActivity,
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[non_exhaustive]
pub enum OutputStream {
    Answer,
    Reasoning,
}

/// Which turn attachment a [`ClientMessageType::GetImage`] / [`DaemonMessageType::Image`]
/// addresses. The two attachments are TURN-SCOPED and share one durable byte
/// store (`session_attachments`), so the fetch protocol addresses them with one
/// message and a tagged key rather than two parallel request/reply pairs. The
/// variant names the slot kind; the `session_attachments` slot name itself
/// (`d{index}` / `r{call_id}`) is an internal storage detail the wire never sees.
///
/// # Exhaustive by design
///
/// Deliberately NOT `#[non_exhaustive]`, matching [`DaemonMessage`]: the variant
/// set IS the wire contract, and each variant maps to a DISTINCT storage slot
/// (`d{index}` / `r{call_id}`) that every match site must know. Without the
/// attribute, adding a variant points the compiler at each site that needs it —
/// the daemon's key→slot mapping (`slot_for`), the client's key↔slot conversions,
/// and the size gauge — instead of letting it silently fall through a wildcard.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ImageKey {
    /// A **displayed** image — the `index`-th entry of the turn's
    /// `displayed_images` (produced by `display_image`, `generate_image`, or a
    /// `retrieve_webpage` screenshot). `displayed_images` is append-only within
    /// a turn, so the positional index is a stable identifier.
    Displayed { index: u32 },
    /// A **tool-result vision image** — the normalized image a tool such as
    /// `read_image` fed back to a vision model, attached to the tool result
    /// whose `call_id` this is. Keyed by call id (not a position) because the
    /// image belongs to a specific tool call and the call id is the only stable
    /// handle across turn rewrites. A turn may carry several (one per
    /// image-bearing tool result).
    ToolResult { call_id: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ImageMetadata {
    pub mime_type: String,
    pub width: u32,
    pub height: u32,
    pub byte_len: u64,
    pub alt: Option<String>,
}

/// Outcome of a `ClientMessageType::RefreshModels` request, reported in
/// `DaemonMessageType::ModelsRefreshed`.
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

/// One provider in a `DaemonMessageType::CatalogUpdated` broadcast: the slug the
/// daemon's catalog is keyed by, plus the human-readable display name. A
/// plain wire pair — the TUI maps it into its own `ProviderInfo`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogProvider {
    pub slug: String,
    pub display_name: String,
}

/// The state of one configured MCP server, as carried in
/// [`DaemonMessageType::McpStatus`].
///
/// The same fields as the daemon's own status record
/// (`choreo-daemon`'s `mcp::McpServerStatus`), so the daemon's connection
/// handler converts one to the other field-for-field. Reports both connected
/// servers (with their advertised name/version and tool count) and servers
/// that failed or were skipped at startup (with the recorded `last_error`),
/// so a client can show the whole configured set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServerStatus {
    /// The server's config key (its tool-name namespace, `mcp__<slug>__*`).
    pub slug: String,
    /// The configuration tier this server came from: `"daemon"` for the
    /// daemon-wide `mcp.json`, `"project"` for a session's own `.mcp.json`.
    pub tier: String,
    /// The resolved transport label (`"stdio"` / `"http"`).
    pub transport: String,
    /// The command (stdio) or URL (http) the transport targets.
    pub target: String,
    /// Whether the server is connected and its tools are registered.
    pub connected: bool,
    /// How many tools (excluding the resource catalogue tools) are registered.
    pub tool_count: usize,
    /// The server's self-reported name, once connected.
    pub server_name: Option<String>,
    /// The server's self-reported version, once connected.
    pub server_version: Option<String>,
    /// The last connect/refresh error, when the server is not connected (or a
    /// refresh failed).
    pub last_error: Option<String>,
}

impl McpServerStatus {
    /// A one-line human-readable summary of this server's state, shared by
    /// every front-end so the rendering cannot drift between them.
    #[must_use]
    pub fn summary(&self) -> String {
        if self.connected {
            format!(
                "{} [{} → {}] connected: {} tool(s)",
                self.slug, self.transport, self.target, self.tool_count
            )
        } else {
            format!(
                "{} [{} → {}] not connected: {}",
                self.slug,
                self.transport,
                self.target,
                self.last_error.as_deref().unwrap_or("not connected")
            )
        }
    }
}

/// Session-scoped events produced by the daemon for a specific session.
///
/// These 31 events used to be `DaemonMessage` variants that each carried
/// their own `session_id` field. They now live inside
/// [`DaemonMessageType::Session`], whose envelope supplies the origin session
/// for every session-scoped event: `session_id: Some(id)`, present on the
/// wire as `Some(id)`, so every event has an origin session **by
/// construction** — it can never be forgotten, mismatched, or duplicated.
/// Consumers that dispatch on session events match the envelope's
/// `session_id` once and then handle the inner event; the wire format nests
/// the event inside the envelope, so the origin is always present on the
/// wire too. The `None` case is reserved for connection-level replies that
/// have no origin session (e.g. daemon "no session attached" failures); on
/// the wire the origin is absent as `null`.
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
    /// Direct reply to the creating connection's [`ClientMessageType::CreateSession`].
    ///
    /// Unlike [`SessionEvent::SessionCreated`] — which is BROADCAST to every
    /// subscriber as a notification — this variant is sent ONLY to the client
    /// that sent the `CreateSession`, and it is the one session-lifecycle
    /// message that carries requester-relative intent: a frontend MAY move its
    /// own view (attach to the session the local user just created). Every
    /// OTHER client learns of the new session through the broadcast
    /// `SessionCreated`, which is notification-only and must NEVER move an
    /// existing client's attachment.
    ///
    /// The two are separate variants rather than one flagged variant because
    /// they have different audiences (requester vs. everyone) and different
    /// handling (attach vs. list-update). Overloading one variant for both is
    /// exactly what let another client's creation hijack this client's view
    /// (the daemon allocated the id, so a reply was required, and the reply was
    /// indistinguishable from the broadcast). A sub-session still arrives as a
    /// broadcast `SessionCreated` with a non-null `parent_session_id`.
    SessionCreatedForRequester {
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
        /// The `prompt_tokens` from the most recent API response (the actual
        /// context size being sent to the model), if available.
        #[serde(default)]
        last_prompt_tokens: Option<u32>,
        /// Current session status (`Inactive`, `Inference`, `ToolCall`, etc.).
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
        /// without waiting for a fresh `ListSessions` round-trip.
        last_modified: i64,
    },
    SessionFailed {
        operation: String,
        error: String,
    },
    Started {
        stream_id: u64,
        turn_id: u32,
        estimated_prompt_tokens: u32,
    },
    ToolCallStarted {
        stream_id: u64,
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
        stream_id: u64,
        call_id: String,
        tool_name: String,
    },
    ToolResultChunk {
        stream_id: u64,
        call_id: String,
        data: Vec<u8>,
    },
    ToolCallFailed {
        stream_id: u64,
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
        stream_id: u64,
        output_tokens: u32,
    },
    OutputChunk {
        stream_id: u64,
        stream: OutputStream,
        data: Vec<u8>,
    },
    Done {
        stream_id: u64,
        /// Token usage for the completed request, if reported by the provider.
        token_usage: Option<TokenUsage>,
        /// The `prompt_tokens` from the most recent API response (the actual
        /// context size that was sent to the model), if available.
        #[serde(default)]
        last_prompt_tokens: Option<u32>,
    },
    Failed {
        stream_id: u64,
        error: String,
    },
    Cancelled {
        stream_id: u64,
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
    /// The session's `pinned`/`archived_at` flags changed. This is a
    /// daemon-GENERATED BROADCAST (it rides `DaemonState::broadcast()`, the
    /// lifecycle fan-out that reaches both activity and summary subscribers),
    /// NOT a targeted reply: the daemon command loop owns the flags, updates
    /// its metadata index, persists them, and emits this once for every client
    /// — the requesting client included. The requester's terminal
    /// acknowledgement is a SEPARATE targeted reply: `Accepted { kind }` on
    /// success, `SessionFailed` on failure. It carries the full post-change
    /// flag state so a subscriber can update its view directly.
    SessionFlagsChanged {
        pinned: bool,
        archived_at: Option<i64>,
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

/// Authoritative daemon keystore STATUS (see [`DaemonMessageType::Keystore`]).
///
/// Three states, because "unbound" is a distinct fact from "locked": a fresh
/// daemon has no binding at all and a client must BIND it (auto-bind with a
/// freshly minted key), whereas a bound-but-locked daemon needs only an
/// `Unlock`. Collapsing the two into a single "locked" boolean is what left a
/// first-run client unable to bind the daemon (it had no key, and the wire
/// carried no signal that the keystore was unbound).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum KeystoreState {
    /// No binding exists yet — a fresh daemon. A client mints a bind key and
    /// sends [`ClientMessageType::BindKeystore`]; the frontends do this
    /// automatically, once per connection, when they observe this state.
    Unbound,
    /// A binding exists, but no cleartext credentials are in memory. A client
    /// unlocks by presenting the bound key via [`ClientMessageType::Unlock`].
    Locked,
    /// A binding exists and the credentials are decrypted in memory.
    Unlocked,
}

/// Messages sent from the daemon to a client.
///
/// Split into two families:
/// - [`DaemonMessageType::Session`] carries a session-scoped [`SessionEvent`]
///   wrapped in an envelope that supplies the origin session — always
///   `session_id: Some(id)` for session-scoped events, present on the wire
///   as `Some(id)` — by construction. These events are delivered to
///   per-session subscribers and the all-activity fan, and the all-activity
///   dedup is keyed on the envelope's origin session.
/// - The remaining flat variants are connection/reply/global messages
///   (`Sessions`, `Pong`, `Models`, keystore and account replies,
///   `ShuttingDown`, …) that have no session scope and stay flat. A
///   connection-level reply with NO origin session (e.g. a daemon "no
///   session attached" failure) carries `session_id: None`, absent on the
///   wire as `null`.
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
///
/// A daemon→client wire frame: an optional correlation `id` plus the payload
/// in [`DaemonMessageType`].
///
/// `id: Some(n)` is the targeted terminal reply to the client request `n`
/// (exactly one such reply per request, success or failure); `id: None` is a
/// broadcast, which never resolves a request. Reply-ness is a property of the
/// SEND, not the payload type: the same `inner` (e.g. `SessionState`,
/// `ReasoningEffortSet`) may be emitted with `id: Some` (a reply) or `id: None`
/// (a broadcast). A client always applies the payload's state effect, and
/// ADDITIONALLY resolves a pending request slot iff `id` is `Some`. A variant
/// is split into a separate type only when the reply carries requester-
/// relative intent that no broadcast may carry (the
/// `SessionCreatedForRequester` precedent).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DaemonMessage {
    /// The request id this frame answers, or `None` for a broadcast. Absent
    /// from the wire when `None` (`skip_serializing_if`), so broadcasts stay
    /// compact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<u64>,
    /// The payload.
    pub inner: DaemonMessageType,
}

impl DaemonMessage {
    /// Build a targeted reply to request `id`.
    #[must_use]
    pub fn reply(id: u64, inner: DaemonMessageType) -> Self {
        Self {
            id: Some(id),
            inner,
        }
    }

    /// Build a broadcast (carries no correlation id and resolves no request).
    #[must_use]
    pub fn broadcast(inner: DaemonMessageType) -> Self {
        Self { id: None, inner }
    }
}

/// The payloads carried inside a [`DaemonMessage`].
///
/// The variants are exactly the former `DaemonMessage` variants, unchanged;
/// only the envelope around them moved to the [`DaemonMessage`] struct.
#[expect(clippy::large_enum_variant)]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum DaemonMessageType {
    /// Single home for all session-scoped events. The envelope supplies the
    /// origin `session_id` that used to ride on each of the 31 moved
    /// variants, so every inner [`SessionEvent`] has an origin session **by
    /// construction** when `session_id` is `Some(id)` (present on the wire
    /// as `Some(id)`). Connection-level replies with no origin session (e.g.
    /// daemon "no session attached" failures) carry `session_id: None`,
    /// absent on the wire as `null`.
    Session {
        session_id: Option<u64>,
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
    /// Targeted reply to [`ClientMessageType::Unlock`]: the presented key matched
    /// the binding and the daemon decrypted its credentials. This is an
    /// OPERATION OUTCOME; the current keystore *status* is pushed separately
    /// as [`DaemonMessageType::Keystore`].
    Unlocked,
    /// Targeted reply to [`ClientMessageType::Lock`]: the daemon wiped its
    /// in-memory credentials and re-latched the locked state. An OPERATION
    /// OUTCOME; the current keystore *status* is pushed separately as
    /// [`DaemonMessageType::Keystore`].
    Locked,
    /// The daemon's authoritative keystore STATUS. Pushed to a client the
    /// moment it registers as an ACTIVITY subscriber and broadcast to every
    /// activity subscriber on each transition, so a client latches the real
    /// state instead of inferring it from operation replies. (A client that
    /// subscribes ONLY to session summaries does NOT receive it — a frontend
    /// that only needs the session list drives its keystore itself, e.g. an
    /// unlock-at-connect probe.) The `Unbound` push is what lets a first-run
    /// client with no key
    /// bind the daemon automatically.
    Keystore {
        state: KeystoreState,
    },
    LockedError {
        error: String,
    },
    /// Targeted reply to [`ClientMessageType::BindKeystore`] when an unbound
    /// keystore adopted the presented key and the implicit unlock succeeded.
    /// Distinct from [`DaemonMessageType::Unlocked`] so the client can tell "I just
    /// created this binding" from "I verified an existing one". Ordering
    /// contract: the daemon serializes this targeted reply to the acting
    /// client's socket BEFORE the lock-state transition broadcast, so the
    /// client can key-record on this message knowing the broadcast cannot
    /// overtake it.
    Bound,
    /// Error reply for `Unlock` / `AddCredential` / `BindKeystore` against a daemon
    /// whose keystore has NO binding yet. Deliberately distinct from
    /// [`DaemonMessageType::LockedError`] (which means "bound but wrong key") so
    /// the client knows this daemon has never been bound and can AUTO-BIND it
    /// with a freshly generated key instead of replaying a stored key that
    /// can never match a nonexistent binding.
    KeystoreUnbound {
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
    /// Reply to [`ClientMessageType::AclAdd`]: `ok` false carries the failure
    /// reason in `message` (rejected transport, bad key, I/O error); `ok`
    /// true carries the new total of authorized clients.
    AclAddResult {
        ok: bool,
        message: String,
    },
    /// Global broadcast (connection-level, no session) after a successful
    /// ACL change: the new total of authorized client keys. Clients that
    /// surface ACL information can refresh; it carries no key material.
    AclUpdated {
        clients: u64,
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
    /// Reply to `ClientMessageType::RefreshModels`. `status` distinguishes
    /// "nothing changed" (304) from a real swap (200), and a forced swap.
    ModelsRefreshed {
        providers: usize,
        models: usize,
        status: RefreshStatus,
    },
    /// Reply to `ClientMessageType::RefreshModels` when the fetch/merge failed.
    ModelsRefreshFailed {
        error: String,
    },
    /// Broadcast whenever the daemon swaps the provider catalog (startup
    /// refresh, user-overlay reload, `/refresh-models`). Carries the full
    /// provider list so clients can replace their static default picker.
    CatalogUpdated {
        providers: Vec<CatalogProvider>,
    },
    /// Targeted reply to [`ClientMessageType::GetImage`]: the raw bytes of the
    /// requested attachment (displayed image or tool-result vision image), or
    /// `None` when it is not found (the session or turn was deleted, the
    /// attachment was evicted, or the key is stale). `Some(vec![])` is a
    /// genuinely zero-byte image — distinct from `None` (unknown), so the client
    /// can tell "empty image" from "fetch failed" and avoid retrying a missing
    /// image forever. The `session_id`, `turn_id`, and `key` echo the request so
    /// a client with several fetches in flight can route the reply.
    Image {
        session_id: u64,
        turn_id: u32,
        key: ImageKey,
        data: Option<Vec<u8>>,
    },
    /// Reply to [`ClientMessageType::McpStatusRequest`], and the success reply to
    /// [`ClientMessageType::McpReconnect`]: the current state of every visible
    /// MCP server (daemon-tier servers plus, for an attached session, that
    /// session's own project servers), each tagged with its `tier`. Carries
    /// the active session's project root and its trust state, plus the slugs
    /// of any project servers that were read but ignored because the root is
    /// untrusted.
    McpStatus {
        servers: Vec<McpServerStatus>,
        /// The active session's project MCP root, when it has a working
        /// directory that resolves to one.
        project_root: Option<String>,
        /// Whether `project_root` (when present) is trusted.
        project_trusted: bool,
        /// Slugs of project servers declared by an UNTRUSTED `.mcp.json` —
        /// read so the operator can see what is being ignored, never spawned.
        ignored_project_servers: Vec<String>,
    },
    /// Reply to [`ClientMessageType::McpReconnect`] when the reconnect failed: the
    /// slug it targeted and the failure reason.
    McpReconnectFailed {
        slug: String,
        error: String,
    },
    /// Reply to [`ClientMessageType::McpReload`]: a one-line human-readable summary
    /// of what the reload changed (added/removed/restarted/unchanged/failed
    /// counts) plus the refreshed state of every configured server.
    McpReloaded {
        summary: String,
        servers: Vec<McpServerStatus>,
    },
    /// Reply to [`ClientMessageType::McpReload`] when the reload could not run at
    /// all (the config file could not be read or parsed): the failure reason.
    McpReloadFailed {
        error: String,
    },
    /// Reply to [`ClientMessageType::McpTrust`] / [`ClientMessageType::McpUntrust`]: the
    /// resulting trust state of the target root (or `None` when the active
    /// session has no resolvable project root) plus a one-line human-readable
    /// summary of what happened.
    McpTrustUpdated {
        root: Option<String>,
        trusted: bool,
        message: String,
    },
    /// Reply to [`ClientMessageType::McpTrustList`]: the trusted project MCP
    /// roots, in stable (sorted) order.
    McpTrustList {
        roots: Vec<String>,
    },
    ShuttingDown,
    /// Best-effort advisory, sent by the daemon immediately before it
    /// disconnects a client that has fallen too far behind the streaming
    /// frontier (lag eviction). Clients use it to distinguish an
    /// "evicted for lag" disconnect from a daemon crash. Best-effort: the
    /// daemon may drop the connection before this message is flushed, so
    /// clients must not treat its absence as meaningful.
    Evicted,
    /// Terminal success reply to a request whose outcome is otherwise silent
    /// (a fire-and-confirm mutation: pinned/archived flags, title/working-dir,
    /// undo/redo, subscriptions, …): the request was accepted and applied. The
    /// request's own outcome broadcast (if any) still rides `id: None`; this
    /// acknowledgement exists so every request has exactly one targeted reply.
    Accepted {
        kind: MessageKind,
    },
    /// Terminal failure reply to any request that could not be honoured, tagged
    /// with the request's [`MessageKind`] and a human-readable reason. This is
    /// the uniform failure channel: a request that used to fall through a
    /// dispatch wildcard now has somewhere concrete to report why.
    Failed {
        kind: MessageKind,
        error: String,
    },
}

#[cfg(test)]
mod tests;
