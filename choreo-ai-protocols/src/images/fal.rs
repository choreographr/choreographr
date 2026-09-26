//! fal.ai image-generation adapter ([`FalImageClient`]).
//!
//! fal.ai serves image models on its **synchronous** API: `POST
//! {base}/{model}` (base `https://fal.run`, e.g.
//! `https://fal.run/fal-ai/flux-2-pro`) returns the finished image in the
//! SAME response — no queue, no polling. (The separate `queue.fal.run`
//! submit/status API is for the later video path, not image generation.)
//!
//! ## Auth
//!
//! `Authorization: Key <FAL_KEY>` — a bare key header, NOT the `Bearer`
//! scheme the OpenAI-compatible adapters use.
//!
//! ## Request
//!
//! flux-2-pro takes `prompt` (required), `image_size` (a named enum string
//! like `square_hd` OR an explicit `{width, height}` object — both
//! dimensions a multiple of 16 in 256..=2560), `output_format`
//! (`jpeg`|`png`), and an optional `seed`. Our request has no `seed` field,
//! so none is sent. flux-2-pro has **no** `quality` or `background` knob:
//! those request-level fields are SILENTLY IGNORED (documented best-effort
//! contract, exactly like [`super::ZaiImageClient`]'s background note) — never
//! sent, never an error.
//!
//! ## Response
//!
//! `{"images":[{"url","content_type","file_name","file_size","width","height"}],"seed":N}`.
//! `images[0].url` is normally an `https://…fal.media/…` URL the adapter must
//! download (through the shared [`super::download`] machinery), but with
//! `sync_mode:true` it is a `data:` URI — the adapter handles BOTH: a data
//! URI is decoded in place (base64 after the first comma), anything else is
//! fetched. A response with no usable image is
//! [`OpenAiError::EmptyResponse`].
//!
//! ## Errors — TWO shapes
//!
//! fal returns two distinct error bodies, keyed by the JSON field shape and
//! switched on the machine-readable `type`/`error_type` value — NEVER the
//! free-text `msg`:
//!
//! 1. **Model / validation (HTTP 422):** `{"detail":[{loc,msg,type,url,…}]}`
//!    — a `content_policy_violation` entry maps to
//!    [`OpenAiError::ContentFiltered`], a `no_media_generated` entry to
//!    [`OpenAiError::EmptyResponse`], anything else to a `422` client error.
//! 2. **Request / infra:** a flat `{"detail":"<str>","error_type":"<snake>"}`
//!    (the same snake value also travels in the `X-Fal-Error-Type` header) —
//!    `client_cancelled`/`client_disconnected` (499)
//!    → [`OpenAiError::Cancelled`]; `request_timeout`/`startup_timeout`
//!    (504) and `runner_*`/`internal_error` → [`OpenAiError::ServerError`];
//!    `bad_request` (400) → [`OpenAiError::ClientError`].
//!
//! Remaining statuses fall back to the HTTP contract: 401 → `Unauthorized`,
//! 429 → `RateLimited` (honored via the shared retry budget), other 5xx →
//! `ServerError`, other 4xx → `ClientError`.
//!
//! ## Why `retry_loop_raw`, not `retry_loop`
//!
//! The standard [`crate::retry::retry_loop`] consumes a terminal error
//! response's body to summarize it for display, which would erase the
//! `type`/`error_type` fields this contract switches on. This adapter drives
//! the POST through [`crate::retry::retry_loop_raw`] — identical
//! retry/backoff/cancellation behavior, but a terminal response is handed
//! back un-read so [`fal_error`] can apply the two-shape mapping above.

use crate::SocketRegistry;
use crate::images::download;
use crate::images::{
    IMAGE_MAX_ATTEMPTS, IMAGE_TOTAL_TIMEOUT_SECS, ImageGenerationClient, ImageGenerationRequest,
    ImageGenerationResult, ImageSize, OutputFormat,
};
use crate::openai::endpoint_url;
use crate::openai::{OpenAiError, ServiceConfig};
use crate::retry::{self, AttemptContext, RetryConfig};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use choreo_proto::InferenceError;
use serde::Deserialize;
use std::io;

