//! Model-fact lookup resolvers over the process-wide [`PROVIDER_CATALOG`].
//!
//! These are the read-side resolvers that turn (provider slug, model slug)
//! pairs into per-model facts — context window, output-token ceiling, wire
//! flags, and reasoning round-trip policy. The catalog *types* and the
//! `ArcSwap` global itself live in [`super`] (mod.rs); this module holds only
//! the lookups, plus the tiny protocol-default helpers they share.
//!
//! Most resolvers funnel through [`with_model_fact`], which holds the
//! `ArcSwap` guard once and hands the matched [`ModelEntry`] to a closure —
//! the per-resolver differences (conservative `None` vs permissive `true`
//! defaults) stay in each resolver so the doc comments and defaults remain
//! the single source of truth for that behavior.

use tracing::trace;

use choreo_proto::ReasoningCapability;

use crate::openai::RequestFormat;

use super::{ModelEntry, PROVIDER_CATALOG, ProviderProtocol, ReasoningPassback};

/// Shared walker: find the model entry for `(provider_slug, model)` against
/// the live catalog, holding the `ArcSwap` guard only for the duration of the
/// call, and map it through `f`.
///
/// This collapses the repeated find-provider/find-model walk that used to be
/// duplicated across the per-fact resolvers: the guard is acquired once here
/// (readers are lock-free, but a shared walk means the traversal logic can
/// never drift between resolvers), and each resolver supplies only its own
/// field projection and default mapping via `f`.
///
/// Returns `None` when the provider slug or the model slug does not match any
/// catalog entry — each resolver decides what that means for its own fact.
fn with_model_fact<T>(
    provider_slug: &str,
    model: &str,
    f: impl FnOnce(&ModelEntry) -> T,
) -> Option<T> {
    let catalog = PROVIDER_CATALOG.load();
    let fact = catalog
        .iter()
        .find(|e| e.slug == provider_slug)
        .and_then(|e| e.models.iter().find(|m| m.model == model))
        .map(f);
    // Trace the miss as well as the hit: a catalog gap that silently falls
    // through to a resolver default is exactly the situation worth seeing in
    // the logs when diagnosing wire-behavior surprises.
    trace!(
        provider = %provider_slug,
        model = %model,
        found = fact.is_some(),
        "model fact lookup"
    );
    fact
}

/// Look up the context window for a model on a given provider.
/// Matches the model slug exactly against known entries.
/// Returns `None` if no entry matches, the provider is unknown, or the
/// entry has no known window (`context_window == 0`, e.g. a model whose
/// window was never recorded — callers then fall back to the client config).
pub fn lookup_context_window(provider_slug: &str, model: &str) -> Option<u32> {
    // `0` is the "unknown" sentinel shared with `max_output_tokens`; map it
    // back to `None` so callers keep their config fallback.
    with_model_fact(provider_slug, model, |m| m.context_window).and_then(|w| (w != 0).then_some(w))
}

/// Look up the maximum output tokens for a model on a given provider,
/// mirroring [`lookup_context_window`]: `None` for an unknown provider or
/// model, and `None` for a recorded `0` (models.dev omits `limit.output`
/// when it has no fact — 0 means unknown, exactly like `context_window`).
pub fn lookup_max_output_tokens(provider_slug: &str, model: &str) -> Option<u32> {
    // Same 0-is-unknown convention as the context window: 0 maps to `None`.
    with_model_fact(provider_slug, model, |m| m.max_output_tokens)
        .and_then(|t| (t != 0).then_some(t))
}

/// Whether the model on the given provider accepts the `temperature`
/// request parameter, from the ingested models.dev `temperature` flag.
/// Unlike the conservative lookups above, unknown providers/models default
/// to `true`: the snapshot records the fact only when the answer is "no",
/// so absence of a record must not silently drop the parameter from
/// requests to new/untracked models (the permissive wire default).
pub fn model_supports_temperature(provider_slug: &str, model: &str) -> bool {
    with_model_fact(provider_slug, model, |m| m.supports_temperature).unwrap_or(true)
}

