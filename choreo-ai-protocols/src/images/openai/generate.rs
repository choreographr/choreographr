//! Generation request plumbing for the `OpenAI` Images adapter.
//!
//! Owns the `POST {base}/images/generations` request loop driven by
//! [`OpenAiImageClient::generate`] and the outgoing JSON body ([`WireBody`]).
//! The body is the shared request `flatten`ed with the pinned `n: 1`; see
//! [`WireBody`] for why `seed` and `response_format` are never sent.

use super::response::ImagesResponse;
use super::{IMAGE_MAX_ATTEMPTS, OpenAiImageClient};
use crate::images::{ImageGenerationRequest, ImageGenerationResult};
use crate::openai::OpenAiError;
use crate::openai::endpoint_url;
use crate::retry::{self, AttemptContext, RetryConfig};
use choreo_proto::InferenceError;
use std::io;

/// Images API generation path under the configured base URL (`OpenAI`: `/v1`).
const IMAGE_GENERATIONS_PATH: &str = "/images/generations";

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

impl OpenAiImageClient {
    /// Send a generation request to `POST {base}/images/generations` and decode
    /// the first image out of the response.
    ///
    /// The wire body is serialized once to compact JSON with an explicit
    /// `Content-Length` (see [`retry::HookedJsonBody`]) and reused across every
    /// attempt, so the shared retry/backoff/cancellation policy applies
    /// unchanged.
    pub(super) fn generate(
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
        // instead of a new error variant. A `url` instead of inline bytes is
        // downloaded through the shared machinery.
        let (image_b64, revised_prompt) =
            self.decode_first_image(payload, &req.model, cancel_rx)?;

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
}

#[cfg(test)]
mod tests {
    use super::WireBody;
    use crate::images::ImageGenerationRequest;

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
}
