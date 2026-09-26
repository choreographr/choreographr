//! Video generation clients: the [`VideoGenerationClient`] trait and the
//! request/result types it is expressed in.
//!
//! Unlike the image path (a single synchronous POST), fal's video models are
//! **queue-only**: a submit returns a job handle, the job is polled to
//! completion, and only then is the finished artifact's URL fetched. The trait
//! is therefore expressed around a **queue handle primitive** ([`submit`] /
//! [`poll`] / [`result`] / [`cancel`]) plus a provided, blocking
//! [`VideoGenerationClient::generate_video`] driver that chains them so a
//! simple caller gets the submit→poll→result flow for free.
//!
//! This mirrors the [`crate::images`] split: a provider-agnostic trait plus
//! per-provider adapters (the fal.ai video queue, [`FalVideoClient`]). Errors
//! reuse [`InferenceError`], so the chat, image, and video paths share one
//! error taxonomy and one metrics-label mapping — no new error taxonomy is
//! invented here.
//!
//! [`submit`]: VideoGenerationClient::submit
//! [`poll`]: VideoGenerationClient::poll
//! [`result`]: VideoGenerationClient::result
//! [`cancel`]: VideoGenerationClient::cancel

mod fal;

// ── Shared adapter policy constants ───────────────────────────────────────

/// Wall-clock budget for one whole video **job**, in seconds — submit through
/// final result.
///
/// fal exposes no caller-settable total-inference deadline for queue jobs, so
/// the job budget lives here and is enforced by the
/// [`VideoGenerationClient::generate_video`] driver: it bounds submit + every
/// poll + the result GET together. 900 s (15 min) covers the slowest
/// legitimate high-resolution render while guaranteeing a wedged job can never
/// pin a worker indefinitely. On expiry the driver best-effort cancels the job
/// and returns [`InferenceError::DeadlineExceeded`].
///
/// `pub` so the daemon can derive a `generate_video` tool deadline from the
/// adapter's worst case — the driver budget is the authoritative number and
/// must not be duplicated.
pub const VIDEO_TOTAL_TIMEOUT_SECS: u64 = 900;

/// Default interval between [`VideoGenerationClient::poll`] calls, in
/// milliseconds, used by the [`VideoGenerationClient::generate_video`] driver
/// when an adapter does not override [`VideoGenerationClient::poll_interval`].
/// 2 s is frequent enough that a short render's completion is noticed promptly
/// without hammering the queue endpoint.
pub const VIDEO_POLL_INTERVAL_MS: u64 = 2_000;

/// Cap on downloaded video bytes for callers that fetch a finished artifact's
/// URL. Video files dwarf images, so this is far larger than the image path's
/// 8 MiB [`crate::images`] ceiling; 256 MiB covers a reasonable short clip
/// while still bounding memory against a hostile/huge response. Enforced
/// during the streaming read (see [`crate::download::download_media_bytes`]).
pub const VIDEO_DOWNLOAD_CAP_BYTES: usize = 256 * 1024 * 1024;

// ── Wire enums ────────────────────────────────────────────────────────────

use choreo_proto::InferenceError;
use serde::{Deserialize, Serialize};

/// Output resolution for a video generation.
///
/// The wire spellings are **per model family** (H3 uses uppercase `P`, e.g.
/// `1080P`; most others use lowercase `p`) and are therefore applied by the
/// adapter's per-family mapping rather than by a single serde rename — these
/// derived renames are the canonical lowercase form used for display and for
/// the generic family. Not every model supports every variant (e.g. veo3 is
/// only `720p`/`1080p`), which is a documented per-family best-effort
/// contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub enum VideoResolution {
    #[serde(rename = "480p")]
    R480,
    #[serde(rename = "720p")]
    R720,
    #[serde(rename = "768p")]
    R768,
    #[serde(rename = "1080p")]
    R1080,
}

impl std::fmt::Display for VideoResolution {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::R480 => "480p",
            Self::R720 => "720p",
            Self::R768 => "768p",
            Self::R1080 => "1080p",
        })
    }
}

