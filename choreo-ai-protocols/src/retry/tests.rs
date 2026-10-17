//! Unit tests for the retry/backoff machinery and the send-body upload hook.

use super::send_body::UploadHookReader;
use super::*;
use std::io::Read;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

fn test_config() -> RetryConfig {
    RetryConfig {
        max_attempts: 5,
        initial_backoff_ms: 1000,
        max_backoff_ms: 30000,
    }
}

// The Retry-After path is deterministic (no jitter), so exact equality is
// fine there.  The backoff path applies 0.75..=1.25 jitter, so those tests
// assert bounds instead.

// ── retry_decision (merged budget gate + delay) ────────────────────

#[test]
fn retry_decision_huge_retry_after_is_terminal() {
    // A malicious/broken provider could send Retry-After: u64::MAX; the
    // merged gate+delay treats it as terminal — ~584bn years never becomes a
    // wait, because the budget ceiling rejects the cooldown outright instead
    // of capping it into a (still enormous) sleep.
    let retry = test_config();
    assert_eq!(retry_decision(429, Some(u64::MAX), 1, &retry), None);
}

#[test]
fn retry_decision_large_but_plausible_retry_after_is_terminal() {
    // A large but plausible header value (≈11.5 days) also exceeds the
    // 30 s ceiling → terminal, no capping guesswork.
    let retry = test_config();
    assert_eq!(retry_decision(429, Some(1_000_000), 1, &retry), None);
}

#[test]
fn retry_decision_in_budget_retry_after_passes_through() {
    // A provider-honoured value at or below the ceiling is used verbatim.
    let retry = test_config();
    assert_eq!(
        retry_decision(429, Some(2), 1, &retry),
        Some(Duration::from_secs(2))
    );
    assert_eq!(
        retry_decision(429, Some(30), 1, &retry),
        Some(Duration::from_secs(30))
    );
    assert_eq!(
        retry_decision(429, Some(0), 1, &retry),
        Some(Duration::ZERO)
    );
}

#[test]
fn retry_decision_absent_retry_after_falls_back_to_backoff() {
    // 429 without a Retry-After header → exponential backoff (jittered).
    let retry = test_config();
    let delay = retry_decision(429, None, 1, &retry).expect("backoff fallback");
    let millis = delay.as_millis() as f64;
    let base = retry.initial_backoff_ms as f64;
    assert!(millis >= base * 0.75, "delay {millis}ms below jitter floor");
    assert!(
        millis <= base * 1.25,
        "delay {millis}ms above jitter ceiling"
    );
}

#[test]
fn retry_decision_statuses_without_defined_retry_after_back_off() {
    // 500/502/504 have no Retry-After per RFC 7231 (it is only defined
    // for 429 and 503); a header on them — even a huge one — must not
    // influence the decision, so the request still backs off
    // exponentially (jittered).
    let retry = test_config();
    for status in [500, 502, 504] {
        let delay = retry_decision(status, Some(u64::MAX), 1, &retry)
            .unwrap_or_else(|| panic!("{status} must back off, got None"));
        let millis = delay.as_millis() as f64;
        let base = retry.initial_backoff_ms as f64;
        assert!(millis >= base * 0.75, "{status}: below jitter floor");
        assert!(millis <= base * 1.25, "{status}: above jitter ceiling");
    }
    // Even a plausible-looking header on a 504 is ignored (500 vs 504
    // makes no difference — neither honors the header).
    let delay = retry_decision(504, Some(2), 1, &retry).expect("504 must back off");
    let millis = delay.as_millis() as f64;
    let base = retry.initial_backoff_ms as f64;
    assert!(millis >= base * 0.75);
    assert!(millis <= base * 1.25);
}

#[test]
fn retry_decision_503_behaves_exactly_like_429() {
    // RFC 7231 defines Retry-After for 503 just as it does for 429:
    // in-budget waited verbatim, beyond the ceiling terminal.
    let retry = test_config();
    assert_eq!(
        retry_decision(503, Some(2), 1, &retry),
        Some(Duration::from_secs(2))
    );
    assert_eq!(retry_decision(503, Some(31), 1, &retry), None);
    assert_eq!(retry_decision(503, Some(172_800), 1, &retry), None);
}

