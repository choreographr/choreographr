//! z.ai / Zhipu GLM Images API adapter ([`ZaiImageClient`]).
//!
//! Talks to `{base}/images/generations` where the documented base is
//! `https://api.z.ai/api/paas/v4` (the full path is therefore
//! `/paas/v4/images/generations`). The `glm-image` model returns a **URL**
//! (a temporary CDN link that expires after 30 days) rather than inline
//! base64 — the only `data[]` field it documents — so this adapter's
//! response handling is a follow-up fetch, unlike [`super::OpenAiImageClient`],
//! whose gpt-image models inline `b64_json`. A `b64_json` field present in a
//! response is still honored (parse-level tolerance), because passing the
//! converged wire format through z.ai-compatible proxies costs nothing.
//!
//! Auth is the same Bearer header as the chat path, and it is NOT carried to
//! the CDN download: the temporary URL is pre-signed, and forwarding the API
//! key to a third-party CDN host would leak the credential to a provider the
//! account never agreed to authenticate against.

use crate::SocketRegistry;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use choreo_proto::InferenceError;
use serde::Deserialize;
use std::io;

use crate::download;
use crate::images::{IMAGE_MAX_ATTEMPTS, IMAGE_TOTAL_TIMEOUT_SECS};
use crate::images::{ImageGenerationClient, ImageGenerationRequest, ImageGenerationResult};
use crate::openai::endpoint_url;
use crate::openai::{OpenAiError, ServiceConfig};
use crate::retry::{self, AttemptContext, RetryConfig};

/// Images API path under the configured base URL (z.ai: the base already
/// ends at `/api/paas/v4`, so the composed URL is `/paas/v4/images/generations`).
const IMAGE_GENERATIONS_PATH: &str = "/images/generations";

/// z.ai chat accounts resolve to the documented standard `PaaS` base
/// (`https://api.z.ai/api/paas/v4` — see the overlay's `[provider.zai]`
/// `base_url`), so this rewrite is a NO-OP PASSTHROUGH for the default
/// configuration. It stays as a safety net for Coding-Plan subscribers
/// who override their account's `base_url` to the coding gateway
/// (`https://api.z.ai/api/coding/paas/v4`): the Images API is NOT served
/// under the `/coding` plan path — the docs pin it at
/// `https://api.z.ai/api/paas/v4`. Strip the `/coding` segment so the
/// image request lands on the plain `PaaS` base. The rewrite is a no-op
/// for any other base — the default z.ai base already ends at
/// `/api/paas/v4`, the mainland zhipuai base does too, proxies and test
/// mocks don't carry the segment at all — so it can never corrupt an
/// unrelated endpoint shape.
fn image_base_url(chat_base: &str) -> String {
    chat_base.replace("/coding/paas", "/paas")
}

/// Wire mapping of our [`ImageQuality`] to z.ai's `quality` enum
/// (`hd` | `standard` ONLY). glm-image defaults to `hd` (~20 s); `standard`
/// renders in ~5-10 s. `Low`/`Medium` both map to the fast lane — z.ai offers
/// only two tiers, and neither of them is a "low-fidelity" one, so the
/// cheapest documented tier is the intent-faithful mapping for both.
fn quality_wire(quality: super::ImageQuality) -> Option<&'static str> {
    match quality {
        super::ImageQuality::Auto => None,
        super::ImageQuality::Low | super::ImageQuality::Medium => Some("standard"),
        super::ImageQuality::High => Some("hd"),
    }
}

/// Wire mapping of our [`super::ImageSize`] to z.ai's `size` string.
/// All three non-auto variants are legal for glm-image's custom-size rule
/// (width/height in 1024..=2048, divisible by 32 — `1024`, `1536` both
/// qualify), and the strings are identical to the `OpenAI` wire strings.
/// `Auto` is omitted: `1280x1280` is glm-image's own default, and z.ai
/// documents no `auto` sentinel value — sending nothing is the only way to
/// express "provider decides" without guessing an undocumented literal.
fn size_wire(size: super::ImageSize) -> Option<&'static str> {
    match size {
        super::ImageSize::Auto => None,
        super::ImageSize::Square1024 => Some("1024x1024"),
        super::ImageSize::Portrait1024x1536 => Some("1024x1536"),
        super::ImageSize::Landscape1536x1024 => Some("1536x1024"),
    }
}