/// Wire mapping of our [`ImageSize`] to fal's `image_size`.
///
/// fal accepts EITHER a named enum string (`square_hd`, `landscape_4_3`, …)
/// OR an explicit `{"width":W,"height":H}` object. We prefer the documented
/// named enum when one matches our size exactly — `square_hd` is fal's
/// 1024×1024 — and the explicit object otherwise, so the caller's requested
/// dimensions (`1024×1536`, `1536×1024`) are honored to the pixel where no
/// named enum matches. `Auto` is omitted so the model applies its own
/// default. Returns `None` for the omitted case.
fn size_wire(size: ImageSize) -> Option<serde_json::Value> {
    match size {
        ImageSize::Auto => None,
        // fal's documented 1024×1024 size.
        ImageSize::Square1024 => Some(serde_json::Value::String("square_hd".to_string())),
        // No named fal enum matches these exactly → send explicit dimensions
        // (both are multiples of 16 and within 256..=2560).
        ImageSize::Portrait1024x1536 => Some(serde_json::json!({ "width": 1024, "height": 1536 })),
        ImageSize::Landscape1536x1024 => Some(serde_json::json!({ "width": 1536, "height": 1024 })),
    }
}

/// Wire mapping of our [`OutputFormat`] to fal's `output_format`.
///
/// flux-2-pro accepts only `jpeg` and `png`; a requested [`OutputFormat::Webp`]
/// therefore falls back to `png` (documented best-effort: the exact pixel
/// format is a per-provider knob, and a lossless fallback beats erroring a
/// request every other provider honors).
fn output_format_wire(format: OutputFormat) -> &'static str {
    match format {
        OutputFormat::Png | OutputFormat::Webp => "png",
        OutputFormat::Jpeg => "jpeg",
    }
}

/// If `value` is an inline `data:` URI (returned when `sync_mode` is on),
/// return the base64 payload after the first comma; `None` for a normal
/// http(s) URL or a malformed `data:` URI with no comma. Base64's alphabet
/// contains no comma, so the first comma always separates the metadata from
/// the payload.
fn data_uri_b64(value: &str) -> Option<&str> {
    value
        .strip_prefix("data:")
        .and_then(|rest| rest.split_once(',').map(|(_, b64)| b64))
}

/// One `images[]` item of the fal response. Only `url` is consumed; the
/// `content_type`/`file_name`/`file_size`/`width`/`height` fields are ignored
/// by serde (bytes are re-validated downstream), and `url` is `Option` so a
/// missing field surfaces as "no image data" ([`OpenAiError::EmptyResponse`])
/// rather than a deserialization failure.
#[derive(Debug, Deserialize)]
struct FalImage {
    url: Option<String>,
}

/// The fal response envelope. `seed` is parsed-but-unexposed (no consumer).
#[derive(Debug, Deserialize)]
struct FalResponse {
    #[serde(default)]
    images: Vec<FalImage>,
}

/// One entry of fal's model/validation error array (the HTTP 422 shape).
///
/// `type` is the machine-readable classifier the adapter switches on (never
/// `msg`); `msg` is the human-readable sentence surfaced in the error detail.
#[derive(Debug, Deserialize)]
struct FalValidationEntry {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    msg: String,
}

/// Parse fal's model/validation error body (`{"detail":[{type,msg,…}]}`).
///
/// Returns `None` when `body` is not that array shape (so the caller can try
/// the flat request-error shape and then the status fallback). The decisions
/// are keyed on the entry `type` — `msg` is prose and is never matched.
fn parse_model_validation_error(status: u16, body: &str) -> Option<OpenAiError> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    // `detail` must be an ARRAY for this shape; a string `detail` belongs to
    // the flat request-error shape handled below.
    let entries: Vec<FalValidationEntry> =
        serde_json::from_value(value.get("detail")?.clone()).ok()?;
    // Content-filter and no-media are carried by `type`, not the HTTP status
    // (every validation failure is a 422), so they must be checked BEFORE the
    // generic mapping.
    if let Some(entry) = entries
        .iter()
        .find(|e| e.kind == "content_policy_violation")
    {
        let detail = if entry.msg.is_empty() {
            entry.kind.clone()
        } else {
            entry.msg.clone()
        };
        return Some(OpenAiError::ContentFiltered { detail });
    }
    if entries.iter().any(|e| e.kind == "no_media_generated") {
        return Some(OpenAiError::EmptyResponse);
    }
    // Any other validation failure is an ordinary client error; join every
    // entry so a multi-field validation error is fully described.
    let detail = entries
        .iter()
        .map(|e| format!("{}: {}", e.kind, e.msg))
        .collect::<Vec<_>>()
        .join("; ");
    Some(OpenAiError::ClientError {
        status,
        detail: if detail.is_empty() {
            body.to_string()
        } else {
            detail
        },
    })
}

