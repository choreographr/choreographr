//! Google Gemini API client.
//!
//! Implements the [`ProviderClient`] trait over Gemini's
//! `generateContent` / `streamGenerateContent` endpoints, including the
//! `thought: true` reasoning-part split and the opaque encrypted
//! `thoughtSignature` round-trip.

mod parts;
mod requests;
#[cfg(test)]
mod tests;

// The generateContent request body lives in `parts`; re-exporting it here is
// what keeps `requests`'s `use super::{…}` list and `tests`'s `use super::*`
// unchanged, and what lets `attach_thought_signatures` below speak in
// `PartPayload`s. Private, so nothing is re-exported out of this module.
use parts::{
    GenerateContentRequest, GenerationConfigPayload, PartPayload, build_message_payloads,
    build_tool_payloads, thinking_config_payload,
};

use std::io;
use tracing::debug;

use serde::Deserialize;

use crate::SocketRegistry;
// Imported here rather than in `parts` so `tests`'s `use super::*` keeps seeing
// them; `parts` reaches the same three names through `super::`.
use crate::openai::{ChatImagePart, ChatRequestMessage, ChatToolDefinition};
use crate::overrides::ProviderOverrides;
use crate::retry::RequestCompression;
use crate::types::{
    ChatAssistantToolUse, ChatToolCall, ChatTurnResult, FinalTextResult, StreamEvent,
};
use crate::{ChatTurnRequest, ContextWindowConfig};
use choreo_proto::{ReasoningArtifact, TokenUsage};

/// Default base URL for the Gemini API.
const DEFAULT_BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta";

/// Configuration for the Google Gemini API client.
#[derive(Debug, Clone)]
pub struct GoogleConfig {
    /// Base URL of the Gemini API (defaults to
    /// `https://generativelanguage.googleapis.com/v1beta`).
    pub base_url: String,
    /// Per-model context-window resolution (catalog facts plus account
    /// overrides).
    pub context_window_config: ContextWindowConfig,
    /// Whether to request a streamed response; when `false` the streaming turn
    /// falls back to a non-streaming call whose events are emitted through the
    /// callback.
    pub streaming: bool,
    /// Maximum number of attempts (including the first) in the retry loop.
    pub retry_max_attempts: u32,
    /// Initial retry backoff before the first retry; grows exponentially after.
    pub retry_initial_backoff_ms: u64,
    /// Ceiling on the retry backoff (clamped to the shared hard bound).
    pub retry_max_backoff_ms: u64,
    /// Bounds each individual connection attempt, in seconds.
    pub connect_timeout_secs: u64,
    /// Idle read timeout in seconds: if no bytes arrive on the response for
    /// this long, the attempt fails.  It resets on every received byte, so a
    /// steadily streaming response is never cut short however long it runs;
    /// `0` disables it.  Enforced by capping the socket read timeout at this
    /// value (ureq's `timeout_recv_body` is a whole-body total, not an idle
    /// timeout, so it is deliberately not used here).
    pub request_timeout_secs: u64,
    /// Hard wall-clock deadline for a single HTTP request attempt, including
    /// the streaming body read; 0 disables.  Unlike `request_timeout_secs`
    /// (which resets per chunk), this fires even when a provider trickles
    /// keep-alive bytes, so it bounds a stalled SSE stream.  It covers one
    /// attempt: each retry restarts the deadline, so retries plus their
    /// backoff can exceed this value in aggregate.
    pub total_timeout_secs: u64,
    /// User-Agent for inference requests. The daemon sets
    /// `choreographr/<version>`; `None` keeps ureq's default (tests).
    pub user_agent: Option<String>,
    /// Catalog provider slug, used to key the process-wide request-compression
    /// downgrade registry (see [`RequestCompression`]): a provider that rejects
    /// a compressed body with HTTP 415 is remembered under this slug so later
    /// requests skip compression. Defaults to `"google"`; the daemon sets it
    /// from the catalog entry, so a gateway provider sharing the Google
    /// protocol keeps its own downgrade state. Deliberately separate from
    /// [`GoogleClient::provider_slug`], which reports the fixed `"google"`
    /// wire protocol for metrics and reasoning-capability gating.
    pub provider_slug: String,
    /// Whether to compress the JSON request body before upload, and with which
    /// coding (`gzip` or `zstd`; see [`RequestCompression`]). Defaults to
    /// identity; the daemon sets it from the provider catalog's overlay policy.
    pub request_compression: RequestCompression,
}

