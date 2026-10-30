//! Tests for the per-client configuration resolvers in [`super`].
//!
//! These pin the [`ServiceConfig`] rules every request builder inherits: the
//! catalog output-token clamp and its clamp-down-only invariant, the Chat
//! Completions output-token field pairs, the programmatic-tool-calling and
//! reasoning gates, the request-format and stateless-dialect facts, and the
//! cache-affinity precedence (per-account override → catalog dialect →
//! process-wide rejection registry). The suite lives in a sibling file so the
//! configuration module stays readable at a glance; the module path is
//! unchanged, so the tests reach [`ServiceConfig`] through the same `super`.
//!
//! Every test that reads or swaps the process-wide catalog carries
//! `#[serial_test::serial(catalog)]` individually, and the module does not carry
//! it: the lock is what keeps those reads from interleaving with the catalog
//! swaps (here and in the catalog suites), and per-process test isolation alone
//! does not cover the libtest fallback, which shares one process across parallel
//! threads. The attribute cannot be declared on an out-of-line `mod tests;`
//! item because the macro works by rewriting the module's item list, which an
//! out-of-line module does not have. The three pure `apply_overrides` cases and
//! the override-only token-pair case resolve from the config value alone, so
//! they take no lock.

// `super` is the `config` module this suite tests; every other item is imported
// from the module that defines it rather than globbed, as in the adapter's other
// out-of-line suites.
use super::ServiceConfig;
use crate::openai::RequestFormat;
use crate::session_routing::CacheAffinity;
use crate::shared::MaxTokensField;

#[test]
#[serial_test::serial(catalog)]
fn catalog_clamp_downs_known_model() {
    // The bundled snapshot records gpt-5.4's limit.output as 128_000, so a
    // larger request must be clamped down to the catalog fact.
    let config = ServiceConfig::default();
    assert_eq!(config.clamp_output_to_catalog("gpt-5.4", 999_999), 128_000);
    // Responses-path resolution inherits the clamp.
    let config = ServiceConfig {
        responses_max_output_tokens: Some(200_000),
        ..Default::default()
    };
    assert_eq!(config.max_output_tokens_for_model("gpt-5.4"), Some(128_000));
    // Chat-completions pair resolution inherits the clamp too.
    let config = ServiceConfig {
        chat_completions_max_tokens: Some(200_000),
        ..Default::default()
    };
    // Default max_tokens_field is MaxCompletionTokens, so the pair puts
    // the clamped value in the second slot.
    assert_eq!(
        config.max_tokens_field_pair("gpt-5.4"),
        (None, Some(128_000))
    );
}

#[test]
fn max_tokens_override_lands_in_the_models_field() {
    // The per-call cap (the cache-warming ping's 1-token cap) replaces the
    // configured default in whatever field the model uses, leaving the
    // other slot empty. The default field is MaxCompletionTokens (o-series /
    // gpt-5.x), so an override lands in the second slot...
    let config = ServiceConfig::default();
    assert_eq!(
        config.max_tokens_field_pair_with_override("gpt-5.4", Some(1)),
        (None, Some(1))
    );
    // ...while a model pinned to `max_tokens` gets it in the first slot.
    let mut config = ServiceConfig::default();
    config
        .model_max_tokens_fields
        .insert("legacy-gpt".to_string(), MaxTokensField::MaxTokens);
    assert_eq!(
        config.max_tokens_field_pair_with_override("legacy-gpt", Some(1)),
        (Some(1), None)
    );
    // With no override the plain pair resolution is used unchanged.
    assert_eq!(
        config.max_tokens_field_pair_with_override("legacy-gpt", None),
        config.max_tokens_field_pair("legacy-gpt")
    );
}

#[test]
#[serial_test::serial(catalog)]
fn catalog_clamp_never_raises_a_lower_request() {
    // A request already under the catalog ceiling must pass through
    // unchanged: the request may be intentionally small.
    let config = ServiceConfig::default();
    assert_eq!(config.clamp_output_to_catalog("gpt-5.4", 128_000), 128_000);
    assert_eq!(config.clamp_output_to_catalog("gpt-5.4", 512), 512);
}

#[test]
#[serial_test::serial(catalog)]
fn catalog_clamp_untouched_for_unknown_model_or_provider() {
    // Unknown model on a known provider, and unknown provider entirely:
    // no fact → no clamp, request passes through as-is.
    let config = ServiceConfig::default();
    assert_eq!(
        config.clamp_output_to_catalog("not-a-real-model", 200_000),
        200_000
    );
    let config = ServiceConfig {
        provider_slug: "no-such-provider".into(),
        ..Default::default()
    };
    assert_eq!(config.clamp_output_to_catalog("gpt-5.4", 200_000), 200_000);
}