/// Parse fal's flat request/infra error body
/// (`{"detail":"<str>","error_type":"<snake>"}`), switching on `error_type`.
///
/// `header_type` (the `X-Fal-Error-Type` header) backstops a body that omits
/// the field. Returns `None` when the body is not this shape OR the
/// `error_type` is one the caller should map by HTTP status instead.
fn parse_request_error(status: u16, body: &str, header_type: Option<&str>) -> Option<OpenAiError> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    let body_type = value.get("error_type").and_then(serde_json::Value::as_str);
    let error_type = body_type.or(header_type)?;
    // Human detail: the flat body's own `detail` string when present, else the
    // error_type itself.
    let detail = value
        .get("detail")
        .and_then(serde_json::Value::as_str)
        .map_or_else(|| error_type.to_string(), str::to_owned);
    match error_type {
        // The client went away mid-request (499): terminal, not retryable.
        "client_cancelled" | "client_disconnected" => Some(OpenAiError::Cancelled),
        // A timeout is infrastructural — the shared retry budget already
        // exhausted its attempts by the time this is mapped.
        "request_timeout" | "startup_timeout" => Some(OpenAiError::ServerError { status, detail }),
        // Worker/runner failures and a generic internal error are the
        // server's fault, not the request's.
        _ if error_type == "internal_error" || error_type.starts_with("runner_") => {
            Some(OpenAiError::ServerError { status, detail })
        }
        "bad_request" => Some(OpenAiError::ClientError { status, detail }),
        _ => None,
    }
}

/// A human-readable detail for a status-fallback error: the flat body's
/// `detail` string when present, else the trimmed raw body, else a generic
/// phrase.
fn status_fallback_detail(body: &str) -> String {
    let trimmed = body.trim();
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed)
        && let Some(detail) = value.get("detail").and_then(serde_json::Value::as_str)
        && !detail.is_empty()
    {
        return detail.to_string();
    }
    if trimmed.is_empty() {
        "request failed".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Map a non-2xx fal response to a provider error, applying the documented
/// precedence: the 422 validation array shape first, then the flat
/// request-error shape (body `error_type`, else the header), then the HTTP
/// status fallback. `retry_after_secs` is read from the response header by the
/// caller so a 429 surfaces the server's stated cooldown.
fn fal_error(
    status: u16,
    header_type: Option<&str>,
    retry_after_secs: Option<u64>,
    body: &str,
) -> OpenAiError {
    if let Some(err) = parse_model_validation_error(status, body) {
        return err;
    }
    if let Some(err) = parse_request_error(status, body, header_type) {
        return err;
    }
    let detail = status_fallback_detail(body);
    match status {
        401 => OpenAiError::Unauthorized { status, detail },
        429 => OpenAiError::RateLimited {
            status,
            retry_after_secs,
            detail,
        },
        s if (500..600).contains(&s) => OpenAiError::ServerError { status: s, detail },
        s if (400..500).contains(&s) => OpenAiError::ClientError { status: s, detail },
        _ => OpenAiError::Io(io::Error::other(detail)),
    }
}

/// Client for the fal.ai synchronous image API (`POST {base}/{model}`).
///
/// Construction mirrors [`super::OpenAiImageClient::new`]: it takes the same
/// [`ServiceConfig`] shape so an account configured for the daemon round-trips
/// as-is (base URL, connect timeout, user agent, slug, backoff knobs), then
/// overrides the one knob the image path must set differently (the 180 s
/// attempt deadline — see [`IMAGE_TOTAL_TIMEOUT_SECS`]).
pub struct FalImageClient {
    config: ServiceConfig,
    api_key: zeroize::Zeroizing<String>,
    http: ureq::Agent,
}

// Manual Debug impl: derived Debug would print the raw API key if a client
// is ever logged — same redaction pattern as the sibling image clients.
impl std::fmt::Debug for FalImageClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FalImageClient")
            .field("config", &self.config)
            .field("api_key", &"***")
            .field("http", &self.http)
            .finish()
    }
}

