//! `OpenAI` Images API adapter ([`OpenAiImageClient`]).
//!
//! Talks to `{base_url}/images/generations` (the same endpoint shape the
//! Chat Completions client builds from `ServiceConfig::base_url`, reusing
//! `endpoint_url` so trailing-slash handling cannot drift). v1 targets the
//! gpt-image model family: they always return `b64_json` and **reject** the
//! legacy `response_format` parameter, so that field is never sent (see the
//! body builder below for the why).
//!
//! Editing has TWO body shapes on the same `/images/edits` path. The default
//! is the gpt-image contract: `multipart/form-data` with an `image` part per
//! input, an optional `mask`, and whichever knobs are non-default. xAI
//! ([`is_xai_image_provider_slug`]) is the exception — it speaks the same
//! `OpenAI` *generation* JSON for images, but its edit endpoint is
//! `Content-Type: application/json` with an `images` field and rejects the
//! multipart form outright, so [`OpenAiImageClient::edit_image`] branches to
//! the JSON body for that slug.
//!
//! The wire bodies and the response decoder live in three concern submodules:
//! the `generate` module owns the generation body and its request loop, the
//! `edit` module owns the multipart and xAI-JSON edit bodies and their request
//! loops, and the `response` module owns the Images response envelope and the
//! shared image extractor.

use crate::SocketRegistry;
use crate::images::{
    ImageEditRequest, ImageGenerationClient, ImageGenerationRequest, ImageGenerationResult,
    is_xai_image_provider_slug,
};
use crate::openai::ServiceConfig;
use choreo_proto::InferenceError;

mod edit;
mod generate;
mod response;

// The wall-clock deadline and retry budget live in [`super`] as shared
// adapter policy constants (see super::IMAGE_TOTAL_TIMEOUT_SECS and
// super::IMAGE_MAX_ATTEMPTS) — identical across every image adapter and
// reused for the URL-download path too.
const IMAGE_MAX_ATTEMPTS: u32 = super::IMAGE_MAX_ATTEMPTS;
const IMAGE_TOTAL_TIMEOUT_SECS: u64 = super::IMAGE_TOTAL_TIMEOUT_SECS;

/// Client for the `OpenAI` Images API (`/images/generations`).
///
/// Construction mirrors [`crate::OpenAiClient::new`]: it takes the same
/// [`ServiceConfig`] shape so an account configured for chat works as-is
/// (base URL, timeouts, retry backoff, user agent are all shared), then
/// overrides the two knobs the image path must set differently (the 180 s
/// attempt deadline and the 2-attempt retry budget).
pub struct OpenAiImageClient {
    config: ServiceConfig,
    api_key: zeroize::Zeroizing<String>,
    http: ureq::Agent,
}

// Manual Debug impl: derived Debug would print the raw API key if a client
// is ever logged — same redaction pattern as OpenAiClient.
impl std::fmt::Debug for OpenAiImageClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAiImageClient")
            .field("config", &self.config)
            .field("api_key", &"***")
            .field("http", &self.http)
            .finish()
    }
}

impl OpenAiImageClient {
    /// The agent is built with the image attempt deadline (see
    /// [`super::IMAGE_TOTAL_TIMEOUT_SECS`]) rather than the chat config's total
    /// timeout — the deadline lives on the agent, so
    /// `config.total_timeout_secs` is deliberately overridden here and the
    /// caller's value for that one field is not honored.
    #[must_use]
    pub fn new(mut config: ServiceConfig, api_key: String, registry: &SocketRegistry) -> Self {
        let http = crate::shared::build_agent(
            registry,
            config.connect_timeout_secs,
            // Idle-read timeout: an image generation is a single silent
            // response (there is no streamed progress to reset the idle bound),
            // so the idle bound is set equal to the wall-clock bound — the
            // total is what actually ends a hung attempt.
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
        }
    }

    /// The client's configuration, with `total_timeout_secs` synced to the
    /// image attempt deadline the agent actually uses.
    #[must_use]
    pub fn config(&self) -> &ServiceConfig {
        &self.config
    }
}

impl ImageGenerationClient for OpenAiImageClient {
    fn provider_slug(&self) -> &str {
        &self.config.provider_slug
    }

    fn generate_image(
        &self,
        req: &ImageGenerationRequest,
        cancel_rx: Option<&crossbeam_channel::Receiver<()>>,
    ) -> Result<ImageGenerationResult, InferenceError> {
        self.generate(req, cancel_rx)
    }

    fn edit_image(
        &self,
        req: &ImageEditRequest,
        cancel_rx: Option<&crossbeam_channel::Receiver<()>>,
    ) -> Result<ImageGenerationResult, InferenceError> {
        // xAI rejects the `multipart/form-data` form its OpenAI-protocol
        // sibling uses and takes a JSON body on the same `/images/edits` path
        // instead; route there before building the multipart parts.
        if is_xai_image_provider_slug(self.provider_slug()) {
            return self.edit_image_json(req, cancel_rx);
        }
        self.edit_image_multipart(req, cancel_rx)
    }
}