#[test]
#[serial_test::serial(catalog)]
fn catalog_clamp_untouched_when_fact_is_zero() {
    // A recorded `max_output_tokens` of `0` is the "unknown" sentinel and
    // resolves to `None` — the request must not be clamped to 0.
    let _restore = crate::catalog::test_util::RestoreBundledOnDrop;
    let bundled = crate::catalog::catalog_snapshot();
    crate::catalog::replace_catalog({
        let mut c = crate::catalog::test_util::tiny_catalog();
        c[0].models[0].max_output_tokens = 0;
        c
    });
    let config = ServiceConfig {
        provider_slug: "tiny-test".into(),
        chat_completions_max_tokens: Some(200_000),
        ..Default::default()
    };
    assert_eq!(config.max_tokens_field_pair("tiny-model").1, Some(200_000));
    crate::catalog::replace_catalog(bundled.to_vec());
}

#[test]
#[serial_test::serial(catalog)]
fn programmatic_tool_calling_default_is_false() {
    let config = ServiceConfig::default();
    assert!(!config.programmatic_tool_calling_for_model("gpt-4.1"));
}

#[test]
#[serial_test::serial(catalog)]
fn programmatic_tool_calling_account_override() {
    let config = ServiceConfig {
        programmatic_tool_calling: true,
        ..Default::default()
    };
    // Override takes precedence regardless of model.
    assert!(config.programmatic_tool_calling_for_model("gpt-4.1"));
    assert!(config.programmatic_tool_calling_for_model("claude-3"));
}

#[test]
#[serial_test::serial(catalog)]
fn programmatic_tool_calling_auto_enables_for_gpt_5_6_responses() {
    let config = ServiceConfig {
        default_request_format: RequestFormat::Responses,
        ..Default::default()
    };
    // Models not in the catalog fall back to default_request_format.
    // When the default is Responses, gpt-5.6 models auto-enable.
    assert!(config.programmatic_tool_calling_for_model("gpt-5.6-chat"));
    // gpt-5.6-sol is Responses in the catalog (OpenAI's default API),
    // so auto-enable fires even without the account-level override.
    assert!(config.programmatic_tool_calling_for_model("gpt-5.6-sol"));
}

#[test]
#[serial_test::serial(catalog)]
fn programmatic_tool_calling_not_auto_enabled_with_chat_completions_default() {
    let config = ServiceConfig {
        default_request_format: RequestFormat::ChatCompletions,
        ..Default::default()
    };
    // An unknown gpt-5.6 model falls back to the ChatCompletions default,
    // so auto-enable (Responses-only) does not fire.
    assert!(!config.programmatic_tool_calling_for_model("gpt-5.6-future-unknown"));
}

#[test]
#[serial_test::serial(catalog)]
fn programmatic_tool_calling_not_auto_enabled_for_other_models() {
    let config = ServiceConfig {
        default_request_format: RequestFormat::Responses,
        ..Default::default()
    };
    assert!(!config.programmatic_tool_calling_for_model("gpt-4.1"));
    assert!(!config.programmatic_tool_calling_for_model("gpt-5.5"));
    assert!(!config.programmatic_tool_calling_for_model("claude-3-opus"));
}

#[test]
#[serial_test::serial(catalog)]
fn responses_stateless_is_chatgpt_only() {
    // The Codex backend is the stateless Responses dialect, pinned by the
    // bundled overlay's provider-level `responses_stateless` fact...
    let codex = ServiceConfig {
        provider_slug: "chatgpt".to_string(),
        ..Default::default()
    };
    assert!(codex.responses_stateless());
    // ...while the standard OpenAI Responses provider chains and keeps the
    // system item inline.
    let openai = ServiceConfig {
        provider_slug: "openai".to_string(),
        ..Default::default()
    };
    assert!(!openai.responses_stateless());
    // A provider the catalog has never heard of gets the standard chaining
    // shape: the stateless dialect is opt-in per backend, never a default.
    let unknown = ServiceConfig {
        provider_slug: "no-such-provider".to_string(),
        ..Default::default()
    };
    assert!(!unknown.responses_stateless());
}

