//! Tests for the model-fact resolvers in [`super`].
//!
//! These exercise the read-side resolvers over the process-wide provider
//! catalog: context window and output ceiling, wire flags, and the reasoning
//! round-trip policy. The suite lives in a sibling file so the resolver module
//! stays readable at a glance; the module path is unchanged, so the moved tests
//! reach their subjects through the same `super` paths.
//!
//! Every test carries `#[serial_test::serial(catalog)]` individually rather
//! than the module carrying it: the lock matters because these tests read (and
//! the swap tests mutate) the process-wide catalog, and nextest's per-process
//! isolation alone does not cover the libtest fallback, which shares one
//! process across parallel threads. The attribute cannot be declared on the
//! out-of-line `mod tests;` item because the macro works by rewriting the
//! module's item list, which an out-of-line module does not have.

use super::super::test_util::{RestoreBundledOnDrop, tiny_catalog};
use super::*;
use crate::catalog::catalog_snapshot;
use crate::catalog::lookup_provider;
use crate::openai::RequestFormat;
use crate::shared::MaxTokensField;

#[test]
#[serial_test::serial(catalog)]
fn lookup_context_window_known_provider() {
    // OpenAI exact slug matches
    assert_eq!(
        lookup_context_window("openai", "gpt-4.1-nano"),
        Some(1_047_576)
    );
    assert_eq!(lookup_context_window("openai", "gpt-5"), Some(400_000));
    // models.dev is authoritative for context windows: gpt-5.4 is a
    // 1.05M-token model there.
    assert_eq!(lookup_context_window("openai", "gpt-5.4"), Some(1_050_000));
    assert_eq!(
        lookup_context_window("openai", "gpt-5.5-pro"),
        Some(1_050_000)
    );
    // DeepSeek exact slug matches
    assert_eq!(
        lookup_context_window("deepseek", "deepseek-v4-flash"),
        Some(1_000_000)
    );
    assert_eq!(
        lookup_context_window("deepseek", "deepseek-v4-pro"),
        Some(1_000_000)
    );
    // Anthropic exact slug matches
    assert_eq!(
        lookup_context_window("anthropic", "claude-sonnet-4-6"),
        Some(1_000_000)
    );
    // Google exact slug matches
    assert_eq!(
        lookup_context_window("google", "gemini-2.5-pro"),
        Some(1_048_576)
    );
}

#[test]
#[serial_test::serial(catalog)]
fn cache_affinity_reads_the_provider_entry() {
    // The bundled overlay pins these dialects; every other provider is `Off`.
    assert_eq!(cache_affinity("openai"), CacheAffinity::PromptCacheKey);
    assert_eq!(
        cache_affinity("openrouter"),
        CacheAffinity::OpenRouterSession
    );
    assert_eq!(cache_affinity("anthropic"), CacheAffinity::Off);
    // An unknown provider resolves to the default that sends no hint rather
    // than to an invented dialect.
    assert_eq!(cache_affinity("nonexistent"), CacheAffinity::Off);
}

#[test]
#[serial_test::serial(catalog)]
fn cache_affinity_follows_a_catalog_swap() {
    // The resolver reads the live `ArcSwap`, not the compiled-in base: a swap
    // that flips the dialect must be visible immediately (and the guard restores
    // the bundled catalog on drop).
    let _restore = RestoreBundledOnDrop;
    let mut catalog = tiny_catalog();
    catalog[0].cache_affinity = CacheAffinity::PromptCacheKey;
    crate::catalog::replace_catalog(catalog);
    assert_eq!(cache_affinity("tiny-test"), CacheAffinity::PromptCacheKey);
}

#[test]
#[serial_test::serial(catalog)]
fn cache_retention_defaults_to_none_and_follows_a_catalog_swap() {
    // Nothing in the bundled overlay opts into extended retention yet, so every
    // model resolves to `None` — the mechanism ships dormant.
    assert_eq!(
        cache_retention("openai", "gpt-5.4"),
        CacheRetentionDialect::None
    );
    assert_eq!(
        cache_retention("nonexistent", "any"),
        CacheRetentionDialect::None
    );

    // The resolver reads the live model entry, so a swap that pins a dialect is
    // visible immediately.
    let _restore = RestoreBundledOnDrop;
    let mut catalog = tiny_catalog();
    catalog[0].models[0].cache_retention = CacheRetentionDialect::PromptCacheOptions;
    crate::catalog::replace_catalog(catalog);
    assert_eq!(
        cache_retention("tiny-test", "tiny-model"),
        CacheRetentionDialect::PromptCacheOptions,
    );
}

#[test]
#[serial_test::serial(catalog)]
fn lookup_context_window_unknown_provider() {
    assert_eq!(lookup_context_window("nonexistent", "any-model"), None);
}

#[test]
#[serial_test::serial(catalog)]
fn lookup_context_window_unknown_model() {
    assert_eq!(lookup_context_window("openai", "unknown-model-xyz"), None);
}

#[test]
#[serial_test::serial(catalog)]
fn model_reasoning_capability_openai_known_model() {
    let cap = model_reasoning_capability("openai", "gpt-5.4");
    assert_eq!(
        cap.available_effort_levels,
        vec!["off", "low", "medium", "high", "xhigh"]
    );
}

#[test]
#[serial_test::serial(catalog)]
fn model_reasoning_capability_openai_unknown_model() {
    let cap = model_reasoning_capability("openai", "gpt-4.1");
    assert_eq!(cap.available_effort_levels, [] as [String; 0]);
}

#[test]
#[serial_test::serial(catalog)]
fn model_reasoning_capability_anthropic_supported() {
    let cap = model_reasoning_capability("anthropic", "claude-sonnet-4-6");
    assert_eq!(
        cap.available_effort_levels,
        vec!["off", "low", "medium", "high"]
    );
}

#[test]
#[serial_test::serial(catalog)]
fn model_reasoning_capability_anthropic_unknown_model_uses_defaults() {
    // Unknown anthropic model → protocol defaults (best-effort fallback).
    let cap = model_reasoning_capability("anthropic", "claude-opus-3");
    assert_eq!(
        cap.available_effort_levels,
        vec!["off", "low", "medium", "high"]
    );
}

