//! OpenAI-compatible client (Chat Completions + Responses API).
//!
//! Owns the client itself — construction, model listing, and dispatching each
//! turn to the Chat Completions or Responses wire format from the model's
//! catalog-recorded [`RequestFormat`] — plus the `TurnParams` shape its request
//! builders share and the one private resolver that fills it in.
//! The canonical [`ChatRequestMessage`] / [`ChatToolDefinition`] types that the
//! Anthropic and Google clients translate into their own wire formats live in
//! `message`, the Responses `input` lowering lives in `responses_input`, the
//! usage / `/models` envelopes in `wire`, and the model-usage error annotation
//! in `error`; each is re-exported here so every consumer keeps naming them as
//! `openai::…`.

mod chat_completions;
mod chat_projection;
mod config;
// The model-usage error annotation (provider verdict → one user-facing line).
mod error;
mod message;
mod reasoning_details;
mod responses;
mod responses_input;
mod retry;
mod sse;
#[cfg(test)]
mod tests;
// Turn-result assembly shared by both wire formats (see the module docs).
mod turn;
// The small envelopes the client parses itself (usage counters, `/models`).
mod wire;
mod zhipu;
pub use crate::shared::{MaxTokensField, RequestAuth};
use crate::types::{ChatTurnResult, StreamEvent};

use crate::SocketRegistry;

pub(crate) use message::validate_tool_call_arguments;
pub use message::{
    AssistantToolCall, AssistantToolFunction, ChatImagePart, ChatRequestMessage,
    ChatToolDefinition, ChatToolFunction,
};
pub(crate) use responses_input::messages_to_responses_input;

pub use chat_projection::{
    CHAT_REASONING_WIRE_KEYS, ChatWireMessage, ChatWireMessages, chat_assistant_payload_is_empty,
    chat_wire_message, chat_wire_messages,
};
pub(crate) use error::annotate_model_usage_error;
// The classifier itself is only named by the tests, which pin the body prose it
// matches; production only ever reaches it through the annotation above.
pub(crate) use config::endpoint_url;
pub use config::{ServiceConfig, completion, validate_and_list_models};
#[cfg(test)]
pub(crate) use error::model_usage_rejection_reason;
pub(crate) use sse::SseReader;
#[cfg(test)]
pub(crate) use sse::build_sse_event;
#[cfg(test)]
pub(crate) use sse::extract_responses_text_delta;
pub(crate) use sse::{ResponsesStreamEvent, parse_responses_stream_event};
use wire::ModelListResponse;
pub(crate) use wire::Usage;

// Re-export common retry types for use by sub-modules and tests.
#[cfg(test)]
pub(crate) use crate::retry::{
    RetryConfig, backoff_duration, is_retryable_status, parse_retry_after_secs,
};
pub use retry::{RequestUploadedCallback, RetryCallback};

use serde::{Deserialize, Serialize};

// Re-exported so every chat-wire call site (`super::ChatReasoningEcho` from the
// submodules, the daemon from `choreo_ai_protocols::openai`) names the route
// policy through the module that applies it.
pub use crate::catalog::ChatReasoningEcho;
use std::io;

/// Re-export the shared provider error type so all `OpenAI` code continues to
/// use `super::OpenAiError` without structural changes.
pub use crate::shared::ProviderError as OpenAiError;

/// The wire format a turn is sent in.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RequestFormat {
    /// The Responses API (`/responses`).
    Responses,
    /// The Chat Completions API (`/chat/completions`).
    ChatCompletions,
}

/// The per-turn inputs the `OpenAI` tool-request builders share after the fixed
/// `agent`/`config`/`api_key`: the conversation, tool definitions, the mapped
/// reasoning effort, the retry/cancellation hooks, the gateway route, and the
/// Responses-API chaining fields. Bundled so each builder takes one params
/// value instead of a long positional list (no `too_many_arguments`
/// suppression).
pub(crate) struct TurnParams<'a> {
    pub model: &'a str,
    pub messages: &'a [ChatRequestMessage],
    pub tools: &'a [ChatToolDefinition],
    pub reasoning_effort: Option<&'a str>,
    pub on_retry: &'a mut Option<RetryCallback>,
    /// Called once the request payload is fully uploaded (see
    /// [`RequestUploadedCallback`]); `None` when the caller does not need it.
    pub on_uploaded: Option<RequestUploadedCallback>,
    pub cancel_rx: Option<&'a crossbeam_channel::Receiver<()>>,
    /// Gateway routing identity (`session_id`, `request_id`) for the opencode
    /// zen/go providers; `None` when the caller has no session.
    pub route: Option<(&'a str, &'a str)>,
    /// Responses API: the previous turn's response id to chain onto.
    pub previous_response_id: Option<&'a str>,
    /// Responses API: enable the programmatic tool-calling tool (gpt-5.6+).
    pub programmatic_tool_calling: bool,
    /// Per-call output-token cap (see `ChatTurnRequest`): `Some(n)` replaces the
    /// provider/config default for this call's output length.
    pub max_output_tokens_override: Option<u32>,
    /// Single best-effort attempt, no retries (see `ChatTurnRequest`).
    pub no_retry: bool,
}