/// Output aspect ratio for a video generation.
///
/// `Auto` means "let the provider decide" and is omitted from the wire body;
/// the ratio strings are shared across families.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
pub enum VideoAspectRatio {
    #[serde(rename = "16:9")]
    R16x9,
    #[serde(rename = "9:16")]
    R9x16,
    #[serde(rename = "1:1")]
    R1x1,
    #[serde(rename = "4:3")]
    R4x3,
    #[serde(rename = "3:4")]
    R3x4,
    #[serde(rename = "21:9")]
    R21x9,
    #[serde(rename = "auto")]
    #[default]
    Auto,
}

impl std::fmt::Display for VideoAspectRatio {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::R16x9 => "16:9",
            Self::R9x16 => "9:16",
            Self::R1x1 => "1:1",
            Self::R4x3 => "4:3",
            Self::R3x4 => "3:4",
            Self::R21x9 => "21:9",
            Self::Auto => "auto",
        })
    }
}

// ── Types ─────────────────────────────────────────────────────────────────

/// A provider-agnostic video generation request.
///
/// The field set is the **normalized intersection** of the supported model
/// families; the adapter maps these to each family's own wire names and
/// formats (see `fal::build_fal_video_body`). `duration_secs` is in whole
/// seconds; `from_frame_url`/`end_frame_url` drive image-to-video (an i2v
/// route with no `from_frame_url` degrades to text-to-video where the model
/// supports it). Serialization is deliberately minimal — a knob left unset is
/// omitted from the wire body rather than sent as an explicit default.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VideoGenerationRequest {
    pub prompt: String,
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_secs: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolution: Option<VideoResolution>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aspect_ratio: Option<VideoAspectRatio>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seed: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub negative_prompt: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from_frame_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_frame_url: Option<String>,
}

impl VideoGenerationRequest {
    /// Convenience constructor: prompt + model, every knob unset.
    #[must_use]
    pub fn new(prompt: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            prompt: prompt.into(),
            model: model.into(),
            duration_secs: None,
            resolution: None,
            aspect_ratio: None,
            seed: None,
            negative_prompt: None,
            from_frame_url: None,
            end_frame_url: None,
        }
    }
}

/// An opaque handle to a submitted video job.
///
/// The three URLs are taken from fal's submit response **verbatim** — they are
/// authoritative and already include the model path — so the adapter never
/// recomposes them. `provider_slug` and `model` are carried so the handle is
/// self-describing for logging and metrics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VideoJobHandle {
    pub request_id: String,
    pub provider_slug: String,
    pub model: String,
    pub status_url: String,
    pub response_url: String,
    pub cancel_url: String,
}

/// Timing metadata fal reports for a completed job.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct VideoMetrics {
    /// Server-side inference time in seconds, when reported. fal's key is not
    /// uniformly documented, so both `inference_time_secs` and the raw
    /// `inference_time` are accepted.
    #[serde(default, alias = "inference_time")]
    pub inference_time_secs: Option<f64>,
}

/// The status of a queued video job, normalized across its lifecycle.
#[derive(Debug, Clone, PartialEq)]
pub enum VideoJobStatus {
    /// Waiting in the queue; `position` is the provider's estimate when given.
    Queued { position: Option<u32> },
    /// Actively rendering. `logs` carries any provider progress lines (only
    /// populated when the poll requested logs).
    InProgress { logs: Vec<String> },
    /// Finished successfully; `metrics` is whatever timing the provider
    /// reported.
    Completed { metrics: Option<VideoMetrics> },
    /// The job failed. `detail` is the human-readable reason and `error_type`
    /// is the provider's machine-readable classifier when supplied (a
    /// `COMPLETED` status body can carry this third error site — see
    /// [`crate::fal::error`]).
    Failed {
        detail: String,
        error_type: Option<String>,
    },
}

/// A finished video generation.
///
/// The artifact is returned as a **URL** ([`Self::url`]) — the adapter never
/// downloads it (see [`crate::download`] for a caller that wants the bytes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoGenerationResult {
    /// URL of the generated video (provider-signed / CDN).
    pub url: String,
    /// MIME type reported by the provider, when given.
    pub content_type: Option<String>,
    /// Size in bytes reported by the provider, when given.
    pub file_size: Option<u64>,
    /// Generation seed, when the provider echoes one.
    pub seed: Option<u64>,
    /// Provider's optional expanded/rewritten prompt.
    pub expanded_prompt: Option<String>,
    /// The model that actually produced the video (echoed from the request).
    pub model: String,
}