// ── RetryConfig::new clamping ─────────────────────────────────────

#[test]
fn retry_config_new_clamps_max_backoff_to_the_ceiling() {
    // u64::MAX ms can never become a field of a config built through the
    // documented constructor.
    let retry = RetryConfig::new(5, 1000, u64::MAX);
    assert_eq!(retry.max_backoff_ms, MAX_BACKOFF_MS);
    assert_eq!(retry.initial_backoff_ms, 1000);
    // A default-range value is preserved untouched.
    let retry = RetryConfig::new(5, 1000, 30000);
    assert_eq!(retry.max_backoff_ms, 30000);
}

#[test]
fn retry_config_new_never_allows_max_below_initial() {
    // initial > max (a common typo) would collapse the exponential
    // backoff to its cap on the first attempt; the constructor lifts max
    // up to initial instead of carrying the contradiction forward.
    let retry = RetryConfig::new(5, 2000, 1000);
    assert_eq!(retry.max_backoff_ms, 2000);
    assert_eq!(retry.initial_backoff_ms, 2000);
}

#[test]
fn retry_config_new_clamps_initial_backoff_too() {
    // A pathological initial backoff (u64::MAX ms) is capped to the same
    // ceiling so the first exponential step cannot exceed it either.
    let retry = RetryConfig::new(5, u64::MAX, u64::MAX);
    assert_eq!(retry.initial_backoff_ms, MAX_BACKOFF_MS);
    assert_eq!(retry.max_backoff_ms, MAX_BACKOFF_MS);
}

#[test]
fn with_no_retry_collapses_to_a_single_attempt() {
    // `no_retry` forces exactly one attempt (the retry loop only resends
    // while `attempt < max_attempts`), while the backoff values are kept
    // intact so the config stays a valid, consistently-built value.
    let retry = RetryConfig::new(5, 1000, 30000).with_no_retry(true);
    assert_eq!(retry.max_attempts, 1);
    assert_eq!(retry.initial_backoff_ms, 1000);
    assert_eq!(retry.max_backoff_ms, 30000);
}

#[test]
fn with_no_retry_false_leaves_the_config_unchanged() {
    // The default path: `no_retry == false` must not touch the config, so
    // ordinary turns keep their full retry budget.
    let retry = RetryConfig::new(5, 1000, 30000).with_no_retry(false);
    assert_eq!(retry.max_attempts, 5);
}

#[test]
fn retry_decision_budget_cannot_be_voided_by_config() {
    // A config built through RetryConfig::new clamps max_backoff_ms to
    // MAX_BACKOFF_MS, so a Retry-After just past the hard ceiling is
    // terminal even though the raw config asked for an unbounded ceiling.
    let retry = RetryConfig::new(5, 1000, u64::MAX); // clamped to 1 h
    let ceiling_secs = MAX_BACKOFF_MS / 1000; // the 1 h ceiling in Retry-After units (s)
    assert_eq!(
        retry_decision(429, Some(ceiling_secs), 1, &retry),
        Some(Duration::from_secs(ceiling_secs)),
        "exactly-at-ceiling Retry-After is in budget"
    );
    assert_eq!(
        retry_decision(429, Some(ceiling_secs + 1), 1, &retry),
        None,
        "a Retry-After 1 s past the ceiling outlives the hard ceiling"
    );
}

#[test]
fn retry_decision_gate_cannot_be_voided_by_hand_built_config() {
    // The RetryConfig fields are pub, so a struct literal bypasses the
    // RetryConfig::new clamp.  The gate must clamp to MAX_BACKOFF_MS at
    // compare time itself — otherwise a u64::MAX ceiling would make every
    // provider cooldown "fit" and silently void the fail-fast property
    // (a 2-day Retry-After would be retried, sleeping 1 h between
    // attempts, instead of failing on the first).
    let retry = RetryConfig {
        max_attempts: 5,
        initial_backoff_ms: 1000,
        max_backoff_ms: u64::MAX, // raw struct literal — no constructor clamp
    };
    let ceiling_secs = MAX_BACKOFF_MS / 1000; // 1 h in Retry-After units
    assert_eq!(
        retry_decision(429, Some(ceiling_secs), 1, &retry),
        Some(Duration::from_secs(ceiling_secs)),
        "exactly-at-ceiling Retry-After is still in budget"
    );
    assert_eq!(
        retry_decision(429, Some(ceiling_secs + 1), 1, &retry),
        None,
        "a Retry-After past the hard ceiling is terminal even for a u64::MAX config"
    );
    assert_eq!(
        retry_decision(503, Some(u64::MAX), 1, &retry),
        None,
        "503 honors the same unvoidable ceiling"
    );
}

