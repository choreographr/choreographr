//! Request-body assembly: the surrounding top-level fields (`model`, `store`,
//! `instructions`, `include`, reasoning, retention) around the lowered input,
//! including the two Responses dialects.
//!
//! The stateless ChatGPT/Codex shape and the standard chaining shape are pinned
//! side by side here, so a change that alters one without the other fails.

// Each subject module imports the items it names directly: it lives inside
// `tests`, so the adapter's own modules are two `super`s up and the `openai`
// parent three.
use super::super::super::*;
use super::super::body::{build_responses_request_body, build_simple_responses_body};
use choreo_proto::ReasoningArtifact;
use serde_json::json;

#[test]
#[serial_test::serial(catalog)]
fn responses_body_nests_reasoning_and_omits_it_for_non_reasoning() {
    let config = ServiceConfig {
        provider_slug: "openai".to_string(),
        ..Default::default()
    };

    // Collect the serialized body's keys as a sorted `Vec<String>` so we
    // can assert the EXACT key set — more precise than poking at
    // individual fields, which would be tautological here because
    // `ResponsesRequest` carries no top-level `reasoning_effort` field of its
    // own.
    let body_keys = |body: &serde_json::Value| -> Vec<String> {
        let mut keys: Vec<String> = body.as_object().unwrap().keys().cloned().collect();
        keys.sort();
        keys
    };

    // Build a tool-less Responses body for `model` with `effort` mapped to
    // the reasoning config; the retry/cancellation/route/chaining fields
    // are inert for body construction.
    let build = |model: &str, effort: Option<&str>| {
        let mut no_retry = None;
        let params = TurnParams {
            model,
            messages: &[],
            tools: &[],
            reasoning_effort: effort,
            on_retry: &mut no_retry,
            on_uploaded: None,
            cancel_rx: None,
            route: None,
            previous_response_id: None,
            programmatic_tool_calling: false,
            max_output_tokens_override: None,
            no_retry: false,
        };
        build_responses_request_body(&config, &params, false).expect("body builds")
    };

    // Case 1: non-reasoning model, no effort — no `reasoning` object at
    // all. A top-level `reasoning_effort` or `include:
    // ["reasoning.summary"]` is a hard 400 on the Responses API, so
    // neither may appear.
    let (_url, body) = build("gpt-4o", None);
    assert!(body.get("reasoning").is_none(), "{body}");
    assert_eq!(body_keys(&body), ["input", "model", "store"]);

    // Case 2: reasoning model with effort "high" — effort and summary
    // both live under the nested `reasoning` object (never top-level).
    let (_url, body) = build("gpt-5.4", Some("high"));
    assert_eq!(body["reasoning"]["effort"], "high");
    assert_eq!(body["reasoning"]["summary"], "auto");
    assert_eq!(body_keys(&body), ["input", "model", "reasoning", "store"]);

    // Case 3: non-reasoning model handed a stray non-`off` effort — the
    // whole `reasoning` object must still be suppressed (gated on the
    // capability, not on effort being present). Emitting
    // `reasoning: { effort }` here would be a provider 400.
    let (_url, body) = build("gpt-4o", Some("high"));
    assert!(body.get("reasoning").is_none(), "{body}");
    assert_eq!(body_keys(&body), ["input", "model", "store"]);
}

/// Build a tool-less Responses body for `provider`/`model` from `messages`,
/// with the same inert wiring the other body tests use.
fn body_for(provider: &str, model: &str, messages: &[ChatRequestMessage]) -> serde_json::Value {
    let config = ServiceConfig {
        provider_slug: provider.to_string(),
        ..Default::default()
    };
    let mut no_retry = None;
    let params = TurnParams {
        model,
        messages,
        tools: &[],
        reasoning_effort: None,
        on_retry: &mut no_retry,
        on_uploaded: None,
        cancel_rx: None,
        route: None,
        previous_response_id: None,
        programmatic_tool_calling: false,
        max_output_tokens_override: None,
        no_retry: false,
    };
    build_responses_request_body(&config, &params, false)
        .expect("body builds")
        .1
}

