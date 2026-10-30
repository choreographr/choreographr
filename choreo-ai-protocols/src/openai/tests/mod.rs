//! The `OpenAI` adapter's non-SSE test suite.
//!
//! Chat-wire serialization, the retry/backoff helpers and `sleep_or_cancel`, the
//! model-usage error annotation, and cache affinity. The SSE subjects — the
//! frame reader, `build_sse_event`, and the Responses event parser — live in
//! [`sse`], beside this file, because they test a different part of the adapter.

mod sse;

use super::*;
use std::time::Duration;

#[test]
fn chat_message_with_images_serializes_content_as_parts_array() {
    // A user message carrying an image serializes `content` as an array of
    // `text` + `image_url` parts (the OpenAI chat vision shape), with the
    // data URL nested under `image_url.url` and `detail` fixed to "auto".
    let msg = ChatRequestMessage {
        role: "user",
        content: Some("what is this?".to_string()),
        images: vec![ChatImagePart {
            data: b"\x89PNG-fake".to_vec(),
            mime_type: "image/png".to_string(),
        }],
        tool_call_id: None,
        tool_calls: None,
        reasoning_artifact: None,
    };
    let value =
        serde_json::to_value(chat_wire_message(&msg, ChatReasoningEcho::default())).unwrap();
    let content = value["content"].as_array().expect("content array");
    assert_eq!(content[0]["type"], "text");
    assert_eq!(content[0]["text"], "what is this?");
    assert_eq!(content[1]["type"], "image_url");
    assert_eq!(
        content[1]["image_url"]["url"],
        "data:image/png;base64,iVBORy1mYWtl"
    );
    assert_eq!(content[1]["image_url"]["detail"], "auto");
    // The images field itself must never appear on the wire as its own field.
    assert!(value.get("images").is_none());
}

#[test]
fn chat_message_without_images_serializes_content_as_plain_string() {
    // A text-only message serializes to the plain role/content shape
    // (content is a plain string; no images field).
    let msg = ChatRequestMessage::simple("user", "hi".to_string());
    let value =
        serde_json::to_value(chat_wire_message(&msg, ChatReasoningEcho::default())).unwrap();
    assert_eq!(value["content"], "hi");
    assert!(value.get("images").is_none());
}

#[test]
fn retryable_statuses() {
    assert!(is_retryable_status(429)); // TOO_MANY_REQUESTS
    assert!(is_retryable_status(500)); // INTERNAL_SERVER_ERROR
    assert!(is_retryable_status(502)); // BAD_GATEWAY
    assert!(is_retryable_status(503)); // SERVICE_UNAVAILABLE
    assert!(is_retryable_status(504)); // GATEWAY_TIMEOUT
    assert!(!is_retryable_status(400)); // BAD_REQUEST
    assert!(!is_retryable_status(401)); // UNAUTHORIZED
    assert!(!is_retryable_status(403)); // FORBIDDEN
    assert!(!is_retryable_status(404)); // NOT_FOUND
    assert!(!is_retryable_status(200)); // OK
}

#[test]
fn backoff_grows_exponentially_within_jitter_bounds() {
    let config = RetryConfig {
        max_attempts: 5,
        initial_backoff_ms: 1000,
        max_backoff_ms: 30000,
    };

    let d1 = backoff_duration(1, &config);
    assert!(d1.as_millis() >= 750 && d1.as_millis() <= 1250);

    let d2 = backoff_duration(2, &config);
    assert!(d2.as_millis() >= 1500 && d2.as_millis() <= 2500);

    let d3 = backoff_duration(3, &config);
    assert!(d3.as_millis() >= 3000 && d3.as_millis() <= 5000);

    let d4 = backoff_duration(4, &config);
    assert!(d4.as_millis() >= 6000 && d4.as_millis() <= 10000);
}

#[test]
fn backoff_respects_max_cap() {
    let config = RetryConfig {
        max_attempts: 5,
        initial_backoff_ms: 1000,
        max_backoff_ms: 5000,
    };

    let d = backoff_duration(10, &config);
    assert!(d.as_millis() >= 3750 && d.as_millis() <= 6250);
}

#[test]
fn parse_retry_after_seconds() {
    assert_eq!(parse_retry_after_secs(Some("42")), Some(42));
}

#[test]
fn parse_retry_after_missing_header() {
    assert_eq!(parse_retry_after_secs(None), None);
}

#[test]
fn parse_retry_after_non_integer() {
    assert_eq!(parse_retry_after_secs(Some("abc")), None);
}

// -- sleep_or_cancel tests -------------------------------------------

