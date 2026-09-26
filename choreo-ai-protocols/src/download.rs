//! Shared media-download machinery for URL-returning adapters.
//!
//! Both fal.ai's image models (`fal.media` link, or — with `sync_mode` — a
//! `data:` URI) and z.ai's glm-image (a temporary, 30-day-expiring CDN link)
//! return the generated artifact as a **URL** the adapter must fetch itself
//! rather than as inline base64, and fal's video queue returns a URL-only
//! result too. The download half they all share lives here so the SSRF
//! guards, the capped/retried/cancellable fetch, and the
//! [`ProviderError::NotReady`]-versus-terminal distinction cannot drift
//! between adapters:
//!
//! - **Scheme + SSRF guard** ([`is_downloadable_url`], [`is_http_url`],
//!   [`is_blocked_ip_literal`]) — the response's `url` field is
//!   provider-controlled text, so it must never be dereferenced as
//!   `file://`/etc., and an IP-literal host in a loopback/private/link-local
//!   range is refused (the URL arrives over an authenticated provider TLS
//!   channel, not from user input, so the residual hostname risk is an
//!   accepted decision — see [`is_downloadable_url`]). The guard only sees the
//!   URL we hand it, so the download request disables redirect **following**
//!   ([`fetch_once`]): otherwise a provider-controlled 3xx would send the fetch
//!   to an internal address the guard never inspected.
//! - **The capped/retried/cancellable fetch** ([`download_media_bytes`]) —
//!   a caller-supplied in-stream byte ceiling, a small caller-supplied retry
//!   budget that rides out a CDN propagation race, and cancellation between
//!   attempts. [`download_image_bytes`] is the thin image wrapper (8 MiB cap,
//!   the image download-attempt budget); a future video download passes a
//!   larger cap and the `video/` content-type prefix.
//!
//! The host half of the guard is RELAXED for a client whose own configured
//! base is a loopback/private host ([`host_guard_relaxed`]) — a local mock
//! provider or dev proxy serves its downloads from loopback, and an operator
//! who already aimed the account at a private endpoint has made that trust
//! decision.

use std::io;
use std::io::Read as _;
use std::net::IpAddr;

use crate::images::{IMAGE_DOWNLOAD_ATTEMPTS, IMAGE_DOWNLOAD_CAP_BYTES};
use crate::openai::ServiceConfig;
use crate::retry;
use crate::shared::ProviderError;

/// Reject URLs whose host is an IP literal in a range that must never be
/// dereferenced from provider-controlled response data (SSRF guard).
///
/// Blocks loopback (127/8, `::1`), private (RFC 1918, RFC 4193 `fc00::/7`),
/// and link-local (169.254/16, `fe80::/10`) addresses. Non-IP hostnames
/// (e.g. `mfile.z.ai`, `fal.media`) are ALLOWED — recorded security decision:
/// the URL's hostname is provider-controlled, but DNS pinning for arbitrary
/// provider hosts is out of scope here; the residual risk is accepted
/// because (a) the URL arrives over the authenticated provider TLS channel,
/// not from user input, and (b) the downloaded bytes are fully validated
/// downstream by the daemon's prepare pipeline (magic bytes, size
/// cap, decode), so a malicious host can at worst waste the download — it
/// cannot smuggle content into a session.
///
/// Returns `Err` with a human-readable reason (surfaced as the error detail).
pub(crate) fn is_downloadable_url(url: &str) -> Result<(), String> {
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
            "media URL host is a private/loopback/link-local IP literal, refusing to fetch: {url}"
        ));
    }
    Ok(())
}

/// Scheme-only half of the download guard (http/https). Used standalone when
/// the host guard is relaxed for local-dev bases (see [`host_guard_relaxed`]).
pub(crate) fn is_http_url(url: &str) -> Result<(), String> {
    let parsed = url::Url::parse(url).map_err(|e| format!("unparseable media URL: {e}"))?;
    // Only http(s) URLs are honored — the response's `url` field is
    // provider-controlled text, and a `file://` (or any non-HTTP scheme)
    // value must never be dereferenced as one.
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(format!(
            "media URL uses an unsupported scheme: {}",
            parsed.scheme()
        ));
    }
    Ok(())
}