#[test]
#[serial_test::serial(catalog)]
fn model_reasoning_capability_anthropic_non_reasoning() {
    // A known non-reasoning model yields no effort levels. None of the
    // current anthropic models are non-reasoning, so assert against a
    // known non-reasoning OpenAI model on the openai provider instead.
    let cap = model_reasoning_capability("openai", "gpt-4.1");
    assert_eq!(cap.available_effort_levels, [] as [String; 0]);
}

#[test]
#[serial_test::serial(catalog)]
fn model_reasoning_capability_google_supported() {
    let cap = model_reasoning_capability("google", "gemini-2.5-pro");
    assert_eq!(cap.available_effort_levels, vec!["off", "on"]);
}

#[test]
#[serial_test::serial(catalog)]
fn model_reasoning_capability_google_unknown_model_uses_defaults() {
    // Unknown google model → protocol defaults (off/on).
    let cap = model_reasoning_capability("google", "gemini-9.9");
    assert_eq!(cap.available_effort_levels, vec!["off", "on"]);
}

#[test]
#[serial_test::serial(catalog)]
fn model_reasoning_capability_none_provider() {
    let cap = model_reasoning_capability("nonexistent", "any-model");
    assert_eq!(cap.available_effort_levels, [] as [String; 0]);
}

#[test]
#[serial_test::serial(catalog)]
fn model_request_format_known_model() {
    // OpenAI's catalog default is the Responses API.
    let fmt = model_request_format("openai", "gpt-4.1");
    assert_eq!(fmt, Some(RequestFormat::Responses));
}

#[test]
#[serial_test::serial(catalog)]
fn model_request_format_opencode_go_matches_gateway_docs() {
    assert_eq!(
        model_request_format("opencode-go", "deepseek-v4-flash"),
        Some(RequestFormat::ChatCompletions)
    );
}

#[test]
#[serial_test::serial(catalog)]
fn model_request_format_opencode_zen_gpt_family_uses_responses() {
    for model in ["gpt-5.4", "gpt-5.4-mini", "gpt-5.5", "gpt-5.6-luna"] {
        assert_eq!(
            model_request_format("opencode", model),
            Some(RequestFormat::Responses),
            "expected {model} to use the Responses API on the Zen gateway"
        );
    }
    assert_eq!(
        model_request_format("opencode", "deepseek-v4-flash"),
        Some(RequestFormat::ChatCompletions)
    );
}

#[test]
#[serial_test::serial(catalog)]
fn model_request_format_unknown_model() {
    let fmt = model_request_format("openai", "nonexistent-model");
    assert_eq!(fmt, None);
}

#[test]
#[serial_test::serial(catalog)]
fn model_request_format_unknown_provider() {
    let fmt = model_request_format("nope", "gpt-4.1");
    assert_eq!(fmt, None);
}

#[test]
#[serial_test::serial(catalog)]
fn model_reasoning_passback_openai_responses_model() {
    // gpt-5.4 uses the Responses API (responses = true) → chain via
    // previous_response_id / opaque reasoning items.
    assert_eq!(
        model_reasoning_passback("openai", "gpt-5.4"),
        ReasoningPassback::ResponseId
    );
}

#[test]
#[serial_test::serial(catalog)]
fn model_reasoning_passback_openai_chat_completions_model() {
    // A chat-completions OpenAI-protocol model would derive ToolLoop from
    // the protocol — but Cerebras' gpt-oss-120b is pinned to `none` by the
    // bundled overlay: the gateway rejects replayed `reasoning_content`,
    // so this model must never replay.
    assert_eq!(
        model_reasoning_passback("cerebras", "gpt-oss-120b"),
        ReasoningPassback::None
    );
}

#[test]
#[serial_test::serial(catalog)]
fn model_reasoning_passback_deepseek_derives_tool_loop() {
    // DeepSeek is a chat-completions OpenAI-protocol provider, so both
    // models derive the ToolLoop default (echo reasoning on tool-call
    // turns) without needing a per-model override.
    assert_eq!(
        model_reasoning_passback("deepseek", "deepseek-v4-flash"),
        ReasoningPassback::ToolLoop
    );
    assert_eq!(
        model_reasoning_passback("deepseek", "deepseek-v4-pro"),
        ReasoningPassback::ToolLoop
    );
}

#[test]
#[serial_test::serial(catalog)]
fn model_reasoning_passback_anthropic_keep_all_turns() {
    // Opus 4.5+ / later Opus, Sonnet 4.6+, and Fable 5 keep ALL prior
    // turns → echo reasoning on every turn.
    for model in [
        "claude-opus-4-5",
        "claude-opus-4-5-20251101",
        "claude-opus-4-6",
        "claude-opus-4-7",
        "claude-opus-5",
        "claude-sonnet-4-6",
        "claude-sonnet-5",
        "claude-fable-5",
    ] {
        assert_eq!(
            model_reasoning_passback("anthropic", model),
            ReasoningPassback::AllTurns,
            "expected {model} to keep all prior turns"
        );
    }
}

#[test]
#[serial_test::serial(catalog)]
fn model_reasoning_passback_anthropic_last_turn_only() {
    // Earlier Opus/Sonnet and all Haiku keep only the last turn → the
    // tool_loop minimum (echo reasoning on assistant tool-call messages).
    for model in [
        "claude-opus-4-1",
        "claude-opus-4-1-20250805",
        "claude-sonnet-4-5",
        "claude-sonnet-4-5-20250929",
        "claude-haiku-4-5",
        "claude-haiku-4-5-20251001",
    ] {
        assert_eq!(
            model_reasoning_passback("anthropic", model),
            ReasoningPassback::ToolLoop,
            "expected {model} to keep only the last turn"
        );
    }
}

#[test]
#[serial_test::serial(catalog)]
fn model_reasoning_passback_google_model() {
    // Gemini → encrypted thought signatures.
    assert_eq!(
        model_reasoning_passback("google", "gemini-2.5-pro"),
        ReasoningPassback::Signature
    );
}

#[test]
#[serial_test::serial(catalog)]
fn model_reasoning_passback_none_provider() {
    // Unknown provider — no protocol to derive a default from.
    assert_eq!(
        model_reasoning_passback("nonexistent", "any-model"),
        ReasoningPassback::None
    );
}

