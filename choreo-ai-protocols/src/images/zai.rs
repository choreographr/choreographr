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
use std::io::Read as _;

use crate::images::{
    IMAGE_DOWNLOAD_ATTEMPTS, IMAGE_DOWNLOAD_CAP_BYTES, IMAGE_MAX_ATTEMPTS, IMAGE_TOTAL_TIMEOUT_SECS,
};
use crate::images::{ImageGenerationClient, ImageGenerationRequest, ImageGenerationResult};
use crate::openai::endpoint_url;
use crate::openai::{OpenAiError, ServiceConfig};
use crate::retry::{self, AttemptContext, RetryConfig};
use std::net::IpAddr;

/// Reject URLs whose host is an IP literal in a range that must never be
/// dereferenced from provider-controlled response data (SSRF guard).
///
/// Blocks loopback (127/8, ::1), private (RFC 1918, RFC 4193 `fc00::/7`),
/// and link-local (169.254/16, `fe80::/10`) addresses. Non-IP hostnames
/// (e.g. `mfile.z.ai`) are ALLOWED — recorded security decision:
/// the URL's hostname is provider-controlled, but DNS pinning for arbitrary
/// provider hosts is out of scope here; the residual risk is accepted
/// because (a) the URL arrives over the authenticated provider TLS channel,
/// not from user input, and (b) the downloaded bytes are fully validated
/// downstream by the daemon's image prepare pipeline (magic bytes, size
/// cap, decode), so a malicious host can at worst waste the download — it
/// cannot smuggle content into a session.
///
/// Returns `Err` with a human-readable reason (surfaced as the error detail).
fn is_downloadable_url(url: &str) -> Result<(), String> {
    is_http_url(url)?;
    // The URL crate renders IPv6 literals bracketed (`[::1]`); unwrap the
    // brackets before parsing. A hostname that is not an IP literal is
    // allowed (see the recorded decision above).
    let blocked = url::Url::parse(url)
        .ok()
        .and_then(|parsed| parsed.host_str().map(str::to_owned))
        .is_some_and(|host| {
            let host = host.trim_start_matches('[').trim_end_matches(']');
            host.parse::<IpAddr>().is_ok_and(is_blocked_ip_literal)
        });
    if blocked {
        return Err(format!(
            "image URL host is a private/loopback/link-local IP literal, refusing to fetch: {url}"
        ));
    }
    Ok(())
}

/// Scheme-only half of the download guard (http/https). Used standalone when
/// the host guard is relaxed for local-dev bases (see
/// `ZaiImageClient::host_guard_relaxed`).
fn is_http_url(url: &str) -> Result<(), String> {
    let parsed = url::Url::parse(url).map_err(|e| format!("unparseable image URL: {e}"))?;
    // Only http(s) URLs are honored — the response's `url` field is
    // provider-controlled text, and a `file://` (or any non-HTTP scheme)
    // value must never be dereferenced as one.
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(format!(
            "image URL uses an unsupported scheme: {}",
            parsed.scheme()
        ));
    }
    Ok(())
}

/// Whether a parsed IP literal falls in a range this adapter must never
/// fetch: loopback, private (RFC 1918 for v4, unique-local `fc00::/7` for
/// v6), or link-local (169.254/16 for v4, `fe80::/10` for v6).
fn is_blocked_ip_literal(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback() || v4.is_private() || v4.is_link_local(),
        IpAddr::V6(v6) => {
            v6.is_loopback()
                // `is_unique_local` is still unstable on std Ipv6Addr —
                // test the RFC 4193 prefix bits directly.
                || (u16::from_be_bytes([v6.octets()[0], v6.octets()[1]]) & 0xfe00) == 0xfc00
                // fe80::/10 link-local: the top 10 bits are 0xfe80..0xfebf.
                || (u16::from_be_bytes([v6.octets()[0], v6.octets()[1]]) & 0xffc0) == 0xfe80
                // IPv4-mapped (::ffff:10.0.0.1 etc.) — judge the embedded v4.
                || matches!(v6.to_ipv4_mapped(), Some(v4) if v4.is_loopback() || v4.is_private() || v4.is_link_local())
        }
    }
}