#[test]
fn sleep_or_cancel_signal_returns_cancelled() {
    let (tx, rx) = crossbeam_channel::unbounded::<()>();
    tx.send(()).unwrap();
    let result = crate::retry::sleep_or_cancel(Duration::from_secs(10), Some(&rx));
    assert!(result.is_err());
}

#[test]
fn sleep_or_cancel_disconnected_returns_ok() {
    let (tx, rx) = crossbeam_channel::unbounded::<()>();
    drop(tx);
    let result = crate::retry::sleep_or_cancel(Duration::from_millis(1), Some(&rx));
    assert!(result.is_ok());
}

// -- model-usage error annotation -------------------------------------

#[test]
fn annotates_not_a_chat_model_rejection_with_the_model() {
    // The exact OpenAI body the live model list can produce when the user
    // selects a legacy completions model. The user-facing message must be
    // short and plain, and must NOT leak the provider's technical wording.
    let err = annotate_model_usage_error(
        "gpt-3.5-turbo-instruct",
        OpenAiError::ClientError {
            status: 404,
            detail: "This is not a chat model and thus not supported in the \
                     v1/chat/completions endpoint. Did you mean to use v1/completions?"
                .to_string(),
        },
    );
    let OpenAiError::ClientError { status, detail } = err else {
        panic!("expected ClientError, got {err:?}");
    };
    assert_eq!(status, 404);
    assert_eq!(
        detail,
        "'gpt-3.5-turbo-instruct' is not a chat model — please try a different one."
    );
    assert!(!detail.contains("v1/chat/completions"), "{detail}");
}

#[test]
fn annotates_model_not_found_with_a_plain_message() {
    // The generic "model doesn't exist / no access" body reads the same to a
    // user: the model can't be used here.
    let err = annotate_model_usage_error(
        "gpt-6-astra",
        OpenAiError::ClientError {
            status: 404,
            detail: "The model 'gpt-6-astra' does not exist or you do not have access to it."
                .to_string(),
        },
    );
    let OpenAiError::ClientError { detail, .. } = err else {
        panic!("expected ClientError, got {err:?}");
    };
    assert_eq!(
        detail,
        "'gpt-6-astra' can't be used here — please try a different one."
    );
}

#[test]
fn leaves_unrelated_client_errors_untouched() {
    // A 400 that has nothing to do with model usability must pass through
    // verbatim so the real cause is not masked.
    let err = annotate_model_usage_error(
        "gpt-4o",
        OpenAiError::ClientError {
            status: 400,
            detail: "Invalid value: 'reasoning.summary'.".to_string(),
        },
    );
    let OpenAiError::ClientError { detail, .. } = err else {
        panic!("expected ClientError, got {err:?}");
    };
    assert_eq!(detail, "Invalid value: 'reasoning.summary'.");
}

#[test]
fn leaves_non_client_errors_untouched() {
    let err = annotate_model_usage_error("gpt-4o", OpenAiError::Cancelled);
    assert!(matches!(err, OpenAiError::Cancelled));
}

#[test]
fn leaves_unsupported_parameter_errors_untouched() {
    // Change #2: OpenAI reuses the SAME "is not supported in the v1/responses"
    // wording for genuine parameter errors. Matching it would rewrite a real
    // parameter bug as a model-usability rejection and hide the cause, so the
    // body must pass through verbatim (character-for-character).
    let body =
        "Unsupported parameter: 'reasoning_effort' is not supported in the v1/responses API.";
    let err = annotate_model_usage_error(
        "gpt-4o",
        OpenAiError::ClientError {
            status: 400,
            detail: body.to_string(),
        },
    );
    let OpenAiError::ClientError { status, detail } = err else {
        panic!("expected ClientError, got {err:?}");
    };
    assert_eq!(status, 400);
    assert_eq!(detail, body);
}

#[test]
fn model_usage_rejection_reason_classifies_bodies() {
    // A realistic OpenAI "not a chat model" body — the exact wording the live
    // model list can produce for a legacy completions model.
    assert_eq!(
        model_usage_rejection_reason(
            "This is not a chat model and thus not supported in the \
             v1/chat/completions endpoint. Did you mean to use v1/completions?"
        ),
        Some("is not a chat model")
    );
    // The parsed-envelope model-not-found signal: `extract_error_message`
    // keeps only `error.message`, so this human prose is what reaches us.
    assert_eq!(
        model_usage_rejection_reason(
            "The model 'x' does not exist or you do not have access to it."
        ),
        Some("can't be used here")
    );
    // The literal code only survives when the body was NOT a parseable
    // envelope (kept verbatim) — still recognized.
    assert_eq!(
        model_usage_rejection_reason("model_not_found"),
        Some("can't be used here")
    );
    // An unrelated body is not a model-usability rejection.
    assert_eq!(
        model_usage_rejection_reason("Invalid value: 'reasoning.summary'."),
        None
    );
}