#[test]
fn wait_before_retry_caps_delay_beyond_the_ceiling() {
    // Layer-2 backstop: a RetryConfig constructed with a raw struct
    // literal (pub fields bypass RetryConfig::new) could still hand a
    // huge delay to wait_before_retry; the cap must apply even then.  A
    // cancel is pre-queued so the wait resolves instantly instead of
    // sleeping (event-driven — no time-based wait in this unit test).
    let (cancel_tx, cancel_rx) = crossbeam_channel::unbounded::<()>();
    cancel_tx.send(()).ok();
    let observed = Arc::new(AtomicU64::new(0));
    let seen = observed.clone();
    let mut cb: Option<RetryCallback> = Some(Box::new(move |_attempt, _max, delay| {
        seen.store(delay.as_millis() as u64, Ordering::Relaxed);
    }));
    let result = wait_before_retry(
        1,
        5,
        Duration::from_millis(u64::MAX),
        &mut cb,
        Some(&cancel_rx),
    );
    assert!(matches!(result, Err(ProviderHttpError::Cancelled)));
    assert_eq!(observed.load(Ordering::Relaxed), MAX_BACKOFF_MS);
}

// ── upload hook (the daemon's `Waiting` boundary) ───────────────────

/// A counting `RequestUploadedCallback` plus the counter it increments, so
/// a test can assert exactly when the hook fires.
fn counting_hook() -> (RequestUploadedCallback, Arc<AtomicU32>) {
    let count = Arc::new(AtomicU32::new(0));
    let seen = count.clone();
    let hook: RequestUploadedCallback = Arc::new(move || {
        seen.fetch_add(1, Ordering::Relaxed);
    });
    (hook, count)
}

#[test]
fn upload_hook_fires_once_on_the_last_byte_not_on_eof() {
    // The hook must fire exactly when the payload's final byte is read, and
    // not on a later zero-length read: ureq's length-delimited send loop
    // stops as soon as `Content-Length` is satisfied, so it never issues a
    // terminating `read` that returns 0.
    let (hook, count) = counting_hook();
    let mut reader = UploadHookReader::new(b"hello".to_vec().into(), Some(hook));
    let mut buf = [0u8; 2];
    assert_eq!(reader.read(&mut buf).expect("read"), 2);
    assert_eq!(count.load(Ordering::Relaxed), 0, "not yet at the end");
    assert_eq!(reader.read(&mut buf).expect("read"), 2);
    assert_eq!(count.load(Ordering::Relaxed), 0, "not yet at the end");
    assert_eq!(reader.read(&mut buf).expect("read"), 1);
    assert_eq!(count.load(Ordering::Relaxed), 1, "fires on the last byte");
    // A subsequent EOF read does not re-fire (the hook is taken once).
    assert_eq!(reader.read(&mut buf).expect("read"), 0);
    assert_eq!(count.load(Ordering::Relaxed), 1);
}

#[test]
fn upload_hook_does_not_fire_before_the_payload_is_exhausted() {
    // If a caller abandons the read (e.g. the request is dropped mid-send),
    // the payload was not fully sent, so the hook must not fire.
    let (hook, count) = counting_hook();
    let mut reader = UploadHookReader::new(vec![0u8; 10].into(), Some(hook));
    let mut buf = [0u8; 4];
    assert_eq!(reader.read(&mut buf).expect("read"), 4);
    assert_eq!(count.load(Ordering::Relaxed), 0);
}

