//! Response decoding for the `OpenAI` Images adapter.
//!
//! Owns the Images response envelope ([`ImagesResponse`] and its
//! [`ImageDataItem`] items) and the shared [`OpenAiImageClient::decode_first_image`]
//! extractor. Both the generation and edit paths pin their output to one image,
//! so "first and only" is the operative rule for both.

use super::OpenAiImageClient;
use crate::download;
use crate::openai::OpenAiError;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use choreo_proto::InferenceError;
use serde::Deserialize;

/// One `data[]` item of the Images API response.
///
/// `b64_json` and `url` are both `Option` rather than required so a *missing*
/// field is detected as "no image data" (reported as
/// [`OpenAiError::EmptyResponse`]) instead of a deserialization failure whose
/// message would not say what was actually wrong with the payload. The
/// gpt-image family returns inline `b64_json`; xAI returns a `url` (with
/// `b64_json` null) the adapter must download.
#[derive(Debug, Deserialize)]
pub(super) struct ImageDataItem {
    b64_json: Option<String>,
    url: Option<String>,
    revised_prompt: Option<String>,
}

/// The standard Images API response envelope: `{data: […], usage?: …}`.
/// `usage` is intentionally not parsed — token accounting for image
/// generations is not surfaced anywhere yet, and ignoring it keeps the
/// adapter forward-compatible with providers that omit the field.
#[derive(Debug, Deserialize)]
pub(super) struct ImagesResponse {
    data: Vec<ImageDataItem>,
}

impl OpenAiImageClient {
    /// Extract the first image's `(image_b64, revised_prompt)` from a decoded
    /// Images response, or [`OpenAiError::EmptyResponse`] when the payload
    /// carried no usable image. Shared by generation and editing: both pin the
    /// output to one image (generation via `n: 1`, editing because the tool
    /// consumes exactly one), so "first and only" is the operative rule for
    /// both.
    ///
    /// A non-empty `b64_json` is used directly (the gpt-image contract);
    /// otherwise a non-empty `url` is downloaded through the shared
    /// [`crate::download`] machinery — the same one the fal adapter uses — so
    /// the SSRF guard and the capped/retried/cancellable fetch cannot drift
    /// between adapters. xAI's `/images/edits` returns `data[0].url` with
    /// `b64_json` null, so it always takes the download path.
    pub(super) fn decode_first_image(
        &self,
        payload: ImagesResponse,
        model: &str,
        cancel_rx: Option<&crossbeam_channel::Receiver<()>>,
    ) -> Result<(String, Option<String>), InferenceError> {
        let item = payload.data.into_iter().next().ok_or_else(|| {
            tracing::warn!(model = %model, "image response carried no image data");
            crate::shared::provider_error_to_inference(OpenAiError::EmptyResponse)
        })?;
        let revised_prompt = item.revised_prompt;
        if let Some(b64) = item.b64_json.filter(|b64| !b64.is_empty()) {
            return Ok((b64, revised_prompt));
        }
        if let Some(image_url) = item.url.filter(|u| !u.trim().is_empty()) {
            tracing::debug!(model = %model, "image response carried a URL instead of inline bytes");
            let bytes =
                download::download_image_bytes(&self.http, &self.config, &image_url, cancel_rx)
                    .map_err(crate::shared::provider_error_to_inference)?;
            return Ok((BASE64.encode(bytes), revised_prompt));
        }
        tracing::warn!(model = %model, "image response carried no image data");
        Err(crate::shared::provider_error_to_inference(
            OpenAiError::EmptyResponse,
        ))
    }
}
