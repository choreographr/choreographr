//! Image generation clients: the [`ImageGenerationClient`] trait and the
//! request/result types it is expressed in.
//!
//! This mirrors the [`crate::ProviderClient`] split: a provider-agnostic
//! trait plus per-provider adapters (the OpenAI Images API,
//! [`OpenAiImageClient`], and the z.ai / Zhipu GLM Images API,
//! [`ZaiImageClient`]). Errors reuse [`InferenceError`] so callers of the
//! chat trait and of this trait share one error type and one metrics-label
//! mapping — no new error taxonomy is invented for the image path.

mod openai;
mod zai;

// ── Shared adapter policy constants ───────────────────────────────────────

/// Wall-clock deadline for a single image-generation attempt, in seconds.
///
/// Image generation is *slow by design* — tens of seconds is normal for a
/// high-quality gpt-image request (and glm-image's `hd` quality renders in
/// ~20 s) — so the chat client's 120 s idle-read default is too tight and
/// the 3600 s total default is absurdly loose for a single bounded POST.
/// 180 s covers the slowest legitimate generation while still guaranteeing a
/// hung attempt cannot wedge a worker for minutes on end. Applied via
/// `build_agent`'s `timeout_global` (the only timeout that fires even when
/// the connection trickles keep-alive bytes), and — because the agent is
/// shared with the URL-download path of URL-returning adapters — it also
/// bounds that post-response fetch.
pub(crate) const IMAGE_TOTAL_TIMEOUT_SECS: u64 = 180;

/// Frugal retry budget for image generations: at most 2 attempts.
///
/// Unlike a chat turn, a failed generation has a user staring at a spinner
/// and the attempt itself can cost tens of seconds — one opportunistic retry
/// (transport error, or 429/503 whose Retry-After fits the budget, decided
/// by the shared `retry_decision`) is enough to ride out a blip; anything
/// beyond that should surface as an error so the caller can decide, rather
/// than silently doubling an already-long wait.
pub(crate) const IMAGE_MAX_ATTEMPTS: u32 = 2;

/// Cap on downloaded image bytes for adapters whose provider returns a
/// temporary URL instead of inline bytes (z.ai: the URL is a CDN link that
/// expires after 30 days). 8 MiB mirrors the daemon's
/// `MAX_DISPLAY_IMAGE_BYTES` — anything larger would be rejected by the
/// prepare pipeline right after decoding, so downloading past the cap only
/// allocates bytes that will be thrown away. Enforced during the streaming
/// read so a hostile multi-gigabyte response cannot balloon memory before
/// the cap fires.
pub(crate) const IMAGE_DOWNLOAD_CAP_BYTES: usize = 8 * 1024 * 1024;

pub use openai::OpenAiImageClient;
pub use zai::ZaiImageClient;

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
///
/// Serialization is deliberately *minimal*: a knob left at its default
/// (`auto`/`png`) is omitted from the wire body entirely rather than sent as
/// an explicit default value. The gpt-image family accepts explicit defaults,
/// but image models reached through OpenAI-compatible proxies (imagen, flux,
/// gemini-image — see the tool's priority pick) often reject parameters they
/// do not implement, so a bare `{model, prompt, n}` body is the maximally
/// compatible request and the knobs opt in only when the caller sets them.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ImageGenerationRequest {
    pub prompt: String,
    pub model: String,
    #[serde(skip_serializing_if = "ImageSize::is_default")]
    pub size: ImageSize,
    #[serde(skip_serializing_if = "ImageQuality::is_default")]
    pub quality: ImageQuality,
    #[serde(skip_serializing_if = "OutputFormat::is_default")]
    pub output_format: OutputFormat,
    #[serde(skip_serializing_if = "Background::is_default")]
    pub background: Background,
}

// `skip_serializing_if` needs path-callable predicates; `PartialEq` derives
// give the comparison, these name it per field type. `is_default` stays
// private — it is a serialization detail, not public API.
impl ImageSize {
    fn is_default(v: &Self) -> bool {
        *v == Self::default()
    }
}
impl ImageQuality {
    fn is_default(v: &Self) -> bool {
        *v == Self::default()
    }
}
impl OutputFormat {
    fn is_default(v: &Self) -> bool {
        *v == Self::default()
    }
}
impl Background {
    fn is_default(v: &Self) -> bool {
        *v == Self::default()
    }
}

// `Display` mirrors the serde wire strings exactly (the single source of
// truth for what goes on the wire), so user-facing renderings — e.g. the
// tool's `describe_invocation` line — show the same values the API receives
// instead of Rust variant names.
impl std::fmt::Display for ImageSize {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Auto => "auto",
            Self::Square1024 => "1024x1024",
            Self::Portrait1024x1536 => "1024x1536",
            Self::Landscape1536x1024 => "1536x1024",
        })
    }
}
impl std::fmt::Display for ImageQuality {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Auto => "auto",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        })
    }
}
impl std::fmt::Display for OutputFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Png => "png",
            Self::Jpeg => "jpeg",
            Self::Webp => "webp",
        })
    }
}
impl std::fmt::Display for Background {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Auto => "auto",
            Self::Opaque => "opaque",
            Self::Transparent => "transparent",
        })
    }
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

    fn generate_image(
        &self,
        req: &ImageGenerationRequest,
        cancel_rx: Option<&crossbeam_channel::Receiver<()>>,
    ) -> Result<ImageGenerationResult, InferenceError>;
}