/// Client for the z.ai (Zhipu GLM) Images API (`/images/generations`).
///
/// Construction mirrors [`super::OpenAiImageClient::new`]: it takes the same
/// [`ServiceConfig`] shape so the chat account's config clones as-is (the
/// daemon dispatch keeps `base_url/user_agent/slug` shared), then overrides
/// the two knobs the image path must set differently (the 180 s attempt
/// deadline and the 2-attempt retry budget — see the shared constants in
/// [`super`]).
pub struct ZaiImageClient {
    config: ServiceConfig,
    api_key: zeroize::Zeroizing<String>,
    http: ureq::Agent,
}

// Manual Debug impl: derived Debug would print the raw API key if a client
// is ever logged — same redaction pattern as OpenAiImageClient.
impl std::fmt::Debug for ZaiImageClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ZaiImageClient")
            .field("config", &self.config)
            .field("api_key", &"***")
            .field("http", &self.http)
            .finish()
    }
}

impl ZaiImageClient {
    /// The agent is built with the image attempt deadline (see
    /// [`IMAGE_TOTAL_TIMEOUT_SECS`]) rather than the chat config's total
    /// timeout — same override pattern as [`super::OpenAiImageClient::new`].
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
    /// client's CDN downloads. Delegates to the shared
    /// [`download::host_guard_relaxed`] so z.ai and fal cannot drift on the
    /// trust decision; test-only because production callers reach it through
    /// [`download::download_image_bytes`], while the wire tests exercise it
    /// through the client.
    #[cfg(test)]
    fn host_guard_relaxed(&self) -> bool {
        download::host_guard_relaxed(&self.config.base_url)
    }

    /// The outgoing request body: `{model, prompt}` always; `size` only when
    /// non-auto and `quality` only when explicitly requested. z.ai documents
    /// NO `n`, `output_format`, or `background` field — they are never sent,
    /// not even with our request's values: extra undocumented fields are
    /// behavior we cannot verify the provider tolerates, and the
    /// request-level knobs are explicitly best-effort per family.
    ///
    /// An explicitly-set [`super::Background`] is therefore SILENTLY IGNORED
    /// (documented decision, not an error): z.ai's image API has no
    /// background-equivalent knob, and the daemon's knob contract is
    /// per-provider best effort — erroring a translucent-background request
    /// that every other provider honors would be worse than a plain
    /// fully-painted result.
    ///
    /// `n` is absent by design already (see `ImageGenerationRequest` —
    /// the struct has no field at all).
    fn request_body(req: &ImageGenerationRequest) -> serde_json::Value {
        let mut body = serde_json::json!({
            "model": req.model,
            "prompt": req.prompt,
        });
        // `Map::insert` (instead of `Value`'s IndexMut, which would panic on
        // a non-object body) is the correct API for setting top-level keys.
        // The json! macro above always produces an object, so the guard is a
        // formality — hoisted out of the two optionals below.
        if let Some(obj) = body.as_object_mut() {
            if let Some(size) = size_wire(req.size) {
                obj.insert("size".into(), size.into());
            }
            if let Some(quality) = quality_wire(req.quality) {
                obj.insert("quality".into(), quality.into());
            }
        }
        body
    }

    /// Download the generated image bytes from the temporary CDN URL via the
    /// shared [`download::download_image_bytes`] (scheme/SSRF guard, capped
    /// stream read, and the dedicated 3-attempt budget that rides out z.ai's
    /// CDN propagation race). No Authorization header is sent: the URL is
    /// pre-signed, so the API key must not travel to a third-party host (see
    /// the module docs).
    fn download_image(
        &self,
        url: &str,
        cancel_rx: Option<&crossbeam_channel::Receiver<()>>,
    ) -> Result<Vec<u8>, OpenAiError> {
        download::download_image_bytes(&self.http, &self.config, url, cancel_rx)
    }
}

