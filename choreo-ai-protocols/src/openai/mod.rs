//! OpenAI-compatible client (Chat Completions + Responses API).
//!
//! Owns the client itself — construction, model listing, and dispatching each
//! turn to the Chat Completions or Responses wire format from the model's
//! catalog-recorded [`RequestFormat`] — plus the `Usage` / `TurnParams` shapes
//! its request builders share. The canonical [`ChatRequestMessage`] /
//! [`ChatToolDefinition`] types that the Anthropic and Google clients translate
//! into their own wire formats live in `message`, and the Responses `input`
//! lowering lives in `responses_input`; both are re-exported here so every
//! consumer keeps naming them as `openai::…`.

mod chat_completions;
mod chat_projection;
mod config;
mod message;
mod reasoning_details;
mod responses;
mod responses_input;
mod retry;
mod sse;
#[cfg(test)]
mod tests;
mod zhipu;
pub use crate::shared::{MaxTokensField, RequestAuth};
use crate::types::{ChatTurnResult, StreamEvent};
use tracing::info;

use crate::SocketRegistry;
use crate::ToolResultItem;

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
pub(crate) use config::endpoint_url;
pub use config::{ServiceConfig, completion, validate_and_list_models};
pub(crate) use sse::SseReader;
#[cfg(test)]
pub(crate) use sse::build_sse_event;
#[cfg(test)]
pub(crate) use sse::extract_responses_text_delta;
pub(crate) use sse::{ResponsesStreamEvent, parse_responses_stream_event};

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
    /// Responses API: tool results fed back from the previous turn.
    pub tool_results: &'a [ToolResultItem],
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

#[derive(Debug, Deserialize)]
struct ModelListResponse {
    data: Vec<ModelInfo>,
}