impl Default for GoogleConfig {
    fn default() -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.to_string(),
            context_window_config: ContextWindowConfig::default(),
            streaming: true,
            retry_max_attempts: 5,
            retry_initial_backoff_ms: 1000,
            retry_max_backoff_ms: 30000,
            connect_timeout_secs: 30,
            request_timeout_secs: 120,
            total_timeout_secs: 3600,
            user_agent: None,
            provider_slug: "google".to_string(),
            request_compression: RequestCompression::Identity,
        }
    }
}

impl GoogleConfig {
    /// Apply provider-agnostic overrides onto this config.
    ///
    /// The daemon converts its `AccountConfig` into a [`ProviderOverrides`]
    /// carrier before calling this, so this crate never depends on daemon
    /// types. `None` fields leave the provider default in place.
    pub fn apply_overrides(&mut self, overrides: &ProviderOverrides) {
        if let Some(base_url) = &overrides.base_url {
            self.base_url.clone_from(base_url);
        }
        if let Some(streaming) = overrides.streaming {
            self.streaming = streaming;
        }
        if let Some(retry) = overrides.retry_max_attempts {
            self.retry_max_attempts = retry;
        }
        if let Some(connect) = overrides.connect_timeout_secs {
            self.connect_timeout_secs = connect;
        }
        if let Some(request) = overrides.request_timeout_secs {
            self.request_timeout_secs = request;
        }
        if let Some(total) = overrides.total_timeout_secs {
            self.total_timeout_secs = total;
        }
        if let Some(ms) = overrides.retry_initial_backoff_ms {
            self.retry_initial_backoff_ms = ms;
        }
        if let Some(ms) = overrides.retry_max_backoff_ms {
            self.retry_max_backoff_ms = ms;
        }
        self.context_window_config.apply_overrides(
            overrides.context_window,
            overrides.model_context_windows.as_ref(),
        );
    }
}

/// Errors from the Google Gemini API.
pub use crate::shared::ProviderError as GoogleError;

/// The Google Gemini API client.
#[derive(Clone)]
pub struct GoogleClient {
    config: GoogleConfig,
    api_key: zeroize::Zeroizing<String>,
    http: ureq::Agent,
}

// Manual Debug impl: derived Debug would print the raw API key if a client is
// ever logged.  Redact the key (`***`) while still delegating the other fields
// (config, http) so the struct stays useful in logs.
impl std::fmt::Debug for GoogleClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GoogleClient")
            .field("config", &self.config)
            .field("api_key", &"***")
            .field("http", &self.http)
            .finish()
    }
}

impl GoogleClient {
    /// Create the client.
    ///
    /// # Errors
    ///
    /// Returns `io::Error` if the shared HTTP agent cannot be built from the
    /// registry/timeouts.
    pub fn new(
        config: GoogleConfig,
        api_key: String,
        registry: &SocketRegistry,
    ) -> io::Result<Self> {
        let http = crate::shared::build_agent(
            registry,
            config.connect_timeout_secs,
            config.request_timeout_secs,
            config.total_timeout_secs,
            config.user_agent.as_deref(),
        );
        // Zeroizing<String> wipes the key bytes from memory when the client is
        // dropped; `new` keeps its `String` signature so callers are unaffected.
        Ok(Self {
            config,
            api_key: zeroize::Zeroizing::new(api_key),
            http,
        })
    }

    /// The client's configuration.
    #[must_use]
    pub fn config(&self) -> &GoogleConfig {
        &self.config
    }

    /// The API key, for callers that must rebuild a client or inspect it. Kept
    /// as a borrowed `&str` so the zeroizing owner stays inside the client.
    #[must_use]
    pub fn api_key(&self) -> &str {
        // `Zeroizing<String>` derefs to `String`, so `as_str()` works directly.
        self.api_key.as_str()
    }

    /// List available models from the API, falling back to the curated static list
    /// if the endpoint is unreachable or the API key lacks permission.
    ///
    /// # Errors
    ///
    /// Returns [`GoogleError`] if the HTTP request fails from a source the
    /// static fallback cannot cover, or the response cannot be decoded.
    pub fn validate_and_list_models(&self) -> Result<Vec<String>, GoogleError> {
        Ok(crate::shared::list_models_with_fallback(
            || requests::list_models_request(&self.http, &self.config, &self.api_key),
            KNOWN_GEMINI_MODELS,
            "Google",
        ))
    }

