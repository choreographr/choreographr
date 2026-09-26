//! fal.ai video-generation adapter ([`FalVideoClient`]).
//!
//! fal serves video models on its **queue** API at `https://queue.fal.run`
//! (distinct from the synchronous `https://fal.run` image API). H3 — the
//! `minimax/h3-max/*` family — is queue-only: there is no synchronous variant.
//!
//! ## Auth
//!
//! `Authorization: Key <FAL_KEY>` — a bare key header, NOT the `Bearer`
//! scheme.
//!
//! ## Lifecycle (the four calls)
//!
//! 1. `POST /{model}` → a [`FalQueueStatus`] carrying `request_id`,
//!    `status_url`, `response_url`, and `cancel_url`. **Never retried** — a
//!    duplicate submit is a duplicate, billable job (fal already re-queues a
//!    failed runner up to 10× server-side).
//! 2. `GET {status_url}` (with `?logs=1` appended when logs are requested) →
//!    another [`FalQueueStatus`] whose `status` is `IN_QUEUE` / `IN_PROGRESS`
//!    / `COMPLETED`. Idempotent — driven through the shared retry layer.
//! 3. `GET {response_url}` → the finished artifact
//!    (`{"video":{url,content_type,file_name,file_size}, expanded_prompt?,
//!    timings?}`). Idempotent — retried.
//! 4. `PUT {cancel_url}` → `{success: bool}`; runtime returns 202
//!    `{"status":"CANCELLATION_REQUESTED"}`, 400 `ALREADY_COMPLETED`, or 404
//!    `NOT_FOUND`. Nothing to cancel is success.
//!
//! The three URLs are used **verbatim** from the submit response — they are
//! authoritative and already include the model path, so the adapter never
//! recomposes them.
//!
//! ## Errors — the shared two shapes, plus a third site
//!
//! fal's HTTP error bodies use the same two shapes the image path already
//! parses (see [`crate::fal::error`]): the 422 `{"detail":[…]}` validation
//! array and the flat `{"detail":"…","error_type":"…"}` request/infra body
//! (mirrored in `X-Fal-Error-Type`). A THIRD site exists on this queue path: a
//! `COMPLETED` status body can itself carry `error` + `error_type` for a job
//! that failed server-side — [`VideoGenerationClient::poll`] surfaces that as
//! [`VideoJobStatus::Failed`] and the driver routes its `error_type` back
//! through the same classifier (unknown ⇒ `ServerError`).
//!
//! ## Per-model request bodies
//!
//! Video models disagree wildly on their input schema, so the wire body is
//! built per **model family** (detected by model-id prefix) from the
//! normalized [`VideoGenerationRequest`]. See [`build_fal_video_body`].

use crate::SocketRegistry;
use crate::fal::error::fal_error;
use crate::openai::{ServiceConfig, endpoint_url};
use crate::retry::{self, AttemptContext, RetryConfig};
use crate::videos::{
    VideoAspectRatio, VideoGenerationClient, VideoGenerationRequest, VideoGenerationResult,
    VideoJobHandle, VideoJobStatus, VideoMetrics, VideoResolution,
};
use choreo_proto::InferenceError;
use serde::Deserialize;
use std::io;
use std::time::Duration;

/// Default queue base when the configured base URL is empty. Production
/// callers set `config.base_url` to the catalog's fal base (which is
/// `https://queue.fal.run` for the video models); this is the documented
/// fallback so a bare `ServiceConfig::default()` still composes a valid URL.
const DEFAULT_QUEUE_BASE: &str = "https://queue.fal.run";

/// Per-attempt HTTP deadline for the quick queue calls (submit, one status
/// GET, one result GET, one cancel PUT). The whole **job** budget lives in the
/// driver ([`crate::videos::VIDEO_TOTAL_TIMEOUT_SECS`]); this only bounds a
/// single hung request so a wedged socket cannot outlive the job deadline's
/// intent.
const FAL_VIDEO_HTTP_TIMEOUT_SECS: u64 = 120;

/// Retry budget for the idempotent queue GETs (status + result). Submit is
/// NEVER retried (see the module docs); cancel is a single PUT.
const VIDEO_GET_MAX_ATTEMPTS: u32 = 3;