/// Whether a parsed IP literal falls in a range this adapter must never
/// fetch: loopback, private (RFC 1918 for v4, unique-local `fc00::/7` for
/// v6), or link-local (169.254/16 for v4, `fe80::/10` for v6).
pub(crate) fn is_blocked_ip_literal(ip: IpAddr) -> bool {
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

/// Whether the SSRF IP-literal host guard may be RELAXED for this client's
/// CDN downloads: true only when the daemon is itself configured against a
/// loopback/private base (a local mock provider or dev proxy — recorded
/// security decision: an operator who already aims the account at a private
/// endpoint has made that trust decision, and such bases are exactly what
/// the scripted wire tests and local proxies serve the download from;
/// production bases are public hostnames, where the guard stays fully
/// active).
pub(crate) fn host_guard_relaxed(base_url: &str) -> bool {
    url::Url::parse(base_url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_owned))
        .is_some_and(|host| {
            let host = host.trim_start_matches('[').trim_end_matches(']');
            host.parse::<IpAddr>().is_ok_and(is_blocked_ip_literal)
        })
}

/// Download the generated **image** bytes from a provider-returned URL.
///
/// Thin wrapper over [`download_media_bytes`] preserving the image adapters'
/// long-standing policy: an `image/` content-type guard, the 8 MiB
/// [`IMAGE_DOWNLOAD_CAP_BYTES`] ceiling, and the
/// [`IMAGE_DOWNLOAD_ATTEMPTS`]-attempt budget that rides out a CDN
/// propagation race.
pub(crate) fn download_image_bytes(
    http: &ureq::Agent,
    config: &ServiceConfig,
    url: &str,
    cancel_rx: Option<&crossbeam_channel::Receiver<()>>,
) -> Result<Vec<u8>, ProviderError> {
    download_media_bytes(
        http,
        config,
        url,
        "image/",
        IMAGE_DOWNLOAD_CAP_BYTES,
        IMAGE_DOWNLOAD_ATTEMPTS,
        cancel_rx,
    )
}

/// Download the generated media bytes from a provider-returned URL.
///
/// No Authorization header (the URL is pre-signed and the API key must not
/// travel to a third-party host), and the shared agent's `timeout_global`
/// bounds each fetch under the same per-attempt budget as the generation
/// POST — a slow CDN cannot escape the adapter's per-attempt deadline. The
/// stream is read with a running `cap` so a hostile/huge response fails at
/// the cap instead of after a full multi-gigabyte read.
///
/// `content_type_prefix` is the content-type family the caller expects (e.g.
/// `"image/"`, `"video/"`); a response whose content type does not start with
/// it is treated as the CDN still materializing the object (retryable), not a
/// hard failure.
///
/// `attempts` is the caller's own small retry budget. z.ai's object storage
/// advertises the URL in the generation response *before* the object is fully
/// published, so an immediate follow-up GET can hit a propagation race and
/// receive a non-media body (an error page or metadata served with a success
/// status) instead of the bytes — the same race fal's CDN can exhibit. A
/// retry with the account's short initial backoff rides that race out well
/// within the overall attempt deadline; a scheme violation, a cap overflow,
/// or an empty body stays terminal — those cannot be fixed by waiting.
///
/// # Errors
///
/// Returns [`ProviderError`] on guard violation, transport error, the download
/// cap, or (after the retry budget) a not-yet-published artifact.
pub(crate) fn download_media_bytes(
    http: &ureq::Agent,
    config: &ServiceConfig,
    url: &str,
    content_type_prefix: &str,
    cap: usize,
    attempts: u32,
    cancel_rx: Option<&crossbeam_channel::Receiver<()>>,
) -> Result<Vec<u8>, ProviderError> {
    // The loop keeps the not-ready detail of the attempt it is retrying; the
    // retry arm only fires while attempts remain, so every exit path below
    // returns a concrete error — no exhausted-loop fallthrough and no Option
    // bookkeeping needed.
    let mut attempt = 1;
    loop {
        match fetch_once(http, &config.base_url, url, content_type_prefix, cap) {
            Ok(bytes) => return Ok(bytes),
            // Only "the CDN answered but not with the expected media yet"
            // (NotReady from fetch_once's content-type / empty-body guards) is
            // retryable — that is the propagation race. Cap overflow,
            // scheme/host-guard violations, transport errors, and the final
            // attempt all return the error verbatim: waiting cannot fix
            // those.
            Err(ProviderError::NotReady { detail }) if attempt < attempts => {
                tracing::warn!(
                    url = %url,
                    attempt,
                    "media URL not yet published — retrying after backoff"
                );
                let wait = std::time::Duration::from_millis(config.retry_initial_backoff_ms);
                // sleep_or_cancel wakes instantly on a cancel (biased select)
                // and errors on a dropped/disconnected channel — either way
                // the wait is over and the racy fetch is nowhere near
                // completing, so the saved not-ready error is the honest
                // outcome.
                if retry::sleep_or_cancel(wait, cancel_rx).is_err() {
                    tracing::warn!("media download retry wait aborted (cancel/close)");
                    return Err(ProviderError::NotReady { detail });
                }
                attempt += 1;
            }
            Err(e) => return Err(e),
        }
    }
}