#[test]
#[serial_test::serial(catalog)]
fn model_reasoning_passback_unknown_model_uses_protocol_default() {
    // Unknown model on a known anthropic provider → AllTurns (default).
    assert_eq!(
        model_reasoning_passback("anthropic", "claude-opus-3"),
        ReasoningPassback::AllTurns
    );
    // Unknown model on a known google provider → Signature (default).
    assert_eq!(
        model_reasoning_passback("google", "gemini-9.9"),
        ReasoningPassback::Signature
    );
    // Unknown model on a known OpenAI-protocol provider → chat-completions
    // default (ToolLoop), matching ServiceConfig::default_request_format.
    assert_eq!(
        model_reasoning_passback("openai", "gpt-9.9-unknown"),
        ReasoningPassback::ToolLoop
    );
}

#[test]
#[serial_test::serial(catalog)]
fn model_reasoning_passback_explicit_override_beats_protocol_default() {
    // claude-haiku-4-5 carries an explicit tool_loop override that beats
    // the Anthropic keep-all-turns protocol default.
    let entry = lookup_provider("anthropic").unwrap();
    let model = entry
        .models
        .iter()
        .find(|m| m.model == "claude-haiku-4-5")
        .unwrap();
    assert_eq!(model.reasoning_passback, Some(ReasoningPassback::ToolLoop));
}

#[test]
#[serial_test::serial(catalog)]
fn resolve_passback_unset_uses_protocol_default() {
    // `None` (unset) on Anthropic → the keep-all-turns protocol default.
    assert_eq!(
        resolve_passback(None, ProviderProtocol::AnthropicMessages, false),
        ReasoningPassback::AllTurns
    );
}

#[test]
#[serial_test::serial(catalog)]
fn resolve_passback_explicit_none_beats_protocol_default() {
    // The new capability: an explicit `none` override suppresses replay
    // even on a protocol whose default would echo (OpenAi chat-completions
    // → ToolLoop) — a Cerebras-style model that rejects replayed
    // `reasoning_content` can be pinned to never replay without inventing
    // a provider.
    let protocol = ProviderProtocol::OpenAi {
        max_tokens_field: MaxTokensField::MaxCompletionTokens,
    };
    assert_eq!(
        resolve_passback(Some(ReasoningPassback::None), protocol, false),
        ReasoningPassback::None
    );
}

#[test]
#[serial_test::serial(catalog)]
fn resolve_passback_explicit_override_beats_responses_default() {
    // An explicit ToolLoop override wins even when the model uses the
    // Responses API, whose protocol default would be ResponseId.
    let protocol = ProviderProtocol::OpenAi {
        max_tokens_field: MaxTokensField::MaxCompletionTokens,
    };
    assert_eq!(
        resolve_passback(Some(ReasoningPassback::ToolLoop), protocol, true),
        ReasoningPassback::ToolLoop
    );
}

#[test]
#[serial_test::serial(catalog)]
fn resolve_passback_unset_openai_responses_uses_response_id() {
    // `None` (unset) on an OpenAI-protocol Responses model → chain
    // continuity via previous_response_id / opaque reasoning items.
    let protocol = ProviderProtocol::OpenAi {
        max_tokens_field: MaxTokensField::MaxCompletionTokens,
    };
    assert_eq!(
        resolve_passback(None, protocol, true),
        ReasoningPassback::ResponseId
    );
}

#[test]
#[serial_test::serial(catalog)]
fn bundled_toml_reasoning_passback_parses() {
    // TOML files WITHOUT the field must still parse (serde default None).
    // Spot-check one untouched provider resolves via protocol default.
    assert_eq!(
        model_reasoning_passback("groq", "groq/compound"),
        ReasoningPassback::ToolLoop
    );
}

#[test]
#[serial_test::serial(catalog)]
fn chat_reasoning_echo_resolves_ingested_snapshot_field() {
    // The snapshot's deepseek models carry `interleaved: {field:
    // "reasoning_content"}` — the echo field is a FACT, no name heuristic
    // involved, and the field NAME survives ingestion (not just "required:
    // yes").
    for (provider, model) in [
        ("deepseek", "deepseek-v4-flash"),
        ("deepseek", "deepseek-v4-pro"),
        ("zhipuai", "glm-5"),
    ] {
        assert_eq!(
            chat_reasoning_echo(provider, model),
            ChatReasoningEcho {
                field: Some(ChatReasoningField::ReasoningContent),
                require_present: true,
            },
            "{provider}/{model}"
        );
    }
}

#[test]
#[serial_test::serial(catalog)]
fn chat_reasoning_echo_resolves_structured_details_fact() {
    // The snapshot's OpenRouter/Ollama routes declare `reasoning_details` in
    // their `interleaved`; the resolver surfaces the structured field so the
    // adapter replays the array instead of guessing a scalar key. The array
    // is a DESTINATION only: no presence requirement is derived for it, so
    // an assistant message with nothing to replay simply omits the key.
    for (provider, model) in [
        ("openrouter", "moonshotai/kimi-k2.5"),
        ("openrouter", "minimax/minimax-m2"),
        ("ollama-cloud", "qwen3.5:397b"),
    ] {
        assert_eq!(
            chat_reasoning_echo(provider, model),
            ChatReasoningEcho {
                field: Some(ChatReasoningField::ReasoningDetails),
                require_present: false,
            },
            "{provider}/{model}"
        );
    }
}

#[test]
#[serial_test::serial(catalog)]
fn chat_reasoning_echo_presence_is_its_own_catalog_fact() {
    // A structured destination CAN be paired with a presence requirement —
    // the two facts are independent, so a catalog entry (or overlay) that
    // sets both resolves both. This pins that the resolver reads the
    // requirement from the catalog rather than inferring it from the field.
    let _restore = RestoreBundledOnDrop;
    let bundled = catalog_snapshot();
    crate::catalog::replace_catalog({
        let mut c = tiny_catalog();
        c[0].models[0].reasoning_echo = Some(ChatReasoningField::ReasoningDetails);
        c[0].models[0].reasoning_echo_required = Some(true);
        c
    });
    assert_eq!(
        chat_reasoning_echo("tiny-test", "tiny-model"),
        ChatReasoningEcho {
            field: Some(ChatReasoningField::ReasoningDetails),
            require_present: true,
        }
    );
    // And the reverse independence: the same destination with the
    // requirement unset resolves no presence.
    crate::catalog::replace_catalog({
        let mut c = tiny_catalog();
        c[0].models[0].reasoning_echo = Some(ChatReasoningField::ReasoningDetails);
        c
    });
    assert_eq!(
        chat_reasoning_echo("tiny-test", "tiny-model"),
        ChatReasoningEcho {
            field: Some(ChatReasoningField::ReasoningDetails),
            require_present: false,
        }
    );
    crate::catalog::replace_catalog(bundled.to_vec());
}