#[test]
#[serial_test::serial(catalog)]
fn extended_cache_retention_matches_what_a_request_would_send() {
    // The warmer schedules its pings against the cache lifetime this answers, so
    // it must agree with the body builder's selector on every gate: the model's
    // dialect, the curated long tier, the API the body targets, and a field the
    // provider already rejected.
    use crate::{CacheRetentionDialect, PromptCacheTtl};
    let provider = "long-retention-extended-test";
    let _restore = crate::catalog::test_util::RestoreBundledOnDrop;

    // Pin one model on a tiny catalog. `openai_responses` decides which API the
    // body targets, which matters for the newer (Responses-only) spelling.
    let catalog_with = |dialect: CacheRetentionDialect, long_secs: Option<u32>, responses: bool| {
        let mut catalog = crate::catalog::test_util::tiny_catalog();
        catalog[0].slug = provider.to_string();
        let entry = &mut catalog[0].models[0];
        entry.model = "ret-model".to_string();
        entry.cache_retention = dialect;
        entry.openai_responses = responses;
        entry.prompt_cache = Some(PromptCacheTtl {
            short_secs: Some(300),
            long_secs,
        });
        crate::catalog::replace_catalog(catalog);
    };
    let client = || {
        let registry = crate::SocketRegistry::new();
        OpenAiClient::new(
            ServiceConfig {
                provider_slug: provider.to_string(),
                ..Default::default()
            },
            RequestAuth::api_key("test-key"),
            &registry,
        )
        .expect("client builds")
    };

    // Dialect + long tier, older spelling: emitted on both APIs.
    catalog_with(
        CacheRetentionDialect::PromptCacheRetention,
        Some(86_400),
        false,
    );
    let chat = client();
    assert!(chat.extended_cache_retention("ret-model"));
    // A model the catalog did not pin sends no selector.
    assert!(!chat.extended_cache_retention("unpinned-model"));
    catalog_with(
        CacheRetentionDialect::PromptCacheRetention,
        Some(86_400),
        true,
    );
    assert!(client().extended_cache_retention("ret-model"));

    // The same dialect with no curated long tier: nothing is emitted, so the
    // cache really does live the short tier.
    catalog_with(CacheRetentionDialect::PromptCacheRetention, None, false);
    assert!(!client().extended_cache_retention("ret-model"));

    // The newer spelling's extended `"12h"` value is Responses-only: Chat
    // Completions gets no selector, so it must not claim the long tier.
    catalog_with(
        CacheRetentionDialect::PromptCacheOptions,
        Some(43_200),
        false,
    );
    assert!(!client().extended_cache_retention("ret-model"));
    catalog_with(
        CacheRetentionDialect::PromptCacheOptions,
        Some(43_200),
        true,
    );
    assert!(client().extended_cache_retention("ret-model"));

    // And a field the provider already rejected for this pair is withheld, so
    // the request will not ask for the long tier.
    catalog_with(
        CacheRetentionDialect::PromptCacheRetention,
        Some(86_400),
        false,
    );
    crate::cache_field_rejection::note_cache_field_rejected(
        provider,
        "ret-model",
        crate::cache_field_rejection::PROMPT_CACHE_RETENTION_FIELD,
    );
    assert!(!client().extended_cache_retention("ret-model"));
}

#[test]
// `responses_stateless` reads the process-wide catalog, so serialize under the
// same key as the catalog-swap tests for libtest determinism.
#[serial_test::serial(catalog)]
fn client_cache_affinity_reports_what_the_request_sends() {
    // The daemon's cache-warming gate reads `ProviderClient::cache_affinity` and
    // documents it as the dialect the request bodies actually send, so the
    // stateless Responses backend must report `Off` even when the configured
    // field is set: its bodies withhold the hint outright.
    let client = |slug: &str| {
        let registry = crate::SocketRegistry::new();
        OpenAiClient::new(
            ServiceConfig {
                provider_slug: slug.to_string(),
                cache_affinity: crate::CacheAffinity::PromptCacheKey,
                ..Default::default()
            },
            RequestAuth::api_key("test-key"),
            &registry,
        )
        .expect("client builds")
    };
    assert_eq!(
        client("chatgpt").cache_affinity(),
        crate::CacheAffinity::Off
    );
    // The standard Responses provider keeps the dialect it was configured with.
    assert_eq!(
        client("openai").cache_affinity(),
        crate::CacheAffinity::PromptCacheKey
    );
}
