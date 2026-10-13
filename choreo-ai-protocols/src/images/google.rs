//! Google Gemini image adapter ([`GoogleImageClient`]).
//!
//! Gemini image models do **not** expose a dedicated Images endpoint: they
//! generate and edit through the ordinary `generateContent` API, returning the
//! rendered pixels as an `inlineData` part of a normal model response. This
//! adapter therefore speaks the same `POST {base}/models/{model}:generateContent`
//! contract as [`crate::GoogleClient`] (it reuses [`crate::google::model_url`]),
//! differing only in the request body it builds and the response part it reads:
//!
//! - **Generation** sends `contents: [{role:"user", parts:[{text: prompt}]}]`
//!   and reads the first `inlineData` part back.
//! - **Editing** sends the same shape but appends one `inlineData` part per
//!   input image (base64 + MIME) after the prompt, so the model edits the
//!   supplied pixels.
//!
//! There is no mask, no `n`, no size/quality/format knob — Gemini's image
//! models take their canvas from the prompt and the input images, so the
//! request-level knobs are silently ignored (documented per-provider best
//! effort, like the z.ai adapter's ignored `background`).

use crate::SocketRegistry;
use crate::google::{GoogleConfig, model_url};
use crate::images::{
    ImageEditRequest, ImageGenerationClient, ImageGenerationRequest, ImageGenerationResult,
};
use crate::retry::{self, AttemptContext, RetryConfig};
use crate::shared::ProviderError;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use choreo_proto::InferenceError;
use serde::Deserialize;
use std::io;

/// Endpoint action for a non-streaming content generation (see
/// [`crate::google::model_url`]). Image models are always called
/// non-streaming: a single image is one bounded part, not a token stream.
const GENERATE_CONTENT: &str = "generateContent";

const IMAGE_MAX_ATTEMPTS: u32 = super::IMAGE_MAX_ATTEMPTS;
const IMAGE_TOTAL_TIMEOUT_SECS: u64 = super::IMAGE_TOTAL_TIMEOUT_SECS;

/// Client for the Gemini `generateContent` image path (generate + edit).
///
/// Construction mirrors [`crate::GoogleClient::new`]: it takes the same
/// [`GoogleConfig`] shape so an account configured for chat works as-is (base
/// URL, timeouts, retry backoff, user agent, slug), then overrides the two
/// knobs the image path must set differently — the 180 s attempt deadline and
/// the 2-attempt retry budget (see the shared constants in [`super`]).
pub struct GoogleImageClient {
    config: GoogleConfig,
    api_key: zeroize::Zeroizing<String>,
    http: ureq::Agent,
}

// Manual Debug impl: derived Debug would print the raw API key if a client is
// ever logged — same redaction pattern as GoogleClient.
impl std::fmt::Debug for GoogleImageClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GoogleImageClient")
            .field("config", &self.config)
            .field("api_key", &"***")
            .field("http", &self.http)
            .finish()
    }
}