/// One status/poll payload. Shared by the submit response and every status
/// GET (the shapes overlap; a status GET simply omits the submit-only URLs
/// and may add `logs`/`metrics`/`error`).
///
/// Every field is `Option` so a missing key is treated as absent rather than a
/// deserialization failure — the adapter decides what it actually needs
/// (e.g. submit needs the URLs; a status body needs `status`).
#[derive(Debug, Deserialize)]
struct FalQueueStatus {
    #[serde(default)]
    request_id: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    status_url: Option<String>,
    #[serde(default)]
    response_url: Option<String>,
    #[serde(default)]
    cancel_url: Option<String>,
    #[serde(default)]
    queue_position: Option<u32>,
    #[serde(default)]
    logs: Option<Vec<String>>,
    #[serde(default)]
    metrics: Option<VideoMetrics>,
    /// The third error site: a 2xx status body describing a FAILED job.
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    error_type: Option<String>,
}

/// The result GET's `video` object.
#[derive(Debug, Deserialize)]
struct FalVideoFile {
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    content_type: Option<String>,
    #[serde(default)]
    file_size: Option<u64>,
}

/// The result GET envelope: a single `video` plus optional `expanded_prompt`
/// and `timings` (and a `seed` we accept opportunistically).
#[derive(Debug, Deserialize)]
struct FalVideoOutput {
    #[serde(default)]
    video: Option<FalVideoFile>,
    #[serde(default)]
    expanded_prompt: Option<String>,
    #[serde(default)]
    seed: Option<u64>,
}

/// Model family for wire-body shaping (see [`build_fal_video_body`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VideoFamily {
    /// `minimax/h3-max/*` — required `prompt_expansion_mode`, uppercase-`P`
    /// resolution, and no `negative_prompt`.
    H3,
    /// `fal-ai/veo3` — duration suffixed with `s` (`"6s"`).
    Veo3,
    /// Everything else: the pragmatic common-name mapping.
    Generic,
}

/// Detect the model family from the model id. Detection is by **prefix** so a
/// versioned/variant id (e.g. `minimax/h3-max/image-to-video`) lands in the
/// right family.
fn video_family(model: &str) -> VideoFamily {
    if model.starts_with("minimax/h3-max/") {
        VideoFamily::H3
    } else if model.starts_with("fal-ai/veo3") {
        VideoFamily::Veo3
    } else {
        VideoFamily::Generic
    }
}

/// Map our [`VideoResolution`] to a family's wire string.
///
/// H3 uses an uppercase `P` (`480P`/`768P`/`1080P`); every other family
/// lowercase `p`. R768 only makes sense for H3; other families are sent the
/// lowercase form as the documented best-effort default (not every variant is
/// valid for every model).
fn resolution_wire(res: VideoResolution, family: VideoFamily) -> &'static str {
    match family {
        VideoFamily::H3 => match res {
            VideoResolution::R480 => "480P",
            VideoResolution::R720 => "720P",
            VideoResolution::R768 => "768P",
            VideoResolution::R1080 => "1080P",
        },
        VideoFamily::Veo3 | VideoFamily::Generic => match res {
            VideoResolution::R480 => "480p",
            VideoResolution::R720 => "720p",
            VideoResolution::R768 => "768p",
            VideoResolution::R1080 => "1080p",
        },
    }
}

/// Map a duration to a family's wire value: H3 takes whole seconds as an
/// integer; veo3 takes a suffixed string (`"6s"`); the generic family takes a
/// bare numeric string (the shape kling/seedance document).
fn duration_wire(secs: u32, family: VideoFamily) -> serde_json::Value {
    match family {
        VideoFamily::H3 => serde_json::Value::from(secs),
        VideoFamily::Veo3 => serde_json::Value::String(format!("{secs}s")),
        VideoFamily::Generic => serde_json::Value::String(secs.to_string()),
    }
}