#[test]
#[serial_test::serial(catalog)]
fn chat_reasoning_echo_presence_pin_without_a_destination_resolves() {
    // The presence requirement stands on its own: an overlay can pin
    // `reasoning_echo_required = true` for a route whose destination the
    // snapshot never named (or named elsewhere), and the resolver must
    // surface it rather than discarding the pin for lack of a field. The
    // adapter then emits the chat default key empty.
    let _restore = RestoreBundledOnDrop;
    let bundled = catalog_snapshot();
    crate::catalog::replace_catalog({
        let mut c = tiny_catalog();
        c[0].models[0].reasoning_echo = None;
        c[0].models[0].reasoning_echo_required = Some(true);
        c
    });
    assert_eq!(
        chat_reasoning_echo("tiny-test", "tiny-model"),
        ChatReasoningEcho {
            field: None,
            require_present: true,
        }
    );
    crate::catalog::replace_catalog(bundled.to_vec());
}

#[test]
#[serial_test::serial(catalog)]
fn chat_reasoning_echo_responses_guard_covers_a_presence_only_pin() {
    // The Responses guard applies to the whole policy, not just to a declared
    // destination: a presence-only pin on a Responses model resolves to the
    // default too, because presence is a chat-completions wire concern.
    let _restore = RestoreBundledOnDrop;
    let bundled = catalog_snapshot();
    crate::catalog::replace_catalog({
        let mut c = tiny_catalog();
        c[0].models[0].openai_responses = true;
        c[0].models[0].reasoning_echo = None;
        c[0].models[0].reasoning_echo_required = Some(true);
        c
    });
    assert_eq!(
        chat_reasoning_echo("tiny-test", "tiny-model"),
        ChatReasoningEcho::default()
    );
    crate::catalog::replace_catalog(bundled.to_vec());
}

#[test]
#[serial_test::serial(catalog)]
fn chat_reasoning_echo_overlay_field_wins() {
    // glm-5.3-flash is not in the snapshot (upstream exposes it under
    // internal names); the bundled overlay defines it wholesale with an
    // explicit echo field — the only path for models the snapshot does not
    // cover.
    assert_eq!(
        chat_reasoning_echo("opencode-go", "glm-5.3-flash").field,
        Some(ChatReasoningField::ReasoningContent)
    );
}

#[test]
#[serial_test::serial(catalog)]
fn chat_reasoning_echo_default_without_the_fact() {
    // glm-4.5 is cataloged but carries no interleaved echo fact in the
    // snapshot → no declared shape: the adapter echoes the observed field.
    assert_eq!(
        chat_reasoning_echo("zhipuai", "glm-4.5"),
        ChatReasoningEcho::default()
    );
}

#[test]
#[serial_test::serial(catalog)]
fn chat_reasoning_echo_unknown_model_is_default() {
    // No name heuristic applies: a plainly-named but uncataloged model
    // resolves to the default — no declared field, no presence requirement.
    // Catalog gaps surface as upstream errors (the provider's own 400)
    // instead of a silent unauditable guess.
    for (provider, model) in [
        ("deepseek", "deepseek-v99-unknown"),
        ("futurmix", "deepseek-chat-unknown"),
        ("no-such-provider", "deepseek-chat"),
    ] {
        assert_eq!(
            chat_reasoning_echo(provider, model),
            ChatReasoningEcho::default(),
            "{provider}/{model}"
        );
    }
}

#[test]
#[serial_test::serial(catalog)]
fn chat_reasoning_echo_declares_a_non_content_field() {
    // The policy is not hard-wired to `reasoning_content`: a route that
    // wants a different scalar field declares it and gets it.
    let _restore = RestoreBundledOnDrop;
    let bundled = catalog_snapshot();
    crate::catalog::replace_catalog({
        let mut c = tiny_catalog();
        c[0].models[0].reasoning_echo = Some(ChatReasoningField::ReasoningText);
        c
    });
    assert_eq!(
        chat_reasoning_echo("tiny-test", "tiny-model").field,
        Some(ChatReasoningField::ReasoningText)
    );
    crate::catalog::replace_catalog(bundled.to_vec());
}

#[test]
#[serial_test::serial(catalog)]
fn chat_reasoning_echo_never_applies_to_responses_models() {
    // `reasoning_content` is a chat-completions wire concept: even if a
    // model entry carries the fact (e.g. a mis-merge between a
    // chat-completions model and its Responses twin), the Responses guard
    // must suppress it — the wire format wins over the fact. Pin the guard
    // with a synthetic catalog entry carrying BOTH `openai_responses: true`
    // and a declared echo field.
    let _restore = RestoreBundledOnDrop;
    let bundled = catalog_snapshot();
    crate::catalog::replace_catalog({
        let mut c = tiny_catalog();
        c[0].models[0].openai_responses = true;
        c[0].models[0].reasoning_echo = Some(ChatReasoningField::ReasoningContent);
        c
    });
    assert_eq!(
        chat_reasoning_echo("tiny-test", "tiny-model"),
        ChatReasoningEcho::default(),
        "Responses models must never receive a chat reasoning field"
    );

    // The negative control: the identical fact on a chat-completions twin
    // (openai_responses = false) DOES resolve, so the guard — not a
    // missing fact — is what produced the default above.
    crate::catalog::replace_catalog({
        let mut c = tiny_catalog();
        c[0].models[0].openai_responses = false;
        c[0].models[0].reasoning_echo = Some(ChatReasoningField::ReasoningContent);
        c[0].models[0].reasoning_echo_required = Some(true);
        c
    });
    assert_eq!(
        chat_reasoning_echo("tiny-test", "tiny-model"),
        ChatReasoningEcho {
            field: Some(ChatReasoningField::ReasoningContent),
            require_present: true,
        }
    );

    // Explicit restore (the guard also restores on panic) so the test is
    // self-documenting about returning the global to its initial state.
    crate::catalog::replace_catalog(bundled.to_vec());
}