#[test]
#[serial_test::serial(catalog)]
fn chatgpt_backend_hoists_system_prompt_into_instructions() {
    // The ChatGPT/Codex backend rejects `system`-role input items, so the
    // system prompt must ride the top-level `instructions` string and never
    // appear as a `role: "system"` input item.
    let messages = vec![
        ChatRequestMessage::simple("system", "be helpful".into()),
        ChatRequestMessage::simple("user", "hi".into()),
    ];
    let body = body_for("chatgpt", "gpt-5.6-luna", &messages);
    assert_eq!(body["instructions"], "be helpful");
    // Stateless: the request must not persist, and it says so explicitly — an
    // omitted `store` would leave the backend's own storing default in force.
    assert_eq!(
        body["store"], false,
        "stateless sends store: false explicitly: {body}"
    );
    // And it asks for the encrypted reasoning payload, the only continuity
    // channel a transcript-less backend leaves open.
    assert_eq!(
        body["include"],
        json!(["reasoning.encrypted_content"]),
        "stateless requests encrypted reasoning: {body}"
    );
    assert!(
        body.get("previous_response_id").is_none(),
        "stateless sends no chain: {body}"
    );
    let input = body["input"].as_array().expect("input array");
    assert_eq!(input.len(), 1, "only the user message remains: {input:?}");
    assert_eq!(input[0]["role"], "user");
    assert!(
        input.iter().all(|item| item["role"] != "system"),
        "no system-role item may reach the Codex backend: {input:?}"
    );
}

#[test]
#[serial_test::serial(catalog)]
fn openai_responses_backend_keeps_the_system_item_and_sends_no_instructions() {
    // The standard OpenAI Responses API accepts inline system items, so it must
    // be left byte-for-byte unchanged: a `system` item in `input`, no
    // `instructions` field.
    let messages = vec![
        ChatRequestMessage::simple("system", "be helpful".into()),
        ChatRequestMessage::simple("user", "hi".into()),
    ];
    let body = body_for("openai", "gpt-5.4", &messages);
    assert!(body.get("instructions").is_none(), "{body}");
    // Chained provider still stores.
    assert_eq!(body["store"], true, "{body}");
    // …and needs no `include`: `previous_response_id` already carries the
    // reasoning, so asking for the encrypted payload would only add a field
    // this path never replays.
    assert!(
        body.get("include").is_none(),
        "chained providers request no include: {body}"
    );
    let input = body["input"].as_array().expect("input array");
    assert_eq!(input[0]["role"], "system");
    assert_eq!(input[0]["content"], "be helpful");
}

#[test]
#[serial_test::serial(catalog)]
fn chatgpt_stateless_withholds_the_reasoning_include_for_a_non_reasoning_model() {
    // `include` is validated strictly by the Responses API and a non-reasoning
    // model returns no reasoning item to carry, so the encrypted-payload request
    // rides the SAME capability gate as the nested `reasoning` object rather than
    // the provider flag alone. The bundled `chatgpt` models all reason, so pin
    // the boundary on a synthetic stateless provider instead.
    let _restore = crate::catalog::test_util::RestoreBundledOnDrop;
    let mut catalog = crate::catalog::test_util::tiny_catalog();
    catalog[0].slug = "tiny-stateless".to_string();
    catalog[0].responses_stateless = true;
    catalog[0].models[0].model = "tiny-chat".to_string();
    catalog[0].models[0].reasoning_supported = false;
    crate::catalog::replace_catalog(catalog);

    let body = body_for(
        "tiny-stateless",
        "tiny-chat",
        &[ChatRequestMessage::simple("user", "hi".into())],
    );
    // The stateless request shape is still in force for the provider…
    assert_eq!(
        body["store"], false,
        "stateless sends store: false explicitly: {body}"
    );
    // …but a model with nothing to reason about is asked for nothing.
    assert!(
        body.get("include").is_none(),
        "a non-reasoning model gets no reasoning include: {body}"
    );
    assert!(
        body.get("reasoning").is_none(),
        "a non-reasoning model gets no reasoning object: {body}"
    );
}