#[test]
#[serial_test::serial(catalog)]
fn programmatic_tool_calling_is_withheld_for_the_stateless_provider() {
    // Even for a gpt-5.6 model and with the account-level override on, the
    // Codex backend must not receive the hosted programmatic-tool-calling
    // tool it rejects.
    let codex = ServiceConfig {
        provider_slug: "chatgpt".to_string(),
        default_request_format: RequestFormat::Responses,
        programmatic_tool_calling: true,
        ..Default::default()
    };
    assert!(!codex.programmatic_tool_calling_for_model("gpt-5.6-luna"));
    // The standard OpenAI provider keeps auto-enabling for gpt-5.6.
    let openai = ServiceConfig {
        provider_slug: "openai".to_string(),
        default_request_format: RequestFormat::Responses,
        ..Default::default()
    };
    assert!(openai.programmatic_tool_calling_for_model("gpt-5.6-sol"));
}

#[test]
#[serial_test::serial(catalog)]
fn request_format_for_model_uses_catalog_lookup() {
    let config = ServiceConfig {
        default_request_format: RequestFormat::Responses,
        provider_slug: "openai".to_string(),
        ..Default::default()
    };
    assert_eq!(
        config.request_format_for_model("gpt-4.1"),
        RequestFormat::Responses
    );
    // Unknown model falls back to default_request_format.
    assert_eq!(
        config.request_format_for_model("totally-unknown-xyz"),
        RequestFormat::Responses
    );
}

#[test]
#[serial_test::serial(catalog)]
fn model_supports_reasoning_reflects_catalog_fact() {
    let config = ServiceConfig {
        provider_slug: "openai".to_string(),
        ..Default::default()
    };
    // Official OpenAI models all use the Responses API (`openai_responses`
    // is provider-level), so the reasoning-summary `include` must be gated
    // on the per-model reasoning fact instead of the endpoint.
    assert_eq!(
        config.request_format_for_model("gpt-4o"),
        RequestFormat::Responses
    );
    // gpt-4o is recorded (models.dev) as a non-reasoning model → no
    // reasoning-summary request for it.
    assert!(!config.model_supports_reasoning("gpt-4o"));
    // A reasoning model keeps the summary request.
    assert!(config.model_supports_reasoning("gpt-5.4"));
    // An unknown model falls back to protocol defaults → true (best-effort).
    assert!(config.model_supports_reasoning("totally-unknown-xyz"));
}

#[test]
#[serial_test::serial(catalog)]
fn programmatic_tool_calling_override_wins_over_auto_disable() {
    let config = ServiceConfig {
        default_request_format: RequestFormat::ChatCompletions,
        programmatic_tool_calling: true,
        ..Default::default()
    };
    // Account-level override takes precedence even when auto-enable
    // would not trigger (ChatCompletions format).
    assert!(config.programmatic_tool_calling_for_model("gpt-5.6-sol"));
    assert!(config.programmatic_tool_calling_for_model("gpt-4.1"));
}

#[test]
#[serial_test::serial(catalog)]
fn cache_affinity_override_forces_off_over_the_catalog() {
    // Catalog opts into prompt_cache_key; the operator forces it off.
    let mut config = ServiceConfig {
        cache_affinity: CacheAffinity::PromptCacheKey,
        ..Default::default()
    };
    config.apply_overrides(&crate::ProviderOverrides {
        prompt_cache_affinity: Some(false),
        ..Default::default()
    });
    assert_eq!(config.cache_affinity, CacheAffinity::Off);
    assert_eq!(
        config.effective_cache_affinity("some-model"),
        CacheAffinity::Off
    );
}

#[test]
fn cache_affinity_override_forces_on_over_the_catalog() {
    // Catalog leaves it off; the operator forces it on, which needs a
    // concrete dialect — the broadly-documented prompt_cache_key.
    let mut config = ServiceConfig::default();
    assert_eq!(config.cache_affinity, CacheAffinity::Off);
    config.apply_overrides(&crate::ProviderOverrides {
        prompt_cache_affinity: Some(true),
        ..Default::default()
    });
    assert_eq!(config.cache_affinity, CacheAffinity::PromptCacheKey);
}

#[test]
fn cache_affinity_override_keeps_a_selected_dialect() {
    // Forcing on keeps the catalog's chosen dialect rather than replacing it
    // with prompt_cache_key (OpenRouter wants its own session_id).
    let mut config = ServiceConfig {
        cache_affinity: CacheAffinity::OpenRouterSession,
        ..Default::default()
    };
    config.apply_overrides(&crate::ProviderOverrides {
        prompt_cache_affinity: Some(true),
        ..Default::default()
    });
    assert_eq!(config.cache_affinity, CacheAffinity::OpenRouterSession);
}