/// Clone `base` under a new slug with a replacement model list — the cheapest
/// way to build a multi-provider synthetic catalog in these tests.
fn derive_provider(base: &ProviderEntry, slug: &str, models: Vec<ModelEntry>) -> ProviderEntry {
    let mut provider = base.clone();
    provider.slug = slug.to_string();
    provider.default_model = models.first().map_or_default(|model| model.model.clone());
    provider.models = models;
    provider
}

/// An offering that records no capacity fact of its own and only (optionally)
/// declares a canonical metadata link.
fn linked(model: &str, canonical: Option<&str>) -> ModelEntry {
    ModelEntry {
        model: model.to_string(),
        canonical_model_id: canonical.map(str::to_string),
        ..Default::default()
    }
}

#[test]
fn canonical_link_requires_two_non_empty_segments() {
    // A half-written link must read as "no link", never as "same-named model on
    // this provider" or as an empty slug that happens to match.
    assert_eq!(
        parse_canonical_link("openai/gpt-5.6-luna"),
        Some(("openai", "gpt-5.6-luna"))
    );
    assert_eq!(
        parse_canonical_link("nope/gpt-5.6-luna"),
        Some(("nope", "gpt-5.6-luna"))
    );
    assert_eq!(parse_canonical_link("gpt-5.6-luna"), None);
    assert_eq!(parse_canonical_link("/gpt-5.6-luna"), None);
    assert_eq!(parse_canonical_link("openai/"), None);
    assert_eq!(parse_canonical_link("/"), None);
    assert_eq!(parse_canonical_link(""), None);
    // The provider segment ends at the FIRST separator: a model id that itself
    // carries a path (a resold `lab/model` id) is kept whole.
    assert_eq!(
        parse_canonical_link("tanzu/openai/gpt-oss-120b"),
        Some(("tanzu", "openai/gpt-oss-120b"))
    );
}

#[test]
#[serial_test::serial(catalog)]
fn canonical_link_is_exact_and_single_hop() {
    let _restore = RestoreBundledOnDrop;

    // Three synthetic providers: an alias pointing at a middle offering, which
    // itself points at the record holding the real capacity. A canonical link
    // is a curated fact naming ONE record, so the chain stops after the first
    // hop — chasing it further would let a stale intermediate link silently
    // attribute an unrelated model's limits.
    let base = tiny_catalog().remove(0);
    // `tiny-test/tiny-model` carries context_window 4096 and max output 1024.
    let mut catalog = vec![base.clone()];
    catalog.push(derive_provider(
        &base,
        "alias-co",
        vec![
            // Direct link: resolves.
            linked("direct-1", Some("tiny-test/tiny-model")),
            // One hop through a link-only middle record: does NOT resolve.
            linked("chained-1", Some("middle-co/mid-1")),
            // Exact-match only: a prefix of a real model id is not a match.
            linked("prefix-1", Some("tiny-test/tiny")),
            // Unknown target provider.
            linked("nowhere-1", Some("nowhere-co/tiny-model")),
            // Malformed link shapes all mean "no link".
            linked("sloppy-1", Some("tiny-test")),
            linked("sloppy-2", Some("/tiny-model")),
            linked("sloppy-3", Some("tiny-test/")),
        ],
    ));
    catalog.push(derive_provider(
        &base,
        "middle-co",
        vec![linked("mid-1", Some("tiny-test/tiny-model"))],
    ));
    crate::catalog::replace_catalog(catalog);

    assert_eq!(
        lookup_context_window("alias-co", "direct-1"),
        Some(4096),
        "a direct canonical link supplies the offering's capacity fact"
    );
    assert_eq!(lookup_max_output_tokens("alias-co", "direct-1"), Some(1024));
    for unresolved in [
        "chained-1",
        "prefix-1",
        "nowhere-1",
        "sloppy-1",
        "sloppy-2",
        "sloppy-3",
    ] {
        assert_eq!(
            lookup_context_window("alias-co", unresolved),
            None,
            "{unresolved} must not borrow another record's capacity"
        );
        assert_eq!(lookup_max_output_tokens("alias-co", unresolved), None);
    }
}

#[test]
#[serial_test::serial(catalog)]
fn portable_capacity_facts_use_only_explicit_canonical_identity() {
    let _restore = RestoreBundledOnDrop;
    // The ChatGPT offering has no local limits. Its explicit overlay link reads
    // OpenAI's portable capacity facts without changing the request model id.
    for model in ["gpt-5.6-luna", "gpt-5.6-sol", "gpt-5.6-terra"] {
        assert_eq!(
            lookup_context_window("chatgpt", model),
            Some(1_050_000),
            "{model} context window comes from its canonical OpenAI record"
        );
        assert_eq!(
            lookup_max_output_tokens("chatgpt", model),
            Some(128_000),
            "{model} output ceiling comes from its canonical OpenAI record"
        );
    }

    let mut catalog = catalog_snapshot().to_vec();
    let chatgpt = catalog
        .iter_mut()
        .find(|provider| provider.slug == "chatgpt")
        .expect("bundled ChatGPT provider");
    let luna = chatgpt
        .models
        .iter_mut()
        .find(|model| model.model == "gpt-5.6-luna")
        .expect("bundled Luna offering");
    // A known exact fact wins over canonical metadata.
    luna.context_window = 42_000;
    luna.max_output_tokens = 4_000;
    crate::catalog::replace_catalog(catalog.clone());
    assert_eq!(
        lookup_context_window("chatgpt", "gpt-5.6-luna"),
        Some(42_000)
    );
    assert_eq!(
        lookup_max_output_tokens("chatgpt", "gpt-5.6-luna"),
        Some(4_000)
    );

    // Clearing the explicit link proves a matching model string alone cannot
    // cross provider boundaries.
    let luna = catalog
        .iter_mut()
        .find(|provider| provider.slug == "chatgpt")
        .and_then(|provider| {
            provider
                .models
                .iter_mut()
                .find(|model| model.model == "gpt-5.6-luna")
        })
        .expect("bundled Luna offering");
    luna.context_window = 0;
    luna.max_output_tokens = 0;
    luna.canonical_model_id = None;
    crate::catalog::replace_catalog(catalog);
    assert_eq!(lookup_context_window("chatgpt", "gpt-5.6-luna"), None);
    assert_eq!(lookup_max_output_tokens("chatgpt", "gpt-5.6-luna"), None);
}

