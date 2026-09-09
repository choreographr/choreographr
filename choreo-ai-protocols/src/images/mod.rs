//! Image generation clients: the [`ImageGenerationClient`] trait and the
//! request/result types it is expressed in.
//!
//! This mirrors the [`crate::ProviderClient`] split: a provider-agnostic
//! trait plus per-provider adapters (currently only the OpenAI Images API,
//! [`OpenAiImageClient`]). Errors reuse [`InferenceError`] so callers of the
//! chat trait and of this trait share one error type and one metrics-label
//! mapping — no new error taxonomy is invented for the image path.

mod openai;
#[cfg(test)]
mod tests;

pub use openai::OpenAiImageClient;

use choreo_proto::InferenceError;
use serde::{Deserialize, Serialize};

/// Output canvas size for an image generation.
///
/// Every variant defaults to `Auto` (the provider decides); the non-auto
/// values are the exact wire strings the OpenAI Images API accepts for
/// gpt-image models. Explicit `rename`s rather than `rename_all` because
/// serde's derived snake_case of a variant like `Size1024x1024` is fragile
/// around digit runs — the wire strings are pinned verbatim instead.
#[derive(
    Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema,
)]
pub enum ImageSize {
    #[serde(rename = "auto")]
    #[default]
    Auto,
    #[serde(rename = "1024x1024")]
    Square1024,
    #[serde(rename = "1024x1536")]
    Portrait1024x1536,
    #[serde(rename = "1536x1024")]
    Landscape1536x1024,
}

/// Render quality hint sent to the provider.
#[derive(
    Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum ImageQuality {
    #[default]
    Auto,
    Low,
    Medium,
    High,
}

/// Encoded file format of the returned image bytes (carried in the base64
/// payload; the daemon decodes with this MIME type).
#[derive(
    Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum OutputFormat {
    #[default]
    Png,
    Jpeg,
    Webp,
}

/// Background handling for the generated image (`transparent` only makes
/// sense with png/webp; the provider rejects it for jpeg).
#[derive(
    Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum Background {
    #[default]
    Auto,
    Opaque,
    Transparent,
}

/// A provider-agnostic image generation request.
///
/// `n` (number of images) is deliberately absent and pinned to 1 in v1: the
/// daemon's UI consumes exactly one image per generation, and multi-image
/// requests multiply latency and cost with no consumer for the extras. A
/// field can be added when a real use case appears — leaving it out keeps
/// the wire body minimal and every provider response handling single-item.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ImageGenerationRequest {
    pub prompt: String,
    pub model: String,
    pub size: ImageSize,
    pub quality: ImageQuality,
    pub output_format: OutputFormat,
    pub background: Background,
}

impl ImageGenerationRequest {
    /// Convenience constructor: every knob at its "auto" default.
    pub fn new(prompt: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            prompt: prompt.into(),
            model: model.into(),
            size: ImageSize::default(),
            quality: ImageQuality::default(),
            output_format: OutputFormat::default(),
            background: Background::default(),
        }
    }
}

/// One generated image, decoded by the caller from the base64 payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageGenerationResult {
    /// Base64-encoded image bytes in the requested [`OutputFormat`].
    pub image_b64: String,
    /// Provider's optional rewritten prompt (gpt-image models return one).
    pub revised_prompt: Option<String>,
    /// The model that actually produced the image (echoed from the request
    /// so callers never have to track it separately).
    pub model: String,
}

/// Provider-agnostic image generation, mirroring [`crate::ProviderClient`]
/// for the Images API surface.
///
/// `cancel_rx` follows the same convention as `ChatTurnRequest::cancel_rx`:
/// a crossbeam receiver checked between attempts and during retry backoff.
/// A cancel cannot interrupt the in-flight blocking HTTP write/read itself —
/// same limitation as the chat turn path — but it stops the retry loop
/// before the next attempt instead of waiting out the full budget.
pub trait ImageGenerationClient: std::fmt::Debug + Send + Sync {
    /// Catalog provider slug (e.g. `"openai"`), same convention as
    /// [`crate::ProviderClient::provider_slug`].
    fn provider_slug(&self) -> &str;

    /// The model used when the caller does not pin one (e.g. `gpt-image-1`).
    fn default_image_model(&self) -> &str;

    fn generate_image(
        &self,
        req: &ImageGenerationRequest,
        cancel_rx: Option<&crossbeam_channel::Receiver<()>>,
    ) -> Result<ImageGenerationResult, InferenceError>;
}
