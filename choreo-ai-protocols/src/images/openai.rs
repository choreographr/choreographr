//! `OpenAI` Images API adapter ([`OpenAiImageClient`]).
//!
//! Talks to `{base_url}/images/generations` (the same endpoint shape the
//! Chat Completions client builds from `ServiceConfig::base_url`, reusing
//! `endpoint_url` so trailing-slash handling cannot drift). v1 targets the
//! gpt-image model family: they always return `b64_json` and **reject** the
//! legacy `response_format` parameter, so that field is never sent (see the
//! body builder below for the why).

use crate::SocketRegistry;
use crate::images::{
    ImageEditRequest, ImageGenerationClient, ImageGenerationRequest, ImageGenerationResult,
    ImageQuality, ImageSize, OutputFormat,
};
use crate::openai::endpoint_url;
use crate::openai::{OpenAiError, ServiceConfig};
use crate::retry::{self, AttemptContext, RetryConfig};
use choreo_proto::InferenceError;
use serde::Deserialize;
use std::io;
use std::io::Read as _;
use ureq::unversioned::multipart::{Form, Part};

// The wall-clock deadline and retry budget live in [`super`] as shared
// adapter policy constants (see super::IMAGE_TOTAL_TIMEOUT_SECS and
// super::IMAGE_MAX_ATTEMPTS) — identical across every image adapter and
// reused for the URL-download path too.

/// Images API generation path under the configured base URL (`OpenAI`: `/v1`).
const IMAGE_GENERATIONS_PATH: &str = "/images/generations";

/// Images API editing path under the configured base URL (multipart edit).
const IMAGE_EDITS_PATH: &str = "/images/edits";

const IMAGE_MAX_ATTEMPTS: u32 = super::IMAGE_MAX_ATTEMPTS;
const IMAGE_TOTAL_TIMEOUT_SECS: u64 = super::IMAGE_TOTAL_TIMEOUT_SECS;

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

/// The outgoing JSON body: the request `flatten`ed with the pinned
/// `n: 1` (see [`ImageGenerationRequest`] — the struct has no `n` field by
/// design, so it is injected at the single serialization point here).
/// `flatten` keeps `skip_serializing_if` on the default knobs working: an
/// all-defaults request serializes to just `{model, prompt, n}`.
///
/// `response_format` is deliberately NOT sent anywhere: gpt-image-1 rejects
/// the parameter outright and *always* returns `b64_json`, while the legacy
/// dall-e models would need it. v1 speaks gpt-image conventions; if a
/// dall-e path is added later it belongs behind a model check, not as a
/// blanket field that breaks the primary model family.
///
/// `seed` is likewise never serialized here: the shared request struct marks
/// the field `skip_serializing`, so a `#[serde(flatten)]` of the request
/// cannot leak it into this body — the gpt-image family documents no `seed`
/// parameter, and an undocumented field risks a 400. Only the fal adapter
/// emits `seed`, and it inserts it explicitly by hand; pinned by the unit
/// test below so a future change cannot silently re-leak it.
#[derive(serde::Serialize)]
struct WireBody<'a> {
    #[serde(flatten)]
    req: &'a ImageGenerationRequest,
    n: u32,
}

/// Extract the first image's `(b64_json, revised_prompt)` from a decoded
/// Images response, or [`OpenAiError::EmptyResponse`] when the payload
/// carried no usable image. Shared by generation and editing: both pin the
/// output to one image (generation via `n: 1`, editing because the tool
/// consumes exactly one), so "first and only" is the operative rule for
/// both.
fn first_image(
    payload: ImagesResponse,
    model: &str,
) -> Result<(String, Option<String>), OpenAiError> {
    payload
        .data
        .into_iter()
        .next()
        .and_then(|item| {
            let revised_prompt = item.revised_prompt;
            item.b64_json
                .filter(|b64| !b64.is_empty())
                .map(|b64| (b64, revised_prompt))
        })
        .ok_or_else(|| {
            tracing::warn!(model = %model, "image response carried no image data");
            OpenAiError::EmptyResponse
        })
}