/// Build the fal wire body for a model from the normalized request.
///
/// The variance across families is real and documented rather than guessed at:
///
/// - **H3** (`minimax/h3-max/*`): `{prompt, prompt_expansion_mode:"balanced",
///   duration?, resolution? ("480P"/"768P"/"1080P"), aspect_ratio?, seed?,
///   image_url?, end_image_url?}`. `prompt_expansion_mode` is REQUIRED by the
///   model, so the default (`"balanced"`) is pinned here. `negative_prompt` is
///   not a knob H3 documents — it is **silently dropped** for this family
///   (documented best-effort contract, mirroring the image adapters). The
///   `image_url`/`end_image_url` fields drive image-to-video; the t2v route
///   simply has neither.
/// - **veo3** (`fal-ai/veo3`): `duration` as `"Ns"` (veo3's `"4s"/"6s"/"8s"`),
///   `resolution` lowercase, plus `negative_prompt`.
/// - **Generic** (kling/wan/seedance/unknown): the common-name mapping —
///   `duration` as a bare numeric string, `resolution` lowercase,
///   `negative_prompt`, `image_url`/`end_image_url`. Note that `wan` documents
///   `num_frames` (17..161) instead of a duration; that model's frame budget
///   is a documented follow-up and a `duration_secs` on a wan request is sent
///   as the generic `duration` (which wan may ignore) rather than
///   guessed at as frames.
///
/// A duration/resolution/aspect/seed left unset is omitted entirely (minimal
/// body). `aspect_ratio` is only emitted for a non-`Auto` value.
pub(crate) fn build_fal_video_body(model: &str, req: &VideoGenerationRequest) -> serde_json::Value {
    let family = video_family(model);
    let mut body = serde_json::json!({ "prompt": req.prompt });
    // The json! macro always builds an object, so the guard is a formality
    // that keeps the `insert` calls panic-free (Map::insert, not IndexMut).
    let Some(obj) = body.as_object_mut() else {
        return body;
    };
    // H3 requires prompt_expansion_mode; pin the documented default.
    if family == VideoFamily::H3 {
        obj.insert("prompt_expansion_mode".into(), "balanced".into());
    }
    if let Some(secs) = req.duration_secs {
        obj.insert("duration".into(), duration_wire(secs, family));
    }
    if let Some(res) = req.resolution {
        obj.insert("resolution".into(), resolution_wire(res, family).into());
    }
    if let Some(ar) = req.aspect_ratio.filter(|a| *a != VideoAspectRatio::Auto) {
        obj.insert("aspect_ratio".into(), ar.to_string().into());
    }
    if let Some(seed) = req.seed {
        obj.insert("seed".into(), seed.into());
    }
    // H3 documents no negative_prompt — drop it rather than send an
    // unsupported field.
    if family != VideoFamily::H3
        && let Some(negative) = req.negative_prompt.as_deref().filter(|s| !s.is_empty())
    {
        obj.insert("negative_prompt".into(), negative.into());
    }
    if let Some(from) = req.from_frame_url.as_deref().filter(|s| !s.is_empty()) {
        obj.insert("image_url".into(), from.into());
    }
    if let Some(end) = req.end_frame_url.as_deref().filter(|s| !s.is_empty()) {
        obj.insert("end_image_url".into(), end.into());
    }
    body
}

/// Client for the fal.ai video queue (`https://queue.fal.run`).
///
/// Construction mirrors the image adapters: it takes the same
/// [`ServiceConfig`] shape so an account configured for the image/chat path
/// clones as-is (base URL, connect timeout, user agent, slug, backoff knobs),
/// then overrides the one knob the video path must set differently (the short
/// per-attempt HTTP deadline — the JOB budget lives in the driver).
pub struct FalVideoClient {
    config: ServiceConfig,
    api_key: zeroize::Zeroizing<String>,
    http: ureq::Agent,
    poll_interval: Duration,
}

// Manual Debug impl: derived Debug would print the raw API key if a client is
// ever logged — same redaction pattern as the image clients.
impl std::fmt::Debug for FalVideoClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FalVideoClient")
            .field("config", &self.config)
            .field("api_key", &"***")
            .field("http", &self.http)
            .field("poll_interval", &self.poll_interval)
            .finish()
    }
}

impl FalVideoClient {
    #[must_use]
    pub fn new(mut config: ServiceConfig, api_key: String, registry: &SocketRegistry) -> Self {
        let http = crate::shared::build_agent(
            registry,
            config.connect_timeout_secs,
            FAL_VIDEO_HTTP_TIMEOUT_SECS,
            FAL_VIDEO_HTTP_TIMEOUT_SECS,
            config.user_agent.as_deref(),
        );
        // Keep the config in sync with the agent so any consumer reading
        // `config()` sees the deadline actually in effect.
        config.total_timeout_secs = FAL_VIDEO_HTTP_TIMEOUT_SECS;
        Self {
            config,
            api_key: zeroize::Zeroizing::new(api_key),
            http,
            poll_interval: Duration::from_millis(crate::videos::VIDEO_POLL_INTERVAL_MS),
        }
    }