/// Whether the given model on the given provider accepts image input (vision).
///
/// The vision gate uses this to decide whether attached/read images are sent
/// to the model natively or replaced with a text placeholder. Unknown models
/// and providers default to `false` (text-only) — the safe conservative
/// choice: sending an image to a text-only model would 400 the whole request,
/// whereas gating it out only degrades the image to a placeholder.
pub fn model_supports_vision(provider_slug: &str, model: &str) -> bool {
    with_model_fact(provider_slug, model, |m| m.supports_vision).unwrap_or(false)
}

/// Whether the model requires `reasoning_content` to be present on every
/// assistant message (e.g. DeepSeek/GLM 5.x chat completions). Purely
/// data-driven now: the ingested models.dev fact (`interleaved.field ==
/// "reasoning_content"`) and the per-model overlay override are the only
/// sources — an explicit `Some(bool)` on the model entry wins, and `None`
/// (no fact) or an unknown provider/model resolve to `false`. There is NO
/// name-based fallback: an uncataloged model gets no injection, so a catalog
/// gap surfaces as the upstream provider's error (a 400 about the missing
/// field) instead of a silent heuristic guess that can never be audited.
///
/// Independently of the fact, the resolver returns `false` for any model
/// whose `openai_responses` is `true`: `reasoning_content` is a
/// chat-completions wire concept, and the Responses path must never receive
/// the injection — even if a (mis-ingested) entry carries
/// `reasoning_content_required = Some(true)`, the two facts cannot both be
/// authoritative, and the wire format wins.
/// Fixes: add the model to the snapshot/overlay with an explicit
/// `reasoning_content_required` flag (chat-completions models only).
pub fn requires_reasoning_content(provider_slug: &str, model: &str) -> bool {
    let required = with_model_fact(provider_slug, model, |m| {
        m.reasoning_content_required.unwrap_or(false) && !m.openai_responses
    })
    .unwrap_or(false);
    trace!(
        provider = %provider_slug,
        model = %model,
        required,
        "requires_reasoning_content"
    );
    required
}

/// Look up whether a model should use OpenAI's Responses API.
/// Returns None for unknown models — caller falls back to default_request_format.
pub fn model_request_format(provider_slug: &str, model: &str) -> Option<RequestFormat> {
    with_model_fact(provider_slug, model, |m| {
        if m.openai_responses {
            RequestFormat::Responses
        } else {
            RequestFormat::ChatCompletions
        }
    })
}

/// Compute the reasoning capability for a given model on a given provider.
/// Falls back to protocol defaults for unknown models (best-effort
/// compatibility with new/untracked models).
pub fn model_reasoning_capability(provider_slug: &str, model: &str) -> ReasoningCapability {
    let catalog = PROVIDER_CATALOG.load();
    let entry = catalog.iter().find(|e| e.slug == provider_slug);

    let levels: Vec<String> = match entry {
        Some(e) => {
            let model_entry = e.models.iter().find(|m| m.model == model);
            match model_entry {
                // Known model that explicitly does not support reasoning
                Some(m) if !m.reasoning_supported => vec![],
                // Known model with explicit effort levels (OpenAi protocol only)
                Some(m)
                    if matches!(e.protocol, ProviderProtocol::OpenAi { .. })
                        && !m.openai_reasoning_levels.is_empty() =>
                {
                    m.openai_reasoning_levels.clone()
                }
                // Known model with reasoning but no explicit levels → protocol defaults
                Some(_) => protocol_default_levels(e.protocol),
                // Unknown model → protocol defaults (best-effort for new models)
                None => protocol_default_levels(e.protocol),
            }
        }
        // Unknown provider — no protocol to infer defaults from
        None => vec![],
    };

    trace!(
        provider = %provider_slug,
        model = %model,
        ?levels,
        "model_reasoning_capability"
    );

    ReasoningCapability {
        available_effort_levels: levels,
    }
}

/// Return the default reasoning-effort slugs for a given protocol.
fn protocol_default_levels(protocol: ProviderProtocol) -> Vec<String> {
    match protocol {
        ProviderProtocol::OpenAi { .. } | ProviderProtocol::AnthropicMessages => {
            vec!["off".into(), "low".into(), "medium".into(), "high".into()]
        }
        ProviderProtocol::GoogleGenerativeAi => {
            vec!["off".into(), "on".into()]
        }
    }
}