#[test]
#[serial_test::serial(catalog)]
fn chatgpt_stateless_body_replays_captured_reasoning_items() {
    // Nothing server-side survives a `store: false` request, so the captured
    // reasoning items are the whole continuity payload: an assistant turn's
    // `ResponsesItems` artifact must ride `input` verbatim — the opaque
    // `encrypted_content` included, since that blob is what the provider
    // decrypts to recover the turn's reasoning — ahead of that turn's message
    // item, in the provider's own output ordering.
    let messages = vec![
        ChatRequestMessage::simple("user", "solve it".into()),
        ChatRequestMessage {
            role: "assistant",
            content: Some("done".into()),
            images: Vec::new(),
            tool_call_id: None,
            tool_calls: None,
            reasoning_artifact: Some(ReasoningArtifact::ResponsesItems(
                br#"[{"type":"reasoning","id":"rs_1","encrypted_content":"eJxT_opaque"}]"#.to_vec(),
            )),
        },
        ChatRequestMessage::simple("user", "and now?".into()),
    ];
    let body = body_for("chatgpt", "gpt-5.6-luna", &messages);
    let input = body["input"].as_array().expect("input array");
    assert_eq!(input.len(), 4, "{input:?}");
    assert_eq!(input[0]["role"], "user");
    assert_eq!(input[1]["type"], "reasoning");
    assert_eq!(input[1]["id"], "rs_1");
    assert_eq!(
        input[1]["encrypted_content"], "eJxT_opaque",
        "the encrypted payload is what makes the replay usable: {input:?}"
    );
    assert_eq!(input[2]["role"], "assistant");
    assert_eq!(input[2]["content"], "done");
    assert_eq!(input[3]["role"], "user");
}

#[test]
fn build_simple_responses_body_sets_store_false() {
    let config = ServiceConfig {
        base_url: "https://api.openai.com/v1".into(),
        ..Default::default()
    };
    let (_url, body) = build_simple_responses_body(&config, "gpt-4", "hello", false).unwrap();
    // An explicit `store: false`, not an omitted field: the one-shot prompt must
    // not persist, and silence would defer that to the backend's default.
    assert_eq!(
        body["store"], false,
        "the one-shot body carries store: false: {body}"
    );
    assert_eq!(body["model"], "gpt-4");
    assert_eq!(body["input"], "hello");
    assert!(body.get("tools").is_none());
    assert!(body.get("instructions").is_none());
}

#[test]
fn build_simple_responses_body_sets_stream() {
    let config = ServiceConfig {
        base_url: "https://api.openai.com/v1".into(),
        ..Default::default()
    };
    let (_url, body) = build_simple_responses_body(&config, "gpt-4", "hello", true).unwrap();
    assert_eq!(body["stream"], true);
}

#[test]
#[serial_test::serial(catalog)]
fn responses_max_output_tokens_prefers_the_override() {
    // A per-call cap (the cache-warming ping's 1-token cap) must win over
    // the configured output cap on the wire; `None` keeps the config value.
    let config = ServiceConfig {
        provider_slug: "openai".to_string(),
        responses_max_output_tokens: Some(4096),
        ..Default::default()
    };
    let build = |override_: Option<u32>| {
        let mut no_retry = None;
        let params = TurnParams {
            model: "gpt-5.4",
            messages: &[],
            tools: &[],
            reasoning_effort: None,
            on_retry: &mut no_retry,
            on_uploaded: None,
            cancel_rx: None,
            route: None,
            previous_response_id: None,
            programmatic_tool_calling: false,
            max_output_tokens_override: override_,
            no_retry: false,
        };
        build_responses_request_body(&config, &params, false)
            .expect("body builds")
            .1
    };
    assert_eq!(build(Some(1))["max_output_tokens"], 1);
    assert_eq!(build(None)["max_output_tokens"], 4096);
}