impl FalImageClient {
    /// The agent is built with the image attempt deadline (see
    /// [`IMAGE_TOTAL_TIMEOUT_SECS`]) rather than the chat config's total
    /// timeout, and the URL-download path shares the same agent, so the
    /// deadline bounds both.
    #[must_use]
    pub fn new(mut config: ServiceConfig, api_key: String, registry: &SocketRegistry) -> Self {
        let http = crate::shared::build_agent(
            registry,
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
        }
    }

    #[must_use]
    pub fn config(&self) -> &ServiceConfig {
        &self.config
    }

    /// Whether the SSRF IP-literal host guard may be RELAXED for this
    /// client's image downloads. Delegates to the shared
    /// [`download::host_guard_relaxed`] so fal and z.ai cannot drift on the
    /// trust decision; test-only because production callers reach it through
    /// [`download::download_image_bytes`], while the wire tests exercise it
    /// through the client.
    #[cfg(test)]
    fn host_guard_relaxed(&self) -> bool {
        download::host_guard_relaxed(&self.config.base_url)
    }

    /// The outgoing request body: `prompt` always; `image_size` only when the
    /// requested size is non-auto; `output_format` always (mapped, so the
    /// bytes come back in the format we declare rather than fal's own
    /// default). `quality` and `background` are NOT documented for
    /// flux-2-pro and are never sent — an explicitly-set
    /// [`super::Background`]/[`super::ImageQuality`] is silently ignored
    /// (documented best-effort contract, mirroring z.ai's background note).
    /// There is no `n` field (see [`ImageGenerationRequest`]) and no `seed`
    /// (the request struct has no field for it), so neither appears.
    fn request_body(req: &ImageGenerationRequest) -> serde_json::Value {
        let mut body = serde_json::json!({ "prompt": req.prompt });
        // `Map::insert` (rather than `Value`'s IndexMut, which would panic on a
        // non-object) is the correct API for setting top-level keys; the
        // json! macro above always produces an object.
        if let Some(obj) = body.as_object_mut() {
            if let Some(size) = size_wire(req.size) {
                obj.insert("image_size".into(), size);
            }
            obj.insert(
                "output_format".into(),
                output_format_wire(req.output_format).into(),
            );
        }
        body
    }
}

impl ImageGenerationClient for FalImageClient {
    fn provider_slug(&self) -> &str {
        &self.config.provider_slug
    }

