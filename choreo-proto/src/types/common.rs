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
    /// File names (relative to each directory walked) treated as session
    /// context and folded into the system prompt, in this order. Defaults to
    /// `["AGENTS.md", "CLAUDE.md"]`.
    #[serde(default = "default_context_file_names")]
    pub context_file_names: Vec<String>,
    /// Per-file byte cap: a discovered context file larger than this is
    /// truncated before it reaches the prompt. Defaults to 32 KiB.
    #[serde(default = "default_context_file_max_bytes")]
    pub context_file_max_bytes: usize,
    /// When `true`, suppresses the built-in `CLAUDE.md` discovery so only the
    /// configured `context_file_names` are read. Defaults to `false`.
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

/// Per-endpoint OAuth grant encoding, as carried on the wire.
///
/// A descriptor exposes the two encodings separately because the code exchange
/// and the refresh can differ (e.g. the `ChatGPT`/Codex backend accepts a
/// form-encoded code exchange but a JSON refresh), so the login library must
/// serialize each request according to its own endpoint. The type is defined
/// once in `choreo-shared` (re-exported from the catalog as `GrantEncoding` and
/// persisted inside an OAuth credential), so this is the same type the catalog
/// and the credential name rather than a one-for-one copy.
pub use choreo_shared::OAuthGrantEncoding;

/// The client-identity posture a provider's OAuth flow uses: register our own
/// client, borrow the vendor CLI's client id behind an opt-in, or delegate to
/// the vendor's tooling.
///
/// It mirrors the compiled client-identity policy one-for-one; the wire enum
/// exists so a frontend learns the posture without embedding a copy of the
/// daemon-side policy table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OAuthClientIdentity {
    /// The provider registers (or dynamically obtains) its own client id.
    Registered,
    /// The provider borrows the vendor CLI's public client id; the daemon only
    /// releases the borrowed id when the per-install opt-in is enabled.
    Borrowed,
    /// The provider does not run the flow itself and delegates to the vendor's
    /// CLI/SDK.
    Delegate,
}

/// A provider's OAuth sign-in parameters, as served by the daemon.
///
/// The daemon assembles one of these per provider from the catalog's OAuth
/// profile data plus the compiled client-identity policy, so a frontend runs a
/// sign-in without embedding a copy of the issuer, scopes, redirect ports,
/// encodings, or client identity.
/// The borrowed vendor client id is withheld (`client_id: None`) unless the
/// per-install opt-in is enabled.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthProviderDescriptor {
    /// The provider slug (the catalog key the account names, e.g. `openai`).
    pub provider: String,
    /// Authorization-server issuer (e.g. `https://auth.openai.com`).
    pub issuer: String,
    /// Path of the authorization endpoint, relative to `issuer`.
    pub authorize_path: String,
    /// Path of the token endpoint, relative to `issuer`.
    pub token_path: String,
    /// The PUBLIC client id to send at the authorize endpoint, or `None` when
    /// the flow must obtain its own (dynamic registration) or the borrowed id
    /// is withheld because the opt-in is off. Never a client secret.
    pub client_id: Option<String>,
    /// Scopes requested at login.
    pub scopes: Vec<String>,
    /// Loopback redirect ports to try, in order (the first free one wins).
    pub redirect_ports: Vec<u16>,
    /// Encoding for the authorization-code exchange request body.
    pub code_exchange_encoding: OAuthGrantEncoding,
    /// Encoding for the refresh request body.
    pub refresh_encoding: OAuthGrantEncoding,
    /// The client-identity posture for this provider.
    pub client_identity: OAuthClientIdentity,
    /// Whether the provider offers a device-code (headless) flow.
    pub device_code: bool,
    /// Path of the RFC 8628 device-authorization endpoint (the `usercode`
    /// request), relative to `issuer`. `None` when the provider has no
    /// device-code flow.
    pub device_authorization_path: Option<String>,
    /// Path of the device-code token-poll endpoint, relative to `issuer`.
    /// `None` when the provider has no device-code flow.
    pub device_token_path: Option<String>,
}