    /// Override the poll interval used by the provided
    /// [`VideoGenerationClient::generate_video`] driver. Tests pass
    /// `Duration::ZERO` so no sleeping happens.
    #[must_use]
    pub fn with_poll_interval(mut self, interval: Duration) -> Self {
        self.poll_interval = interval;
        self
    }

    #[must_use]
    pub fn config(&self) -> &ServiceConfig {
        &self.config
    }

    /// The queue base: the configured base URL, or [`DEFAULT_QUEUE_BASE`] when
    /// it is empty. Trailing slashes are trimmed so `endpoint_url` composes a
    /// clean `/{model}` path.
    fn queue_base(&self) -> &str {
        let base = self.config.base_url.trim().trim_end_matches('/');
        if base.is_empty() {
            DEFAULT_QUEUE_BASE
        } else {
            base
        }
    }

    /// The `Key <key>` auth header value.
    fn auth_header(&self) -> zeroize::Zeroizing<String> {
        zeroize::Zeroizing::new(format!("Key {}", self.api_key.trim()))
    }

    /// Retry config for the idempotent GETs.
    fn get_retry_config(&self) -> RetryConfig {
        RetryConfig::new(
            VIDEO_GET_MAX_ATTEMPTS,
            self.config.retry_initial_backoff_ms,
            self.config.retry_max_backoff_ms,
        )
    }

    /// Issue an idempotent GET through the shared retry layer, returning the
    /// terminal response UNCONSUMED (so the caller applies the fal error
    /// contract) — exactly like the image adapter's `retry_loop_raw` usage.
    fn get_raw(
        &self,
        url: &str,
        auth: &str,
        cancel_rx: Option<&crossbeam_channel::Receiver<()>>,
    ) -> Result<ureq::http::Response<ureq::Body>, InferenceError> {
        let retry_cfg = self.get_retry_config();
        let mut on_retry: Option<retry::RetryCallback> = None;
        let mut ctx = AttemptContext::new(&mut on_retry, cancel_rx, None);
        retry::retry_loop_raw(
            || self.http.get(url).header("Authorization", auth).call(),
            &retry_cfg,
            &mut ctx,
        )
        .map_err(crate::shared::ProviderError::from)
        .map_err(crate::shared::provider_error_to_inference)
    }

    /// Read the classifier header, the Retry-After budget input, and the body
    /// from a non-2xx response and apply the shared fal error mapping.
    fn response_error(response: ureq::http::Response<ureq::Body>) -> InferenceError {
        let status = response.status().as_u16();
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
            "fal video request failed"
        );
        crate::shared::provider_error_to_inference(fal_error(
            status,
            header_type.as_deref(),
            retry_after_secs,
            &body_text,
        ))
    }
}

/// Read the response body as JSON, mapping a decode failure to an `Io` error.
fn read_json<T: serde::de::DeserializeOwned>(
    response: ureq::http::Response<ureq::Body>,
) -> Result<T, InferenceError> {
    response
        .into_body()
        .read_json()
        .map_err(|e| InferenceError::Io(io::Error::other(e)))
}

/// Map a parsed status payload to a normalized [`VideoJobStatus`].
///
/// The `error`/`error_type` pair is checked FIRST: fal reports a failed job
/// inside a body whose `status` may still read `COMPLETED`, so the presence of
/// `error` is the authoritative failure signal.
fn map_status(payload: FalQueueStatus) -> VideoJobStatus {
    if let Some(detail) = payload.error.filter(|s| !s.trim().is_empty()) {
        return VideoJobStatus::Failed {
            detail,
            error_type: payload.error_type,
        };
    }
    match payload.status.as_deref() {
        Some("COMPLETED") => VideoJobStatus::Completed {
            metrics: payload.metrics,
        },
        Some("IN_PROGRESS") => VideoJobStatus::InProgress {
            logs: payload.logs.unwrap_or_default(),
        },
        // `IN_QUEUE` (and an absent status on a fresh submit) is queued; an
        // unrecognized status is treated as still-in-progress so polling
        // continues rather than failing on a status we don't know.
        Some("IN_QUEUE") | None => VideoJobStatus::Queued {
            position: payload.queue_position,
        },
        Some(other) => {
            tracing::warn!(status = other, "unrecognized fal video status — polling on");
            VideoJobStatus::InProgress {
                logs: payload.logs.unwrap_or_default(),
            }
        }
    }
}