impl GoogleImageClient {
    /// The agent is built with the image attempt deadline (see
    /// [`super::IMAGE_TOTAL_TIMEOUT_SECS`]) rather than the chat config's total
    /// timeout — same override pattern as [`super::OpenAiImageClient::new`].
    #[must_use]
    pub fn new(mut config: GoogleConfig, api_key: String, registry: &SocketRegistry) -> Self {
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
    pub fn config(&self) -> &GoogleConfig {
        &self.config
    }

    /// POST a `generateContent` request and return the first image part.
    ///
    /// Shared by generation and editing: only `body` differs (a text-only
    /// `contents` for generation, a text + per-image `inlineData` `contents` for
    /// editing). The retry/deadline machinery is the same shared [`retry`] path
    /// every other adapter uses, with the frugal 2-attempt budget and the
    /// account's backoff knobs. Uses `identity` request-body compression: the
    /// image path never opts into the chat path's compression because a decoded
    /// response body is what is wanted, and a compressed request here buys
    /// nothing.
    fn post_generate_content<T: serde::Serialize>(
        &self,
        model: &str,
        body: &T,
        cancel_rx: Option<&crossbeam_channel::Receiver<()>>,
    ) -> Result<ImageGenerationResult, InferenceError> {
        let url = model_url(&self.config.base_url, model, GENERATE_CONTENT);
        let retry_cfg = RetryConfig::new(
            IMAGE_MAX_ATTEMPTS,
            self.config.retry_initial_backoff_ms,
            self.config.retry_max_backoff_ms,
        );
        // The key already lives in wipe-on-drop storage (see
        // `GoogleClient::api_key`), so borrowing a trimmed `&str` here adds no
        // heap copy; it is only ever handed to ureq's request object.
        let api_key = self.api_key.trim();
        let http = &self.http;
        let mut on_retry: Option<retry::RetryCallback> = None;
        let mut ctx = AttemptContext::new(&mut on_retry, cancel_rx, None);

        tracing::debug!(
            url = %url,
            model = %model,
            max_attempts = retry_cfg.max_attempts,
            "sending Google image request"
        );

        // Compact JSON plus an explicit `Content-Length` (see
        // `retry::HookedJsonBody`); no upload hook — image generation has no
        // `Waiting` boundary.
        let hooked = retry::HookedJsonBody::new(body, None, retry::RequestCompression::Identity)
            .map_err(InferenceError::Io)?;
        let response = retry::retry_loop(
            || {
                hooked
                    .apply_headers(http.post(&url).header("x-goog-api-key", api_key))
                    .send(hooked.send_body())
            },
            &retry_cfg,
            &mut ctx,
        )
        .map_err(ProviderError::from)
        .map_err(crate::shared::provider_error_to_inference)?;

        let payload: GenerateContentResponseBody = response
            .into_body()
            .read_json()
            .map_err(|e| ProviderError::Io(io::Error::other(e)))
            .map_err(crate::shared::provider_error_to_inference)?;

        let (image_b64, mime_type) = extract_first_image(payload).ok_or_else(|| {
            tracing::warn!(
                model = %model,
                "Google image response carried no inlineData image part"
            );
            crate::shared::provider_error_to_inference(ProviderError::EmptyResponse)
        })?;

        tracing::info!(
            model = %model,
            image_b64_len = image_b64.len(),
            // The returned MIME (e.g. image/png) is metadata, not payload.
            mime = mime_type.as_deref().unwrap_or("unknown"),
            "Google image generation succeeded"
        );

        Ok(ImageGenerationResult {
            image_b64,
            // Gemini image models return no rewritten prompt.
            revised_prompt: None,
            model: model.to_string(),
        })
    }
}

/// Outgoing `generateContent` body: `{contents: [{role, parts}]}`. Only
/// `contents` is sent — Gemini accepts the request with just this field.
#[derive(Debug, serde::Serialize)]
struct GenerateContentBody<'a> {
    contents: Vec<ContentPayload<'a>>,
}

#[derive(Debug, serde::Serialize)]
struct ContentPayload<'a> {
    role: &'a str,
    parts: Vec<PartPayload<'a>>,
}

/// One `part` of a request `content` block. Gemini's wire format is camelCase
/// (`inlineData`, `mimeType`); the API also tolerates `snake_case`, but the
/// documented spelling is pinned here so the body matches the reference.
#[derive(Debug, serde::Serialize)]
#[serde(untagged)]
enum PartPayload<'a> {
    /// A text part (the edit/generation prompt).
    Text { text: &'a str },
    /// An image part: `{"inlineData": {"mimeType": …, "data": <base64>}}`.
    InlineData {
        #[serde(rename = "inlineData")]
        inline_data: InlineDataPayload<'a>,
    },
}

#[derive(Debug, serde::Serialize)]
struct InlineDataPayload<'a> {
    #[serde(rename = "mimeType")]
    mime_type: &'a str,
    data: String,
}

/// The generation body: a single user content of the text prompt.
fn generate_body(req: &ImageGenerationRequest) -> GenerateContentBody<'_> {
    GenerateContentBody {
        contents: vec![ContentPayload {
            role: "user",
            parts: vec![PartPayload::Text { text: &req.prompt }],
        }],
    }
}