/// Token usage for a single LLM turn or accumulated for a session.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct TokenUsage {
    /// The FULL prompt token count for the request (or accumulated for the
    /// session) — it includes any prompt-cache reads (`cached_tokens`) and
    /// writes (`cache_write_tokens`), which are a subset of this count, not an
    /// addition to it. Every provider reports its input this way by the time
    /// the count reaches here; a provider whose raw input counter excludes
    /// cache (Anthropic's post-breakpoint `input_tokens`) folds the cache
    /// counts in at the adapter boundary.
    pub input_tokens: u32,
    /// Completion (output) tokens generated by the model.
    pub output_tokens: u32,
    /// The provider's reported total (`input + output`); kept as its own field
    /// so a provider that reports a total differing from the sum is trusted
    /// verbatim.
    pub total_tokens: u32,
    /// Prompt tokens served from the provider's prompt cache — the cache
    /// **read/hit** count. Each adapter maps its own wire field here: Anthropic's
    /// `usage.cache_read_input_tokens`, the `OpenAI`-compatible
    /// `usage.prompt_tokens_details.cached_tokens` (chat completions) or
    /// `usage.input_tokens_details.cached_tokens` (Responses), `DeepSeek`'s flat
    /// `usage.prompt_cache_hit_tokens`, and Gemini's
    /// `usageMetadata.cachedContentTokenCount`. A subset of `input_tokens`, not
    /// an addition to it: cached input is priced differently, so cost/usage
    /// reporting tracks it separately. 0 when the provider does not report it;
    /// `#[serde(default)]` keeps old payloads and providers that omit the count
    /// deserializing cleanly.
    #[serde(default)]
    pub cached_tokens: u32,
    /// Prompt tokens written to the provider's prompt cache (e.g. Anthropic's
    /// `usage.cache_creation_input_tokens`). A subset of `input_tokens`, not an
    /// addition to it, and priced differently from a read (`cached_tokens`), so
    /// cost reporting tracks it separately. 0 when the provider does not report
    /// it.
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

    /// Add `other`'s counts into `self`, a running session total.
    ///
    /// This is the summation counterpart to [`Self::merge_max`]: where
    /// `merge_max` keeps a monotonic running maximum across repeated
    /// snapshots of the *same* cumulative figure (the daemon's mid-turn sync),
    /// `accumulate` folds one *turn's* usage into a total rebuilt from scratch
    /// over the turn history. Every field is summed — including the prompt-cache
    /// read (`cached_tokens`) and write (`cache_write_tokens`) counts — so a
    /// session's accumulated usage carries the same cache accounting its
    /// individual turns reported. Both the worker's per-turn accumulation and
    /// the on-load reconstruction route through this one method, so the two can
    /// never silently diverge on which fields they add.
    pub fn accumulate(&mut self, other: TokenUsage) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.total_tokens += other.total_tokens;
        self.cached_tokens += other.cached_tokens;
        self.cache_write_tokens += other.cache_write_tokens;
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
    /// The tool name the provider invoked (e.g. `write_file`).
    pub name: String,
    /// The raw — possibly cropped or non-JSON — argument payload the provider
    /// actually returned, retained so the failure stays diagnosable without
    /// the raw network log.
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
    /// The API key was rejected (`401`).
    #[error("unauthorized ({status}): {detail}")]
    Unauthorized {
        /// The HTTP status the provider returned.
        status: u16,
        /// The provider's error body, verbatim.
        detail: String,
    },
    /// The provider rate-limited the request (`429`).
    #[error("rate limited ({status}): {detail}")]
    RateLimited {
        /// The HTTP status the provider returned.
        status: u16,
        /// The `Retry-After` delay, when the provider supplied one.
        retry_after_secs: Option<u64>,
        /// The provider's error body, verbatim.
        detail: String,
    },
    /// The provider returned a `5xx` server error.
    #[error("server error ({status}): {detail}")]
    ServerError {
        /// The HTTP status the provider returned.
        status: u16,
        /// The provider's error body, verbatim.
        detail: String,
    },
    /// The provider returned a non-`429` `4xx` client error.
    #[error("client error ({status}): {detail}")]
    ClientError {
        /// The HTTP status the provider returned.
        status: u16,
        /// The provider's error body, verbatim.
        detail: String,
    },
    /// The provider returned a success status with no message content.
    #[error("provider returned an empty response")]
    EmptyResponse,
    /// The provider accepted the request but the referenced artifact (e.g. a
    /// generated image on its CDN) is not available yet — a propagation race,
    /// not an HTTP error and not an empty response. Retryable by callers that
    /// know how to wait; distinct from [`InferenceError::EmptyResponse`] so a
    /// genuinely empty body is never mistaken for "not published yet".
    #[error("provider artifact not ready yet: {detail}")]
    NotReady {
        /// The provider's explanation of what is not yet available.
        detail: String,
    },
    /// The provider's content filter blocked the generation. No HTTP status
    /// accompanied the response (it came back with a success code), so this
    /// variant carries none — policy denial is honest as-is, and resending
    /// the same prompt can never clear it.
    #[error("provider content filter blocked the generation: {detail}")]
    ContentFiltered {
        /// The provider's filter explanation.
        detail: String,
    },
    /// The prompt exceeded the model's declared context window (e.g. z.ai's
    /// `finish_reason: "model_context_window_exceeded"`). Terminal and never
    /// retryable: resending the same prompt cannot shrink it, and surfacing a
    /// distinct variant (instead of a generic 4xx) makes it a compaction-bug
    /// signal rather than an opaque provider failure.
    #[error("prompt exceeded the model's context window: {detail}")]
    ContextWindowExceeded {
        /// The provider's explanation (e.g. its finish-reason string).
        detail: String,
    },
    /// The resolved provider has no capability for the requested operation —
    /// e.g. its image client implements generation but not editing. Unlike a
    /// 4xx, no HTTP exchange happened: this is a client-side capability miss
    /// surfaced before (or instead of) a request. Terminal and never
    /// retryable — re-sending an identical request cannot conjure the
    /// capability — and distinct from [`InferenceError::ClientError`] (which
    /// carries a status the provider actually returned).
    #[error("provider does not support this operation: {detail}")]
    Unsupported {
        /// Which capability is missing and on which provider/client.
        detail: String,
    },
    /// The request was cancelled (or its daemon is shutting down) while it sat
    /// in the retry backoff, so no further attempt is made.
    #[error("request cancelled during retry backoff")]
    Cancelled,
    /// The total request deadline elapsed while streaming the response, before
    /// the stream completed.
    #[error("total request deadline exceeded while reading streaming response")]
    DeadlineExceeded,
    /// The provider returned a tool call whose arguments were truncated or
    /// otherwise not valid JSON, so the call could not be dispatched.
    #[error("tool call arguments truncated by provider: {}", .discarded.iter().map(ToString::to_string).collect::<Vec<_>>().join(", "))]
    TruncatedToolCall {
        /// The calls that were dropped, each with the partial payload the
        /// provider sent (see [`DiscardedToolCall`]).
        discarded: Vec<DiscardedToolCall>,
    },
    /// The account's OAuth grant is dead: a refresh was rejected terminally
    /// (`invalid_grant` / `invalid_token` / `refresh_token_reused`) and cannot
    /// be recovered without the user signing in again. Terminal and never
    /// retryable — distinct from [`InferenceError::Unauthorized`] (a 401 the
    /// daemon can still transparently refresh) so the UI can prompt re-login
    /// instead of showing a generic auth failure.
    #[error("re-login required: {detail}")]
    OAuthReloginRequired {
        /// A short, token-free reason (a fixed re-login message, or the
        /// account-resolution error) — never a token or a provider payload.
        detail: String,
    },
    /// An underlying I/O error (connect/socket failure, read timeout, …).
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
            InferenceError::Unsupported { .. } => "unsupported",
            InferenceError::Cancelled => "cancelled",
            InferenceError::DeadlineExceeded => "deadline_exceeded",
            InferenceError::TruncatedToolCall { .. } => "truncated_tool_call",
            InferenceError::OAuthReloginRequired { .. } => "oauth_relogin_required",
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