/// Inputs for a one-shot (prompt, no tools, no session) streaming completion,
/// shared by the Responses and Chat Completions simple paths. Bundled so each
/// takes one value instead of a positional list.
#[derive(Clone, Copy)]
pub(crate) struct SimpleParams<'a> {
    pub model: &'a str,
    pub prompt: &'a str,
    pub reasoning_effort: Option<&'a str>,
    pub cancel_rx: Option<&'a crossbeam_channel::Receiver<()>>,
}

/// The kinds of caller a tool may be restricted to.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AllowedCaller {
    /// A caller that is the end user (the normal path).
    Direct,
    /// A caller that is itself a model call — programmatic tool calling.
    Programmatic,
}

/// The OpenAI-compatible client: an `OpenAI`, OpenAI-protocol gateway, or any
/// other provider served over the Chat Completions / Responses wire formats.
#[derive(Clone)]
pub struct OpenAiClient {
    config: ServiceConfig,
    /// The request auth (an API key or an OAuth access token plus its derived
    /// extra headers) consumed per request. Zeroizing is inside `RequestAuth`,
    /// so the bearer bytes are wiped when the client drops.
    auth: RequestAuth,
    http: ureq::Agent,
}

// Manual Debug impl: derived Debug would print the raw API key if a client is
// ever logged.  Redact the auth (`***`) while still delegating the other fields
// (config, http) so the struct stays useful in logs.
impl std::fmt::Debug for OpenAiClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAiClient")
            .field("config", &self.config)
            .field("auth", &"***")
            .field("http", &self.http)
            .finish()
    }
}

impl OpenAiClient {
    /// Create the client.
    ///
    /// # Errors
    ///
    /// Returns `io::Error` if the shared HTTP agent cannot be built from
    /// the registry/timeouts.
    pub fn new(
        config: ServiceConfig,
        auth: RequestAuth,
        registry: &SocketRegistry,
    ) -> io::Result<Self> {
        let http = crate::shared::build_agent(
            registry,
            config.connect_timeout_secs,
            config.request_timeout_secs,
            config.total_timeout_secs,
            config.user_agent.as_deref(),
        );
        Ok(Self { config, auth, http })
    }

    /// The client's configuration.
    #[must_use]
    pub fn config(&self) -> &ServiceConfig {
        &self.config
    }

    /// The API key when this client authenticates with one; `None` for an OAuth
    /// client. Kept as a borrowed `&str` so the zeroizing owner stays inside the
    /// client.
    #[must_use]
    pub fn api_key(&self) -> Option<&str> {
        match &self.auth {
            RequestAuth::ApiKey(key) => Some(key.as_str()),
            RequestAuth::OAuth { .. } => None,
        }
    }

    // ── High-level dispatch methods ────────────────────────────────────
    //
    // Each method inspects `self.config.request_format_for_model(model)` to
    // delegate to either Chat Completions or Responses API logic.

    /// List available models; falls back per `list_models_with_fallback` when a
    /// provider path is in play.
    ///
    /// # Errors
    ///
    /// Returns [`OpenAiError`] on HTTP failure, provider error responses, or
    /// body decoding failures.
    pub fn validate_and_list_models(&self) -> Result<Vec<String>, OpenAiError> {
        use tracing::info;
        info!("listing models from {}", self.config.base_url);
        let url = endpoint_url(&self.config.base_url, &self.config.model_list_path)?;
        let retry = retry::retry_config_from_config(&self.config);
        // Hoist the no-op retry callback into a named local: a bare `&mut None`
        // temporary would be dropped before the retry call below (E0716).
        let mut no_retry = None;
        let mut ctx = retry::AttemptContext::new(&mut no_retry, None, None);
        let response =
            retry::retry_send_get(&self.http, &url, &self.auth, &self.config, &retry, &mut ctx)?;
        let payload: ModelListResponse = response
            .into_body()
            .read_json()
            .map_err(|e| OpenAiError::Io(io::Error::other(e)))?;
        let models: Vec<String> = payload.data.into_iter().map(|model| model.id).collect();
        info!("models returned: {}", models.len());
        Ok(models)
    }