/// The editing body: a single user content with the prompt followed by one
/// `inlineData` part per input image (base64-encoded). Gemini's `generateContent`
/// editing takes no mask and none of the size/quality/format knobs, so the
/// caller's `mask` and knob values are dropped.
fn edit_body(req: &ImageEditRequest) -> GenerateContentBody<'_> {
    let mut parts = Vec::with_capacity(req.images.len() + 1);
    parts.push(PartPayload::Text { text: &req.prompt });
    for image in &req.images {
        parts.push(PartPayload::InlineData {
            inline_data: InlineDataPayload {
                mime_type: &image.mime_type,
                // Base64 of the raw input bytes; the daemon has already
                // normalized these to image/png or image/jpeg.
                data: BASE64.encode(&image.data),
            },
        });
    }
    GenerateContentBody {
        contents: vec![ContentPayload {
            role: "user",
            parts,
        }],
    }
}

/// Response body from `generateContent`. Only the `candidates` the image
/// extraction needs are modeled; everything else (usage, finish reason, …) is
/// ignored by serde.
#[derive(Debug, Deserialize)]
struct GenerateContentResponseBody {
    #[serde(default)]
    candidates: Vec<Candidate>,
}

#[derive(Debug, Deserialize)]
struct Candidate {
    content: Option<ContentBlock>,
}

#[derive(Debug, Deserialize)]
struct ContentBlock {
    #[serde(default)]
    parts: Vec<ResponsePart>,
}

/// One response `part`, keeping only the `inlineData` field the image path
/// cares about. A text or `functionCall` part simply carries no `inlineData`
/// and is skipped.
#[derive(Debug, Deserialize)]
struct ResponsePart {
    #[serde(default, rename = "inlineData")]
    inline_data: Option<InlineDataBlock>,
}

#[derive(Debug, Deserialize)]
struct InlineDataBlock {
    #[serde(default, rename = "mimeType")]
    mime_type: Option<String>,
    #[serde(default)]
    data: Option<String>,
}

/// Extract the first non-empty `inlineData` image from the response, as
/// `(base64, mimeType)`. A candidate with no content, or a part with no image,
/// is skipped; an empty `data` counts as absent.
fn extract_first_image(payload: GenerateContentResponseBody) -> Option<(String, Option<String>)> {
    payload
        .candidates
        .into_iter()
        .filter_map(|candidate| candidate.content)
        .flat_map(|content| content.parts)
        .filter_map(|part| part.inline_data)
        .find_map(|inline| {
            let data = inline.data.filter(|d| !d.is_empty())?;
            Some((data, inline.mime_type))
        })
}

impl ImageGenerationClient for GoogleImageClient {
    fn provider_slug(&self) -> &str {
        &self.config.provider_slug
    }

    fn generate_image(
        &self,
        req: &ImageGenerationRequest,
        cancel_rx: Option<&crossbeam_channel::Receiver<()>>,
    ) -> Result<ImageGenerationResult, InferenceError> {
        let body = generate_body(req);
        tracing::debug!(
            model = %req.model,
            // Length only: the prompt is user content.
            prompt_len = req.prompt.len(),
            "sending Google image generation request"
        );
        self.post_generate_content(&req.model, &body, cancel_rx)
    }

    fn edit_image(
        &self,
        req: &ImageEditRequest,
        cancel_rx: Option<&crossbeam_channel::Receiver<()>>,
    ) -> Result<ImageGenerationResult, InferenceError> {
        let body = edit_body(req);
        tracing::debug!(
            model = %req.model,
            prompt_len = req.prompt.len(),
            image_count = req.images.len(),
            total_input_bytes = req.images.iter().map(|i| i.data.len()).sum::<usize>(),
            "sending Google image edit request"
        );
        self.post_generate_content(&req.model, &body, cancel_rx)
    }
}

#[cfg(test)]
mod tests {
    use super::{GenerateContentResponseBody, edit_body, extract_first_image, generate_body};
    use crate::google::model_url;
    use crate::images::{ImageEditRequest, ImageGenerationRequest, ImageInput};
    use base64::Engine as _;