#[test]
fn upload_hook_does_not_fire_on_an_empty_payload() {
    // An empty payload has no final byte to fire on, so the hook must stay
    // silent rather than report an upload that sent nothing.
    let (hook, count) = counting_hook();
    let mut reader = UploadHookReader::new(Vec::new().into(), Some(hook));
    let mut buf = [0u8; 4];
    assert_eq!(reader.read(&mut buf).expect("read"), 0);
    assert_eq!(count.load(Ordering::Relaxed), 0);
}

#[test]
fn hooked_json_body_is_compact_and_matches_the_serialized_bytes() {
    // The reader must yield exactly the bytes whose length the body declares as
    // `Content-Length` (a declared length that disagreed would spin ureq's
    // length-delimited send loop), and the payload is compact JSON (no
    // pretty-print whitespace) — smaller uploads.
    let value = serde_json::json!({"a": 1, "b": [1, 2]});
    let body = HookedJsonBody::new(&value, None, RequestCompression::Identity).expect("serialize");
    let mut wire = Vec::new();
    body.send_body()
        .into_reader()
        .read_to_end(&mut wire)
        .expect("read the send body");
    assert_eq!(wire, serde_json::to_vec(&value).expect("serialize"));
    assert!(!wire.contains(&b' '), "payload must be compact");
}

#[test]
fn hooked_json_body_gzips_and_round_trips_when_requested() {
    // With gzip requested, the reader yields the compressed stream, which must
    // gunzip back to exactly the compact JSON a provider would parse.
    let value = serde_json::json!({"a": 1, "b": [1, 2]});
    let body = HookedJsonBody::new(&value, None, RequestCompression::Gzip).expect("serialize");
    assert!(body.is_compressed());
    let mut wire = Vec::new();
    body.send_body()
        .into_reader()
        .read_to_end(&mut wire)
        .expect("read the send body");
    let mut decoded = Vec::new();
    flate2::read::GzDecoder::new(&wire[..])
        .read_to_end(&mut decoded)
        .expect("gunzip the send body");
    assert_eq!(decoded, serde_json::to_vec(&value).expect("serialize"));
}

#[test]
fn hooked_json_body_downgrade_falls_back_to_plain() {
    // A 415 downgrade drops gzip; the reader must then yield the retained plain
    // bytes (so the provider gets a body it can decode).
    let value = serde_json::json!({"hello": "world"});
    let body = HookedJsonBody::new(&value, None, RequestCompression::Gzip).expect("serialize");
    assert!(body.is_compressed());
    body.downgrade();
    assert!(!body.is_compressed());
    let mut wire = Vec::new();
    body.send_body()
        .into_reader()
        .read_to_end(&mut wire)
        .expect("read the send body");
    assert_eq!(wire, serde_json::to_vec(&value).expect("serialize"));
}

#[test]
fn hooked_json_body_zstd_compresses_and_round_trips_when_requested() {
    // With zstd requested, the reader yields a standard zstd frame that decodes
    // back to exactly the compact JSON a provider would parse.
    let value = serde_json::json!({"a": 1, "b": [1, 2]});
    let body = HookedJsonBody::new(&value, None, RequestCompression::Zstd).expect("serialize");
    assert!(body.is_compressed());
    let mut wire = Vec::new();
    body.send_body()
        .into_reader()
        .read_to_end(&mut wire)
        .expect("read the send body");
    // The frame must carry the standard zstd magic (0xFD2FB528, little-endian).
    assert_eq!(wire.get(..4), Some([0x28, 0xB5, 0x2F, 0xFD].as_slice()));
    let mut decoder =
        structured_zstd::decoding::StreamingDecoder::new(std::io::Cursor::new(&wire[..]));
    let mut decoded = Vec::new();
    decoder
        .read_to_end(&mut decoded)
        .expect("decode the send body");
    assert_eq!(decoded, serde_json::to_vec(&value).expect("serialize"));
}

#[test]
fn hooked_json_body_downgrade_from_zstd_falls_back_to_plain() {
    // A 415 downgrade drops zstd the same as gzip; the reader must then yield
    // the retained plain bytes.
    let value = serde_json::json!({"hello": "world"});
    let body = HookedJsonBody::new(&value, None, RequestCompression::Zstd).expect("serialize");
    assert!(body.is_compressed());
    body.downgrade();
    assert!(!body.is_compressed());
    let mut wire = Vec::new();
    body.send_body()
        .into_reader()
        .read_to_end(&mut wire)
        .expect("read the send body");
    assert_eq!(wire, serde_json::to_vec(&value).expect("serialize"));
}

