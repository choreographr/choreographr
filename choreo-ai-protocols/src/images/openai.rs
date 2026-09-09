//! OpenAI Images API adapter ([`OpenAiImageClient`]).
//!
//! Talks to `{base_url}/images/generations` (the same endpoint shape the
//! Chat Completions client builds from `ServiceConfig::base_url`, reusing
//! `endpoint_url` so trailing-slash handling cannot drift). v1 targets the
//! gpt-image model family: they always return `b64_json` and **reject** the
//! legacy `response_format` parameter, so that field is never sent (see the
//! body builder below for the why).

use crate::images::{ImageGenerationClient, ImageGenerationRequest, ImageGenerationResult};
use crate::openai::endpoint_url;
use crate::openai::{OpenAiError, ServiceConfig};
use crate::retry::{self, AttemptContext, RetryConfig};
use choreo_proto::InferenceError;
use serde::Deserialize;
use std::io;

/// Wall-clock deadline for a single image-generation attempt, in seconds.
///
/// Image generation is *slow by design* — tens of seconds is normal for a
/// high-quality gpt-image request — so the chat client's 120 s idle-read
/// default is too tight and the 3600 s total default is absurdly loose for a
/// single bounded POST. 180 s covers the slowest legitimate generation while
/// still guaranteeing a hung attempt cannot wedge a worker for minutes on
/// end. Applied via `build_agent`'s `timeout_global` (the only timeout that
/// fires even when the connection trickles keep-alive bytes).
const IMAGE_TOTAL_TIMEOUT_SECS: u64 = 180;

/// Images API path under the configured base URL (OpenAI: `/v1`).
const IMAGE_GENERATIONS_PATH: &str = "/images/generations";

/// Frugal retry budget for image generations: at most 2 attempts.
///
/// Unlike a chat turn, a failed generation has a user staring at a spinner
/// and the attempt itself can cost tens of seconds — one opportunistic retry
/// (transport error, or 429/503 whose Retry-After fits the budget, decided
/// by the shared `retry_decision`) is enough to ride out a blip; anything
/// beyond that should surface as an error so the caller can decide, rather
/// than silently doubling an already-long wait.
const IMAGE_MAX_ATTEMPTS: u32 = 2;

/// One `data[]` item of the Images API response.
///
/// `b64_json` is `Option` rather than required so a *missing* field is
/// detected as "no image data" (reported as [`OpenAiError::EmptyResponse`])
/// instead of a deserialization failure whose message would not say what was
/// actually wrong with the payload.
#[derive(Debug, Deserialize)]
struct ImageDataItem {
    b64_json: Option<String>,
    revised_prompt: Option<String>,
}

/// The standard Images API response envelope: `{data: […], usage?: …}`.
/// `usage` is intentionally not parsed — token accounting for image
/// generations is not surfaced anywhere yet, and ignoring it keeps the
/// adapter forward-compatible with providers that omit the field.
#[derive(Debug, Deserialize)]
struct ImagesResponse {
    data: Vec<ImageDataItem>,
}

/// Client for the OpenAI Images API (`/images/generations`).
///
/// Construction mirrors [`super::OpenAiClient::new`]: it takes the same
/// [`ServiceConfig`] shape so an account configured for chat works as-is
/// (base URL, timeouts, retry backoff, user agent are all shared), then
/// overrides the two knobs the image path must set differently (the 180 s
/// attempt deadline and the 2-attempt retry budget).
pub struct OpenAiImageClient {
    config: ServiceConfig,
    api_key: zeroize::Zeroizing<String>,
    http: ureq::Agent,
    default_image_model: String,
}

// Manual Debug impl: derived Debug would print the raw API key if a client
// is ever logged — same redaction pattern as OpenAiClient.
impl std::fmt::Debug for OpenAiImageClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAiImageClient")
            .field("config", &self.config)
            .field("api_key", &"***")
            .field("http", &self.http)
            .field("default_image_model", &self.default_image_model)
            .finish()
    }
}

impl OpenAiImageClient {
    pub fn new(config: ServiceConfig, api_key: String) -> Self {
        Self::with_default_model(config, api_key, "gpt-image-1")
    }

    /// Like [`OpenAiImageClient::new`], with an explicit fallback model for
    /// callers that do not pin one. The agent is built with the image
    /// attempt deadline (see [`IMAGE_TOTAL_TIMEOUT_SECS`]) rather than the
    /// chat config's total timeout — the deadline lives on the agent, so
    /// `config.total_timeout_secs` is deliberately overridden here and the
    /// caller's value for that one field is not honored.
    pub fn with_default_model(
        mut config: ServiceConfig,
        api_key: String,
        default_image_model: impl Into<String>,
    ) -> Self {
        let http = crate::shared::build_agent(
            config.connect_timeout_secs,
            // Idle-read timeout: a generation can be silent for a long time,
            // so the idle bound must not be tighter than the wall-clock
            // bound or it would fire first and misreport the failure mode.
            IMAGE_TOTAL_TIMEOUT_SECS,
            IMAGE_TOTAL_TIMEOUT_SECS,
            config.user_agent.as_deref(),
        );
        // Keep the config in sync with the agent so any consumer reading
        // `config()` sees the deadline actually in effect, not the chat one.
        config.total_timeout_secs = IMAGE_TOTAL_TIMEOUT_SECS;
        Self {
            config,
            api_key: zeroize::Zeroizing::new(api_key),
            http,
            default_image_model: default_image_model.into(),
        }
    }