#[derive(Debug, Deserialize)]
struct ModelInfo {
    id: String,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Usage {
    prompt_tokens: u32,
    completion_tokens: u32,
    total_tokens: u32,
    /// OpenAI-style per-prompt breakdown. z.ai (and `OpenAI` itself) report
    /// `prompt_tokens_details.cached_tokens` here; providers that omit the
    /// object entirely (e.g. plain `DeepSeek`) still parse via `#[serde(default)]`.
    /// NOTE: `DeepSeek` also exposes a differently-shaped flat
    /// `prompt_cache_hit_tokens` field — parsing that is a possible follow-up.
    #[serde(default)]
    prompt_tokens_details: Option<PromptTokensDetails>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct PromptTokensDetails {
    #[serde(default)]
    cached_tokens: u32,
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
        params: crate::ChatTurnRequest<'_>,
    ) -> Result<ChatTurnResult, OpenAiError> {
        // `params.model` is a `Copy` `&str`; capture it before `params` is
        // moved into the request builders below so the error annotation can
        // still name the model.
        let model = params.model;
        // z.ai models only accept a subset of the OpenAI effort slugs, and
        // the accepted set differs by GLM generation — dispatch to the
        // Zhipu-specific mapper for those providers, plain pass-through for
        // everything else.
        let reasoning_effort = if is_zhipu_provider_slug(&self.config.provider_slug) {
            zhipu_reasoning_effort_api_value(&params.thinking_effort, params.model)
        } else {
            reasoning_effort_api_value(&params.thinking_effort)
        };
        tracing::debug!(effort = %params.thinking_effort, ?reasoning_effort, "chat_completion_turn");
        // The turn's real session/request ids drive the opencode gateway's
        // per-session sticky routing (see `shared::opencode_gateway_headers`).
        let route = Some((params.session_id.as_str(), params.request_id.as_str()));
        let turn = TurnParams {
            model,
            messages: params.messages,
            tools: params.tools,
            reasoning_effort,
            on_retry: params.on_retry,
            on_uploaded: params.on_uploaded,
            cancel_rx: params.cancel_rx,
            route,
            previous_response_id: params.previous_response_id,
            tool_results: params.tool_results,
            programmatic_tool_calling: params.programmatic_tool_calling,
            max_output_tokens_override: params.max_output_tokens_override,
            no_retry: params.no_retry,
        };
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
        params: crate::ChatTurnRequest<'_>,
        mut on_event: F,
    ) -> Result<ChatTurnResult, OpenAiError>
    where
        F: FnMut(StreamEvent) -> io::Result<()>,
    {
        // Same `Copy` capture as `chat_completion_turn`, for the model-named
        // error annotation on the streaming dispatch below.
        let model = params.model;
        // Same Zhipu dispatch as `chat_completion_turn` — both turn paths
        // must apply the identical mapping so streaming and non-streaming
        // requests carry the same wire value.
        let reasoning_effort = if is_zhipu_provider_slug(&self.config.provider_slug) {
            zhipu_reasoning_effort_api_value(&params.thinking_effort, params.model)
        } else {
            reasoning_effort_api_value(&params.thinking_effort)
        };
        tracing::debug!(
            effort = %params.thinking_effort,
            ?reasoning_effort,
            "chat_completion_turn_streaming"
        );
        if !self.config.streaming {
            let result = self.chat_completion_turn(params)?;
            let result = crate::shared::emit_non_streaming_events(result, &mut on_event)?;
            return Ok(result);
        }

        // The turn's real session/request ids drive the opencode gateway's
        // per-session sticky routing (see `shared::opencode_gateway_headers`).
        let route = Some((params.session_id.as_str(), params.request_id.as_str()));
        let turn = TurnParams {
            model,
            messages: params.messages,
            tools: params.tools,
            reasoning_effort,
            on_retry: params.on_retry,
            on_uploaded: params.on_uploaded,
            cancel_rx: params.cancel_rx,
            route,
            previous_response_id: params.previous_response_id,
            tool_results: params.tool_results,
            programmatic_tool_calling: params.programmatic_tool_calling,
            max_output_tokens_override: params.max_output_tokens_override,
            no_retry: params.no_retry,
        };
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
}

/// Map reasoning slug string to `OpenAI`'s `reasoning_effort` API value.
/// "off" → None (omit the field). Others → Some(slug).
pub(crate) fn reasoning_effort_api_value(slug: &str) -> Option<&str> {
    if slug == "off" { None } else { Some(slug) }
}

/// Rewrite a provider rejection that means "this model can't be used here"
/// into a short, plain message, e.g. `'gpt-4o' is not a chat model — please
/// try a different one.`
///
/// The live model picker deliberately lists the provider's *full* catalogue —
/// including models choreographr cannot drive (legacy completions models,
/// embeddings, audio, image, moderation). A just-released or custom model must
/// stay selectable too, so the list is never filtered; instead, when a turn is
/// sent with an unusable model, the provider's verdict (e.g. the `OpenAI`
/// `404 This is not a chat model …`) becomes a short user-facing line. The
/// provider is the only reliable authority on usability (the `/v1/models`
/// payload carries no modality), so this reacts to its verdict rather than
/// guessing locally. The provider's raw wording is jargon to a user, so it is
/// dropped from the message and kept in the log instead.
fn annotate_model_usage_error(model: &str, err: OpenAiError) -> OpenAiError {
    let OpenAiError::ClientError { status, detail } = &err else {
        return err;
    };
    // The helper lowercases once and decides both *whether* this is a
    // model-usability rejection and *how* to phrase it; a `None` here means
    // the body is unrelated and must pass through verbatim.
    let Some(reason) = model_usage_rejection_reason(detail) else {
        return err;
    };
    // Keep the provider's technical wording for operators — the user only sees
    // the plain line below.
    info!(model = %model, provider_detail = %detail, "provider rejected the model as unusable");
    OpenAiError::ClientError {
        status: *status,
        detail: format!("'{model}' {reason} — please try a different one."),
    }
}

/// Classify a provider error body: `Some(reason)` when it means the selected
/// model cannot be used on the endpoint, with the short phrase to name the
/// cause; `None` when it is unrelated (and must pass through verbatim). The
/// body is free-form prose, so this is a deliberately narrow substring match
/// over the known OpenAI-family rejections.
///
/// `crate::retry::extract_error_message` (in `retry.rs`) unwraps the standard
/// JSON error envelope and returns only `error.message`, DROPPING the `code`
/// field. On that parsed-envelope path the human-readable
/// `"does not exist or you do not have access"` prose is therefore the primary
/// model-not-found signal — the literal `"model_not_found"` code never
/// survives extraction. That literal only reaches here when the body was NOT a
/// parseable envelope, in which case the verbatim body is kept.
///
/// Note: `"not supported in the v1/chat/completions"` / `"not supported in the
/// v1/responses"` are deliberately NOT matched — `OpenAI` uses the same wording
/// for genuine "Unsupported parameter" errors, so matching them would mask real
/// parameter bugs behind the generic line. The intended legacy-completions case
/// is already covered by `"not a chat model"`.
fn model_usage_rejection_reason(detail: &str) -> Option<&'static str> {
    // One lowercase pass for both the classification and the phrasing.
    let d = detail.to_ascii_lowercase();
    if d.contains("not a chat model") {
        Some("is not a chat model")
    } else if d.contains("model_not_found")
        || d.contains("does not exist or you do not have access")
    {
        Some("can't be used here")
    } else {
        None
    }
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