impl VideoGenerationClient for FalVideoClient {
    fn provider_slug(&self) -> &str {
        &self.config.provider_slug
    }

    fn poll_interval(&self) -> Duration {
        self.poll_interval
    }

    fn submit(&self, req: &VideoGenerationRequest) -> Result<VideoJobHandle, InferenceError> {
        let url = endpoint_url(self.queue_base(), &format!("/{}", req.model))
            .map_err(InferenceError::Io)?;
        let auth = self.auth_header();
        let body = build_fal_video_body(&req.model, req);
        tracing::debug!(url = %url, model = %req.model, body = %body, "submitting fal video job");
        // NO retry: a duplicate submit is a duplicate, billable job.
        let response = self
            .http
            .post(&url)
            .header("Authorization", auth.as_str())
            .send_json(&body)
            .map_err(|e| InferenceError::Io(io::Error::other(e)))?;
        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            return Err(Self::response_error(response));
        }
        let payload: FalQueueStatus = read_json(response)?;
        let request_id = payload
            .request_id
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| {
                tracing::warn!(model = %req.model, "fal video submit returned no request_id");
                InferenceError::EmptyResponse
            })?;
        // A submit missing any URL cannot be polled/collected/cancelled.
        let missing = || {
            tracing::warn!(
                model = %req.model,
                "fal video submit returned an incomplete job (url missing)"
            );
            InferenceError::EmptyResponse
        };
        Ok(VideoJobHandle {
            request_id,
            provider_slug: self.config.provider_slug.clone(),
            model: req.model.clone(),
            status_url: payload
                .status_url
                .filter(|s| !s.is_empty())
                .ok_or_else(missing)?,
            response_url: payload
                .response_url
                .filter(|s| !s.is_empty())
                .ok_or_else(missing)?,
            cancel_url: payload
                .cancel_url
                .filter(|s| !s.is_empty())
                .ok_or_else(missing)?,
        })
    }

    fn poll(
        &self,
        handle: &VideoJobHandle,
        want_logs: bool,
    ) -> Result<VideoJobStatus, InferenceError> {
        // Append `?logs=1` verbatim (fal's status_url carries no query); use
        // `&` when one is already present so a future URL shape is honored.
        let url = if want_logs {
            if handle.status_url.contains('?') {
                format!("{}&logs=1", handle.status_url)
            } else {
                format!("{}?logs=1", handle.status_url)
            }
        } else {
            handle.status_url.clone()
        };
        let auth = self.auth_header();
        let response = self.get_raw(&url, auth.as_str(), None)?;
        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            return Err(Self::response_error(response));
        }
        let payload: FalQueueStatus = read_json(response)?;
        Ok(map_status(payload))
    }

    fn result(&self, handle: &VideoJobHandle) -> Result<VideoGenerationResult, InferenceError> {
        let auth = self.auth_header();
        let response = self.get_raw(&handle.response_url, auth.as_str(), None)?;
        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            return Err(Self::response_error(response));
        }
        let payload: FalVideoOutput = read_json(response)?;
        let file = payload.video.ok_or_else(|| {
            tracing::warn!(request_id = %handle.request_id, "fal video result carried no video");
            InferenceError::EmptyResponse
        })?;
        let url = file.url.filter(|s| !s.trim().is_empty()).ok_or_else(|| {
            tracing::warn!(request_id = %handle.request_id, "fal video result carried no url");
            InferenceError::EmptyResponse
        })?;
        Ok(VideoGenerationResult {
            url,
            content_type: file.content_type,
            file_size: file.file_size,
            seed: payload.seed,
            expanded_prompt: payload.expanded_prompt,
            model: handle.model.clone(),
        })
    }

    fn cancel(&self, handle: &VideoJobHandle) -> Result<(), InferenceError> {
        let auth = self.auth_header();
        let response = self
            .http
            .put(&handle.cancel_url)
            .header("Authorization", auth.as_str())
            .send_empty()
            .map_err(|e| InferenceError::Io(io::Error::other(e)))?;
        let status = response.status().as_u16();
        // 2xx: accepted (202 `CANCELLATION_REQUESTED`). 400 ALREADY_COMPLETED
        // and 404 NOT_FOUND both mean "nothing to cancel" — success, not a
        // failure (a job that finished before the cancel landed is not an
        // error the caller should surface).
        if (200..300).contains(&status) || status == 400 || status == 404 {
            return Ok(());
        }
        Err(Self::response_error(response))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        FalVideoClient, VideoFamily, build_fal_video_body, duration_wire, resolution_wire,
        video_family,
    };
    use crate::openai::ServiceConfig;
    use crate::videos::{VideoAspectRatio, VideoGenerationRequest, VideoResolution};

    fn base_request() -> VideoGenerationRequest {
        VideoGenerationRequest::new("a rocket launch", "minimax/h3-max/text-to-video")
    }

    // ── family detection ─────────────────────────────────────────────────

    #[test]
    fn family_detection_is_prefix_based() {
        assert_eq!(
            video_family("minimax/h3-max/text-to-video"),
            VideoFamily::H3
        );
        assert_eq!(
            video_family("minimax/h3-max/image-to-video"),
            VideoFamily::H3
        );
        assert_eq!(video_family("fal-ai/veo3"), VideoFamily::Veo3);
        assert_eq!(video_family("fal-ai/veo3/fast"), VideoFamily::Veo3);
        assert_eq!(
            video_family("fal-ai/kling-video/v2/master/text-to-video"),
            VideoFamily::Generic
        );
        assert_eq!(video_family("some/unknown-model"), VideoFamily::Generic);
    }

    // ── resolution wire ──────────────────────────────────────────────────

    #[test]
    fn resolution_wire_is_uppercase_for_h3_lowercase_otherwise() {
        assert_eq!(
            resolution_wire(VideoResolution::R480, VideoFamily::H3),
            "480P"
        );
        assert_eq!(
            resolution_wire(VideoResolution::R768, VideoFamily::H3),
            "768P"
        );
        assert_eq!(
            resolution_wire(VideoResolution::R1080, VideoFamily::H3),
            "1080P"
        );
        for family in [VideoFamily::Veo3, VideoFamily::Generic] {
            assert_eq!(resolution_wire(VideoResolution::R480, family), "480p");
            assert_eq!(resolution_wire(VideoResolution::R720, family), "720p");
            assert_eq!(resolution_wire(VideoResolution::R768, family), "768p");
            assert_eq!(resolution_wire(VideoResolution::R1080, family), "1080p");
        }
    }

    // ── duration wire ────────────────────────────────────────────────────

    #[test]
    fn duration_wire_per_family() {
        assert_eq!(duration_wire(6, VideoFamily::H3), serde_json::json!(6));
        assert_eq!(duration_wire(6, VideoFamily::Veo3), serde_json::json!("6s"));
        assert_eq!(
            duration_wire(6, VideoFamily::Generic),
            serde_json::json!("6")
        );
    }

    // ── body builder: H3 ─────────────────────────────────────────────────

    #[test]
    fn h3_body_pins_expansion_mode_and_uppercase_resolution() {
        let req = VideoGenerationRequest {
            duration_secs: Some(10),
            resolution: Some(VideoResolution::R1080),
            aspect_ratio: Some(VideoAspectRatio::R16x9),
            seed: Some(42),
            // H3 documents no negative_prompt — it must be dropped.
            negative_prompt: Some("blurry".to_string()),
            ..base_request()
        };
        let body = build_fal_video_body("minimax/h3-max/text-to-video", &req);
        assert_eq!(body["prompt"], "a rocket launch");
        assert_eq!(body["prompt_expansion_mode"], "balanced");
        assert_eq!(body["duration"], 10); // integer seconds
        assert_eq!(body["resolution"], "1080P"); // uppercase P
        assert_eq!(body["aspect_ratio"], "16:9");
        assert_eq!(body["seed"], 42);
        assert!(body.get("negative_prompt").is_none(), "{body}");
        // A text-to-video request carries no frame URLs.
        assert!(body.get("image_url").is_none());
        assert!(body.get("end_image_url").is_none());
    }

    #[test]
    fn h3_image_to_video_body_carries_frame_urls() {
        let req = VideoGenerationRequest {
            from_frame_url: Some("https://cdn/first.png".to_string()),
            end_frame_url: Some("https://cdn/last.png".to_string()),
            ..VideoGenerationRequest::new("morph", "minimax/h3-max/image-to-video")
        };
        let body = build_fal_video_body("minimax/h3-max/image-to-video", &req);
        assert_eq!(body["image_url"], "https://cdn/first.png");
        assert_eq!(body["end_image_url"], "https://cdn/last.png");
    }

    #[test]
    fn h3_body_is_minimal_when_only_prompt_is_set() {
        let body = build_fal_video_body("minimax/h3-max/text-to-video", &base_request());
        // Only prompt + the required expansion mode.
        assert_eq!(
            body,
            serde_json::json!({
                "prompt": "a rocket launch",
                "prompt_expansion_mode": "balanced",
            })
        );
    }

    // ── body builder: veo3 + generic ─────────────────────────────────────

    #[test]
    fn veo3_body_suffixes_duration_and_keeps_negative_prompt() {
        let req = VideoGenerationRequest {
            duration_secs: Some(6),
            resolution: Some(VideoResolution::R720),
            negative_prompt: Some("rain".to_string()),
            ..VideoGenerationRequest::new("a sunset", "fal-ai/veo3")
        };
        let body = build_fal_video_body("fal-ai/veo3", &req);
        assert_eq!(body["duration"], "6s");
        assert_eq!(body["resolution"], "720p");
        assert_eq!(body["negative_prompt"], "rain");
        // veo3 is not H3 → no prompt_expansion_mode.
        assert!(body.get("prompt_expansion_mode").is_none());
    }

    #[test]
    fn generic_body_uses_bare_numeric_duration_string() {
        let req = VideoGenerationRequest {
            duration_secs: Some(5),
            resolution: Some(VideoResolution::R720),
            negative_prompt: Some("low quality".to_string()),
            ..VideoGenerationRequest::new("waves", "fal-ai/kling-video/v2/master/text-to-video")
        };
        let body = build_fal_video_body("fal-ai/kling-video/v2/master/text-to-video", &req);
        assert_eq!(body["duration"], "5"); // bare numeric string
        assert_eq!(body["resolution"], "720p");
        assert_eq!(body["negative_prompt"], "low quality");
        assert!(body.get("prompt_expansion_mode").is_none());
    }

    #[test]
    fn auto_aspect_ratio_is_omitted() {
        let req = VideoGenerationRequest {
            aspect_ratio: Some(VideoAspectRatio::Auto),
            ..base_request()
        };
        let body = build_fal_video_body("minimax/h3-max/text-to-video", &req);
        assert!(body.get("aspect_ratio").is_none(), "{body}");
    }

    // ── construction / accessors ─────────────────────────────────────────

    #[test]
    fn new_forces_the_http_deadline_and_defaults_the_queue_base() {
        let client = FalVideoClient::new(
            ServiceConfig {
                base_url: String::new(), // empty → default queue base
                provider_slug: "fal".to_string(),
                total_timeout_secs: 3600,
                ..Default::default()
            },
            "k".to_string(),
            &choreo_ai_protocols::SocketRegistry::new(),
        );
        assert_eq!(client.config().total_timeout_secs, 120);
        assert_eq!(client.queue_base(), "https://queue.fal.run");
        // Strip trailing slashes on a configured base.
        let client = FalVideoClient::new(
            ServiceConfig {
                base_url: "http://127.0.0.1:9/fal/".to_string(),
                ..Default::default()
            },
            "k".to_string(),
            &choreo_ai_protocols::SocketRegistry::new(),
        );
        assert_eq!(client.queue_base(), "http://127.0.0.1:9/fal");
    }

    #[test]
    fn debug_redacts_the_api_key() {
        let client = FalVideoClient::new(
            ServiceConfig::default(),
            "super-secret-key".to_string(),
            &choreo_ai_protocols::SocketRegistry::new(),
        );
        let rendered = format!("{client:?}");
        assert!(!rendered.contains("super-secret-key"), "{rendered}");
        assert!(rendered.contains("***"), "{rendered}");
    }
}