/// Whether a media response's content type satisfies the caller's expected
/// family `prefix` (e.g. `"image/"`, `"video/"`). An ABSENT header defers to
/// the daemon's byte-level validation downstream and is allowed.
fn content_type_allowed(prefix: &str, content_type: Option<&str>) -> bool {
    content_type.is_none_or(|ct| ct.starts_with(prefix))
}

/// The human-readable detail for a content-type mismatch: `"non-image content
/// type (text/html); the object is likely not published yet"` (the family is
/// the prefix with its trailing slash stripped) so the image and video paths
/// read naturally while sharing one implementation.
fn content_type_mismatch_detail(prefix: &str, content_type: Option<&str>) -> String {
    format!(
        "non-{} content type ({}); the object is likely not published yet",
        prefix.trim_end_matches('/'),
        content_type.unwrap_or("")
    )
}

/// One download fetch (URL guard → GET → status guard → content-type guard →
/// capped stream read). Split from [`download_media_bytes`] so the retry loop
/// above can distinguish a *retryable* outcome — [`ProviderError::NotReady`],
/// meaning "the CDN answered but not with the expected media yet" — from
/// terminal ones.
fn fetch_once(
    http: &ureq::Agent,
    base_url: &str,
    url: &str,
    content_type_prefix: &str,
    cap: usize,
) -> Result<Vec<u8>, ProviderError> {
    // Scheme + SSRF guard (see `is_downloadable_url`): a scheme violation or
    // a private/loopback IP-literal host is terminal — no amount of waiting
    // makes a hostile URL fetchable. The host half is relaxed only for
    // local-dev bases (mock providers serve the download from loopback); the
    // scheme half always applies.
    let guard = if host_guard_relaxed(base_url) {
        is_http_url(url)
    } else {
        is_downloadable_url(url)
    };
    if let Err(reason) = guard {
        tracing::warn!(url = %url, %reason, "media URL rejected by the download guard");
        return Err(ProviderError::Io(io::Error::other(reason)));
    }
    tracing::debug!(url = %url, "downloading generated media from provider URL");
    // Redirects are DISABLED for the download: the SSRF guard above validated
    // only THIS url, so honouring a provider-controlled 3xx could send the
    // fetch to an internal address the guard never saw (a classic guard
    // bypass). With `max_redirects(0)` ureq returns the 3xx response verbatim
    // instead of following it, and the status check below rejects it like any
    // other non-2xx.
    let response = http
        .get(url)
        .config()
        .max_redirects(0)
        .build()
        .call()
        .map_err(|e| ProviderError::Io(io::Error::other(e)))?;

    // Status guard: only a 2xx can carry the artifact. A non-2xx means the CDN
    // is not (yet) serving the object — most often the propagation race, or a
    // 3xx the redirect policy above declined to follow. Surfaced as retryable
    // `NotReady` (never read as bytes) so the CDN race is still ridden out,
    // while a genuinely absent object fails cleanly after the retry budget
    // instead of being mistaken for a body.
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        tracing::warn!(url = %url, status, "media URL returned a non-success status");
        return Err(ProviderError::NotReady {
            detail: format!(
                "CDN answered with status {status}; the object is likely not published yet"
            ),
        });
    }

    // Loose content-type guard: the daemon's prepare pipeline validates the
    // bytes properly, but an obviously-wrong content type (an HTML error page
    // served with a 200 by the CDN — which happens transiently while the
    // object is still propagating, see download_media_bytes) is cheap to
    // reject here, before meaningful bytes are read. `content_type_prefix`
    // covers the payloads the providers serve (e.g. `image/*`, `video/*`); an
    // octet-stream from a quirky proxy is let through deliberately (bytes are
    // validated downstream anyway). Mapped to NotReady rather than a plain Io
    // error so the download retry loop can treat this specific outcome as
    // retryable while a genuinely empty body stays EmptyResponse (terminal).
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    if !content_type_allowed(content_type_prefix, content_type.as_deref()) {
        tracing::warn!(
            url = %url,
            content_type = content_type.as_deref().unwrap_or(""),
            "media URL did not return the expected content type"
        );
        return Err(ProviderError::NotReady {
            detail: format!(
                "CDN answered with a {}",
                content_type_mismatch_detail(content_type_prefix, content_type.as_deref()),
            ),
        });
    }

    // Stream with the cap enforced *during* the read: a +1 reserve byte lets
    // us abort at the first chunk over the limit instead of buffering the
    // whole oversized body first.
    let mut reader = response.into_body().into_reader();
    let mut bytes = Vec::with_capacity(64 * 1024);
    let mut chunk = [0u8; 16 * 1024];
    loop {
        let n = reader
            .read(&mut chunk)
            .map_err(|e| ProviderError::Io(io::Error::other(e)))?;
        if n == 0 {
            break;
        }
        if bytes.len() + n > cap {
            return Err(ProviderError::Io(io::Error::other(
                "generated media exceeds the adapter's download cap",
            )));
        }
        bytes.extend_from_slice(crate::shared::read_slice(&chunk, n));
    }
    if bytes.is_empty() {
        // An empty body with a matching content type is the same propagation
        // race (the CDN started serving before writing) — retryable, not the
        // terminal "provider returned an empty response" of the generation
        // path.
        return Err(ProviderError::NotReady {
            detail: "CDN returned an empty body for the media URL".to_string(),
        });
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::{
        content_type_allowed, content_type_mismatch_detail, is_blocked_ip_literal,
        is_downloadable_url,
    };

    // ── content-type guards (shared image/video path) ───────────────────

    #[test]
    fn content_type_allowed_matches_the_caller_prefix_and_defers_on_absent() {
        // A matching family passes; a mismatched one is rejected; an absent
        // header defers to downstream byte validation (allowed).
        assert!(content_type_allowed("image/", Some("image/png")));
        assert!(content_type_allowed("video/", Some("video/mp4")));
        assert!(!content_type_allowed("image/", Some("text/html")));
        assert!(!content_type_allowed("video/", Some("image/png")));
        assert!(content_type_allowed("image/", None));
    }

    #[test]
    fn content_type_mismatch_detail_is_derived_from_the_prefix() {
        assert_eq!(
            content_type_mismatch_detail("image/", Some("text/html")),
            "non-image content type (text/html); the object is likely not published yet"
        );
        assert_eq!(
            content_type_mismatch_detail("video/", None),
            "non-video content type (); the object is likely not published yet"
        );
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
        // The real provider CDN hostnames (non-IP hostnames are allowed — see
        // the recorded security decision on is_downloadable_url) plus public
        // IP literals and the https scheme.
        for url in [
            "https://mfile.z.ai/cdn/img/generated.png",
            "https://fal.media/files/flux/out.png",
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
}