/// Compute how reasoning is replayed back to the provider for a given
/// model. Mirrors `model_reasoning_capability`: an explicit per-model
/// `reasoning_passback` TOML override wins — including an explicit `none`,
/// which suppresses replay even where the protocol default would echo —
/// otherwise the format is derived from the provider protocol (falling back
/// to protocol defaults for unknown/new models, and `None` for unknown
/// providers).
pub fn model_reasoning_passback(provider_slug: &str, model: &str) -> ReasoningPassback {
    let catalog = PROVIDER_CATALOG.load();
    let entry = catalog.iter().find(|e| e.slug == provider_slug);

    let passback = match entry {
        Some(e) => match e.models.iter().find(|m| m.model == model) {
            // Known model: an explicit TOML override wins (`Some(..)`, incl.
            // `Some(ReasoningPassback::None)` for "never replay"); an unset
            // field (`None`) derives from the protocol (and, for OpenAi
            // providers, whether the model uses Responses).
            Some(m) => resolve_passback(m.reasoning_passback, e.protocol, m.openai_responses),
            // Unknown model → protocol default (best-effort for new models;
            // OpenAi assumed chat-completions, matching
            // `ServiceConfig::default_request_format`).
            None => protocol_default_passback(e.protocol, false),
        },
        // Unknown provider — no protocol to infer a default from.
        None => ReasoningPassback::None,
    };

    trace!(
        provider = %provider_slug,
        model = %model,
        ?passback,
        "model_reasoning_passback"
    );

    passback
}

/// Resolve the effective passback format: an explicit per-model override
/// wins; `None` (unset) derives from the provider protocol.
fn resolve_passback(
    explicit: Option<ReasoningPassback>,
    protocol: ProviderProtocol,
    openai_responses: bool,
) -> ReasoningPassback {
    explicit.unwrap_or_else(|| protocol_default_passback(protocol, openai_responses))
}

/// Protocol-level default passback format, used when a model has no explicit
/// override (or is unknown). OpenAI-protocol providers that use the
/// Responses API chain continuity via `previous_response_id`; chat-completions
/// providers echo reasoning on tool-call turns (DeepSeek/Kimi minimum).
/// Anthropic echoes across all turns by default (last-turn-only models carry
/// an explicit `tool_loop` override); Google sends encrypted signatures.
fn protocol_default_passback(
    protocol: ProviderProtocol,
    openai_responses: bool,
) -> ReasoningPassback {
    match protocol {
        ProviderProtocol::OpenAi { .. } if openai_responses => ReasoningPassback::ResponseId,
        ProviderProtocol::OpenAi { .. } => ReasoningPassback::ToolLoop,
        ProviderProtocol::AnthropicMessages => ReasoningPassback::AllTurns,
        ProviderProtocol::GoogleGenerativeAi => ReasoningPassback::Signature,
    }
}

#[cfg(test)]
// All tests in this module read (and the swap tests mutate) the process-wide
// `PROVIDER_CATALOG`, so serialize them under one key: nextest gives
// per-process isolation anyway, but the libtest fallback shares one process
// across parallel threads and a mid-flight swap would race the readers.
#[serial_test::serial(catalog)]
mod tests {
    use super::super::test_util::{RestoreBundledOnDrop, tiny_catalog};
    use super::*;
    use crate::catalog::catalog_snapshot;
    use crate::catalog::lookup_provider;
    use crate::openai::RequestFormat;
    use crate::shared::MaxTokensField;