    /// Non-streaming completion in the model's configured wire format.
    ///
    /// # Errors
    ///
    /// Returns [`OpenAiError`] on HTTP, provider, retry, or decoding
    /// failures.
    pub fn completion(&self, model: &str, prompt: &str) -> Result<String, OpenAiError> {
        let result = match self.config.request_format_for_model(model) {
            RequestFormat::Responses => responses::responses_request(
                &self.http,
                &self.config,
                &self.auth,
                model,
                prompt,
                None,
            ),
            RequestFormat::ChatCompletions => chat_completions::chat_completions_request(
                &self.http,
                &self.config,
                &self.auth,
                model,
                prompt,
                None,
            ),
        };
        result.map_err(|e| annotate_model_usage_error(model, e))
    }

    /// Streaming completion in the model's configured wire format.
    ///
    /// # Errors
    ///
    /// Returns [`OpenAiError`] on HTTP, provider, cancellation, or
    /// event-callback failures.
    pub fn completion_stream<F>(
        &self,
        model: &str,
        prompt: &str,
        mut on_event: F,
    ) -> Result<(), OpenAiError>
    where
        F: FnMut(StreamEvent) -> io::Result<()>,
    {
        if !self.config.streaming {
            let content = self.completion(model, prompt)?;
            if !content.is_empty() {
                on_event(StreamEvent::Answer(content))?;
            }
            return Ok(());
        }

        let result = match self.config.request_format_for_model(model) {
            RequestFormat::Responses => responses::responses_request_streaming(
                &self.http,
                &self.config,
                &self.auth,
                SimpleParams {
                    model,
                    prompt,
                    reasoning_effort: None,
                    cancel_rx: None,
                },
                &mut on_event,
            ),
            RequestFormat::ChatCompletions => chat_completions::chat_completions_request_streaming(
                &self.http,
                &self.config,
                &self.auth,
                SimpleParams {
                    model,
                    prompt,
                    reasoning_effort: None,
                    cancel_rx: None,
                },
                &mut on_event,
            ),
        };
        result.map_err(|e| annotate_model_usage_error(model, e))
    }

    /// Non-streaming multiline chat-completion dispatch.
    ///
    /// # Errors
    ///
    /// Returns [`OpenAiError`] on HTTP, provider, cancellation, or decoding
    /// failures.
    // Passed by value: the body moves `params`' borrowed parts straight into
    // the request builders; taking a reference would ripple through every
    // caller across the daemon and tests.
    pub fn chat_completion_turn(
        &self,
        mut params: crate::ChatTurnRequest<'_>,
    ) -> Result<ChatTurnResult, OpenAiError> {
        // `params.model` is a `Copy` `&str`; capture it before `params` is
        // borrowed for the request builders so the error annotation can still
        // name the model.
        let model = params.model;
        let turn = self.turn_params(&mut params);
        let result = match self.config.request_format_for_model(model) {
            RequestFormat::Responses => {
                responses::responses_request_with_tools(&self.http, &self.config, &self.auth, turn)
            }
            RequestFormat::ChatCompletions => {
                chat_completions::chat_completions_request_with_tools(
                    &self.http,
                    &self.config,
                    &self.auth,
                    turn,
                )
            }
        };
        result.map_err(|e| annotate_model_usage_error(model, e))
    }

    /// Streaming multiline dispatch (see [`Self::chat_completion_turn`]).
    ///
    /// # Errors
    ///
    /// Returns [`OpenAiError`] on HTTP, provider, cancellation, or
    /// event-callback failures.
    pub fn chat_completion_turn_streaming<F>(
        &self,
        mut params: crate::ChatTurnRequest<'_>,
        mut on_event: F,
    ) -> Result<ChatTurnResult, OpenAiError>
    where
        F: FnMut(StreamEvent) -> io::Result<()>,
    {
        // Same `Copy` capture as `chat_completion_turn`, for the model-named
        // error annotation on the streaming dispatch below.
        let model = params.model;
        if !self.config.streaming {
            let result = self.chat_completion_turn(params)?;
            let result = crate::shared::emit_non_streaming_events(result, &mut on_event)?;
            return Ok(result);
        }

        // The non-streaming fallback above goes through
        // `chat_completion_turn`, which resolves the same params, so the
        // mapping is still applied exactly once per request.
        let turn = self.turn_params(&mut params);
        let result = match self.config.request_format_for_model(model) {
            RequestFormat::Responses => responses::responses_request_streaming_with_tools(
                &self.http,
                &self.config,
                &self.auth,
                turn,
                &mut on_event,
            ),
            RequestFormat::ChatCompletions => {
                chat_completions::chat_completions_request_streaming_with_tools(
                    &self.http,
                    &self.config,
                    &self.auth,
                    turn,
                    &mut on_event,
                )
            }
        };
        result.map_err(|e| annotate_model_usage_error(model, e))
    }