// ── parse_retry_after_secs (HTTP-date form) ───────────────────────

#[test]
fn parse_retry_after_rejects_unknown_values() {
    // Absent, empty, whitespace-only, and non-date garbage stay None
    // (the caller falls back to exponential backoff) — a malformed
    // header is never guessed at.
    assert_eq!(parse_retry_after_secs(None), None);
    assert_eq!(parse_retry_after_secs(Some("")), None);
    assert_eq!(parse_retry_after_secs(Some("  \t ")), None);
    assert_eq!(parse_retry_after_secs(Some("abc")), None);
}

#[test]
fn parse_retry_after_http_date_in_the_past_means_retry_now() {
    // An HTTP-date already in the past floors to 0 ("retry now") — never
    // None, and never a negative wait.
    let secs = parse_retry_after_secs(Some("Wed, 21 Oct 2015 07:28:00 GMT"))
        .expect("IMF-fixdate must parse");
    assert_eq!(secs, 0, "past date must floor to 0, got {secs}");
}

#[test]
fn parse_retry_after_http_date_in_the_future_returns_remaining_seconds() {
    // An HTTP-date well in the future maps to the remaining whole
    // seconds — far longer than any backoff window we use.
    let secs = parse_retry_after_secs(Some("Wed, 21 Oct 2099 07:28:00 GMT"))
        .expect("IMF-fixdate must parse");
    assert!(secs > 365 * 24 * 60 * 60, "delta was only {secs}s");
}

#[test]
fn parse_retry_after_http_date_formats() {
    // The exact IMF-fixdate form servers are required to send per RFC
    // 7231 must parse, as must whitespace-padded delta-seconds.
    assert!(parse_retry_after_secs(Some("Sun, 06 Nov 1994 08:49:37 GMT")).is_some());
    assert!(parse_retry_after_secs(Some("  120  ")).is_some());
}

// ── Retry-After budget (probes retry_decision boundaries) ─────────

#[test]
fn retry_decision_two_day_cooldown_is_terminal_on_429() {
    // The motivating case for the budget gate: a 429 with a "resets in
    // two days" Retry-After must fail now, not burn attempts the
    // cooldown can never clear within the budget.  The other boundaries
    // (exactly-at-ceiling, just-past-ceiling, header-free backoff) are
    // pinned by the retry_decision tests above.
    let retry = test_config(); // max_backoff_ms = 30000 → 30 s ceiling
    assert_eq!(retry_decision(429, Some(172_800), 1, &retry), None); // 2 days
}

// ── extract_error_message ────────────────────────────────────────────

#[test]
fn extract_error_message_openai_envelope() {
    // The standard OpenAI-compatible shape (DeepSeek, OpenRouter, …):
    // `error.message` is the human-readable summary.
    let body = r#"{"error":{"message":"Insufficient Balance","type":"unknown_error","param":null,"code":"invalid_request_error"}}"#;
    assert_eq!(
        extract_error_message(body).as_deref(),
        Some("Insufficient Balance")
    );
}

#[test]
fn extract_error_message_anthropic_and_gemini_shapes() {
    // Anthropic nests under `type` + `error`; Gemini puts `code` next to
    // `message`.  Both still expose `error.message`, so the same lookup
    // unwraps them.
    let anthropic =
        r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#;
    assert_eq!(
        extract_error_message(anthropic).as_deref(),
        Some("Overloaded")
    );
    let gemini =
        r#"{"error":{"code":429,"message":"Quota exceeded","status":"RESOURCE_EXHAUSTED"}}"#;
    assert_eq!(
        extract_error_message(gemini).as_deref(),
        Some("Quota exceeded")
    );
}