    /// Non-streaming chat completion turn via the Gemini generateContent API.
    ///
    /// # Errors
    ///
    /// Returns [`GoogleError`] on HTTP, provider, decoding, cancellation,
    /// or retry-callback failures.
    // Passed by value: the body moves `params`' borrowed parts straight into
    // `generate_content_request`; taking a reference would ripple through every
    // caller across the daemon and tests.
    pub fn chat_completion_turn(
        &self,
        params: ChatTurnRequest<'_>,
    ) -> Result<ChatTurnResult, GoogleError> {
        debug!(effort = %params.thinking_effort, "Google chat completion turn");
        requests::generate_content_request(&self.http, &self.config, &self.api_key, params)
    }

    /// Streaming chat completion turn via the Gemini streamGenerateContent API.
    ///
    /// # Errors
    ///
    /// Returns [`GoogleError`] on HTTP, provider, decoding, cancellation, or
    /// event-callback failures.
    pub fn chat_completion_turn_streaming<F>(
        &self,
        params: ChatTurnRequest<'_>,
        mut on_event: F,
    ) -> Result<ChatTurnResult, GoogleError>
    where
        F: FnMut(StreamEvent) -> io::Result<()>,
    {
        debug!(effort = %params.thinking_effort, "google chat_completion_turn_streaming");
        if !self.config.streaming {
            let result = self.chat_completion_turn(params)?;
            let result = crate::shared::emit_non_streaming_events(result, &mut on_event)?;
            return Ok(result);
        }

        requests::generate_content_request_streaming(
            &self.http,
            &self.config,
            &self.api_key,
            params,
            on_event,
        )
    }
}

// ── ProviderClient trait impl ───────────────────────────────────────────

use crate::ProviderClient;
use choreo_proto::InferenceError;

impl ProviderClient for GoogleClient {
    fn provider_slug(&self) -> &'static str {
        "google"
    }

    fn chat_completion_turn(
        &self,
        params: ChatTurnRequest<'_>,
    ) -> Result<ChatTurnResult, InferenceError> {
        self.chat_completion_turn(params)
            .map_err(crate::shared::provider_error_to_inference)
    }

    fn chat_completion_turn_streaming(
        &self,
        params: ChatTurnRequest<'_>,
        on_event: &mut dyn FnMut(StreamEvent) -> io::Result<()>,
    ) -> Result<ChatTurnResult, InferenceError> {
        self.chat_completion_turn_streaming(params, on_event)
            .map_err(crate::shared::provider_error_to_inference)
    }

    fn list_models(&self) -> Result<Vec<String>, InferenceError> {
        let result = self.validate_and_list_models();
        result.map_err(crate::shared::provider_error_to_inference)
    }

    fn context_window_for_model(&self, model: &str) -> Option<u32> {
        self.config
            .context_window_config
            .context_window_for_model(model)
    }
}

/// Response from GET /v1beta/models.
#[derive(Debug, Deserialize)]
pub(super) struct ModelListResponse {
    models: Vec<ModelInfo>,
}

#[derive(Debug, Deserialize)]
pub(super) struct ModelInfo {
    name: String,
}

/// Known Gemini models (curated static list).
const KNOWN_GEMINI_MODELS: &[&str] = &[
    "gemini-2.5-pro-exp-03-25",
    "gemini-2.5-pro",
    "gemini-2.5-flash-preview-05-06",
    "gemini-2.5-flash",
    "gemini-2.0-flash",
    "gemini-2.0-flash-lite",
    "gemini-2.0-flash-exp",
    "gemini-2.0-flash-thinking-exp-01-21",
    "gemini-2.0-flash-thinking-exp",
    "gemini-1.5-pro",
    "gemini-1.5-flash",
    "gemini-1.5-flash-8b",
    "gemini-1.5-flash-8b-exp-0827",
    "gemma-3-27b-it",
    "gemma-3-12b-it",
    "gemma-3-4b-it",
    "gemma-3-1b-it",
    "gemma-2-27b-it",
    "gemma-2-9b-it",
    "gemma-2-2b-it",
    "embedding-001",
    "text-embedding-004",
];

// ── API types ──────────────────────────────────────────────────────────