    /// Resolve one turn's request inputs, identically for the streaming and
    /// non-streaming tool paths.
    ///
    /// One place decides the wire `reasoning_effort` — z.ai's GLM models accept
    /// only a subset of the `OpenAI` effort slugs and the accepted set differs
    /// by generation, so those providers route through the Zhipu mapper while
    /// everything else passes the slug through — and one place names the
    /// gateway route. Both dispatch methods call it, so a provider-specific
    /// mapping can never be applied to one wire format and forgotten on the
    /// other.
    fn turn_params<'p, 'a: 'p>(
        &self,
        params: &'p mut crate::ChatTurnRequest<'a>,
    ) -> TurnParams<'p> {
        let reasoning_effort = if is_zhipu_provider_slug(&self.config.provider_slug) {
            zhipu_reasoning_effort_api_value(&params.thinking_effort, params.model)
        } else {
            reasoning_effort_api_value(&params.thinking_effort)
        };
        tracing::debug!(
            effort = %params.thinking_effort,
            ?reasoning_effort,
            "chat turn reasoning effort"
        );
        TurnParams {
            model: params.model,
            messages: params.messages,
            tools: params.tools,
            reasoning_effort,
            // Reborrowed rather than moved: the caller keeps ownership of the
            // callback slot for as long as it needs it.
            on_retry: &mut *params.on_retry,
            on_uploaded: params.on_uploaded.clone(),
            cancel_rx: params.cancel_rx,
            // The turn's real session/request ids drive the opencode gateway's
            // per-session sticky routing (see `shared::opencode_gateway_headers`).
            route: Some((params.session_id.as_str(), params.request_id.as_str())),
            previous_response_id: params.previous_response_id,
            programmatic_tool_calling: params.programmatic_tool_calling,
            max_output_tokens_override: params.max_output_tokens_override,
            no_retry: params.no_retry,
        }
    }
}

/// Map reasoning slug string to `OpenAI`'s `reasoning_effort` API value.
/// "off" → None (omit the field). Others → Some(slug).
pub(crate) fn reasoning_effort_api_value(slug: &str) -> Option<&str> {
    if slug == "off" { None } else { Some(slug) }
}

// Zhipu (z.ai / bigmodel.cn) provider-specific request shaping lives in the
// `zhipu` submodule; these re-exports keep the existing call sites (the two
// turn paths above) and tests importing through `super::` working unchanged.
pub(crate) use zhipu::{is_zhipu_provider_slug, zhipu_reasoning_effort_api_value};

// ── ProviderClient trait impl ───────────────────────────────────────────

use crate::{ChatTurnRequest, ProviderClient};
use choreo_proto::InferenceError;

impl ProviderClient for OpenAiClient {
    fn provider_slug(&self) -> &'static str {
        "openai"
    }

    fn cache_affinity(&self) -> crate::session_routing::CacheAffinity {
        self.config.cache_affinity
    }

    fn extended_cache_retention(&self, model: &str) -> bool {
        // Ask the SAME resolver the body builders use, so the answer can never
        // disagree with what a request actually sends: the model's dialect, the
        // curated long tier, the API the body targets (the newer spelling's
        // extended value is Responses-only), and the rejection registry.
        let is_responses = self.config.request_format_for_model(model) == RequestFormat::Responses;
        !crate::cache_retention::cache_retention_fields(
            &self.config.provider_slug,
            model,
            is_responses,
        )
        .is_empty()
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

    fn supports_programmatic_tool_calling(&self, model: &str) -> bool {
        self.config.programmatic_tool_calling_for_model(model)
    }

    fn context_window_for_model(&self, model: &str) -> Option<u32> {
        self.config
            .context_window_config
            .context_window_for_model(model)
    }
}