/// One `data[]` item of the z.ai Images response.
///
/// Both fields are `Option` rather than required: a missing field must show
/// up as "no image data" (reported as [`OpenAiError::EmptyResponse`]), not
/// as a deserialization failure whose message would not say what was wrong.
/// `b64_json` is not part of the documented glm-image contract but is
/// parsed for tolerance (proxy/pass-through convergence with the standard
/// Images API); when present it wins over the URL download.
#[derive(Debug, Deserialize)]
struct ZaiImageDataItem {
    url: Option<String>,
    b64_json: Option<String>,
    // revised_prompt: z.ai's image endpoint does not document one; any
    // field in the JSON that is not listed here is ignored by serde —
    // no revision surface, so the result type's `revised_prompt` stays None.
}

/// A `content_filter` entry z.ai may attach to an image response.
///
/// `0` is the MOST severe level and `3` the least (inverted from what most
/// moderation APIs do — comment hooking this to the docs), carried per
/// `role` (`assistant` | `user` | `history`). Only the level matters for
/// the blocked decision; the offending role is surfaced in the error detail
/// so the user can see which turn was flagged.
#[derive(Debug, Deserialize)]
struct ZaiContentFilterEntry {
    level: u8,
    #[serde(rename = "role", default)]
    role: Option<String>,
}

/// The z.ai Images response envelope: `{created, data: […], content_filter?}`.
/// `created` is unexposed (no consumer for it — the daemon timestamps the
/// image itself at receipt).
#[derive(Debug, Deserialize)]
struct ZaiImagesResponse {
    #[serde(default)]
    data: Vec<ZaiImageDataItem>,
    #[serde(default)]
    content_filter: Option<Vec<ZaiContentFilterEntry>>,
}

impl ImageGenerationClient for ZaiImageClient {
    fn provider_slug(&self) -> &str {
        &self.config.provider_slug
    }