#[test]
fn extract_error_message_compat_shapes() {
    // Top-level `message` (proxy-unwrapped) and a plain-string `error`
    // (rare compat layer) are both recognized.
    assert_eq!(
        extract_error_message(r#"{"message":"bad request"}"#).as_deref(),
        Some("bad request")
    );
    assert_eq!(
        extract_error_message(r#"{"error":"plain failure"}"#).as_deref(),
        Some("plain failure")
    );
}

#[test]
fn extract_error_message_falls_back_for_non_envelope_bodies() {
    // Not JSON, JSON without a string message, or an empty message all
    // yield None — the caller keeps the verbatim body rather than
    // mis-summarizing an unknown shape.
    assert_eq!(extract_error_message("<html>502 Bad Gateway</html>"), None);
    assert_eq!(extract_error_message(r#"{"error":{"code":7}}"#), None);
    assert_eq!(
        extract_error_message(r#"{"error":{"message":"   "}}"#),
        None
    );
    assert_eq!(extract_error_message(r#"{"error":null}"#), None);
    assert_eq!(extract_error_message("not json at all"), None);
}

#[test]
fn rate_limited_status_survives_the_whole_error_chain() {
    // `RateLimited` carries the status explicitly so it survives the whole
    // error chain: a parseable body renders as `rate limited (429): …`, never
    // a status-less `rate limited: …`, at every layer.
    let http = ProviderHttpError::RateLimited {
        status: 429,
        retry_after_secs: None,
        detail: "Quota exceeded".into(),
    };
    assert_eq!(http.to_string(), "rate limited (429): Quota exceeded");

    let with_retry_after = ProviderHttpError::RateLimited {
        status: 429,
        retry_after_secs: Some(30),
        detail: "Quota exceeded".into(),
    };
    assert_eq!(
        with_retry_after.to_string(),
        "rate limited (429, retry after 30s): Quota exceeded"
    );

    // The provider → inference conversions must thread the status through
    // unchanged (each Display still carries it).
    let provider: crate::shared::ProviderError = http.into();
    assert_eq!(provider.to_string(), "rate limited (429): Quota exceeded");
    let inference = crate::shared::provider_error_to_inference(provider);
    assert_eq!(inference.to_string(), "rate limited (429): Quota exceeded");
}

// ── AttemptDeadline ──────────────────────────────────────────────────

#[test]
fn attempt_deadline_disabled_when_zero() {
    // A zero budget disables the deadline entirely (matches the
    // `total_timeout_secs = 0 disables` contract).
    let deadline = AttemptDeadline::new(0);
    assert!(deadline.current().is_none());
}

#[test]
fn attempt_deadline_arms_on_reset() {
    // `new` leaves the deadline unarmed; `reset` arms it for an attempt
    // (`retry_loop` calls reset at the top of every attempt, including
    // the first).  Deterministic: we only observe the clock — no waiting.
    let before = std::time::Instant::now();
    let mut deadline = AttemptDeadline::new(3600);
    assert!(
        deadline.current().is_none(),
        "new() must not arm the deadline; retry_loop does that on reset"
    );
    deadline.reset();
    let first = deadline
        .current()
        .expect("reset must arm a deadline with a nonzero budget");
    assert!(
        first > before,
        "deadline must be in the future (budget 3600s)"
    );
    // A second reset must move the deadline forward, not keep a stale
    // instant.
    deadline.reset();
    let second = deadline
        .current()
        .expect("reset must keep the deadline armed");
    assert!(
        second >= first,
        "reset must not move the deadline backwards"
    );
}

// ── gateway error-code recovery + transient-400 gate ───────────────────────

#[test]
fn gateway_error_code_recovers_the_bracketed_label() {
    assert_eq!(
        gateway_error_code(
            "Upstream request failed: [unsupported_request] Message content must contain text."
        ),
        Some("unsupported_request")
    );
    // The label grammar mirrors the gateway's own `<prefix>: [<code>]` shape.
    assert_eq!(gateway_error_code("[rate_limit] slow down"), None);
    assert_eq!(
        gateway_error_code("Upstream request failed: [] empty"),
        None
    );
    assert_eq!(gateway_error_code("a: b: [code] tail"), None);
    assert_eq!(gateway_error_code("prefix: [bad code] tail"), None);
    // An opaque body carries no label.
    assert_eq!(
        gateway_error_code("request failed with status 400: {\"model\":\"x\"}"),
        None
    );
}

#[test]
fn transient_gateway_400_matches_only_the_gateway_content_signatures() {
    let text = "Upstream request failed: [unsupported_request] Message content must contain text.";
    for slug in [
        "opencode",
        "opencode-go",
        "opencode-go-anthropic-compatible",
    ] {
        assert!(is_transient_gateway_400(slug, text), "slug {slug}");
    }
    // The same wording is a deterministic rejection for any other provider.
    assert!(!is_transient_gateway_400("openai", text));
    assert!(!is_transient_gateway_400("deepseek", text));

    // The sibling content validator (media not enabled) on the gateway.
    assert!(is_transient_gateway_400(
        "opencode-go",
        "Upstream request failed: [unsupported_request] Message content supports text content only.",
    ));

    // A structured, deterministic gateway rejection is NOT retried.
    assert!(!is_transient_gateway_400(
        "opencode-go",
        "Upstream request failed: [content_filter] The response was filtered",
    ));
    assert!(!is_transient_gateway_400(
        "opencode-go",
        "request failed with status 400: not the stub"
    ));
}

#[test]
fn transient_gateway_400_matches_the_opaque_model_stub() {
    assert!(is_transient_gateway_400(
        "opencode-go",
        "request failed with status 400: {\"model\":\"deepseek-v4.1-flash\"}",
    ));
    // A body that carries a real reason is not the opaque stub.
    assert!(!is_transient_gateway_400(
        "opencode-go",
        "request failed with status 400: {\"model\":\"x\",\"error\":{\"message\":\"bad\"}}",
    ));
    // Non-gateway providers never match, even the stub.
    assert!(!is_transient_gateway_400(
        "gemini",
        "request failed with status 400: {\"model\":\"x\"}",
    ));
}

// ── retry_loop_with_provider_policies (the resend drivers) ─────────────────

/// The signature the gateway returns for a transient message-content rejection.
const GATEWAY_CONTENT_400: &str = r#"{"error":{"message":"Upstream request failed: [unsupported_request] Message content must contain text."}}"#;

/// Build a synthetic HTTP response for driving the retry loop with no socket.
fn synthetic_response(status: u16, body: &str) -> ureq::http::Response<ureq::Body> {
    ureq::http::Response::builder()
        .status(status)
        .body(
            ureq::Body::builder()
                .mime_type("application/json")
                .data(body),
        )
        .expect("build a synthetic response")
}

/// The outcome of driving `retry_loop_with_provider_policies` over a fixed
/// sequence of replies.
struct DrivenRetry {
    /// How many times the send closure was called.
    calls: usize,
    /// Whether the request body was still compressed when the loop returned.
    body_compressed: bool,
    /// The final result.
    result: Result<ureq::http::Response<ureq::Body>, ProviderHttpError>,
}

/// Drive `retry_loop_with_provider_policies` against a fixed sequence of
/// `(status, body)` replies (the last reply repeating once the sequence is
/// exhausted), with a zero backoff so the loop is fully deterministic and never
/// sleeps — the resend budget, not real time, is what the tests observe.
fn drive_gateway_retry(
    slug: &str,
    compression: RequestCompression,
    replies: &[(u16, &str)],
    max_attempts: u32,
) -> DrivenRetry {
    use std::cell::Cell;

    let body = HookedJsonBody::new(&serde_json::json!({"messages": []}), None, compression)
        .expect("serialize the request body");
    let calls = Cell::new(0usize);
    let send = || {
        let n = calls.get();
        calls.set(n + 1);
        let (status, text) = replies
            .get(n)
            .copied()
            .unwrap_or_else(|| *replies.last().expect("at least one reply"));
        Ok(synthetic_response(status, text))
    };
    // `initial_backoff_ms = 0` makes `backoff_duration` return `Duration::ZERO`,
    // so `wait_before_retry` resolves instantly (a zero-length sleep).
    let retry = RetryConfig {
        max_attempts,
        initial_backoff_ms: 0,
        max_backoff_ms: 0,
    };
    let mut on_retry = None;
    let mut ctx = AttemptContext::new(&mut on_retry, None, None);
    let result = retry_loop_with_provider_policies(&body, slug, send, &retry, &mut ctx);
    DrivenRetry {
        calls: calls.get(),
        body_compressed: body.is_compressed(),
        result,
    }
}

#[test]
fn gateway_400_is_resent_then_succeeds() {
    let out = drive_gateway_retry(
        "opencode-go",
        RequestCompression::Identity,
        &[(400, GATEWAY_CONTENT_400), (200, "{}")],
        5,
    );
    assert_eq!(out.calls, 2, "a matching gateway 400 must be resent once");
    assert!(
        out.result.is_ok(),
        "the resend must surface the eventual 200"
    );
}

#[test]
fn gateway_400_resends_are_bounded() {
    // Every reply carries the transient signature; the loop must stop after the
    // first attempt plus `GATEWAY_TRANSIENT_400_ATTEMPTS` resends, never loop.
    let out = drive_gateway_retry(
        "opencode-go",
        RequestCompression::Identity,
        &[(400, GATEWAY_CONTENT_400)],
        5,
    );
    assert_eq!(
        out.calls,
        1 + GATEWAY_TRANSIENT_400_ATTEMPTS as usize,
        "the resends must be capped at GATEWAY_TRANSIENT_400_ATTEMPTS"
    );
    assert!(matches!(
        out.result,
        Err(ProviderHttpError::ClientError { status: 400, .. })
    ));
}

#[test]
fn gateway_400_resend_count_is_clamped_by_the_attempt_budget() {
    // With `max_attempts == 2` the policy may add only one resend (it never
    // exceeds `max_attempts - 1`), so there are two sends in total.
    let out = drive_gateway_retry(
        "opencode-go",
        RequestCompression::Identity,
        &[(400, GATEWAY_CONTENT_400)],
        2,
    );
    assert_eq!(out.calls, 2);
}

#[test]
fn no_retry_skips_the_gateway_400_policy() {
    // `max_attempts == 1` (the cache-warming ping's `no_retry`) disables the
    // policy: the transient 400 is returned after a single send.
    let out = drive_gateway_retry(
        "opencode-go",
        RequestCompression::Identity,
        &[(400, GATEWAY_CONTENT_400), (200, "{}")],
        1,
    );
    assert_eq!(out.calls, 1, "a no_retry request must not be resent");
    assert!(matches!(
        out.result,
        Err(ProviderHttpError::ClientError { status: 400, .. })
    ));
}

#[test]
fn non_gateway_slug_400_is_never_resent() {
    let out = drive_gateway_retry(
        "openai",
        RequestCompression::Identity,
        &[(400, GATEWAY_CONTENT_400)],
        5,
    );
    assert_eq!(out.calls, 1, "a non-gateway slug must never resend a 400");
}

#[test]
fn unmatched_gateway_400_is_not_resent() {
    // Gateway slug, but a deterministic rejection with a structured reason — not
    // one of the transient content signatures — so it stays terminal.
    let out = drive_gateway_retry(
        "opencode-go",
        RequestCompression::Identity,
        &[(
            400,
            r#"{"error":{"message":"Upstream request failed: [content_filter] filtered"}}"#,
        )],
        5,
    );
    assert_eq!(out.calls, 1, "a non-signature 400 must stay terminal");
}

#[test]
fn gateway_opaque_stub_400_is_resent() {
    let out = drive_gateway_retry(
        "opencode-go",
        RequestCompression::Identity,
        &[(400, r#"{"model":"deepseek-v4.1-flash"}"#), (200, "{}")],
        5,
    );
    assert_eq!(out.calls, 2, "the opaque model-only stub must be resent");
    assert!(out.result.is_ok());
}

#[test]
fn http_415_downgrades_compression_and_resends_once() {
    // Policy 1 still holds after the refactor: a gzip body answered 415 is
    // downgraded in place and re-sent uncompressed. The slug is unique so the
    // process-global downgrade registry (`COMPRESSION_DOWNGRADED`) picks up no
    // other test's entry.
    let out = drive_gateway_retry(
        "test-provider-415",
        RequestCompression::Gzip,
        &[(415, "unsupported"), (200, "{}")],
        5,
    );
    assert_eq!(out.calls, 2, "a 415 must trigger one uncompressed resend");
    assert!(
        !out.body_compressed,
        "the body must be downgraded to identity"
    );
    assert!(out.result.is_ok());
}
