use super::{MaxTokensField, OpenAiClient, RequestAuth, RequestFormat};
use crate::retry::RequestCompression;
use crate::session_routing::CacheAffinity;
use choreo_proto::ContextConfig;
use std::{collections::HashMap, io};

const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
const DEFAULT_MODEL_LIST_PATH: &str = "/models";
const DEFAULT_RESPONSES_PATH: &str = "/responses";
const DEFAULT_CHAT_COMPLETIONS_PATH: &str = "/chat/completions";

// Extra headers for the opencode.ai gateway live in
// [`crate::shared::opencode_gateway_headers`], applied per request from the
// turn's real session/request ids (see `ChatTurnRequest`).

/// Per-client configuration for an OpenAI-compatible service.
///
/// Provider-level settings (endpoints, timeouts, retry, token limits,
/// request format) live here; daemon-level settings live in
/// `choreographr`'s `config::DaemonConfig`.
#[derive(Debug, Clone)]
pub struct ServiceConfig {
    /// API root the endpoint paths are appended to.
    pub base_url: String,
    /// Path of the model-list endpoint (e.g. `/models`).
    pub model_list_path: String,
    /// Path of the Responses API endpoint.
    pub responses_path: String,
    /// Path of the Chat Completions endpoint.
    pub chat_completions_path: String,
    /// Wire format used for models the catalog has no recorded format for.
    pub default_request_format: RequestFormat,
    /// Catalog slug for this service (e.g. `"opencode"` for an OpenAI-format
    /// gateway). Owned: the catalog lookup that supplies it returns a clone,
    /// so the slug cannot be a `'static` reference anymore.
    pub provider_slug: String,
    /// Default Chat Completions output-token limit, when the model records no
    /// per-model value.
    pub chat_completions_max_tokens: Option<u32>,
    /// Per-model Chat Completions output-token limits, keyed by model id.
    pub model_max_tokens: HashMap<String, u32>,
    /// Per-model context-window resolution (catalog facts plus account
    /// overrides).
    pub context_window_config: crate::ContextWindowConfig,
    /// Default Responses API `max_output_tokens`, when the model records no
    /// per-model value.
    pub responses_max_output_tokens: Option<u32>,
    /// Per-model Responses API `max_output_tokens` limits, keyed by model id.
    pub model_responses_max_output_tokens: HashMap<String, u32>,
    /// Default Chat Completions field that carries the output-token limit.
    pub chat_completions_max_tokens_field: MaxTokensField,
    /// Per-model output-token limit field override, keyed by model id.
    pub model_max_tokens_fields: HashMap<String, MaxTokensField>,
    /// Whether to request streamed responses; `false` falls back to a
    /// non-streaming call with events emitted through the callback.
    pub streaming: bool,
    /// Whether to request the streamed usage/options trailer (`stream_options`).
    pub stream_options: bool,
    /// Optional cap on conversational turns.
    pub max_turns: Option<u32>,
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
    /// Conversational-context shaping config (system-prompt policy etc.).
    pub context: ContextConfig,
    /// Whether programmatic tool calling is enabled account-wide; also
    /// auto-enabled per-model for gpt-5.6+ Responses models (see
    /// [`Self::programmatic_tool_calling_for_model`]).
    pub programmatic_tool_calling: bool,
    /// User-Agent for inference requests. The daemon sets
    /// `choreographr/<version>`; `None` keeps ureq's default (tests).
    pub user_agent: Option<String>,
    /// Whether to compress the JSON request body before upload, and with which
    /// coding (`gzip` or `zstd`; see [`RequestCompression`]). Defaults to
    /// identity; the daemon sets it from the provider catalog's overlay policy.
    /// Only consulted for the chat-turn paths — the image-generation paths
    /// always send identity.
    pub request_compression: RequestCompression,
    /// Which per-session cache-affinity body field this provider accepts, if
    /// any (see [`CacheAffinity`]). Defaults to [`CacheAffinity::Off`]; the
    /// daemon sets it from the provider catalog's overlay policy. Consulted for
    /// the chat-turn paths only, and only when the turn carries a real session
    /// id (the session-less one-shot prompt API sends no hint). This is the raw
    /// configured value: read [`Self::configured_cache_affinity`] for the dialect
    /// a request will actually send.
    pub cache_affinity: CacheAffinity,
}