    fn generate_image(
        &self,
        req: &ImageGenerationRequest,
        cancel_rx: Option<&crossbeam_channel::Receiver<()>>,
    ) -> Result<ImageGenerationResult, InferenceError> {
        let url = endpoint_url(
            &image_base_url(&self.config.base_url),
            IMAGE_GENERATIONS_PATH,
        )
        .map_err(OpenAiError::Io)
        .map_err(crate::shared::provider_error_to_inference)?;
        // Frugal budget identical to the OpenAI adapter (2 attempts), with
        // the account's backoff knobs so the Retry-After budget gate behaves
        // exactly like every other image path.
        let retry_cfg = RetryConfig::new(
            IMAGE_MAX_ATTEMPTS,
            self.config.retry_initial_backoff_ms,
            self.config.retry_max_backoff_ms,
        );
        let auth_header = zeroize::Zeroizing::new(format!("Bearer {}", self.api_key.trim()));
        let http = &self.http;
        let body = Self::request_body(req);
        let mut on_retry: Option<retry::RetryCallback> = None;
        let mut ctx = AttemptContext::new(&mut on_retry, cancel_rx, None);

        tracing::debug!(
            url = %url,
            model = %req.model,
            ?req.size,
            ?req.quality,
            body = %body,
            max_attempts = retry_cfg.max_attempts,
            "sending z.ai image generation request"
        );

        let response = retry::retry_loop(
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

        let payload: ZaiImagesResponse = response
            .into_body()
            .read_json()
            .map_err(|e| OpenAiError::Io(io::Error::other(e)))
            .map_err(crate::shared::provider_error_to_inference)?;

        // Content-filter check FIRST: a blocked generation is denied by
        // policy, and returning the (possibly absent) image data as a raw
        // success — or a blank-image EmptyResponse — would both mask the
        // real reason. Level 0 (most severe) through 2 count as blocked;
        // level 3 is treated as a passing advisory per docs (only three
        // real severity levels below it). No retry: resending the same
        // prompt can never clear a policy flag.
        if let Some(entries) = &payload.content_filter
            && let Some(blocked) = entries.iter().find(|e| e.level <= 2)
        {
            let role = blocked.role.as_deref().unwrap_or("unknown");
            let detail = format!("level {}, role {role}", blocked.level);
            tracing::warn!(model = %req.model, level = blocked.level, role, "z.ai image generation blocked by content filter");
            // Honest representation: the HTTP response itself succeeded, so
            // no status is fabricated — the error is a policy denial, not a
            // 4xx.
            return Err(crate::shared::provider_error_to_inference(
                OpenAiError::ContentFiltered { detail },
            ));
        }

        let first = payload
            .data
            .into_iter()
            .next()
            .ok_or_else(|| {
                tracing::warn!(
                    model = %req.model,
                    "z.ai image generation response carried an empty data array"
                );
                OpenAiError::EmptyResponse
            })
            .map_err(crate::shared::provider_error_to_inference)?;

        // Prefer inline bytes, fall back to the (30-day-expiring) URL
        // fetch. A present-but-empty `b64_json` counts as absent — the same
        // convention as the OpenAI adapter.
        let ZaiImageDataItem { url, b64_json } = first;
        let inline_b64 = b64_json.filter(|b64| !b64.is_empty());
        let image_b64 = if let Some(b64) = inline_b64 {
            tracing::debug!(model = %req.model, "z.ai image response carried inline b64_json (tolerated, not documented)");
            b64
        } else {
            let Some(url) = url.filter(|u| !u.trim().is_empty()) else {
                tracing::warn!(
                    model = %req.model,
                    "z.ai image generation response carried neither url nor b64_json"
                );
                return Err(crate::shared::provider_error_to_inference(
                    OpenAiError::EmptyResponse,
                ));
            };
            BASE64.encode(
                self.download_image(&url, cancel_rx)
                    .map_err(crate::shared::provider_error_to_inference)?,
            )
        };

        tracing::info!(
            model = %req.model,
            image_b64_len = image_b64.len(),
            "z.ai image generation succeeded"
        );

        Ok(ImageGenerationResult {
            image_b64,
            // glm-image does not document nor return a revised prompt; left
            // None even if the proxy bent the contract (we don't parse one).
            revised_prompt: None,
            model: req.model.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{ZaiImageClient, image_base_url};
    use crate::openai::ServiceConfig;

    // ── image_base_url: /coding/paas → /paas rewrite ─────────────────────

    #[test]
    fn image_base_url_strips_the_coding_plan_segment() {
        // z.ai chat accounts resolve to the coding gateway base; the Images
        // API lives at the plain PaaS base. The exact documented rewrite.
        assert_eq!(
            image_base_url("https://api.z.ai/api/coding/paas/v4"),
            "https://api.z.ai/api/paas/v4"
        );
    }

    #[test]
    fn image_base_url_is_a_noop_for_other_shapes() {
        // The mainland bigmodel base already ends at the plain PaaS path.
        assert_eq!(
            image_base_url("https://open.bigmodel.cn/api/paas/v4"),
            "https://open.bigmodel.cn/api/paas/v4"
        );
        // Proxies and test mocks carry no /coding segment at all.
        assert_eq!(
            image_base_url("http://127.0.0.1:9/v1"),
            "http://127.0.0.1:9/v1"
        );
        // And a bare base is left untouched.
        assert_eq!(image_base_url("https://api.z.ai"), "https://api.z.ai");
    }

    #[test]
    fn host_guard_relaxation_keys_on_the_configured_base() {
        // The relaxation is derived from the daemon's own base URL: a
        // loopback/private base (mock/dev) relaxes the download host guard,
        // a production public-hostname base does not.
        for base in [
            "http://127.0.0.1:9/paas/v4",
            "http://192.168.1.20:8080/v1",
            "http://[::1]:9/v1",
        ] {
            assert!(
                ZaiImageClient::new(
                    ServiceConfig {
                        base_url: base.to_string(),
                        ..Default::default()
                    },
                    "k".to_string(),
                    &choreo_ai_protocols::SocketRegistry::new()
                )
                .host_guard_relaxed(),
                "{base} must relax the host guard"
            );
        }
        for base in [
            "https://api.z.ai/api/paas/v4",
            "https://open.bigmodel.cn/api/paas/v4",
            "https://mfile.z.ai",
        ] {
            assert!(
                !ZaiImageClient::new(
                    ServiceConfig {
                        base_url: base.to_string(),
                        ..Default::default()
                    },
                    "k".to_string(),
                    &choreo_ai_protocols::SocketRegistry::new()
                )
                .host_guard_relaxed(),
                "{base} must NOT relax the host guard"
            );
        }
    }
}