    pub fn config(&self) -> &ServiceConfig {
        &self.config
    }

    /// Build the outgoing JSON body.
    ///
    /// `response_format` is deliberately NOT sent: gpt-image-1 rejects the
    /// parameter outright and *always* returns `b64_json`, while the legacy
    /// dall-e models would need it. v1 speaks gpt-image conventions; if a
    /// dall-e path is added later it belongs behind a model check here, not
    /// as a blanket field that breaks the primary model family.
    ///
    /// `n: 1` is pinned (see [`ImageGenerationRequest`] — the struct has no
    /// `n` field by design, so it is injected at the single serialization
    /// point here).
    fn request_body(req: &ImageGenerationRequest) -> Result<serde_json::Value, OpenAiError> {
        let mut body =
            serde_json::to_value(req).map_err(|e| OpenAiError::Io(io::Error::other(e)))?;
        let map = body
            .as_object_mut()
            // `ImageGenerationRequest` serializes to a struct, so this arm is
            // unreachable — but a corrupted custom Serialize impl must not
            // panic; it degrades to an error instead.
            .ok_or_else(|| OpenAiError::Io(io::Error::other("request body is not an object")))?;
        map.insert("n".to_string(), serde_json::Value::from(1));
        Ok(body)
    }
}

impl ImageGenerationClient for OpenAiImageClient {
    fn provider_slug(&self) -> &str {
        &self.config.provider_slug
    }

    fn default_image_model(&self) -> &str {
        &self.default_image_model
    }

    fn generate_image(
        &self,
        req: &ImageGenerationRequest,
        cancel_rx: Option<&crossbeam_channel::Receiver<()>>,
    ) -> Result<ImageGenerationResult, InferenceError> {
        let url = endpoint_url(&self.config.base_url, IMAGE_GENERATIONS_PATH)
            .map_err(OpenAiError::Io)
            .map_err(crate::shared::provider_error_to_inference)?;
        let body = Self::request_body(req).map_err(crate::shared::provider_error_to_inference)?;
        // Frugal budget: max 2 attempts (see IMAGE_MAX_ATTEMPTS), with the
        // account's backoff knobs so the Retry-After budget gate behaves
        // exactly like the chat path's.
        let retry_cfg = RetryConfig::new(
            IMAGE_MAX_ATTEMPTS,
            self.config.retry_initial_backoff_ms,
            self.config.retry_max_backoff_ms,
        );
        let auth_header = zeroize::Zeroizing::new(format!("Bearer {}", self.api_key.trim()));
        let http = &self.http;
        // No opencode gateway routing headers: image generation carries no
        // session identity today, and the gateway routes inference upstreams
        // only.
        let mut on_retry: Option<retry::RetryCallback> = None;
        let mut ctx = AttemptContext::new(&mut on_retry, cancel_rx, None);

        tracing::debug!(
            url = %url,
            model = %req.model,
            ?req.size,
            ?req.quality,
            ?req.output_format,
            ?req.background,
            max_attempts = retry_cfg.max_attempts,
            "sending image generation request"
        );

        let response = retry::retry_loop(
            || {
                http.post(&url)
                    .header("Authorization", auth_header.as_str())
                    .send_json(body.clone())
            },
            &retry_cfg,
            &mut ctx,
        )
        .map_err(OpenAiError::from)
        .map_err(crate::shared::provider_error_to_inference)?;

        let payload: ImagesResponse = response
            .into_body()
            .read_json()
            .map_err(|e| OpenAiError::Io(io::Error::other(e)))
            .map_err(crate::shared::provider_error_to_inference)?;

        // Take exactly the first image (n is pinned to 1, see
        // `request_body`); an empty or b64-less payload means the provider
        // acknowledged the request but produced nothing usable — mapped to
        // the shared EmptyResponse so it joins the existing metrics label
        // instead of a new error variant.
        let first = payload.data.into_iter().next().and_then(|item| {
            let revised_prompt = item.revised_prompt;
            item.b64_json
                .filter(|b64| !b64.is_empty())
                .map(|b64| (b64, revised_prompt))
        })
            .ok_or_else(|| {
                tracing::warn!(model = %req.model, "image generation response carried no image data");
                OpenAiError::EmptyResponse
            })
            .map_err(crate::shared::provider_error_to_inference)?;
        let (image_b64, revised_prompt) = first;

        tracing::info!(
            model = %req.model,
            image_b64_len = image_b64.len(),
            revised_prompt = revised_prompt.as_deref().unwrap_or(""),
            "image generation succeeded"
        );

        Ok(ImageGenerationResult {
            image_b64,
            revised_prompt,
            model: req.model.clone(),
        })
    }
}