impl Default for ServiceConfig {
    fn default() -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.to_string(),
            model_list_path: DEFAULT_MODEL_LIST_PATH.to_string(),
            responses_path: DEFAULT_RESPONSES_PATH.to_string(),
            chat_completions_path: DEFAULT_CHAT_COMPLETIONS_PATH.to_string(),
            default_request_format: RequestFormat::ChatCompletions,
            provider_slug: "openai".to_string(),
            chat_completions_max_tokens: None,
            model_max_tokens: HashMap::new(),
            context_window_config: crate::ContextWindowConfig::default(),
            responses_max_output_tokens: None,
            model_responses_max_output_tokens: HashMap::new(),
            chat_completions_max_tokens_field: MaxTokensField::MaxCompletionTokens,
            model_max_tokens_fields: HashMap::new(),
            streaming: true,
            stream_options: true,
            max_turns: None,
            retry_max_attempts: 5,
            retry_initial_backoff_ms: 1000,
            retry_max_backoff_ms: 30000,
            connect_timeout_secs: 30,
            request_timeout_secs: 120,
            total_timeout_secs: 3600,
            context: ContextConfig::default(),
            programmatic_tool_calling: false,
            user_agent: None,
            request_compression: RequestCompression::Identity,
            cache_affinity: CacheAffinity::Off,
        }
    }
}

pub(crate) fn endpoint_url(base_url: &str, path: &str) -> io::Result<String> {
    if !path.starts_with('/') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "path must start with '/'",
        ));
    }
    Ok(format!("{}{}", base_url.trim_end_matches('/'), path))
}

impl ServiceConfig {
    /// Resolve the request format for a model: catalog lookup first,
    /// falling back to the configured default for unknown models.
    #[must_use]
    pub fn request_format_for_model(&self, model: &str) -> RequestFormat {
        crate::catalog::model_request_format(&self.provider_slug, model)
            .unwrap_or(self.default_request_format)
    }

    /// Whether this Responses backend is the stateless ChatGPT/Codex dialect.
    ///
    /// The Codex backend is a non-standard Responses client: it rejects a
    /// `system`-role input item outright (`{"detail":"System messages are not
    /// allowed"}`) and takes the system prompt in the top-level `instructions`
    /// field instead, does not chain via `previous_response_id`, and expects
    /// `store: false` with the full history on every request — the shape every
    /// other Codex client sends. The standard `OpenAI` Responses API chains via
    /// `previous_response_id` with `store: true` and accepts inline system
    /// items, so gating on this flag leaves those providers byte-for-byte
    /// unchanged.
    ///
    /// This is a **catalog fact** (the overlay's provider-level
    /// `responses_stateless` key), not a slug comparison here: the daemon's
    /// reasoning policy already reads the same catalog to decide whether a route
    /// chains, and one hand-kept slug list in the adapter beside it would be a
    /// second switch that could silently disagree.
    #[must_use]
    pub fn responses_stateless(&self) -> bool {
        crate::catalog::provider_responses_stateless(&self.provider_slug)
    }

    /// Whether the catalogue records the model as reasoning-capable, i.e.
    /// whether it is valid to ask the Responses API for a reasoning summary.
    ///
    /// The Responses API validates `include` **strictly**: `reasoning.summary`
    /// sent for a non-reasoning model (`gpt-4o`, `gpt-4.1`, …) is a hard 400
    /// (`Invalid value: 'reasoning.summary'`), not a silently-ignored field —
    /// so the caller must gate on this fact rather than always requesting it.
    ///
    /// `model_reasoning_capability` returns an empty effort set exactly when
    /// the catalogue knows the model does not reason; unknown models fall back
    /// to (non-empty) protocol defaults, so a brand-new model keeps the
    /// summary request and a known non-reasoning one drops it.
    #[must_use]
    pub fn model_supports_reasoning(&self, model: &str) -> bool {
        !crate::catalog::model_reasoning_capability(&self.provider_slug, model)
            .available_effort_levels
            .is_empty()
    }