/// A named AI provider account as surfaced to clients: the account's own name,
/// the provider it binds to, and whether a credential is currently stored for
/// it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AccountInfo {
    /// The account's unique name (the key credential and session commands use).
    pub name: String,
    /// The provider the account targets (e.g. `anthropic`, `openai`).
    pub provider: String,
    /// Whether a credential is stored for the account in the daemon keystore.
    pub has_credential: bool,
}

#[cfg(test)]
mod tests {
    use super::TokenUsage;

    #[test]
    fn accumulate_sums_every_field_including_cache_counts() {
        let mut total = TokenUsage::default();
        total.accumulate(TokenUsage {
            input_tokens: 10,
            output_tokens: 20,
            total_tokens: 30,
            cached_tokens: 4,
            cache_write_tokens: 3,
        });
        total.accumulate(TokenUsage {
            input_tokens: 100,
            output_tokens: 50,
            total_tokens: 150,
            cached_tokens: 40,
            cache_write_tokens: 30,
        });

        // Every field is summed — the cache read/write counts are the ones a
        // partial accumulator would silently drop.
        assert_eq!(total.input_tokens, 110);
        assert_eq!(total.output_tokens, 70);
        assert_eq!(total.total_tokens, 180);
        assert_eq!(total.cached_tokens, 44);
        assert_eq!(total.cache_write_tokens, 33);
    }

    #[test]
    fn accumulate_from_default_is_a_noop() {
        let mut total = TokenUsage {
            input_tokens: 5,
            output_tokens: 6,
            total_tokens: 11,
            cached_tokens: 1,
            cache_write_tokens: 2,
        };
        let before = total;
        total.accumulate(TokenUsage::default());
        assert_eq!(total, before);
    }
}