    fn generate_image(
        &self,
        req: &ImageGenerationRequest,
        cancel_rx: Option<&crossbeam_channel::Receiver<()>>,
    ) -> Result<ImageGenerationResult, InferenceError> {
        // fal has no `/images/generations`-style fixed suffix: the model IS the
        // path segment. This deliberate deviation from the OpenAI-shaped
        // adapters is why the URL is composed here rather than via a shared
        // path constant.
        let url = endpoint_url(&self.config.base_url, &format!("/{}", req.model))
            .map_err(OpenAiError::Io)
            .map_err(crate::shared::provider_error_to_inference)?;
        // Frugal budget identical to the other image adapters (2 attempts),
        // with the account's backoff knobs so the Retry-After budget gate
        // behaves exactly like every other image path.
        let retry_cfg = RetryConfig::new(
            IMAGE_MAX_ATTEMPTS,
            self.config.retry_initial_backoff_ms,
            self.config.retry_max_backoff_ms,
        );
        // fal's auth header is `Key <key>`, NOT the Bearer scheme.
        let auth_header = zeroize::Zeroizing::new(format!("Key {}", self.api_key.trim()));
        let http = &self.http;
        let body = Self::request_body(req);
        let mut on_retry: Option<retry::RetryCallback> = None;
        let mut ctx = AttemptContext::new(&mut on_retry, cancel_rx, None);

        tracing::debug!(
            url = %url,
            model = %req.model,
            ?req.size,
            ?req.output_format,
            body = %body,
            max_attempts = retry_cfg.max_attempts,
            "sending fal image generation request"
        );

        // `retry_loop_raw` (not `retry_loop`): a terminal error response is
        // handed back with its body intact so `fal_error` can switch on the
        // `type`/`error_type` fields — see the module docs.
        let response = retry::retry_loop_raw(
            || {
                http.post(&url)
                    .header("Authorization", auth_header.as_str())
                    .send_json(&body)
            },
            &retry_cfg,
            &mut ctx,
        )
        .map_err(OpenAiError::from)
        .map_err(crate::shared::provider_error_to_inference)?;

        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            // Read the classifier header, the Retry-After budget input, and
            // the body, then apply the two-shape error mapping.
            let header_type = response
                .headers()
                .get("x-fal-error-type")
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned);
            let retry_after_secs = retry::parse_retry_after_secs(
                response
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok()),
            );
            let body_text = response.into_body().read_to_string().unwrap_or_default();
            tracing::warn!(
                status,
                error_type = header_type.as_deref().unwrap_or(""),
                "fal image generation failed"
            );
            return Err(crate::shared::provider_error_to_inference(fal_error(
                status,
                header_type.as_deref(),
                retry_after_secs,
                &body_text,
            )));
        }

        let payload: FalResponse = response
            .into_body()
            .read_json()
            .map_err(|e| OpenAiError::Io(io::Error::other(e)))
            .map_err(crate::shared::provider_error_to_inference)?;

        let image_ref = payload
            .images
            .into_iter()
            .next()
            .and_then(|image| image.url)
            .filter(|u| !u.trim().is_empty())
            .ok_or_else(|| {
                tracing::warn!(
                    model = %req.model,
                    "fal image generation response carried no usable image url"
                );
                OpenAiError::EmptyResponse
            })
            .map_err(crate::shared::provider_error_to_inference)?;

        // Handle BOTH url forms: an inline `data:` URI (sync_mode) is decoded
        // in place, anything else is downloaded through the shared machinery.
        let image_b64 = if let Some(b64) = data_uri_b64(&image_ref) {
            tracing::debug!(model = %req.model, "fal image response carried an inline data URI");
            b64.to_string()
        } else {
            BASE64.encode(
                download::download_image_bytes(http, &self.config, &image_ref, cancel_rx)
                    .map_err(crate::shared::provider_error_to_inference)?,
            )
        };

        tracing::info!(
            model = %req.model,
            image_b64_len = image_b64.len(),
            "fal image generation succeeded"
        );

        Ok(ImageGenerationResult {
            image_b64,
            // fal does not return a revised prompt.
            revised_prompt: None,
            model: req.model.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{
        FalImageClient, data_uri_b64, fal_error, output_format_wire, parse_model_validation_error,
        parse_request_error, size_wire,
    };
    use crate::images::{ImageSize, OutputFormat};
    use crate::openai::{OpenAiError, ServiceConfig};

    // ── size mapper ──────────────────────────────────────────────────────

    #[test]
    fn size_wire_maps_auto_to_none_and_sizes_to_fal_forms() {
        assert_eq!(size_wire(ImageSize::Auto), None);
        // The named enum for the exactly-matching 1024×1024 size.
        assert_eq!(
            size_wire(ImageSize::Square1024),
            Some(serde_json::json!("square_hd"))
        );
        // Explicit dimensions where no named enum matches.
        assert_eq!(
            size_wire(ImageSize::Portrait1024x1536),
            Some(serde_json::json!({ "width": 1024, "height": 1536 }))
        );
        assert_eq!(
            size_wire(ImageSize::Landscape1536x1024),
            Some(serde_json::json!({ "width": 1536, "height": 1024 }))
        );
    }

    // ── output-format mapper ─────────────────────────────────────────────

    #[test]
    fn output_format_wire_only_emits_jpeg_or_png() {
        assert_eq!(output_format_wire(OutputFormat::Png), "png");
        assert_eq!(output_format_wire(OutputFormat::Jpeg), "jpeg");
        // webp is not supported by flux-2-pro → lossless png fallback.
        assert_eq!(output_format_wire(OutputFormat::Webp), "png");
    }

    // ── data-URI decoder ─────────────────────────────────────────────────

    #[test]
    fn data_uri_b64_extracts_payload_and_rejects_urls() {
        assert_eq!(
            data_uri_b64("data:image/png;base64,aGVsbG8="),
            Some("aGVsbG8=")
        );
        // A normal CDN URL is not a data URI.
        assert_eq!(data_uri_b64("https://fal.media/files/x.png"), None);
        // A malformed data URI with no comma is not decodable here.
        assert_eq!(data_uri_b64("data:image/png;base64"), None);
        // Base64's alphabet has no comma, so the first comma is the boundary.
        assert_eq!(
            data_uri_b64("data:image/jpeg;base64,AAA/BBB+CCC="),
            Some("AAA/BBB+CCC=")
        );
    }

    // ── model/validation (422 array) parser ──────────────────────────────

    #[test]
    fn validation_content_policy_maps_to_content_filtered() {
        let body =
            r#"{"detail":[{"loc":["body"],"msg":"flagged","type":"content_policy_violation"}]}"#;
        match parse_model_validation_error(422, body) {
            Some(OpenAiError::ContentFiltered { detail }) => assert_eq!(detail, "flagged"),
            other => panic!("expected ContentFiltered, got {other:?}"),
        }
    }

    #[test]
    fn validation_no_media_maps_to_empty_response() {
        let body = r#"{"detail":[{"loc":[],"msg":"no image","type":"no_media_generated"}]}"#;
        assert!(matches!(
            parse_model_validation_error(422, body),
            Some(OpenAiError::EmptyResponse)
        ));
    }

    #[test]
    fn validation_other_type_maps_to_client_error_with_type_and_msg() {
        let body =
            r#"{"detail":[{"loc":["body","prompt"],"msg":"too long","type":"string_too_long"}]}"#;
        match parse_model_validation_error(422, body) {
            Some(OpenAiError::ClientError { status, detail }) => {
                assert_eq!(status, 422);
                assert!(detail.contains("string_too_long"), "{detail}");
                assert!(detail.contains("too long"), "{detail}");
            }
            other => panic!("expected ClientError, got {other:?}"),
        }
    }

    #[test]
    fn validation_parser_rejects_the_flat_shape() {
        // A flat `detail` string is not the array shape → None so the caller
        // falls through to the request-error parser.
        assert!(parse_model_validation_error(400, r#"{"detail":"boom"}"#).is_none());
        assert!(parse_model_validation_error(400, "not json").is_none());
    }

    // ── flat request/infra error parser ──────────────────────────────────

    #[test]
    fn request_error_cancelled_types_map_to_cancelled() {
        for kind in ["client_cancelled", "client_disconnected"] {
            let body = format!(r#"{{"detail":"gone","error_type":"{kind}"}}"#);
            assert!(
                matches!(
                    parse_request_error(499, &body, None),
                    Some(OpenAiError::Cancelled)
                ),
                "{kind} must map to Cancelled"
            );
        }
    }

    #[test]
    fn request_error_timeout_and_runner_types_map_to_server_error() {
        for kind in [
            "request_timeout",
            "startup_timeout",
            "runner_error",
            "runner_crash",
            "internal_error",
        ] {
            let body = format!(r#"{{"detail":"upstream","error_type":"{kind}"}}"#);
            match parse_request_error(504, &body, None) {
                Some(OpenAiError::ServerError { detail, .. }) => assert_eq!(detail, "upstream"),
                other => panic!("{kind} expected ServerError, got {other:?}"),
            }
        }
    }

    #[test]
    fn request_error_bad_request_maps_to_client_error() {
        let body = r#"{"detail":"malformed","error_type":"bad_request"}"#;
        match parse_request_error(400, body, None) {
            Some(OpenAiError::ClientError { status, detail }) => {
                assert_eq!(status, 400);
                assert_eq!(detail, "malformed");
            }
            other => panic!("expected ClientError, got {other:?}"),
        }
    }

    #[test]
    fn request_error_header_type_backstops_a_body_without_it() {
        // Body has no error_type; the X-Fal-Error-Type header supplies it.
        let body = r#"{"detail":"disconnected"}"#;
        assert!(matches!(
            parse_request_error(499, body, Some("client_disconnected")),
            Some(OpenAiError::Cancelled)
        ));
        // Neither body nor header → None (fall through to status).
        assert!(parse_request_error(499, body, None).is_none());
    }

    #[test]
    fn request_error_unknown_type_is_not_this_shape() {
        // An unknown error_type is left for the status fallback.
        assert!(parse_request_error(500, r#"{"error_type":"mystery"}"#, None).is_none());
    }

    // ── full error mapping ───────────────────────────────────────────────

    #[test]
    fn fal_error_status_fallbacks() {
        // 401 → Unauthorized (no error_type matches).
        assert!(matches!(
            fal_error(401, None, None, r#"{"detail":"invalid key"}"#),
            OpenAiError::Unauthorized { status: 401, .. }
        ));
        // 429 → RateLimited carrying Retry-After.
        match fal_error(429, None, Some(30), r#"{"detail":"slow down"}"#) {
            OpenAiError::RateLimited {
                status,
                retry_after_secs,
                detail,
            } => {
                assert_eq!(status, 429);
                assert_eq!(retry_after_secs, Some(30));
                assert_eq!(detail, "slow down");
            }
            other => panic!("expected RateLimited, got {other:?}"),
        }
        // Other 5xx → ServerError, other 4xx → ClientError.
        assert!(matches!(
            fal_error(503, None, None, ""),
            OpenAiError::ServerError { status: 503, .. }
        ));
        assert!(matches!(
            fal_error(418, None, None, ""),
            OpenAiError::ClientError { status: 418, .. }
        ));
    }

    #[test]
    fn fal_error_prefers_the_validation_shape_over_status() {
        // A 422 content_policy_violation must NOT be flattened to a plain
        // ClientError by the status fallback.
        let body = r#"{"detail":[{"type":"content_policy_violation","msg":"blocked"}]}"#;
        assert!(matches!(
            fal_error(422, None, None, body),
            OpenAiError::ContentFiltered { .. }
        ));
    }

    // ── construction / accessors ─────────────────────────────────────────

    #[test]
    fn new_forces_the_image_deadline_and_relaxes_for_local_bases() {
        let mut config = ServiceConfig {
            base_url: "http://127.0.0.1:9/fal".to_string(),
            provider_slug: "fal".to_string(),
            total_timeout_secs: 3600,
            ..Default::default()
        };
        config.total_timeout_secs = 3600;
        let client = FalImageClient::new(
            config,
            "k".to_string(),
            &choreo_ai_protocols::SocketRegistry::new(),
        );
        // The 180 s attempt deadline is forced regardless of the chat value.
        assert_eq!(client.config().total_timeout_secs, 180);
        // A loopback base relaxes the download host guard; a public one does
        // not.
        assert!(client.host_guard_relaxed());
        let public = FalImageClient::new(
            ServiceConfig {
                base_url: "https://fal.run".to_string(),
                ..Default::default()
            },
            "k".to_string(),
            &choreo_ai_protocols::SocketRegistry::new(),
        );
        assert!(!public.host_guard_relaxed());
    }

    #[test]
    fn debug_redacts_the_api_key() {
        let client = FalImageClient::new(
            ServiceConfig::default(),
            "super-secret-key".to_string(),
            &choreo_ai_protocols::SocketRegistry::new(),
        );
        let rendered = format!("{client:?}");
        assert!(!rendered.contains("super-secret-key"), "{rendered}");
        assert!(rendered.contains("***"), "{rendered}");
    }
}