/// Build a Responses body for `provider`/`model` whose catalog entry declares
/// `dialect` and (when `long_secs` is `Some`) a long TTL tier. The catalog is
/// swapped under the catalog lock and restored on drop.
fn retention_responses_body(
    provider: &str,
    model: &str,
    dialect: crate::CacheRetentionDialect,
    long_secs: Option<u32>,
) -> serde_json::Value {
    let mut catalog = crate::catalog::test_util::tiny_catalog();
    catalog[0].slug = provider.to_string();
    let entry = &mut catalog[0].models[0];
    entry.model = model.to_string();
    entry.cache_retention = dialect;
    entry.prompt_cache = Some(crate::PromptCacheTtl {
        short_secs: Some(300),
        long_secs,
    });
    crate::catalog::replace_catalog(catalog);

    let config = ServiceConfig {
        provider_slug: provider.to_string(),
        ..Default::default()
    };
    let mut no_retry = None;
    let params = TurnParams {
        model,
        messages: &[],
        tools: &[],
        reasoning_effort: None,
        on_retry: &mut no_retry,
        on_uploaded: None,
        cancel_rx: None,
        route: None,
        previous_response_id: None,
        programmatic_tool_calling: false,
        max_output_tokens_override: None,
        no_retry: false,
    };
    build_responses_request_body(&config, &params, false)
        .expect("body builds")
        .1
}

#[test]
#[serial_test::serial(catalog)]
fn responses_body_emits_no_retention_by_default() {
    let _restore = crate::catalog::test_util::RestoreBundledOnDrop;
    let body = retention_responses_body(
        "retention-responses-test",
        "ret-model",
        crate::CacheRetentionDialect::None,
        Some(86_400),
    );
    assert!(body.get("prompt_cache_retention").is_none(), "{body}");
    assert!(body.get("prompt_cache_options").is_none(), "{body}");
}

#[test]
#[serial_test::serial(catalog)]
fn responses_body_emits_the_pre_5_6_retention_spelling() {
    let _restore = crate::catalog::test_util::RestoreBundledOnDrop;
    let body = retention_responses_body(
        "retention-responses-test",
        "ret-model",
        crate::CacheRetentionDialect::PromptCacheRetention,
        Some(86_400),
    );
    assert_eq!(body["prompt_cache_retention"], "24h");
    assert!(body.get("prompt_cache_options").is_none(), "{body}");
}

#[test]
#[serial_test::serial(catalog)]
fn responses_body_emits_the_12h_options_spelling() {
    // The Responses API is where the newer dialect's extended `12h` value is
    // accepted; it nests under `prompt_cache_options.ttl` and never coexists
    // with the older top-level field.
    let _restore = crate::catalog::test_util::RestoreBundledOnDrop;
    let body = retention_responses_body(
        "retention-responses-test",
        "ret-model",
        crate::CacheRetentionDialect::PromptCacheOptions,
        Some(43_200),
    );
    assert_eq!(body["prompt_cache_options"]["ttl"], "12h");
    assert!(body.get("prompt_cache_retention").is_none(), "{body}");
}

#[test]
#[serial_test::serial(catalog)]
fn responses_body_withholds_retention_after_a_rejected_field() {
    let provider = "retention-rejected-responses-test";
    let _restore = crate::catalog::test_util::RestoreBundledOnDrop;
    crate::cache_field_rejection::note_cache_field_rejected(
        provider,
        "ret-model",
        crate::cache_field_rejection::PROMPT_CACHE_OPTIONS_FIELD,
    );
    let body = retention_responses_body(
        provider,
        "ret-model",
        crate::CacheRetentionDialect::PromptCacheOptions,
        Some(43_200),
    );
    assert!(body.get("prompt_cache_options").is_none(), "{body}");
}