#[test]
fn cache_affinity_override_none_keeps_the_catalog_value() {
    let mut config = ServiceConfig {
        cache_affinity: CacheAffinity::PromptCacheKey,
        ..Default::default()
    };
    config.apply_overrides(&crate::ProviderOverrides::default());
    assert_eq!(config.cache_affinity, CacheAffinity::PromptCacheKey);
}

#[test]
#[serial_test::serial(catalog)]
fn stateless_provider_never_sends_a_cache_affinity_hint() {
    // The stateless dialect documents no affinity field, so it is withheld —
    // including against a forced-on per-account override, which is the only
    // way the configured dialect could become a non-`Off` value here.
    let mut codex = ServiceConfig {
        provider_slug: "chatgpt".to_string(),
        ..Default::default()
    };
    codex.apply_overrides(&crate::ProviderOverrides {
        prompt_cache_affinity: Some(true),
        ..Default::default()
    });
    // The configured value was forced on…
    assert_eq!(codex.cache_affinity, CacheAffinity::PromptCacheKey);
    // …but the request-time resolution withholds it for the whole dialect.
    assert_eq!(
        codex.effective_cache_affinity("gpt-5.6-luna"),
        CacheAffinity::Off
    );
    // The standard Responses provider keeps honouring the same override, so
    // the withholding is the dialect's and not a global one.
    let mut openai = ServiceConfig {
        provider_slug: "openai".to_string(),
        ..Default::default()
    };
    openai.apply_overrides(&crate::ProviderOverrides {
        prompt_cache_affinity: Some(true),
        ..Default::default()
    });
    assert_eq!(
        openai.effective_cache_affinity("gpt-5.6-sol"),
        CacheAffinity::PromptCacheKey
    );
}

#[test]
#[serial_test::serial(catalog)]
fn configured_cache_affinity_applies_the_stateless_withholding() {
    // The provider-level resolution both readers share. The stateless dialect
    // withholds the hint regardless of the configured field (so an account
    // that forced it on cannot outrank "this backend takes no such field"),
    // while the standard provider reports the dialect it was configured with.
    let codex = ServiceConfig {
        provider_slug: "chatgpt".to_string(),
        cache_affinity: CacheAffinity::PromptCacheKey,
        ..Default::default()
    };
    assert_eq!(codex.configured_cache_affinity(), CacheAffinity::Off);
    let openai = ServiceConfig {
        provider_slug: "openai".to_string(),
        cache_affinity: CacheAffinity::PromptCacheKey,
        ..Default::default()
    };
    assert_eq!(
        openai.configured_cache_affinity(),
        CacheAffinity::PromptCacheKey
    );
    // The request-time resolution is unchanged by the split: the stateless
    // dialect still sends nothing, and the standard provider still sends its
    // configured dialect for a model that has not rejected it.
    assert_eq!(
        codex.effective_cache_affinity("gpt-5.6-luna"),
        CacheAffinity::Off
    );
    assert_eq!(
        openai.effective_cache_affinity("gpt-5.6-sol"),
        CacheAffinity::PromptCacheKey
    );
}

#[test]
#[serial_test::serial(catalog)]
fn rejection_registry_beats_a_forced_on_override() {
    // Precedence is override > catalog > registry: the registry is the
    // lowest priority, but it can only withhold a hint, so a provider that
    // has already rejected the field stays off even when forced on. The
    // slug is unique to this test because the registry is process-wide.
    let slug = "override-rejection-test-slug";
    crate::cache_field_rejection::note_cache_field_rejected(
        slug,
        "forced-model",
        crate::cache_field_rejection::PROMPT_CACHE_KEY_FIELD,
    );
    let mut config = ServiceConfig {
        provider_slug: slug.to_string(),
        ..Default::default()
    };
    config.apply_overrides(&crate::ProviderOverrides {
        prompt_cache_affinity: Some(true),
        ..Default::default()
    });
    // The configured dialect was forced on…
    assert_eq!(config.cache_affinity, CacheAffinity::PromptCacheKey);
    // …but the request-time resolution withholds it for the rejected model…
    assert_eq!(
        config.effective_cache_affinity("forced-model"),
        CacheAffinity::Off
    );
    // …and a different model still receives the forced-on hint.
    assert_eq!(
        config.effective_cache_affinity("other-model"),
        CacheAffinity::PromptCacheKey
    );
}