#[test]
#[serial_test::serial(catalog)]
fn lookup_max_output_tokens_mirrors_context_window() {
    // Real snapshot facts: gpt-5.4's limit.output.
    assert_eq!(lookup_max_output_tokens("openai", "gpt-5.4"), Some(128_000));
    assert_eq!(
        lookup_max_output_tokens("zhipuai", "glm-5.2"),
        Some(131_072)
    );
    // Unknown provider/model → None.
    assert_eq!(lookup_max_output_tokens("nope", "gpt-5.4"), None);
    assert_eq!(lookup_max_output_tokens("openai", "not-a-model"), None);
}

#[test]
#[serial_test::serial(catalog)]
fn lookup_max_output_tokens_zero_is_unknown() {
    let _restore = RestoreBundledOnDrop;
    let bundled = catalog_snapshot();
    // A model whose entry records 0 (no snapshot fact) resolves None,
    // mirroring the context_window convention.
    crate::catalog::replace_catalog({
        let mut c = tiny_catalog();
        c[0].models[0].max_output_tokens = 0;
        c
    });
    assert_eq!(lookup_max_output_tokens("tiny-test", "tiny-model"), None);
    // Restoring the bundled catalog removes the synthetic provider — the
    // restore is verified against a real snapshot fact, not tiny-test.
    crate::catalog::replace_catalog(bundled.to_vec());
    assert_eq!(lookup_max_output_tokens("openai", "gpt-5.4"), Some(128_000));
    assert_eq!(lookup_max_output_tokens("tiny-test", "tiny-model"), None);
}

#[test]
#[serial_test::serial(catalog)]
fn model_supports_temperature_resolves_ingested_flag() {
    // The snapshot marks GPT-5.4 temperature:false (reasoning-first
    // models reject the parameter); GLM models accept it.
    assert!(!model_supports_temperature("openai", "gpt-5.4"));
    assert!(model_supports_temperature("zhipuai", "glm-4.5"));
    // Unknown models/providers stay permissive: models.dev only records
    // the fact when the answer is "no", so absence must not drop the
    // parameter from requests to untracked models.
    assert!(model_supports_temperature("openai", "gpt-99-unknown"));
    assert!(model_supports_temperature("no-such-provider", "any"));
}

#[test]
#[serial_test::serial(catalog)]
fn replace_catalog_swaps_what_lookup_sees() {
    // Swapping the process-wide catalog must be visible to readers
    // immediately (the ArcSwap store is atomic). Restore the bundled
    // catalog on drop so a failure here cannot poison later tests.
    let _restore = RestoreBundledOnDrop;
    let bundled = catalog_snapshot();
    assert!(lookup_provider("openai").is_some());

    crate::catalog::replace_catalog(tiny_catalog());

    let entry = lookup_provider("tiny-test").expect("swapped catalog has tiny-test");
    assert_eq!(entry.slug, "tiny-test");
    assert_eq!(entry.base_url, "https://tiny-test.example/v1");
    // The bundled catalog is gone while the tiny one is installed.
    assert!(lookup_provider("openai").is_none());
    assert_eq!(crate::catalog::all_slugs(), vec!["tiny-test".to_string()]);

    // Sanity-check the lookup functions read through the same global.
    assert_eq!(lookup_context_window("tiny-test", "tiny-model"), Some(4096));
    assert_eq!(
        model_reasoning_capability("tiny-test", "tiny-model").available_effort_levels,
        vec!["off", "high"]
    );

    // Explicit restore (the guard also restores on panic) so the test is
    // self-documenting about returning the global to its initial state.
    crate::catalog::replace_catalog(bundled.to_vec());
    assert!(lookup_provider("openai").is_some());
}

#[test]
#[serial_test::serial(catalog)]
fn model_supports_vision_resolves_from_catalog() {
    let _restore = RestoreBundledOnDrop;
    // The bundled overlay adds deepseek-v4-flash-vision-exp with vision on;
    // a known text-only model resolves false; an unknown model defaults false.
    assert!(model_supports_vision(
        "deepseek",
        "deepseek-v4-flash-vision-exp"
    ));
    assert!(!model_supports_vision("deepseek", "deepseek-v4-pro"));
    assert!(!model_supports_vision("no-such-provider", "any-model"));
}

#[test]
#[serial_test::serial(catalog)]
fn model_supports_image_output_resolves_from_catalog() {
    let _restore = RestoreBundledOnDrop;
    // Synthetic two-flag catalog: the two modalities sides are
    // independent facts, and unknown provider/model default false.
    let bundled = catalog_snapshot();
    crate::catalog::replace_catalog({
        let mut c = tiny_catalog();
        c[0].models[0].supports_vision = true;
        c[0].models[0].supports_image_output = true;
        c
    });
    assert!(model_supports_image_output("tiny-test", "tiny-model"));
    // Dropping only the output flag leaves vision on: they do not imply
    // each other.
    crate::catalog::replace_catalog({
        let mut c = tiny_catalog();
        c[0].models[0].supports_vision = true;
        c
    });
    assert!(!model_supports_image_output("tiny-test", "tiny-model"));
    assert!(model_supports_vision("tiny-test", "tiny-model"));
    // Unknown provider/model → false, the safe default.
    crate::catalog::replace_catalog(bundled.to_vec());
    assert!(!model_supports_image_output(
        "no-such-provider",
        "any-model"
    ));
    assert!(!model_supports_image_output("openai", "not-a-real-model"));
}

#[test]
#[serial_test::serial(catalog)]
fn image_models_for_provider_lists_flagged_models_in_catalog_order() {
    let _restore = RestoreBundledOnDrop;
    // A synthetic multi-model provider: only the flagged entries are
    // listed, in the catalog's insertion order.
    crate::catalog::replace_catalog({
        let mut c = tiny_catalog();
        let flagged = |id: &str| crate::catalog::ModelEntry {
            model: id.to_string(),
            supports_image_output: true,
            ..Default::default()
        };
        c[0].models = vec![
            flagged("image-a"),
            crate::catalog::ModelEntry::default(), // unflagged
            flagged("image-b"),
        ];
        c
    });
    assert_eq!(
        image_models_for_provider("tiny-test"),
        vec!["image-a".to_string(), "image-b".to_string()]
    );
    // Unknown provider → empty, never an error.
    assert_eq!(
        image_models_for_provider("no-such-provider"),
        [] as [String; 0]
    );
}