/// Serialize an [`ImageEditRequest`] into a `multipart/form-data` body plus
/// its `Content-Type` (the boundary is ureq's random per-`Form` value).
///
/// Rendered eagerly to bytes rather than streaming the live `Form` through
/// the retry loop: a `Form` is consumed by `send`, and rebuilding one per
/// attempt would re-hash the (potentially multi-megabyte) image parts each
/// time. Rendering once keeps the body reusable across attempts exactly like
/// the JSON path's [`retry::HookedJsonBody`], so the shared retry/backoff/
/// cancellation policy applies unchanged.
///
/// Field policy mirrors the generation body: `prompt` and `model` always;
/// one `image` part (filename + MIME + bytes) per input; each knob sent ONLY
/// when it is non-default, so a minimal edit is `{prompt, model, image}`.
/// `response_format` is never sent (gpt-image rejects it and always returns
/// `b64_json`, same as generation). When the request carries a `mask`, it is
/// added as a `mask` part (filename + MIME + bytes) — the daemon normalizes it
/// to PNG so the transparent region survives as the inpaint target.
fn edit_multipart(req: &ImageEditRequest) -> io::Result<(Vec<u8>, String)> {
    let mut form = Form::new()
        .text("prompt", req.prompt.as_str())
        .text("model", req.model.as_str());
    for image in &req.images {
        let part = Part::bytes(&image.data)
            .file_name(&image.filename)
            // The daemon normalizes inputs to image/png or image/jpeg, so the
            // MIME always parses; a malformed one surfaces as an I/O error
            // rather than a panic.
            .mime_str(&image.mime_type)
            .map_err(io::Error::other)?;
        form = form.part("image", part);
    }
    // The optional inpainting mask rides as its own `mask` part, carrying the
    // same filename + MIME + bytes shape as an `image` part (the daemon
    // normalizes it to PNG, never JPEG, so alpha is preserved).
    if let Some(mask) = &req.mask {
        let part = Part::bytes(&mask.data)
            .file_name(&mask.filename)
            .mime_str(&mask.mime_type)
            .map_err(io::Error::other)?;
        form = form.part("mask", part);
    }
    // Knobs are rendered to owned strings first (the `Form` borrows them) and
    // only non-default values are attached — the minimal-body policy.
    let size = (req.size != ImageSize::default()).then(|| req.size.to_string());
    let quality = (req.quality != ImageQuality::default()).then(|| req.quality.to_string());
    let output_format =
        (req.output_format != OutputFormat::default()).then(|| req.output_format.to_string());
    let background =
        (req.background != super::Background::default()).then(|| req.background.to_string());
    if let Some(value) = size.as_deref() {
        form = form.text("size", value);
    }
    if let Some(value) = quality.as_deref() {
        form = form.text("quality", value);
    }
    if let Some(value) = output_format.as_deref() {
        form = form.text("output_format", value);
    }
    if let Some(value) = background.as_deref() {
        form = form.text("background", value);
    }
    let content_type = format!("multipart/form-data; boundary={}", form.boundary());
    let mut body = Vec::new();
    form.read_to_end(&mut body)?;
    Ok((body, content_type))
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
        let url = endpoint_url(&self.config.base_url, IMAGE_GENERATIONS_PATH)
            .map_err(OpenAiError::Io)
            .map_err(crate::shared::provider_error_to_inference)?;
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

        // Serialize once to compact JSON with an explicit `Content-Length`
        // (see `retry::HookedJsonBody`); every attempt reuses the shared bytes.
        // No upload hook — image generation has no `Waiting` boundary.
        let hooked = retry::HookedJsonBody::new(
            &WireBody { req, n: 1 },
            None,
            retry::RequestCompression::Identity,
        )
        .map_err(InferenceError::Io)?;
        let response = retry::retry_loop(
            || {
                hooked
                    .apply_headers(
                        http.post(&url)
                            .header("Authorization", auth_header.as_str()),
                    )
                    .send(hooked.send_body())
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
        let (image_b64, revised_prompt) =
            first_image(payload, &req.model).map_err(crate::shared::provider_error_to_inference)?;

        tracing::info!(
            model = %req.model,
            image_b64_len = image_b64.len(),
            // Length only: the revised prompt is derived from the user's prompt.
            revised_prompt_len = revised_prompt.as_deref().map_or(0, str::len),
            "image generation succeeded"
        );

        Ok(ImageGenerationResult {
            image_b64,
            revised_prompt,
            model: req.model.clone(),
        })
    }

    fn edit_image(
        &self,
        req: &ImageEditRequest,
        cancel_rx: Option<&crossbeam_channel::Receiver<()>>,
    ) -> Result<ImageGenerationResult, InferenceError> {
        let url = endpoint_url(&self.config.base_url, IMAGE_EDITS_PATH)
            .map_err(OpenAiError::Io)
            .map_err(crate::shared::provider_error_to_inference)?;
        // Same frugal budget as generation (see IMAGE_MAX_ATTEMPTS).
        let retry_cfg = RetryConfig::new(
            IMAGE_MAX_ATTEMPTS,
            self.config.retry_initial_backoff_ms,
            self.config.retry_max_backoff_ms,
        );
        let auth_header = zeroize::Zeroizing::new(format!("Bearer {}", self.api_key.trim()));
        let http = &self.http;
        // Render the multipart body once; the shared retry loop re-sends the
        // same bytes on each attempt (see `edit_multipart` for why the body is
        // materialized rather than streamed as a live `Form`).
        let (body, content_type) = edit_multipart(req)
            .map_err(OpenAiError::Io)
            .map_err(crate::shared::provider_error_to_inference)?;
        let mut on_retry: Option<retry::RetryCallback> = None;
        let mut ctx = AttemptContext::new(&mut on_retry, cancel_rx, None);

        tracing::debug!(
            url = %url,
            model = %req.model,
            image_count = req.images.len(),
            total_input_bytes = req.images.iter().map(|i| i.data.len()).sum::<usize>(),
            ?req.size,
            ?req.quality,
            ?req.output_format,
            ?req.background,
            max_attempts = retry_cfg.max_attempts,
            "sending image edit request"
        );

        let response = retry::retry_loop(
            || {
                http.post(&url)
                    .header("Authorization", auth_header.as_str())
                    .header("Content-Type", content_type.as_str())
                    .send(body.as_slice())
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

        let (image_b64, revised_prompt) =
            first_image(payload, &req.model).map_err(crate::shared::provider_error_to_inference)?;

        tracing::info!(
            model = %req.model,
            image_b64_len = image_b64.len(),
            revised_prompt_len = revised_prompt.as_deref().map_or(0, str::len),
            "image edit succeeded"
        );

        Ok(ImageGenerationResult {
            image_b64,
            revised_prompt,
            model: req.model.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{WireBody, edit_multipart};
    use crate::images::{
        Background, ImageEditRequest, ImageGenerationRequest, ImageInput, ImageQuality, ImageSize,
        OutputFormat,
    };

    #[test]
    fn wire_body_never_carries_seed_even_when_the_request_sets_one() {
        // The flattened request must NOT leak `seed` to OpenAI-protocol
        // providers: the gpt-image family documents no seed parameter and an
        // undocumented field risks a 400. The field is `skip_serializing` on
        // the shared struct precisely so this body can never carry it —
        // pinned here so a future change cannot silently re-leak it.
        let mut req = ImageGenerationRequest::new("a lighthouse", "gpt-image-1");
        req.seed = Some(77);
        let value = serde_json::to_value(WireBody { req: &req, n: 1 }).expect("serializes");
        assert!(value.get("seed").is_none(), "{value}");
        // The minimal all-knobs-default shape is still {model, prompt, n}.
        assert_eq!(
            value.get("model").and_then(serde_json::Value::as_str),
            Some("gpt-image-1")
        );
        assert_eq!(
            value.get("prompt").and_then(serde_json::Value::as_str),
            Some("a lighthouse")
        );
        assert_eq!(value.get("n").and_then(serde_json::Value::as_u64), Some(1));
    }

    #[test]
    fn edit_multipart_uses_image_fields_and_omits_default_knobs() {
        // Two input images: one `image` part each, carrying filename + MIME +
        // bytes; prompt and model always present; every default knob absent
        // (the minimal-body policy), and response_format never sent.
        let req = ImageEditRequest {
            images: vec![
                ImageInput {
                    filename: "a.png".to_string(),
                    mime_type: "image/png".to_string(),
                    data: vec![1, 2, 3],
                },
                ImageInput {
                    filename: "b.jpg".to_string(),
                    mime_type: "image/jpeg".to_string(),
                    data: vec![4, 5],
                },
            ],
            ..ImageEditRequest::new("add a hat", "gpt-image-1", Vec::new())
        };
        let (body, content_type) = edit_multipart(&req).expect("multipart renders");
        let text = String::from_utf8(body).expect("utf-8 body");
        assert!(
            content_type.starts_with("multipart/form-data; boundary="),
            "{content_type}"
        );
        assert!(text.contains("name=\"prompt\""), "{text}");
        assert!(text.contains("add a hat"));
        assert!(text.contains("name=\"model\""));
        assert!(text.contains("gpt-image-1"));
        // One part per input image, each with the uploaded filename + MIME.
        assert_eq!(text.matches("name=\"image\"").count(), 2);
        assert!(text.contains("filename=\"a.png\""));
        assert!(text.contains("filename=\"b.jpg\""));
        assert!(text.contains("Content-Type: image/png"));
        assert!(text.contains("Content-Type: image/jpeg"));
        // Default knobs omitted entirely; response_format never sent; a
        // request with no mask emits no `mask` part.
        for omitted in ["size", "quality", "output_format", "background"] {
            assert!(!text.contains(&format!("name=\"{omitted}\"")), "{text}");
        }
        assert!(!text.contains("response_format"));
        assert!(!text.contains("name=\"mask\""), "{text}");

        // Non-default knobs ARE included, verbatim wire strings; a present
        // mask adds a single `mask` part with its filename + MIME.
        let req = ImageEditRequest {
            size: ImageSize::Square1024,
            quality: ImageQuality::High,
            output_format: OutputFormat::Webp,
            background: Background::Transparent,
            mask: Some(ImageInput {
                filename: "mask.png".to_string(),
                mime_type: "image/png".to_string(),
                data: vec![7, 8],
            }),
            ..ImageEditRequest::new(
                "p",
                "gpt-image-1",
                vec![ImageInput {
                    filename: "x.png".to_string(),
                    mime_type: "image/png".to_string(),
                    data: vec![0],
                }],
            )
        };
        let (body, _) = edit_multipart(&req).expect("multipart renders");
        let text = String::from_utf8(body).expect("utf-8 body");
        assert!(text.contains("name=\"size\"") && text.contains("1024x1024"));
        assert!(text.contains("name=\"quality\"") && text.contains("high"));
        assert!(text.contains("name=\"output_format\"") && text.contains("webp"));
        assert!(text.contains("name=\"background\"") && text.contains("transparent"));
        // Exactly one `mask` part, carrying the mask's own filename + MIME.
        assert_eq!(text.matches("name=\"mask\"").count(), 1, "{text}");
        assert!(text.contains("filename=\"mask.png\""), "{text}");
    }
}