/// Response body from POST /v1beta/models/{model}:generateContent.
#[derive(Debug, Deserialize)]
struct GenerateContentResponse {
    #[serde(default)]
    candidates: Vec<Candidate>,
    #[serde(default)]
    usage_metadata: Option<UsageMetadata>,
}

#[derive(Debug, Deserialize)]
pub(super) struct UsageMetadata {
    #[serde(default, rename = "promptTokenCount")]
    prompt_token_count: u32,
    #[serde(default, rename = "candidatesTokenCount")]
    candidates_token_count: u32,
    #[serde(default, rename = "totalTokenCount")]
    total_token_count: u32,
    /// Prompt tokens served from the provider's context cache
    /// (`cachedContentTokenCount`). A subset of [`Self::prompt_token_count`] —
    /// Gemini reports the full effective prompt size there even when
    /// `cachedContent` is in play — so it is never added to the input count.
    /// Absent on responses that used no cached content.
    #[serde(default, rename = "cachedContentTokenCount")]
    cached_content_token_count: u32,
}

impl UsageMetadata {
    /// Project the provider usage onto the shared [`TokenUsage`] counter.
    ///
    /// The ONE projection both the non-streaming conversion and the streaming
    /// loop use, so a newly modelled cache field cannot reach one path and
    /// miss the other. Gemini reports no cache-write count (the context cache
    /// is created out of band, not billed per turn), so `cache_write_tokens`
    /// stays zero.
    fn to_token_usage(&self) -> TokenUsage {
        TokenUsage {
            input_tokens: self.prompt_token_count,
            output_tokens: self.candidates_token_count,
            total_tokens: self.total_token_count,
            cached_tokens: self.cached_content_token_count,
            cache_write_tokens: 0,
        }
    }
}

#[derive(Debug, Deserialize)]
#[expect(dead_code)]
struct Candidate {
    content: Option<ContentBlock>,
    #[serde(default)]
    finish_reason: Option<String>,
    #[serde(default)]
    index: i64,
}

#[derive(Debug, Deserialize)]
#[expect(dead_code)]
struct ContentBlock {
    parts: Vec<ResponsePart>,
    #[serde(default)]
    role: Option<String>,
}

/// One `part` entry inside a Gemini `content` block, per the real wire format.
///
/// The Gemini API has NO `thinking` key. A thinking step is a regular `text`
/// part flagged with `thought: true`; the optional `thoughtSignature` is an
/// encrypted representation of the internal thought process and can appear on
/// ANY part type (text, functionCall, …). `Text` and `FunctionCall` are
/// disjoint in the untagged union — one carries `text`, the other
/// `functionCall` — so serde picks the right variant unambiguously.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ResponsePart {
    Text {
        text: String,
        /// `thought: true` marks a thinking part (its text is reasoning, not
        /// the assistant answer). Absent/false on ordinary text parts.
        #[serde(default)]
        thought: Option<bool>,
        #[serde(default, rename = "thoughtSignature")]
        thought_signature: Option<String>,
    },
    FunctionCall {
        #[serde(rename = "functionCall")]
        function_call: FunctionCallResponse,
        #[serde(default, rename = "thoughtSignature")]
        thought_signature: Option<String>,
    },
}

/// A response part routed to its consumer. Shared by the non-streaming and
/// streaming consumption paths so a thinking part can never leak into the
/// assistant answer and signatures are captured in both paths.
#[derive(Debug)]
enum ClassifiedPart {
    /// An ordinary text part (`thought` absent or false): display text.
    Answer(String),
    /// A thinking part (`thought: true`): its text is reasoning (display-only),
    /// and the optional encrypted signature is captured for round-trip.
    Reasoning {
        text: String,
        signature: Option<String>,
    },
    /// A function call, optionally carrying a thought signature.
    ToolCall {
        name: String,
        args: serde_json::Value,
        signature: Option<String>,
    },
}

impl ResponsePart {
    /// Classify this part for consumption. This is the single source of truth
    /// for the `thought: true` routing decision.
    fn classify(self) -> ClassifiedPart {
        match self {
            ResponsePart::Text {
                text,
                thought,
                thought_signature,
            } => {
                if thought == Some(true) {
                    ClassifiedPart::Reasoning {
                        text,
                        signature: thought_signature,
                    }
                } else {
                    ClassifiedPart::Answer(text)
                }
            }
            ResponsePart::FunctionCall {
                function_call,
                thought_signature,
            } => ClassifiedPart::ToolCall {
                name: function_call.name,
                args: function_call.args,
                signature: thought_signature,
            },
        }
    }
}

