//! Shared leaf types used across every message family: reasoning capability,
//! session context config, token usage, discarded tool calls, the provider
//! error type, and account metadata.

use serde::{Deserialize, Serialize};
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