    /// Clamp a requested output-token limit down to the catalog's per-model
    /// `max_output_tokens` fact (`lookup_max_output_tokens`), when one exists.
    ///
    /// Why clamp-only: the catalog fact is a *ceiling* — a request may be
    /// intentionally small (a low-turn budget, a tool-loop headroom split), so
    /// the clamp must never *raise* a lower request. `None` (unknown provider,
    /// unknown model, or the recorded `0` = unknown sentinel) leaves the
    /// request untouched: an unfactored model's own API default is a better
    /// guess than any heuristic here.
    fn clamp_output_to_catalog(&self, model: &str, requested: u32) -> u32 {
        match crate::catalog::lookup_max_output_tokens(&self.provider_slug, model) {
            Some(limit) if requested > limit => {
                tracing::info!(
                    provider = %self.provider_slug,
                    model = %model,
                    requested,
                    clamped_to = limit,
                    "clamped outgoing output-token limit to the catalog max_output_tokens fact"
                );
                limit
            }
            // No fact, or the request already fits under the ceiling: pass the
            // request through unchanged (clamp-down only, never clamp-up).
            _ => requested,
        }
    }

    /// The Responses API `max_output_tokens` for a model, clamped down to the
    /// catalog's per-model ceiling when one is recorded.
    #[must_use]
    pub fn max_output_tokens_for_model(&self, model: &str) -> Option<u32> {
        // Clamp-down against the catalog ceiling at the single resolution
        // point so every Responses-path caller inherits the clamp without
        // each body-builder having to remember it.
        self.model_responses_max_output_tokens
            .get(model)
            .copied()
            .or(self.responses_max_output_tokens)
            .map(|requested| self.clamp_output_to_catalog(model, requested))
    }

    /// The Chat Completions output-token limit for a model: the per-model value
    /// if recorded, else the configured default.
    #[must_use]
    pub fn max_tokens_for_model(&self, model: &str) -> Option<u32> {
        self.model_max_tokens
            .get(model)
            .copied()
            .or(self.chat_completions_max_tokens)
    }

    /// The context-window size for a model, when the catalog or an override
    /// records one.
    #[must_use]
    pub fn context_window_for_model(&self, model: &str) -> Option<u32> {
        self.context_window_config.context_window_for_model(model)
    }

    /// Returns whether programmatic tool calling should be enabled for a given model.
    ///
    /// Auto-enables for gpt-5.6+ models when the default is the Responses API.
    /// The account-level `programmatic_tool_calling` override takes precedence.
    #[must_use]
    pub fn programmatic_tool_calling_for_model(&self, model: &str) -> bool {
        // The stateless ChatGPT/Codex backend rejects unknown hosted tools, so
        // the `programmatic_tool_calling` capability is withheld for it
        // regardless of the account-level override (enabling it there would
        // only 400 the request).
        if self.responses_stateless() {
            return false;
        }
        if self.programmatic_tool_calling {
            return true;
        }
        // Auto-enable for gpt-5.6 models when using the Responses API
        if self.request_format_for_model(model) == RequestFormat::Responses
            && model.starts_with("gpt-5.6")
        {
            return true;
        }
        false
    }

    /// The output-token field this model uses: the per-model override if
    /// recorded, else the configured default.
    #[must_use]
    pub fn max_tokens_field_for_model(&self, model: &str) -> MaxTokensField {
        let field = self
            .model_max_tokens_fields
            .get(model)
            .copied()
            .unwrap_or(self.chat_completions_max_tokens_field);
        tracing::debug!(
            model = %model,
            ?field,
            "max_tokens_field_for_model"
        );
        field
    }

