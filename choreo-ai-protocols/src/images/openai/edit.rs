//! Edit request plumbing for the `OpenAI` Images adapter.
//!
//! Editing has two body shapes on the same `POST {base}/images/edits` path.
//! The default is the gpt-image `multipart/form-data` contract ([`edit_multipart`],
//! sent by [`OpenAiImageClient::edit_image_multipart`]); xAI is the exception,
//! taking an `application/json` `{model, prompt, images}` body of `data:` URIs
//! ([`edit_json_body`], sent by [`OpenAiImageClient::edit_image_json`]).
//! [`OpenAiImageClient::edit_image`](super::OpenAiImageClient) picks between the
//! two by provider slug.

use super::response::ImagesResponse;
use super::{IMAGE_MAX_ATTEMPTS, OpenAiImageClient};
use crate::images::{
    Background, ImageEditRequest, ImageGenerationResult, ImageQuality, ImageSize, OutputFormat,
};
use crate::openai::OpenAiError;
use crate::openai::endpoint_url;
use crate::retry::{self, AttemptContext, RetryConfig};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use choreo_proto::InferenceError;
use std::io;
use std::io::Read as _;
use ureq::unversioned::multipart::{Form, Part};

/// Images API editing path under the configured base URL (multipart edit).
const IMAGE_EDITS_PATH: &str = "/images/edits";

impl OpenAiImageClient {
    /// Edit via the xAI JSON contract: `POST {base}/images/edits` with
    /// `Content-Type: application/json` and a `{model, prompt, images}` body of
    /// `data:` URIs (see [`edit_json_body`]). Shares the multipart edit's
    /// retry/attempt machinery; only the body and `Content-Type` differ. The
    /// response is decoded by [`Self::decode_first_image`], so an xAI `url`
    /// (its `data[0]` carries `b64_json` null) is fetched through the shared
    /// download machinery rather than assumed to be inline base64.
    pub(super) fn edit_image_json(
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
        let body = edit_json_body(req);
        let mut on_retry: Option<retry::RetryCallback> = None;
        let mut ctx = AttemptContext::new(&mut on_retry, cancel_rx, None);

        tracing::debug!(
            url = %url,
            model = %req.model,
            image_count = req.images.len(),
            total_input_bytes = req.images.iter().map(|i| i.data.len()).sum::<usize>(),
            max_attempts = retry_cfg.max_attempts,
            "sending xAI image edit request"
        );

        // Compact JSON plus an explicit `Content-Length` (see
        // `retry::HookedJsonBody`); no upload hook — image editing has no
        // `Waiting` boundary.
        let hooked = retry::HookedJsonBody::new(&body, None, retry::RequestCompression::Identity)
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

        let (image_b64, revised_prompt) =
            self.decode_first_image(payload, &req.model, cancel_rx)?;

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

    /// Edit via the gpt-image `multipart/form-data` contract:
    /// `POST {base}/images/edits` with the body rendered once by
    /// [`edit_multipart`] and re-sent across attempts.
    pub(super) fn edit_image_multipart(
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
            self.decode_first_image(payload, &req.model, cancel_rx)?;

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

/// The outgoing **xAI edit** JSON body: `{model, prompt, images}` where every
/// input image is a `data:<mime>;base64,<...>` URI (xAI accepts a data URI
/// anywhere it takes an image URL) and `images` is always the vector form —
/// the single-input `image` field is the same shape with one element, so one
/// field covers both. Only `model`/`prompt`/`images` are sent: xAI's edit
/// endpoint accepts no `mask` and none of the `size`/`quality`/`background`/
/// `output_format` knobs, which the multipart path would otherwise emit
/// whenever a caller sets one.
fn edit_json_body(req: &ImageEditRequest) -> serde_json::Value {
    let images: Vec<String> = req
        .images
        .iter()
        .map(|image| {
            // Base64's alphabet contains no comma, so the MIME/payload
            // boundary in a `data:` URI is unambiguous.
            format!(
                "data:{};base64,{}",
                image.mime_type,
                BASE64.encode(&image.data)
            )
        })
        .collect();
    serde_json::json!({
        "model": req.model,
        "prompt": req.prompt,
        "images": images,
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
    let background = (req.background != Background::default()).then(|| req.background.to_string());
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

#[cfg(test)]
mod tests {
    use super::{edit_json_body, edit_multipart};
    use crate::images::{
        Background, ImageEditRequest, ImageInput, ImageQuality, ImageSize, OutputFormat,
    };
    use base64::Engine as _;

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

    #[test]
    fn edit_json_body_carries_data_uris_and_omits_every_knob() {
        // The xAI edit body is JSON: `{model, prompt, images}` where each input
        // is a `data:<mime>;base64,<...>` URI. A `mask` and every non-default
        // knob are present on the request but MUST NOT appear — xAI's edit
        // endpoint accepts none of them (and no `mask` at all), unlike the
        // multipart path which emits whichever knobs the caller set.
        let req = ImageEditRequest {
            size: ImageSize::Square1024,
            quality: ImageQuality::High,
            output_format: OutputFormat::Webp,
            background: Background::Transparent,
            mask: Some(ImageInput {
                filename: "mask.png".to_string(),
                mime_type: "image/png".to_string(),
                data: vec![9, 9],
            }),
            ..ImageEditRequest::new(
                "add a hat",
                "grok-imagine-image",
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
        let body = edit_json_body(&req);
        assert_eq!(
            body.get("model").and_then(serde_json::Value::as_str),
            Some("grok-imagine-image"),
            "{body}"
        );
        assert_eq!(
            body.get("prompt").and_then(serde_json::Value::as_str),
            Some("add a hat"),
            "{body}"
        );
        // `images` is always the vector form; each entry is a data URI whose
        // MIME comes from the input and whose payload is that input's bytes.
        let images = body
            .get("images")
            .and_then(serde_json::Value::as_array)
            .expect("images array");
        assert_eq!(images.len(), 2, "{body}");
        let expected_png = format!(
            "data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD.encode([1u8, 2, 3])
        );
        let expected_jpeg = format!(
            "data:image/jpeg;base64,{}",
            base64::engine::general_purpose::STANDARD.encode([4u8, 5])
        );
        assert_eq!(images[0].as_str(), Some(expected_png.as_str()), "{body}");
        assert_eq!(images[1].as_str(), Some(expected_jpeg.as_str()), "{body}");
        // No mask, no single-input `image`, no knobs.
        for omitted in [
            "mask",
            "image",
            "size",
            "quality",
            "output_format",
            "background",
        ] {
            assert!(
                body.get(omitted).is_none(),
                "{omitted} must be absent from the xAI edit body: {body}"
            );
        }
    }
}