// ── Trait ─────────────────────────────────────────────────────────────────

/// Provider-agnostic video generation over fal's queue protocol.
///
/// The trait is a **sibling of [`crate::ProviderClient`] and
/// [`crate::ImageGenerationClient`], not a sub-trait**: a video job is neither
/// a chat turn nor a synchronous image request (different endpoint, different
/// auth surface, a multi-step job lifecycle), so the daemon resolves it
/// through a separate handle.
///
/// The queue-handle primitive ([`submit`] / [`poll`] / [`result`] / [`cancel`])
/// is deliberately exposed so a caller that wants to drive the lifecycle
/// itself (progress reporting, its own scheduling) can; the provided
/// [`generate_video`] driver is the convenient blocking default.
///
/// # Retry / billing semantics
///
/// fal re-queues a failed runner up to 10× server-side, so a **duplicate
/// submit is a duplicate job and double billing**. [`submit`] must therefore
/// be called exactly once per job and implementations MUST NOT retry it. The
/// following GETs ([`poll`], [`result`]) and the cancel PUT are idempotent and
/// MAY use the retry layer.
///
/// [`submit`]: VideoGenerationClient::submit
/// [`poll`]: VideoGenerationClient::poll
/// [`result`]: VideoGenerationClient::result
/// [`cancel`]: VideoGenerationClient::cancel
/// [`generate_video`]: VideoGenerationClient::generate_video
pub trait VideoGenerationClient: std::fmt::Debug + Send + Sync {
    /// Catalog provider slug (e.g. `"fal"`), same convention as
    /// [`crate::ProviderClient::provider_slug`].
    fn provider_slug(&self) -> &str;

    /// Submit one job. Called **exactly once** per job — never retried (a
    /// duplicate submit is a duplicate, billable job). Returns the handle the
    /// other methods operate on.
    ///
    /// # Errors
    ///
    /// Returns [`InferenceError`] on HTTP/transport failure, provider error
    /// responses, or a response missing the job's URLs.
    fn submit(&self, req: &VideoGenerationRequest) -> Result<VideoJobHandle, InferenceError>;

    /// Poll a job's status. `want_logs` requests the provider's progress
    /// lines (only meaningful for an in-progress job). Idempotent — safe to
    /// retry.
    ///
    /// # Errors
    ///
    /// Returns [`InferenceError`] on HTTP/transport failure or provider error
    /// responses.
    fn poll(
        &self,
        handle: &VideoJobHandle,
        want_logs: bool,
    ) -> Result<VideoJobStatus, InferenceError>;

    /// Fetch the finished artifact for a completed job. Idempotent — safe to
    /// retry.
    ///
    /// # Errors
    ///
    /// Returns [`InferenceError`] on HTTP/transport failure, provider error
    /// responses, or a response with no usable video URL.
    fn result(&self, handle: &VideoJobHandle) -> Result<VideoGenerationResult, InferenceError>;

    /// Best-effort cancel of a job. Cancelling a job still `Queued` means it
    /// was never processed (free); cancelling one `InProgress` only sends the
    /// signal — the job may still complete and bill. A job that has already
    /// finished (or never existed) is **not** an error: nothing to cancel is
    /// success.
    ///
    /// # Errors
    ///
    /// Returns [`InferenceError`] on transport failure or a genuine provider
    /// error (a 5xx, auth failure, …) — never for "already completed"/"not
    /// found".
    fn cancel(&self, handle: &VideoJobHandle) -> Result<(), InferenceError>;

    /// Interval between polls used by the provided [`Self::generate_video`]
    /// driver. Defaults to [`VIDEO_POLL_INTERVAL_MS`]; adapters override it so
    /// tests can inject a zero interval and avoid sleeping.
    fn poll_interval(&self) -> std::time::Duration {
        std::time::Duration::from_millis(VIDEO_POLL_INTERVAL_MS)
    }