#[test]
#[serial_test::serial(catalog)]
fn image_edit_models_for_provider_lists_flagged_models_in_catalog_order() {
    let _restore = RestoreBundledOnDrop;
    // Only entries with the edit flag are listed, in insertion order —
    // mirroring `image_models_for_provider` over the edit flag.
    crate::catalog::replace_catalog({
        let mut c = tiny_catalog();
        let flagged = |id: &str| crate::catalog::ModelEntry {
            model: id.to_string(),
            supports_image_edit: true,
            ..Default::default()
        };
        c[0].models = vec![
            flagged("edit-a"),
            crate::catalog::ModelEntry::default(), // unflagged
            flagged("edit-b"),
        ];
        c
    });
    assert_eq!(
        image_edit_models_for_provider("tiny-test"),
        vec!["edit-a".to_string(), "edit-b".to_string()]
    );
    // Unknown provider → empty, never an error.
    assert_eq!(
        image_edit_models_for_provider("no-such-provider"),
        [] as [String; 0]
    );
}

#[test]
#[serial_test::serial(catalog)]
fn bundled_overlay_lists_edit_models_for_openai_and_fal() {
    // Read-only against the REAL bundled catalog: the edit flag is
    // overlay-only (models.dev has no edit modality), so these assertions
    // pin the bundled overlay plumbing end to end. OpenAI gpt-image models
    // and the wholesale fal provider both carry it.
    assert!(
        image_edit_models_for_provider("openai")
            .iter()
            .any(|m| m == "gpt-image-1")
    );
    assert!(
        image_edit_models_for_provider("fal")
            .iter()
            .any(|m| m == "fal-ai/flux-2-pro")
    );
    // fal sets BOTH flags — the same models serve generation and editing.
    assert!(
        image_models_for_provider("fal")
            .iter()
            .any(|m| m == "fal-ai/flux-2-pro")
    );
    // An image-output provider with no edit pins (glm-image) resolves to an
    // empty edit list — generation and editing are independent facts.
    assert_eq!(image_edit_models_for_provider("zai"), [] as [String; 0]);
}

#[test]
#[serial_test::serial(catalog)]
fn bundled_overlay_lists_xai_edit_models() {
    // The REAL bundled catalog: the xAI grok-imagine image models carry the
    // edit flag solely from the bundled overlay (models.dev has no edit
    // modality), so this pins the overlay plumbing end to end for xAI. The
    // adapter routes this slug to a JSON `/images/edits` body
    // (images::is_xai_image_provider_slug); the tool reads these through
    // image_edit_models_for_provider("xai"). Read-only (no catalog swap),
    // so no restore guard is needed.
    let edit_models = image_edit_models_for_provider("xai");
    assert!(edit_models.iter().any(|m| m == "grok-imagine-image"));
    assert!(
        edit_models
            .iter()
            .any(|m| m == "grok-imagine-image-quality")
    );
    // The grok-imagine video model is image-INPUT only and must not be an
    // edit candidate.
    assert!(!edit_models.iter().any(|m| m == "grok-imagine-video"));
}

#[test]
#[serial_test::serial(catalog)]
fn bundled_overlay_lists_google_edit_models() {
    // The REAL bundled catalog: the Gemini image models carry the edit flag
    // solely from the bundled overlay (models.dev has an image-OUTPUT
    // modality but no edit modality), so this pins the overlay plumbing end
    // to end for Google. Gemini generates AND edits through the
    // generateContent API (images::GoogleImageClient); the tool reads these
    // through image_edit_models_for_provider("google"). Read-only (no catalog
    // swap), so no restore guard is needed.
    let edit_models = image_edit_models_for_provider("google");
    for model in [
        "gemini-2.5-flash-image",
        "gemini-3-pro-image",
        "gemini-3-pro-image-preview",
        "gemini-3.1-flash-image",
        "gemini-3.1-flash-image-preview",
        "gemini-3.1-flash-lite-image",
    ] {
        assert!(
            edit_models.iter().any(|m| m == model),
            "{model} must be a Google edit candidate: {edit_models:?}"
        );
    }
    // A non-image Gemini model must never be an edit candidate.
    assert!(!edit_models.iter().any(|m| m == "gemini-2.5-pro"));
}

#[test]
#[serial_test::serial(catalog)]
fn bundled_overlay_flips_image_output_flag() {
    // The bundled overlay can override the flag like vision's. Swap in a
    // catalog with the flag ON and overlay it back OFF (and vice versa)
    // via merge_overlay to pin the override plumbing end to end.
    let _restore = RestoreBundledOnDrop;
    let merged = crate::catalog::merge_overlay(
        &tiny_catalog(),
        "[provider.tiny-test.models.\"tiny-model\"]\nsupports_image_output = true\n",
    );
    crate::catalog::replace_catalog(merged);
    assert!(model_supports_image_output("tiny-test", "tiny-model"));
    assert_eq!(
        image_models_for_provider("tiny-test"),
        vec!["tiny-model".to_string()]
    );
    // And back off again.
    let off = crate::catalog::merge_overlay(
        &tiny_catalog(),
        "[provider.tiny-test.models.\"tiny-model\"]\nsupports_image_output = false\n",
    );
    crate::catalog::replace_catalog(off);
    assert!(!model_supports_image_output("tiny-test", "tiny-model"));
    assert_eq!(image_models_for_provider("tiny-test"), [] as [String; 0]);
}

#[test]
#[serial_test::serial(catalog)]
fn bundled_overlay_pins_max_tokens_field_for_the_zhipu_slugs() {
    // z.ai's chat-completions API documents only `max_tokens` (no
    // `max_completion_tokens`), but the models.dev derivation defaults
    // every OpenAI-protocol provider to `max_completion_tokens`. The
    // bundled overlay must win for both Zhipu slugs — this pins the
    // field-wise provider-scalar merge end to end for the real catalog.
    // Read-only (no catalog swap), so no restore guard is needed.
    for slug in ["zai", "zhipuai"] {
        let entry = lookup_provider(slug).unwrap_or_else(|| panic!("{slug} in bundled catalog"));
        assert!(
            matches!(
                entry.protocol,
                ProviderProtocol::OpenAi {
                    max_tokens_field: MaxTokensField::MaxTokens
                }
            ),
            "{slug} must resolve max_tokens, not max_completion_tokens"
        );
    }
}