    /// Resolve the `(max_tokens, max_completion_tokens)` pair for a model.
    ///
    /// Which field to use depends on the model family (o-series uses
    /// `max_completion_tokens`, gpt-series uses `max_tokens`).
    /// Returns `(Some(n), None)` or `(None, Some(n))` depending on the
    /// resolved field.  If `max_tokens_for_model` returns `None`, both
    /// fields are `None` (the API will use its own default).
    pub(crate) fn max_tokens_field_pair(&self, model: &str) -> (Option<u32>, Option<u32>) {
        // Clamp-down against the catalog's `max_output_tokens` fact here, at
        // the single resolution point, so every chat-completions body-builder
        // (plain, turn, and both streaming variants) inherits the clamp.
        let max_tokens = self
            .max_tokens_for_model(model)
            .map(|requested| self.clamp_output_to_catalog(model, requested));
        match self.max_tokens_field_for_model(model) {
            MaxTokensField::MaxTokens => (max_tokens, None),
            MaxTokensField::MaxCompletionTokens => (None, max_tokens),
        }
    }

    /// The cache affinity this provider is *configured* to send: the [`Self::cache_affinity`]
    /// field, with the stateless-dialect withholding applied.
    ///
    /// The stateless Responses dialect is outside the verified prompt-cache
    /// surface. It documents no affinity body field, and this route resends the
    /// whole history on every request, so an unverified field would either 400
    /// the turn or be silently ignored — neither is worth the risk for what is
    /// only a routing hint on top of the provider's automatic prefix caching.
    ///
    /// Withheld here rather than in a body builder so every Responses path
    /// inherits it, including a per-account `prompt_cache_affinity = true`
    /// override: "the backend does not take this field" outranks an operator
    /// forcing it on, exactly as the hosted-tool withholding does in
    /// [`Self::programmatic_tool_calling_for_model`].
    ///
    /// This method is the provider-level resolution both readers share, so they
    /// cannot disagree: the body builders resolve through
    /// [`Self::effective_cache_affinity`] (this value, then the per-model
    /// rejection registry), and `ProviderClient::cache_affinity` reports this
    /// value directly. The trait method carries no model, so it deliberately
    /// stops here — the registry is keyed by the exact `(provider, model)` pair
    /// and only the request-time resolution can consult it.
    #[must_use]
    pub fn configured_cache_affinity(&self) -> CacheAffinity {
        if self.responses_stateless() {
            CacheAffinity::Off
        } else {
            self.cache_affinity
        }
    }

    /// The cache affinity to actually send for this provider and `model`, after
    /// applying the process-wide rejection registry (see
    /// `session_routing::effective_cache_affinity`).
    ///
    /// [`Self::configured_cache_affinity`] already folds in the per-account
    /// override, the catalog dialect, and the stateless-dialect withholding;
    /// this withholds a hint the provider has rejected with a 400 in this
    /// process for `model` (the registry is keyed by the exact provider+model
    /// pair). Provides a slug/config pairing so the body builders cannot consult
    /// the registry with the wrong slug.
    ///
    /// The stateless fact is re-derived here on every call rather than accepted
    /// as a caller-supplied flag, even where the caller has just resolved it for
    /// its own purposes. That is deliberate: the affinity is a provider-scoped
    /// resolution with a second caller on the chat path, and a caller-computed
    /// flag is exactly how the request shape a body builder emits and the
    /// affinity this reports could drift apart — one site would keep passing the
    /// old fact while the other saw a refreshed catalog. Re-deriving it keeps
    /// both views reading the same catalog fact.
    #[must_use]
    pub fn effective_cache_affinity(&self, model: &str) -> CacheAffinity {
        // Resolve the registry against the provider-level value, never the raw
        // field: an `Off` short-circuits before the registry lock is taken, so
        // the withheld stateless dialect (and any provider that never opted in)
        // pays for no lookup.
        crate::session_routing::effective_cache_affinity(
            &self.provider_slug,
            model,
            self.configured_cache_affinity(),
        )
    }