/// Record one `thoughtSignature` (if any) for the round-trip artifact.
///
/// Signatures are provider-owned opaque blobs; we capture them verbatim in
/// wire order so a later passback can echo them back (Phase 4a). Log at
/// capture time for observability.
fn capture_signature(signatures: &mut Vec<String>, signature: Option<String>) {
    if let Some(sig) = signature {
        debug!(sig_len = sig.len(), "captured google thought signature");
        signatures.push(sig);
    }
}

/// Serialize the captured thought signatures into the opaque
/// [`ReasoningArtifact::GoogleSignatures`] payload, or `None` when nothing was
/// captured. The payload is the JSON serialization of the signature array as
/// received (order preserved), matching the encrypted blobs the provider
/// expects back verbatim.
fn google_signatures_artifact(
    signatures: &[String],
) -> Result<Option<ReasoningArtifact>, GoogleError> {
    if signatures.is_empty() {
        return Ok(None);
    }
    let bytes = serde_json::to_vec(signatures).map_err(|e| GoogleError::Io(io::Error::other(e)))?;
    debug!(
        signature_count = signatures.len(),
        payload_bytes = bytes.len(),
        "captured google thought signatures artifact",
    );
    Ok(Some(ReasoningArtifact::GoogleSignatures(bytes)))
}

#[derive(Debug, Deserialize)]
struct FunctionCallResponse {
    name: String,
    #[serde(default)]
    args: serde_json::Value,
}

/// Gemini API error response body.
#[derive(Debug, Deserialize)]
pub(crate) struct GeminiErrorBody {
    #[serde(default)]
    pub(crate) error: Option<GeminiErrorDetail>,
}

#[derive(Debug, Deserialize)]
#[expect(dead_code)]
pub(crate) struct GeminiErrorDetail {
    #[serde(default)]
    pub(crate) code: i64,
    #[serde(default)]
    pub(crate) message: String,
    #[serde(default)]
    pub(crate) status: String,
}

/// Gemini uses the REST pattern `models/{model}:{action}` rather than a path-segment
/// approach like `OpenAI`.  We append the action as a colon-delimited suffix to keep
/// the endpoint self-describing for different capabilities (generateContent vs
/// streamGenerateContent).
pub(crate) fn model_url(base_url: &str, model: &str, action: &str) -> String {
    let base = base_url.trim_end_matches('/');
    format!("{base}/models/{model}:{action}")
}