    #[test]
    fn lookup_context_window_known_provider() {
        // OpenAI exact slug matches
        assert_eq!(
            lookup_context_window("openai", "gpt-4.1-nano"),
            Some(1_047_576)
        );
        assert_eq!(lookup_context_window("openai", "gpt-5"), Some(400_000));
        // models.dev is authoritative for context windows: gpt-5.4 is a
        // 1.05M-token model there (the old TOML's 272k was stale).
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
    fn lookup_context_window_unknown_provider() {
        assert_eq!(lookup_context_window("nonexistent", "any-model"), None);
    }

    #[test]
    fn lookup_context_window_unknown_model() {
        assert_eq!(lookup_context_window("openai", "unknown-model-xyz"), None);
    }

    #[test]
    fn model_reasoning_capability_openai_known_model() {
        let cap = model_reasoning_capability("openai", "gpt-5.4");
        assert_eq!(
            cap.available_effort_levels,
            vec!["off", "low", "medium", "high", "xhigh"]
        );
    }

    #[test]
    fn model_reasoning_capability_openai_unknown_model() {
        let cap = model_reasoning_capability("openai", "gpt-4.1");
        assert!(cap.available_effort_levels.is_empty());
    }

    #[test]
    fn model_reasoning_capability_anthropic_supported() {
        let cap = model_reasoning_capability("anthropic", "claude-sonnet-4-6");
        assert_eq!(
            cap.available_effort_levels,
            vec!["off", "low", "medium", "high"]
        );
    }

    #[test]
    fn model_reasoning_capability_anthropic_unknown_model_uses_defaults() {
        // Unknown anthropic model → protocol defaults (best-effort fallback).
        let cap = model_reasoning_capability("anthropic", "claude-opus-3");
        assert_eq!(
            cap.available_effort_levels,
            vec!["off", "low", "medium", "high"]
        );
    }

    #[test]
    fn model_reasoning_capability_anthropic_non_reasoning() {
        // A known non-reasoning model yields no effort levels. None of the
        // current anthropic models are non-reasoning, so assert against a
        // known non-reasoning OpenAI model on the openai provider instead.
        let cap = model_reasoning_capability("openai", "gpt-4.1");
        assert!(cap.available_effort_levels.is_empty());
    }

    #[test]
    fn model_reasoning_capability_google_supported() {
        let cap = model_reasoning_capability("google", "gemini-2.5-pro");
        assert_eq!(cap.available_effort_levels, vec!["off", "on"]);
    }

    #[test]
    fn model_reasoning_capability_google_unknown_model_uses_defaults() {
        // Unknown google model → protocol defaults (off/on).
        let cap = model_reasoning_capability("google", "gemini-9.9");
        assert_eq!(cap.available_effort_levels, vec!["off", "on"]);
    }

    #[test]
    fn model_reasoning_capability_none_provider() {
        let cap = model_reasoning_capability("nonexistent", "any-model");
        assert!(cap.available_effort_levels.is_empty());
    }

    #[test]
    fn model_request_format_known_model() {
        // OpenAI's catalog default is the Responses API.
        let fmt = model_request_format("openai", "gpt-4.1");
        assert_eq!(fmt, Some(RequestFormat::Responses));
    }

    #[test]
    fn model_request_format_opencode_go_matches_gateway_docs() {
        assert_eq!(
            model_request_format("opencode-go", "deepseek-v4-flash"),
            Some(RequestFormat::ChatCompletions)
        );
    }

    #[test]
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
    fn model_request_format_unknown_model() {
        let fmt = model_request_format("openai", "nonexistent-model");
        assert_eq!(fmt, None);
    }

    #[test]
    fn model_request_format_unknown_provider() {
        let fmt = model_request_format("nope", "gpt-4.1");
        assert_eq!(fmt, None);
    }

    #[test]
    fn model_reasoning_passback_openai_responses_model() {
        // gpt-5.4 uses the Responses API (responses = true) → chain via
        // previous_response_id / opaque reasoning items.
        assert_eq!(
            model_reasoning_passback("openai", "gpt-5.4"),
            ReasoningPassback::ResponseId
        );
    }

    #[test]
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
    fn model_reasoning_passback_deepseek_derives_tool_loop() {
        // DeepSeek is a chat-completions OpenAI-protocol provider, so both
        // models derive the ToolLoop default (echo reasoning on tool-call
        // turns) — the old TOML's explicit tool_loop override was redundant
        // with the derived default and is not carried into the overlay.
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
    fn model_reasoning_passback_google_model() {
        // Gemini → encrypted thought signatures.
        assert_eq!(
            model_reasoning_passback("google", "gemini-2.5-pro"),
            ReasoningPassback::Signature
        );
    }

    #[test]
    fn model_reasoning_passback_none_provider() {
        // Unknown provider — no protocol to derive a default from.
        assert_eq!(
            model_reasoning_passback("nonexistent", "any-model"),
            ReasoningPassback::None
        );
    }

    #[test]
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
    fn resolve_passback_unset_uses_protocol_default() {
        // `None` (unset) on Anthropic → the keep-all-turns protocol default.
        assert_eq!(
            resolve_passback(None, ProviderProtocol::AnthropicMessages, false),
            ReasoningPassback::AllTurns
        );
    }

    #[test]
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
    fn bundled_toml_reasoning_passback_parses() {
        // TOML files WITHOUT the field must still parse (serde default None).
        // Spot-check one untouched provider resolves via protocol default.
        assert_eq!(
            model_reasoning_passback("groq", "groq/compound"),
            ReasoningPassback::ToolLoop
        );
    }

    #[test]
    fn requires_reasoning_content_resolves_ingested_snapshot_fact() {
        // The snapshot's deepseek models carry `interleaved: {field:
        // "reasoning_content"}` — the echo requirement is a FACT now, no
        // name heuristic involved.
        assert!(requires_reasoning_content("deepseek", "deepseek-v4-flash"));
        assert!(requires_reasoning_content("deepseek", "deepseek-v4-pro"));
        // GLM 5.x on zhipuai likewise carries the interleaved field.
        assert!(requires_reasoning_content("zhipuai", "glm-5"));
    }

    #[test]
    fn requires_reasoning_content_overlay_override_wins() {
        // glm-5.3-flash is not in the snapshot (upstream exposes it under
        // internal names); the bundled overlay defines it wholesale with an
        // explicit `reasoning_content_required = true` — the explicit flag
        // is the only path for models the snapshot does not cover.
        assert!(requires_reasoning_content("opencode-go", "glm-5.3-flash"));
    }

    #[test]
    fn requires_reasoning_content_false_without_the_fact() {
        // glm-4.5 is cataloged but carries no interleaved reasoning_content
        // fact in the snapshot → no echo requirement.
        assert!(!requires_reasoning_content("zhipuai", "glm-4.5"));
    }

    #[test]
    fn requires_reasoning_content_unknown_model_is_false() {
        // Behavior change (pinned): the old deepseek/kimi NAME heuristic is
        // gone. A plainly-named but uncataloged model resolves FALSE — no
        // injection. Catalog gaps now surface as upstream errors (the
        // provider's own 400 about the missing field) instead of a silent
        // heuristic guess that could never be audited or overridden.
        assert!(!requires_reasoning_content(
            "deepseek",
            "deepseek-v99-unknown"
        ));
        assert!(!requires_reasoning_content(
            "futurmix",
            "deepseek-chat-unknown"
        ));
        assert!(!requires_reasoning_content(
            "no-such-provider",
            "deepseek-chat"
        ));
    }

    #[test]
    fn requires_reasoning_content_never_applies_to_responses_models() {
        // `reasoning_content` is a chat-completions wire concept: even if a
        // model entry carries the fact `Some(true)` (e.g. a mis-merge between
        // a chat-completions model and its Responses twin), the Responses
        // guard must suppress the injection — the wire format wins over the
        // fact. Pin the guard with a synthetic catalog entry carrying BOTH
        // `openai_responses: true` and `reasoning_content_required: Some(true)`.
        let _restore = RestoreBundledOnDrop;
        let bundled = catalog_snapshot();
        crate::catalog::replace_catalog({
            let mut c = tiny_catalog();
            c[0].models[0].openai_responses = true;
            c[0].models[0].reasoning_content_required = Some(true);
            c
        });
        assert!(
            !requires_reasoning_content("tiny-test", "tiny-model"),
            "Responses models must never receive the reasoning_content injection"
        );

        // The negative control: the identical fact on a chat-completions twin
        // (openai_responses = false) DOES resolve true, so the guard — not a
        // missing fact — is what produced the `false` above.
        crate::catalog::replace_catalog({
            let mut c = tiny_catalog();
            c[0].models[0].openai_responses = false;
            c[0].models[0].reasoning_content_required = Some(true);
            c
        });
        assert!(requires_reasoning_content("tiny-test", "tiny-model"));

        // Explicit restore (the guard also restores on panic) so the test is
        // self-documenting about returning the global to its initial state.
        crate::catalog::replace_catalog(bundled.to_vec());
    }

    #[test]
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
}