    /// Blocking driver: submit once, then poll until the job completes, fails,
    /// is cancelled, or the job budget ([`VIDEO_TOTAL_TIMEOUT_SECS`]) expires,
    /// then fetch the result.
    ///
    /// `on_progress` is invoked with every polled status (including the
    /// terminal one) so a caller can surface progress. `cancel_rx`, when
    /// supplied, is checked during each inter-poll wait; a cancel or a
    /// dropped/disconnected sender wakes the wait immediately (the wait is
    /// biased toward the cancel arm — never a poll-interval sleep).
    ///
    /// On cancel the driver best-effort [`Self::cancel`]s the job and returns
    /// [`InferenceError::Cancelled`]; on job-budget expiry it best-effort
    /// cancels and returns [`InferenceError::DeadlineExceeded`]. A failed job
    /// status is classified through the shared fal error-type classifier
    /// (unknown ⇒ `ServerError`).
    ///
    /// # Errors
    ///
    /// Returns [`InferenceError`] from submit/poll/result, or
    /// `Cancelled`/`DeadlineExceeded`/a classified job failure.
    fn generate_video(
        &self,
        req: &VideoGenerationRequest,
        cancel_rx: Option<&crossbeam_channel::Receiver<()>>,
        on_progress: &mut dyn FnMut(VideoJobStatus),
    ) -> Result<VideoGenerationResult, InferenceError> {
        let handle = self.submit(req)?;
        tracing::debug!(
            provider = %self.provider_slug(),
            request_id = %handle.request_id,
            model = %req.model,
            "video job submitted"
        );
        // Best-effort cancel helper: log and swallow any error so a cancel
        // failure never masks the real reason the driver is stopping.
        let cancel_job = |handle: &VideoJobHandle| {
            if let Err(e) = self.cancel(handle) {
                tracing::debug!(
                    request_id = %handle.request_id,
                    error = %e,
                    "best-effort video cancel failed"
                );
            }
        };
        let deadline =
            std::time::Instant::now() + std::time::Duration::from_secs(VIDEO_TOTAL_TIMEOUT_SECS);
        loop {
            let status = self.poll(&handle, false)?;
            match &status {
                VideoJobStatus::Completed { .. } => {
                    on_progress(status.clone());
                    let result = self.result(&handle)?;
                    tracing::info!(
                        request_id = %handle.request_id,
                        model = %req.model,
                        "video generation succeeded"
                    );
                    return Ok(result);
                }
                VideoJobStatus::Failed { detail, error_type } => {
                    // Classify BEFORE moving `status` into the progress
                    // callback (the classified error borrows `detail`).
                    let err = job_failure_to_inference(detail, error_type.as_deref());
                    tracing::warn!(
                        request_id = %handle.request_id,
                        error_type = error_type.as_deref().unwrap_or(""),
                        "video job failed"
                    );
                    on_progress(status.clone());
                    return Err(err);
                }
                // Queued or in-progress: report and wait before the next poll.
                VideoJobStatus::Queued { .. } | VideoJobStatus::InProgress { .. } => {
                    on_progress(status.clone());
                }
            }

            // Job-budget check precedes the wait so an already-expired budget
            // never sleeps; cap the wait to the remaining budget so the
            // deadline is honored within one poll interval.
            let now = std::time::Instant::now();
            let Some(remaining) = deadline.checked_duration_since(now) else {
                tracing::warn!(
                    request_id = %handle.request_id,
                    "video job budget expired — cancelling"
                );
                cancel_job(&handle);
                return Err(InferenceError::DeadlineExceeded);
            };
            let wait = self.poll_interval().min(remaining);
            // A cancel (or a dropped sender) aborts the wait; either way the
            // job is abandoned and we best-effort cancel it.
            if crate::retry::sleep_or_cancel(wait, cancel_rx).is_err() {
                tracing::info!(
                    request_id = %handle.request_id,
                    "video job cancelled — sending cancel"
                );
                cancel_job(&handle);
                return Err(InferenceError::Cancelled);
            }
        }
    }
}