    /// Apply the per-account cache-affinity override from the protocol-agnostic
    /// [`ProviderOverrides`](crate::ProviderOverrides) carrier.
    ///
    /// This is the one carrier field the `OpenAI` adapter consumes here; the
    /// account's other overrides (base URL, streaming, retry/timeout knobs,
    /// endpoints, model limits) are applied directly from the daemon's account
    /// config before this runs. `Some(true)` forces a hint on, `Some(false)`
    /// forces it off, and `None` keeps the catalog dialect in place.
    ///
    /// Precedence, lowest priority first: the process-wide rejection registry
    /// (consulted later in [`Self::effective_cache_affinity`], which can only
    /// withhold a hint), then the catalog's `cache_affinity` overlay, then this
    /// override. Forcing on keeps a dialect the catalog already selected and
    /// lifts an `Off` catalog to the broadly-documented
    /// [`CacheAffinity::PromptCacheKey`], since "on" needs a concrete wire
    /// shape and `prompt_cache_key` is the one most providers document. A forced
    /// hint is still suppressed at request time once the provider has rejected
    /// it.
    pub fn apply_overrides(&mut self, overrides: &crate::ProviderOverrides) {
        if let Some(requested) = overrides.prompt_cache_affinity {
            self.cache_affinity = if requested {
                match self.cache_affinity {
                    CacheAffinity::Off => CacheAffinity::PromptCacheKey,
                    configured => configured,
                }
            } else {
                CacheAffinity::Off
            };
        }
    }

    /// Resolve the `(max_tokens, max_completion_tokens)` pair for a model,
    /// honouring a per-call output-token cap.
    ///
    /// When `max_output_tokens_override` is `Some(n)`, `n` replaces the
    /// configured default in the slot the model would use (chosen by
    /// [`Self::max_tokens_field_for_model`]), leaving the other slot `None`;
    /// the override is a hard per-call cap, so it is not run through the
    /// catalog clamp.  When `None`, this is exactly [`Self::max_tokens_field_pair`].
    /// The cache-warming ping passes `Some(1)` here.
    pub(crate) fn max_tokens_field_pair_with_override(
        &self,
        model: &str,
        max_output_tokens_override: Option<u32>,
    ) -> (Option<u32>, Option<u32>) {
        match max_output_tokens_override {
            Some(n) => match self.max_tokens_field_for_model(model) {
                MaxTokensField::MaxTokens => (Some(n), None),
                MaxTokensField::MaxCompletionTokens => (None, Some(n)),
            },
            None => self.max_tokens_field_pair(model),
        }
    }
}

/// List models via the provider's own base/config (used by the
/// /refresh-models path that has no client instance).
///
/// # Errors
///
/// Returns `io::Error` on HTTP failure, provider error responses, or body
/// decoding failures.
pub fn validate_and_list_models(config: &ServiceConfig, api_key: &str) -> io::Result<Vec<String>> {
    // Standalone helper (no callers in the workspace): builds its own
    // throwaway registry — callers wanting force-close coverage should use
    // `OpenAiClient::new` with a shared registry instead.
    let registry = crate::SocketRegistry::new();
    let client = OpenAiClient::new(config.clone(), RequestAuth::api_key(api_key), &registry)?;
    Ok(client.validate_and_list_models()?)
}

/// Non-streaming completion built straight from a [`ServiceConfig`].
///
/// # Errors
///
/// Returns `io::Error` on HTTP failure, provider error responses, or body
/// decoding failures.
pub fn completion(
    config: &ServiceConfig,
    api_key: &str,
    model: &str,
    prompt: &str,
) -> io::Result<String> {
    let registry = crate::SocketRegistry::new();
    let client = OpenAiClient::new(config.clone(), RequestAuth::api_key(api_key), &registry)?;
    Ok(client.completion(model, prompt)?)
}

#[cfg(test)]
mod tests;
