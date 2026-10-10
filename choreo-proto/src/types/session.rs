//! Session-scoped types: the persisted [`Turn`], the session
//! summary/status metadata, and the 31 [`SessionEvent`]s a run fans out to
//! every subscriber.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use super::TimestampMs;
use super::common::{ReasoningCapability, TokenUsage};
use super::daemon::{ImageMetadata, OutputStream};

/// One displayed image attached to a turn: the image's metadata, its raw
/// bytes, and the id of the tool call that produced it (when one did).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DisplayedImageRecord {
    /// The image's MIME type, pixel dimensions, byte length, and alt text.
    pub metadata: ImageMetadata,
    /// The raw encoded image bytes.
    pub data: Vec<u8>,
    /// The `call_id` of the tool call that displayed this image (e.g.
    /// `display_image`), or `None` for a user-supplied or non-tool image.
    pub tool_call_id: Option<String>,
}

/// One assistant tool call recorded on a turn: the provider's call id, the tool
/// name, and the raw JSON arguments the model produced.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AssistantToolCallRecord {
    /// The provider-assigned call id, echoed back on the matching tool result.
    pub call_id: String,
    /// The tool name the model invoked.
    pub name: String,
    /// The JSON-encoded arguments the model produced for the call.
    pub arguments_json: String,
}

/// One tool result recorded on a turn: the matching call id, the tool name, the
/// result content, and whether the tool reported an error.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolResultRecord {
    /// The `call_id` of the tool call this result answers.
    pub call_id: String,
    /// The tool name that produced the result.
    pub name: String,
    /// The result payload (text, or a serialized tool-specific rendering).
    pub content: String,
    /// Whether the tool reported the call as failed.
    pub is_error: bool,
    /// Human-readable description of the invocation (e.g. the command run),
    /// rendered alongside the result.
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
        /// Which chat field the reasoning text was captured from, so it is
        /// re-emitted to the same field.
        field: ChatReasoningField,
        /// The reasoning text bytes, exactly as the provider sent them.
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
    /// The slug of the provider that produced the artifact.
    pub provider_slug: String,
    /// The model id that produced the artifact.
    pub model: String,
}

/// One persisted turn of a session: the user's input, the assistant's response
/// (text and/or tool calls), any reasoning, the tool results, and the images
/// displayed, plus the turn's own status flags.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Turn {
    /// When the turn was created.
    pub created_at: TimestampMs,
    /// Whether the turn has been undone.
    pub undone: bool,
    /// A terminal error that aborted the turn, if any.
    pub error: Option<String>,
    /// The user's input text for this turn, when it carried one.
    pub user_text: Option<String>,
    /// The assistant's final answer text for this turn, when it produced one.
    pub assistant_text: Option<String>,
    /// The assistant's reasoning text for this turn, when the model exposed it.
    pub assistant_reasoning: Option<String>,
    /// The tool calls the assistant requested this turn, in order.
    pub tool_calls: Vec<AssistantToolCallRecord>,
    /// Token usage for this turn, if the provider reported it.
    pub token_usage: Option<TokenUsage>,
    /// The results of the tool calls, in order.
    pub tool_results: Vec<ToolResultRecord>,
    /// The images displayed during this turn, in display order.
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