    #[test]
    fn generate_body_is_a_single_user_text_part() {
        // The generation body is exactly contents:[{role:user, parts:[{text}]}];
        // no generationConfig, no knobs — Gemini takes the canvas from the
        // prompt.
        let req = ImageGenerationRequest::new("a lighthouse at dusk", "gemini-2.5-flash-image");
        let value = serde_json::to_value(generate_body(&req)).expect("serializes");
        assert_eq!(value["contents"][0]["role"], "user");
        assert_eq!(
            value["contents"][0]["parts"][0]["text"],
            "a lighthouse at dusk"
        );
        assert_eq!(
            value["contents"][0]["parts"].as_array().map(Vec::len),
            Some(1)
        );
        assert!(value.get("model").is_none(), "{value}");
    }

    #[test]
    fn edit_body_carries_prompt_then_one_inline_data_part_per_image() {
        // Editing appends one camelCase `inlineData` part per input image after
        // the prompt; the base64 payload is that input's bytes and the MIME
        // rides as `mimeType`. A mask and every knob on the request are dropped.
        let req = ImageEditRequest {
            mask: Some(ImageInput {
                filename: "mask.png".to_string(),
                mime_type: "image/png".to_string(),
                data: vec![9, 9],
            }),
            ..ImageEditRequest::new(
                "add a hat",
                "gemini-3-pro-image",
                vec![
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
            )
        };
        let value = serde_json::to_value(edit_body(&req)).expect("serializes");
        let parts = value["contents"][0]["parts"]
            .as_array()
            .expect("parts array");
        assert_eq!(parts.len(), 3, "{value}");
        assert_eq!(parts[0]["text"], "add a hat");
        assert_eq!(parts[1]["inlineData"]["mimeType"], "image/png", "{value}");
        assert_eq!(
            parts[1]["inlineData"]["data"],
            base64::engine::general_purpose::STANDARD.encode([1u8, 2, 3]),
            "{value}"
        );
        assert_eq!(parts[2]["inlineData"]["mimeType"], "image/jpeg");
        assert_eq!(
            parts[2]["inlineData"]["data"],
            base64::engine::general_purpose::STANDARD.encode([4u8, 5]),
            "{value}"
        );
        // The mask is never sent (Gemini's generateContent editing takes none).
        assert!(!value.to_string().contains("mask"), "{value}");
    }

    #[test]
    fn extract_first_image_finds_the_first_non_empty_inline_data() {
        // A leading text part is skipped; the first non-empty inlineData wins,
        // and its mimeType is carried alongside the base64 data.
        let payload: GenerateContentResponseBody = serde_json::from_value(serde_json::json!({
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [
                        { "text": "here is your image" },
                        { "inlineData": { "mimeType": "image/png", "data": "YWJj" } },
                        { "inlineData": { "mimeType": "image/png", "data": "ZGVm" } }
                    ]
                }
            }]
        }))
        .expect("parses");
        let (data, mime) = extract_first_image(payload).expect("an image part");
        assert_eq!(data, "YWJj");
        assert_eq!(mime.as_deref(), Some("image/png"));
    }

    #[test]
    fn extract_first_image_skips_empty_data_and_missing_parts() {
        // A candidates array with no image (text only) yields None; a present-
        // but-empty data field counts as absent.
        let text_only: GenerateContentResponseBody = serde_json::from_value(serde_json::json!({
            "candidates": [{ "content": { "parts": [{ "text": "no image here" }] } }]
        }))
        .expect("parses");
        assert!(extract_first_image(text_only).is_none());

        let empty_data: GenerateContentResponseBody = serde_json::from_value(serde_json::json!({
            "candidates": [{
                "content": { "parts": [{ "inlineData": { "mimeType": "image/png", "data": "" } }] }
            }]
        }))
        .expect("parses");
        assert!(extract_first_image(empty_data).is_none());

        let no_candidates: GenerateContentResponseBody =
            serde_json::from_value(serde_json::json!({})).expect("parses");
        assert!(extract_first_image(no_candidates).is_none());
    }

    #[test]
    fn image_url_targets_generate_content_on_the_configured_base() {
        // The adapter reuses the chat path's endpoint builder, so the composed
        // URL is {base}/models/{model}:generateContent with any trailing slash
        // normalized away.
        assert_eq!(
            model_url(
                "https://generativelanguage.googleapis.com/v1beta/",
                "gemini-2.5-flash-image",
                "generateContent"
            ),
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-flash-image:generateContent"
        );
    }
}