/// Attach the captured thought signatures from the round-trip artifact to the
/// assistant parts, per Gemini's `thoughtSignature` contract.
///
/// The artifact stores signatures in wire order but NOT the part each came
/// from, and this adapter does not re-emit thinking text (display-only), so
/// the wire cannot attach them to their original thinking parts. Gemini
/// allows `thoughtSignature` on ANY part, so the
/// turn's FINAL signature is attached to the LAST part — in a tool loop that
/// is the functionCall part.
/// Attaching the final signature keeps the most recent thinking step's
/// continuity token on the wire.
fn attach_thought_signatures(
    parts: &mut [PartPayload<'_>],
    artifact: Option<&ReasoningArtifact>,
) -> Result<(), GoogleError> {
    let Some(ReasoningArtifact::GoogleSignatures(bytes)) = artifact else {
        return Ok(());
    };
    let signatures: Vec<String> =
        serde_json::from_slice(bytes).map_err(|e| GoogleError::Io(io::Error::other(e)))?;
    let Some(last_signature) = signatures.last() else {
        return Ok(());
    };
    let Some(last) = parts.last_mut() else {
        // No parts to carry the signature (e.g. an assistant message with
        // nothing but a signature) — nothing to attach to.
        return Ok(());
    };
    debug!(
        signature_count = signatures.len(),
        "re-emitting google thought signatures from artifact"
    );
    match last {
        PartPayload::Text {
            thought_signature, ..
        }
        | PartPayload::FunctionCall {
            thought_signature, ..
        } => *thought_signature = Some(last_signature.clone()),
        // Tool results are user-role parts and never appear in assistant
        // content, so this arm is unreachable in practice. Inline data
        // (vision) also never appears in assistant content.
        PartPayload::FunctionResponse { .. } | PartPayload::InlineData { .. } => {}
    }
    Ok(())
}

/// Mint the call id for one `functionCall` part.
///
/// Gemini sends no call id at all: a `functionResponse` is matched back to its
/// `functionCall` by **name**, so the wire never carries a token the adapter
/// could reuse. An id is still required, because the daemon keys its whole
/// tool-result bookkeeping by it — `seed_tool_results`/`update_tool_result`
/// fill a placeholder by `call_id`, `executed_tool_calls` is a set of ids, and
/// the concurrent dispatch batch tracks delivered ids in a set — so two results
/// sharing an id collapse onto one transcript record and one of them is lost.
///
/// The name alone is not unique: Gemini's parallel function calling allows
/// several `functionCall` parts in one candidate, and they may well name the
/// same function (two `get_temp` calls for two cities). The id therefore
/// carries `position`, the call's 0-based index **among that reply's tool
/// calls** — not its part index, so a text part before the first call does not
/// shift the numbering.
///
/// The position is deterministic for a given reply, so a replayed turn mints
/// the same ids and a recorded result still matches its call. The shape is kept
/// to `[A-Za-z0-9_-]` because a session may later switch to a provider that
/// sends ids on the wire and rejects one outside that alphabet.
pub(super) fn function_call_id(name: &str, position: usize) -> String {
    // Gemini function names permit `.` and `:`, which are outside the id
    // alphabet some providers accept, so any such character folds to `_`.
    // Position already distinguishes same-reply calls, so folding cannot merge
    // two ids that position kept apart.
    let sanitized: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("fc_{sanitized}_{position}")
}

/// Convert a non-streaming Gemini response into a `ChatTurnResult`.
fn response_to_turn_result(
    response: GenerateContentResponse,
) -> Result<ChatTurnResult, GoogleError> {
    let usage: Option<TokenUsage> = response.usage_metadata.map(|u| u.to_token_usage());
    let candidate = response
        .candidates
        .into_iter()
        .next()
        .ok_or(GoogleError::EmptyResponse)?;

    let content = candidate.content.ok_or(GoogleError::EmptyResponse)?;

    let mut text_parts: Vec<String> = Vec::new();
    let mut tool_calls: Vec<ChatToolCall> = Vec::new();
    let mut reasoning_parts: Vec<String> = Vec::new();
    // Encrypted thought signatures in wire order, captured for the opaque
    // round-trip artifact (see `google_signatures_artifact`).
    let mut signatures: Vec<String> = Vec::new();

    for part in content.parts {
        match part.classify() {
            ClassifiedPart::Answer(text) => {
                // Ordinary text part: display text only.
                text_parts.push(text);
            }
            ClassifiedPart::Reasoning { text, signature } => {
                // `thought: true` part: the text is reasoning, never the
                // assistant answer; its signature (if any) is captured.
                capture_signature(&mut signatures, signature);
                reasoning_parts.push(text);
            }
            ClassifiedPart::ToolCall {
                name,
                args,
                signature,
            } => {
                capture_signature(&mut signatures, signature);
                // Number the call by its index among this reply's tool calls,
                // so parallel calls to one tool stay distinct while a text
                // part earlier in the parts list cannot shift the numbering.
                let id = function_call_id(&name, tool_calls.len());
                let args_json = args.to_string();
                tool_calls.push(ChatToolCall {
                    id,
                    name,
                    arguments_json: args_json,
                    caller: None,
                });
            }
        }
    }

    let reasoning_artifact = google_signatures_artifact(&signatures)?;

    let reasoning = if reasoning_parts.is_empty() {
        None
    } else {
        Some(reasoning_parts.join("\n"))
    };

    if !tool_calls.is_empty() {
        let content = if text_parts.is_empty() {
            None
        } else {
            Some(text_parts.join(""))
        };
        return Ok(ChatTurnResult::ToolUse(ChatAssistantToolUse {
            content,
            tool_calls,
            reasoning,
            usage,
            response_id: None,
            reasoning_artifact,
        }));
    }

    let content = text_parts.join("");
    if content.is_empty() {
        return Err(GoogleError::EmptyResponse);
    }

    Ok(ChatTurnResult::FinalText(FinalTextResult {
        content,
        truncated: false,
        discarded_tool_calls: Vec::new(),
        reasoning,
        usage,
        response_id: None,
        reasoning_artifact,
    }))
}