/// The processing state of a session, as last reported by its thread.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[non_exhaustive]
pub enum SessionStatus {
    /// The session's thread has parked; only an `AttachSession` reloads it.
    Sleeping,
    /// Default initial state — session is loaded and ready but not processing.
    #[default]
    Inactive,
    /// The session is streaming an inference response.
    Inference,
    /// The session is running a tool call; the string is the tool name.
    ToolCall(String),
    /// The daemon received a retryable HTTP error (429/5xx/connection) and is
    /// waiting before the next attempt.  Displayed in the TUI so the user
    /// knows the model call hasn't stalled and can choose to cancel.
    Retrying {
        /// The attempt number about to be made (1-based).
        attempt: u32,
        /// The maximum number of attempts configured before giving up.
        max_attempts: u32,
        /// The backoff delay before the next attempt, in milliseconds.
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

/// A session's metadata row, as listed and ordered by the sessions manager:
/// identity, display fields, timestamps, status, and the pinned/archived flags.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionSummary {
    /// The session's unique id.
    pub session_id: u64,
    /// The session's title, when set.
    pub title: Option<String>,
    /// The currently selected model id, when set.
    pub selected_model: Option<String>,
    /// The current reasoning-effort slug, when set.
    pub reasoning_effort: Option<String>,
    /// The parent session this one branches from, when it is a sub-session.
    pub parent_session_id: Option<u64>,
    /// The session's working directory, when set.
    pub working_dir: Option<String>,
    /// Session creation time, Unix-epoch-milliseconds.
    pub created_at: i64,
    /// Most recent modification time, Unix-epoch-milliseconds.  Bumped by the
    /// daemon whenever the session's status, title, model, or turn count
    /// changes, and used to order the sessions list (newest first).
    pub last_modified: i64,
    /// The number of turns the session currently holds.
    pub turn_count: u32,
    /// The session's current processing state.
    pub status: SessionStatus,
    /// The tool groups active for the session (e.g. `shell`, `web`).
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

/// Session-scoped events produced by the daemon for a specific session.
///
/// These 31 events used to be `DaemonMessage` variants that each carried
/// their own `session_id` field. They now live inside
/// [`DaemonMessageType::Session`](crate::DaemonMessageType::Session), whose envelope supplies the origin session
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
    /// Broadcast notification that a new session was created, sent to every
    /// subscriber. A sub-session arrives here with a non-null
    /// `parent_session_id`.
    SessionCreated {
        /// The new session's initial title, when set.
        title: Option<String>,
        /// The parent session the new one branches from, when it is a
        /// sub-session.
        parent_session_id: Option<u64>,
        /// The new session's working directory, when set.
        working_dir: Option<String>,
        /// The account the new session is bound to, when set.
        account_name: Option<String>,
        /// The model the new session selected, when set.
        selected_model: Option<String>,
        /// The reasoning-effort slug the new session started with, when set.
        reasoning_effort: Option<String>,
    },
    /// Direct reply to the creating connection's [`ClientMessageType::CreateSession`](crate::ClientMessageType::CreateSession).
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
        /// The new session's initial title, when set.
        title: Option<String>,
        /// The parent session the new one branches from, when a sub-session.
        parent_session_id: Option<u64>,
        /// The new session's working directory, when set.
        working_dir: Option<String>,
        /// The account the new session is bound to, when set.
        account_name: Option<String>,
        /// The model the new session selected, when set.
        selected_model: Option<String>,
        /// The reasoning-effort slug the new session started with, when set.
        reasoning_effort: Option<String>,
    },
    /// Reply confirming the requester is now attached to a session; carries no
    /// payload (the state arrives separately as
    /// [`SessionEvent::SessionState`]).
    SessionAttached,
    /// The full state of a session, sent on attach (and as a resync): the
    /// session's metadata, every turn, and the live status/token fields.
    SessionState {
        /// The session's title, when set.
        title: Option<String>,
        /// The currently selected model id, when set.
        selected_model: Option<String>,
        /// The parent session this one branches from, when a sub-session.
        parent_session_id: Option<u64>,
        /// The session's working directory, when set.
        working_dir: Option<String>,
        /// Every turn of the session, keyed by turn id.
        turns: BTreeMap<u32, Turn>,
        /// The tool groups active for the session.
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
        /// The current reasoning-effort slug, when set.
        #[serde(default)]
        reasoning_effort: Option<String>,
        /// The selected model's reasoning capability, when known.
        #[serde(default)]
        reasoning_capability: Option<ReasoningCapability>,
    },
    /// A new turn was appended to the session.
    TurnAppended {
        /// The id assigned to the appended turn.
        turn_id: u32,
        /// The turn's full content.
        turn: Turn,
    },
    /// The session's processing status changed.
    SessionStatusChanged {
        /// The new status.
        status: SessionStatus,
        /// Unix-epoch-milliseconds timestamp of this status change, so the
        /// TUI can re-sort the sessions list (most recently modified first)
        /// without waiting for a fresh `ListSessions` round-trip.
        last_modified: i64,
    },
    /// A session-scoped operation failed; distinct from a per-run
    /// [`SessionEvent::Failed`].
    SessionFailed {
        /// The operation that failed (e.g. `SetSessionPinned`).
        operation: String,
        /// The human-readable failure reason (also delivered to the requester
        /// as its targeted reply).
        error: String,
    },
    /// The daemon accepted a run: it carries the allocated `stream_id` and the
    /// turn about to be generated, so subscribers can key the stream.
    Started {
        /// The run's stream id, allocated by the daemon.
        stream_id: u64,
        /// The id of the turn this run will produce.
        turn_id: u32,
        /// The daemon's estimate of the run's prompt tokens.
        estimated_prompt_tokens: u32,
    },
    /// A tool call has started; carries the call's identity and arguments
    /// before any output arrives.
    ToolCallStarted {
        /// The run's stream id.
        stream_id: u64,
        /// The provider-assigned call id for this tool call.
        call_id: String,
        /// The tool name being invoked.
        tool_name: String,
        /// The raw JSON arguments the model produced for the call.
        arguments_json: String,
        /// Human-readable invocation description (e.g. "Running command:
        /// `cargo build`.") so clients can render the tool's context as soon
        /// as the call starts, without waiting for the first streaming chunk
        /// or the final result.  Mirrors `ToolOutput.invocation_description`.
        invocation_description: String,
    },
    /// A tool call finished successfully.
    ToolCallFinished {
        /// The run's stream id.
        stream_id: u64,
        /// The call id of the finished tool call.
        call_id: String,
        /// The tool name that finished.
        tool_name: String,
    },
    /// A chunk of a tool's streaming output.
    ToolResultChunk {
        /// The run's stream id.
        stream_id: u64,
        /// The call id the chunk belongs to.
        call_id: String,
        /// A chunk of the tool's streaming output.
        data: Vec<u8>,
    },
    /// A tool call failed.
    ToolCallFailed {
        /// The run's stream id.
        stream_id: u64,
        /// The call id of the failed tool call.
        call_id: String,
        /// The tool name that failed.
        tool_name: String,
        /// The human-readable failure reason.
        error: String,
    },
    /// A mid-run token-usage update for the session.
    TokenUsageUpdate {
        /// The updated accumulated token usage.
        token_usage: TokenUsage,
        /// The most recent `prompt_tokens`, if available.
        last_prompt_tokens: Option<u32>,
    },
    /// Cumulative output-token estimate for the current turn, updated as
    /// each stream chunk arrives.  Used by the TUI for live token display.
    LiveOutputTokenCount {
        /// The run's stream id.
        stream_id: u64,
        /// The cumulative output-token count so far.
        output_tokens: u32,
    },
    /// A chunk of streamed model output (answer or reasoning).
    OutputChunk {
        /// The run's stream id.
        stream_id: u64,
        /// Which stream (answer vs. reasoning) this chunk belongs to.
        stream: OutputStream,
        /// The chunk's raw bytes.
        data: Vec<u8>,
    },
    /// The run completed successfully.
    Done {
        /// The run's stream id.
        stream_id: u64,
        /// Token usage for the completed request, if reported by the provider.
        token_usage: Option<TokenUsage>,
        /// The `prompt_tokens` from the most recent API response (the actual
        /// context size that was sent to the model), if available.
        #[serde(default)]
        last_prompt_tokens: Option<u32>,
    },
    /// The run failed terminally.
    Failed {
        /// The run's stream id.
        stream_id: u64,
        /// The human-readable failure reason.
        error: String,
    },
    /// The run was cancelled.
    Cancelled {
        /// The run's stream id.
        stream_id: u64,
    },
    /// The session's model was selected (or changed).
    ModelSelected {
        /// The newly selected model id.
        model: String,
        /// The new model's reasoning capability, when known.
        #[serde(default)]
        reasoning_capability: Option<ReasoningCapability>,
    },
    /// The session's model could not be selected.
    ModelSelectionFailed {
        /// The model id that failed to select.
        model: String,
        /// The human-readable failure reason.
        error: String,
    },
    /// The session was deleted; subscribers can drop it from their lists.
    SessionDeleted,
    /// The session could not be deleted.
    SessionDeleteFailed {
        /// The human-readable failure reason.
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
        /// Whether the session is now pinned.
        pinned: bool,
        /// When the session was archived, or `None` when it is not archived.
        archived_at: Option<i64>,
    },
    /// Turns were undone; the listed turn ids are now marked undone.
    TurnsUndone {
        /// The ids of the turns that were undone.
        turn_ids: Vec<u32>,
    },
    /// Turns were redone; carries their restored content keyed by turn id.
    TurnsRedone {
        /// The redone turns, keyed by turn id.
        turns: BTreeMap<u32, Turn>,
    },
    /// The session's bound provider account changed.
    SessionAccountSet {
        /// The new account name.
        account: String,
    },
    /// The session's model context window was resolved.
    ContextWindowResolved {
        /// The resolved context window size, in tokens.
        context_window: u32,
    },
    /// The session's working directory changed.
    SessionWorkingDirSet {
        /// The new working directory, or `None` when cleared.
        path: Option<String>,
    },
    /// The session's title changed.
    SessionTitleSet {
        /// The new title.
        title: String,
    },
    /// The session's reasoning effort was set.
    ReasoningEffortSet {
        /// The new reasoning-effort slug.
        effort: String,
    },
    /// The session's reasoning effort could not be set.
    ReasoningEffortSetFailed {
        /// The effort slug that failed to apply.
        effort: String,
        /// The human-readable failure reason.
        error: String,
    },
}