/// Classify a failed video-job status. Routes the provider's `error_type`
/// through the shared fal classifier (see [`crate::fal::error`]); an unknown
/// or absent type falls back to `ServerError` (a job that failed server-side
/// is not the caller's fault).
fn job_failure_to_inference(detail: &str, error_type: Option<&str>) -> InferenceError {
    // The queue status GET itself succeeded (2xx) and carries no HTTP error
    // status — 500 is the honest "server-side job failure" carrier for the
    // classifier's status field.
    let provider = error_type
        .and_then(|t| crate::fal::error::classify_error_type(t, 500, detail))
        .unwrap_or_else(|| crate::shared::ProviderError::ServerError {
            status: 500,
            detail: detail.to_string(),
        });
    crate::shared::provider_error_to_inference(provider)
}

pub use fal::FalVideoClient;

/// Whether a catalog provider slug is one of the fal.ai **video** provider
/// slugs the daemon must route to the dedicated [`FalVideoClient`]:
///
/// - `"fal"` — the fal.ai platform slug;
/// - `"fal-ai"` — the models.dev display alias some catalogs use.
///
/// Both serve the queue-only `https://queue.fal.run` video contract (see the
/// module docs on [`FalVideoClient`]). The client crate owns this knowledge so
/// callers do not hardcode provider-family facts. Like
/// [`crate::images::is_fal_image_provider_slug`], this helper is consulted only
/// where a video-capable backend is being resolved — fal has no chat API, so
/// the chat-protocol dispatch never sees it.
#[must_use]
pub fn is_fal_video_provider_slug(slug: &str) -> bool {
    matches!(slug, "fal" | "fal-ai")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fal_slug_allowlist_is_exact() {
        // Exactly the two documented fal slugs route to the dedicated
        // FalVideoClient; near-misses and other families must not.
        for slug in ["fal", "fal-ai"] {
            assert!(
                is_fal_video_provider_slug(slug),
                "{slug} must be a fal video-provider slug"
            );
        }
        for slug in ["fal-future", "falai", "fal_ai", "openai", "zai", "", "FAL"] {
            assert!(
                !is_fal_video_provider_slug(slug),
                "{slug:?} must NOT be a fal video-provider slug"
            );
        }
    }

    #[test]
    fn failed_job_classification_routes_error_type() {
        assert!(matches!(
            job_failure_to_inference("died", Some("runner_crash")),
            InferenceError::ServerError { .. }
        ));
        assert!(matches!(
            job_failure_to_inference("gone", Some("client_disconnected")),
            InferenceError::Cancelled
        ));
        // Unknown / absent type → ServerError with the detail preserved.
        match job_failure_to_inference("mystery", Some("weird_future_type")) {
            InferenceError::ServerError { detail, .. } => assert_eq!(detail, "mystery"),
            other => panic!("expected ServerError, got {other:?}"),
        }
        assert!(matches!(
            job_failure_to_inference("boom", None),
            InferenceError::ServerError { .. }
        ));
    }

    #[test]
    fn request_defaults_omit_every_knob() {
        let req = VideoGenerationRequest::new("p", "minimax/h3-max/text-to-video");
        assert_eq!(req.duration_secs, None);
        assert_eq!(req.resolution, None);
        assert_eq!(req.aspect_ratio, None);
        // Serialization is minimal: only prompt + model survive.
        let value = serde_json::to_value(&req).unwrap();
        let obj = value.as_object().unwrap();
        assert_eq!(obj.len(), 2, "an all-defaults request is {{prompt, model}}");
        assert!(obj.contains_key("prompt"));
        assert!(obj.contains_key("model"));
    }

    #[test]
    fn aspect_ratio_default_is_auto_and_display_mirrors_wire() {
        assert_eq!(VideoAspectRatio::default(), VideoAspectRatio::Auto);
        assert_eq!(VideoAspectRatio::Auto.to_string(), "auto");
        assert_eq!(VideoAspectRatio::R16x9.to_string(), "16:9");
        assert_eq!(VideoResolution::R1080.to_string(), "1080p");
        assert_eq!(VideoResolution::R768.to_string(), "768p");
    }
}