/// Images API path under the configured base URL (z.ai: the base already
/// ends at `/api/paas/v4`, so the composed URL is `/paas/v4/images/generations`).
const IMAGE_GENERATIONS_PATH: &str = "/images/generations";

/// z.ai chat accounts resolve to the documented standard PaaS base
/// (`https://api.z.ai/api/paas/v4` — see the overlay's `[provider.zai]`
/// base_url), so this rewrite is a NO-OP PASSTHROUGH for the default
/// configuration. It stays as a safety net for Coding-Plan subscribers
/// who override their account's base_url to the coding gateway
/// (`https://api.z.ai/api/coding/paas/v4`): the Images API is NOT served
/// under the `/coding` plan path — the docs pin it at
/// `https://api.z.ai/api/paas/v4`. Strip the `/coding` segment so the
/// image request lands on the plain PaaS base. The rewrite is a no-op
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
/// qualify), and the strings are identical to the OpenAI wire strings.
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
/// daemon dispatch keeps base_url/user_agent/slug shared), then overrides
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

    pub fn config(&self) -> &ServiceConfig {
        &self.config
    }

    /// Whether the SSRF IP-literal host guard may be RELAXED for this
    /// client's CDN downloads: true only when the daemon is itself
    /// configured against a loopback/private base (a local mock provider or
    /// dev proxy — recorded security decision: an operator who already aims
    /// the account at a private endpoint has made that trust decision, and
    /// such bases are exactly what the scripted wire tests and local
    /// proxies serve the download from; production bases are public
    /// hostnames, where the guard stays fully active).
    fn host_guard_relaxed(&self) -> bool {
        url::Url::parse(&self.config.base_url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_owned))
            .is_some_and(|host| {
                let host = host.trim_start_matches('[').trim_end_matches(']');
                host.parse::<IpAddr>().is_ok_and(is_blocked_ip_literal)
            })
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
        if let Some(size) = size_wire(req.size) {
            if let Some(obj) = body.as_object_mut() {
                obj.insert("size".into(), size.into());
            }
        }
        if let Some(quality) = quality_wire(req.quality) {
            if let Some(obj) = body.as_object_mut() {
                obj.insert("quality".into(), quality.into());
            }
        }
        body
    }

    /// Download the generated image bytes from the temporary CDN URL.
    ///
    /// No Authorization header (see the module docs: the URL is pre-signed
    /// and the API key must not travel to a third-party host), and the
    /// shared agent's `timeout_global` bounds each fetch under the same
    /// per-attempt budget as the generation POST — a slow CDN cannot escape
    /// the 180 s per-attempt deadline. The stream is read with a running cap
    /// ([`IMAGE_DOWNLOAD_CAP_BYTES`]) so a hostile/huge response fails at
    /// the cap instead of after a full multi-gigabyte read.
    ///
    /// The whole download gets its OWN small retry budget
    /// ([`IMAGE_DOWNLOAD_ATTEMPTS`]) separate from the generation POST's:
    /// z.ai's object storage advertises the URL in the generation response
    /// *before* the object is fully published, so an immediate follow-up
    /// GET can hit a propagation race and receive a non-image body (an
    /// error page or metadata served with a success status) instead of the
    /// bytes. Observed in production: the identical URL served an HTML-ish
    /// body on the first GET and a clean `image/png` seconds later (the
    /// CDN's `X-Ufile-Create-Time` confirms lazy materialization). A retry
    /// with the account's short initial backoff (~1-2 s typically) rides
    /// that race out well within the overall attempt deadline; a scheme
    /// violation, a cap overflow, or an empty body stays terminal — those
    /// cannot be fixed by waiting.
    fn download_image(
        &self,
        url: &str,
        cancel_rx: Option<&crossbeam_channel::Receiver<()>>,
    ) -> Result<Vec<u8>, OpenAiError> {
        // The loop keeps the not-ready detail of the attempt it is retrying;
        // the retry arm only fires while attempts remain, so every exit
        // path below returns a concrete error — no exhausted-loop
        // fallthrough and no Option bookkeeping needed.
        let mut attempt = 1;
        loop {
            match self.fetch_once(url) {
                Ok(bytes) => return Ok(bytes),
                // Only "the CDN answered but not with an image yet"
                // (NotReady from fetch_once's content-type / empty-body
                // guards) is retryable — that is the propagation race. Cap
                // overflow, scheme/host-guard violations, transport errors,
                // and the final attempt all return the error verbatim:
                // waiting cannot fix those.
                Err(OpenAiError::NotReady { detail }) if attempt < IMAGE_DOWNLOAD_ATTEMPTS => {
                    tracing::warn!(
                        url = %url,
                        attempt,
                        "z.ai image URL not yet published — retrying after backoff"
                    );
                    let wait =
                        std::time::Duration::from_millis(self.config.retry_initial_backoff_ms);
                    // sleep_or_cancel wakes instantly on a cancel (biased
                    // select) and errors on a dropped/disconnected channel —
                    // either way the wait is over and the racy fetch is
                    // nowhere near completing, so the saved not-ready error
                    // is the honest outcome.
                    if retry::sleep_or_cancel(wait, cancel_rx).is_err() {
                        tracing::warn!("z.ai image download retry wait aborted (cancel/close)");
                        return Err(OpenAiError::NotReady { detail });
                    }
                    attempt += 1;
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// One download fetch (URL guard → GET → content-type guard →
    /// capped stream read). Split from [`Self::download_image`] so the
    /// retry loop above can distinguish a *retryable* outcome —
    /// [`OpenAiError::NotReady`], meaning "the CDN answered but not with an
    /// image yet" — from terminal ones.
    fn fetch_once(&self, url: &str) -> Result<Vec<u8>, OpenAiError> {
        // Scheme + SSRF guard (see `is_downloadable_url`): a scheme
        // violation or a private/loopback IP-literal host is terminal — no
        // amount of waiting makes a hostile URL fetchable. The host half is
        // relaxed only for local-dev bases (mock providers serve the
        // download from loopback); the scheme half always applies.
        let guard = if self.host_guard_relaxed() {
            is_http_url(url)
        } else {
            is_downloadable_url(url)
        };
        if let Err(reason) = guard {
            tracing::warn!(url = %url, %reason, "z.ai image URL rejected by the download guard");
            return Err(OpenAiError::Io(io::Error::other(reason)));
        }
        tracing::debug!(url = %url, "downloading generated image from provider URL");
        let response = self
            .http
            .get(url)
            .call()
            .map_err(|e| OpenAiError::Io(io::Error::other(e)))?;

        // Loose content-type guard: the daemon's prepare pipeline validates
        // the bytes properly, but an obviously-wrong content type (an HTML
        // error page served with a 200 by the CDN — which happens transiently
        // while the object is still propagating, see download_image) is
        // cheap to reject here, before meaningful bytes are read.
        // `image/*` covers the JPEG/PNG/WebP payloads z.ai serves; an
        // octet-stream from a quirky proxy is let through deliberately
        // (bytes are validated downstream anyway). Mapped to NotReady rather
        // than a plain Io error so the download retry loop can treat this
        // specific outcome as retryable while a genuinely empty body stays
        // EmptyResponse (terminal).
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let mime_ok = content_type
            .as_deref()
            .map(|ct| ct.starts_with("image/"))
            .unwrap_or(true); // absent header → defer to byte-level validation
        if !mime_ok {
            tracing::warn!(
                url = %url,
                content_type = content_type.as_deref().unwrap_or(""),
                "z.ai image URL did not return an image content type"
            );
            return Err(OpenAiError::NotReady {
                detail: format!(
                    "CDN answered with a non-image content type ({}); the object is likely not published yet",
                    content_type.as_deref().unwrap_or(""),
                ),
            });
        }

        // Stream with the cap enforced *during* the read: a +1 reserve byte
        // lets us abort at the first chunk over the limit instead of
        // buffering the whole oversized body first.
        let mut reader = response.into_body().into_reader();
        let mut bytes = Vec::with_capacity(64 * 1024);
        let mut chunk = [0u8; 16 * 1024];
        loop {
            let n = reader
                .read(&mut chunk)
                .map_err(|e| OpenAiError::Io(io::Error::other(e)))?;
            if n == 0 {
                break;
            }
            if bytes.len() + n > IMAGE_DOWNLOAD_CAP_BYTES {
                return Err(OpenAiError::Io(io::Error::other(
                    "generated image exceeds the adapter's download cap",
                )));
            }
            // `read` returns n <= chunk.len() by contract, so the slice always
            // succeeds; the empty fallback is unreachable.
            let fresh = chunk.get(..n).unwrap_or(&[]);
            bytes.extend_from_slice(fresh);
        }
        if bytes.is_empty() {
            // An empty body with an image content type is the same
            // propagation race (the CDN started serving before writing) —
            // retryable, not the terminal "provider returned an empty
            // response" of the generation path.
            return Err(OpenAiError::NotReady {
                detail: "CDN returned an empty body for the image URL".to_string(),
            });
        }
        Ok(bytes)
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
    use super::{ZaiImageClient, image_base_url, is_blocked_ip_literal, is_downloadable_url};
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

    // ── SSRF download guard ─────────────────────────────────────────────

    #[test]
    fn guard_rejects_private_and_loopback_ip_literals() {
        for url in [
            "http://127.0.0.1/cdn/img.png",         // v4 loopback
            "http://10.1.2.3/cdn/img.png",          // RFC 1918 10/8
            "http://192.168.1.5/cdn/img.png",       // RFC 1918 192.168/16
            "http://169.254.7.7/cdn/img.png",       // v4 link-local
            "http://[::1]/cdn/img.png",             // v6 loopback (bracketed)
            "http://[fc00::1]/cdn/img.png",         // v6 unique-local (fc00::/7)
            "http://[fd12:3456::a]/cdn/img.png",    // v6 unique-local (fd00::/8)
            "http://[fe80::1]/cdn/img.png",         // v6 link-local
            "http://[::ffff:10.0.0.9]/cdn/img.png", // IPv4-mapped private
        ] {
            assert!(
                is_downloadable_url(url).is_err(),
                "{url} must be rejected by the download guard"
            );
        }
    }

    #[test]
    fn guard_accepts_public_hosts_and_hostnames() {
        // The real z.ai CDN hostname (non-IP hostnames are allowed — see the
        // recorded security decision on is_downloadable_url) plus public IP
        // literals and the https scheme.
        for url in [
            "https://mfile.z.ai/cdn/img/generated.png",
            "http://example.com/cdn/img.png",
            "http://8.8.8.8/cdn/img.png",
            "https://[2606:4700::1111]/cdn/img.png",
            "https://[::ffff:8.8.8.8]/cdn/img.png", // IPv4-mapped public
        ] {
            assert!(
                is_downloadable_url(url).is_ok(),
                "{url} must be accepted by the download guard"
            );
        }
    }

    #[test]
    fn guard_rejects_non_http_schemes_and_garbage() {
        for url in [
            "file:///etc/passwd",
            "ftp://example.com/img.png",
            "data:image/png;base64,aGk=",
            "not a url at all",
        ] {
            assert!(
                is_downloadable_url(url).is_err(),
                "{url} must be rejected by the download guard"
            );
        }
    }

    #[test]
    fn ip_literal_classifier_covers_the_documented_ranges() {
        // Direct coverage of the classifier behind the URL guard, so a
        // regression in one range cannot hide behind URL-parse behavior.
        for blocked in [
            "127.0.0.1",
            "10.0.0.1",
            "172.16.0.1",
            "192.168.0.1",
            "169.254.0.1",
        ] {
            assert!(
                is_blocked_ip_literal(blocked.parse().unwrap()),
                "{blocked} must be classified blocked"
            );
        }
        for allowed in ["8.8.8.8", "1.1.1.1", "203.0.113.9"] {
            assert!(
                !is_blocked_ip_literal(allowed.parse().unwrap()),
                "{allowed} must be classified allowed"
            );
        }
        // fc00::/7: both fc and fd prefixes, and fe80::/10 link-local.
        assert!(is_blocked_ip_literal("fc00::1".parse().unwrap()));
        assert!(is_blocked_ip_literal("fd00::1".parse().unwrap()));
        assert!(is_blocked_ip_literal("fe80::1".parse().unwrap()));
        assert!(!is_blocked_ip_literal("2606:4700::1111".parse().unwrap()));
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