impl SessionEvent {
    /// A stable, payload-free name for the variant, for logging and metrics.
    ///
    /// A `SessionEvent` carries session payload — turn text, tool arguments
    /// and output, stream bytes — so its `Debug` output must never be formatted
    /// into a log line or a bail string. Log this tag instead (alongside the
    /// envelope's origin session id). See [`DaemonMessageType::kind`](crate::DaemonMessageType::kind).
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::SessionCreated { .. } => "SessionCreated",
            Self::SessionCreatedForRequester { .. } => "SessionCreatedForRequester",
            Self::SessionAttached => "SessionAttached",
            Self::SessionState { .. } => "SessionState",
            Self::TurnAppended { .. } => "TurnAppended",
            Self::SessionStatusChanged { .. } => "SessionStatusChanged",
            Self::SessionFailed { .. } => "SessionFailed",
            Self::Started { .. } => "Started",
            Self::ToolCallStarted { .. } => "ToolCallStarted",
            Self::ToolCallFinished { .. } => "ToolCallFinished",
            Self::ToolResultChunk { .. } => "ToolResultChunk",
            Self::ToolCallFailed { .. } => "ToolCallFailed",
            Self::TokenUsageUpdate { .. } => "TokenUsageUpdate",
            Self::LiveOutputTokenCount { .. } => "LiveOutputTokenCount",
            Self::OutputChunk { .. } => "OutputChunk",
            Self::Done { .. } => "Done",
            Self::Failed { .. } => "Failed",
            Self::Cancelled { .. } => "Cancelled",
            Self::ModelSelected { .. } => "ModelSelected",
            Self::ModelSelectionFailed { .. } => "ModelSelectionFailed",
            Self::SessionDeleted => "SessionDeleted",
            Self::SessionDeleteFailed { .. } => "SessionDeleteFailed",
            Self::SessionFlagsChanged { .. } => "SessionFlagsChanged",
            Self::TurnsUndone { .. } => "TurnsUndone",
            Self::TurnsRedone { .. } => "TurnsRedone",
            Self::SessionAccountSet { .. } => "SessionAccountSet",
            Self::ContextWindowResolved { .. } => "ContextWindowResolved",
            Self::SessionWorkingDirSet { .. } => "SessionWorkingDirSet",
            Self::SessionTitleSet { .. } => "SessionTitleSet",
            Self::ReasoningEffortSet { .. } => "ReasoningEffortSet",
            Self::ReasoningEffortSetFailed { .. } => "ReasoningEffortSetFailed",
        }
    }
}