#[test]
#[serial_test::serial(catalog)]
fn bundled_overlay_pins_glm_5_3_flash_reasoning_levels() {
    // The REAL bundled catalog: glm-5.3-flash exists only as the
    // wholesale opencode-go overlay entry, and z.ai's chat docs accept
    // ONLY low/high/max as reasoning_effort wire values for the 5.3
    // generation. The overlay must pin that documented set (with `off`
    // offered UI-side as omit/minimum) so the UI never advertises
    // slugs the API would reject. Read-only (no catalog swap), so no
    // restore guard is needed.
    let cap = model_reasoning_capability("opencode-go", "glm-5.3-flash");
    assert_eq!(
        cap.available_effort_levels,
        vec!["off", "low", "high", "max"]
    );
}

#[test]
#[serial_test::serial(catalog)]
fn bundled_overlay_lists_glm_image_under_the_zai_and_zhipuai_slugs() {
    // The REAL bundled catalog: glm-image is not in the models.dev
    // snapshot, so its image-output flag comes solely from the bundled
    // overlay's `[provider.<slug>.models."glm-image"]` entries — this
    // pins the overlay plumbing end to end for the Zhipu image model.
    // Read-only (no catalog swap), so no restore guard is needed.
    assert!(model_supports_image_output("zai", "glm-image"));
    assert!(model_supports_image_output("zhipuai", "glm-image"));
    // The tool's candidate list picks it up under both slugs, and it is
    // the single (hence priority-fallback) image candidate for each.
    assert_eq!(
        image_models_for_provider("zai"),
        vec!["glm-image".to_string()]
    );
    assert_eq!(
        image_models_for_provider("zhipuai"),
        vec!["glm-image".to_string()]
    );
    // Sanity: chat models under the same slug stay non-image.
    assert!(!model_supports_image_output("zai", "glm-5.1"));
}

#[test]
#[serial_test::serial(catalog)]
fn model_cost_returns_recorded_cost_and_none_when_absent() {
    let _restore = RestoreBundledOnDrop;
    let bundled = catalog_snapshot();
    crate::catalog::replace_catalog({
        let mut c = tiny_catalog();
        c[0].models[0].cost = Some(ModelCost {
            input: 1.5,
            output: 6.0,
            cache_read: Some(0.15),
            cache_write: None,
        });
        c[0].models.push(crate::catalog::ModelEntry {
            model: "no-cost".into(),
            ..Default::default()
        });
        c
    });
    let cost = model_cost("tiny-test", "tiny-model").expect("recorded cost");
    assert_eq!(cost.input, 1.5);
    assert_eq!(cost.output, 6.0);
    assert_eq!(cost.cache_read, Some(0.15));
    assert_eq!(cost.cache_write, None);
    // A model with no cost fact, an unknown model, and an unknown provider
    // all resolve None.
    assert_eq!(model_cost("tiny-test", "no-cost"), None);
    assert_eq!(model_cost("tiny-test", "no-such-model"), None);
    assert_eq!(model_cost("no-such-provider", "tiny-model"), None);
    crate::catalog::replace_catalog(bundled.to_vec());
}

#[test]
#[serial_test::serial(catalog)]
fn prompt_cache_ttl_prefers_model_override_then_provider_default() {
    let _restore = RestoreBundledOnDrop;
    let bundled = catalog_snapshot();
    // Provider default only.
    crate::catalog::replace_catalog({
        let mut c = tiny_catalog();
        c[0].prompt_cache = Some(PromptCacheTtl {
            short_secs: Some(300),
            long_secs: Some(3_600),
        });
        c
    });
    let ttl = prompt_cache_ttl("tiny-test", "tiny-model").expect("provider default");
    assert_eq!(ttl.short_secs, Some(300));
    assert_eq!(ttl.long_secs, Some(3_600));

    // The model override wins wholesale over the provider default (its
    // unset long tier does NOT fall back to the provider's).
    crate::catalog::replace_catalog({
        let mut c = tiny_catalog();
        c[0].prompt_cache = Some(PromptCacheTtl {
            short_secs: Some(300),
            long_secs: Some(3_600),
        });
        c[0].models[0].prompt_cache = Some(PromptCacheTtl {
            short_secs: Some(60),
            long_secs: None,
        });
        c
    });
    let ttl = prompt_cache_ttl("tiny-test", "tiny-model").expect("model override");
    assert_eq!(ttl.short_secs, Some(60));
    assert_eq!(ttl.long_secs, None);

    // Neither level declares a TTL → None; unknown provider/model → None.
    crate::catalog::replace_catalog(tiny_catalog());
    assert_eq!(prompt_cache_ttl("tiny-test", "tiny-model"), None);
    assert_eq!(prompt_cache_ttl("no-such-provider", "tiny-model"), None);
    assert_eq!(prompt_cache_ttl("tiny-test", "no-such-model"), None);
    crate::catalog::replace_catalog(bundled.to_vec());
}

#[test]
#[serial_test::serial(catalog)]
fn bundled_overlay_sets_anthropic_prompt_cache_ttl() {
    // The REAL bundled catalog: the anthropic provider default comes solely
    // from the bundled overlay's `[provider.anthropic]` TTL keys, so this
    // pins the overlay plumbing end to end. Read-only (no catalog swap), so
    // no restore guard is needed.
    let entry = lookup_provider("anthropic").expect("anthropic in bundled catalog");
    let ttl = entry.prompt_cache.expect("anthropic provider default");
    assert_eq!(ttl.short_secs, Some(300));
    assert_eq!(ttl.long_secs, Some(3_600));
    // A real model resolves the provider default (no per-model override).
    assert_eq!(
        prompt_cache_ttl("anthropic", "claude-sonnet-4-6"),
        Some(PromptCacheTtl {
            short_secs: Some(300),
            long_secs: Some(3_600),
        })
    );
}
