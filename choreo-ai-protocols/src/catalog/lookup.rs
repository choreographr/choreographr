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

use choreo_proto::{ChatReasoningField, ReasoningCapability};

use crate::openai::RequestFormat;

use super::{
    ModelCost, ModelEntry, PROVIDER_CATALOG, PromptCacheTtl, ProviderEntry, ProviderProtocol,
    ReasoningPassback,
};

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

/// Sibling of [`with_model_fact`] for facts whose fallback lives on the
/// *provider* entry rather than the model: it holds the `ArcSwap` guard once
/// across a single walk and hands the closure both the matched
/// [`ProviderEntry`] and the matched [`ModelEntry`]. Kept in the same style as
/// `with_model_fact` so the traversal logic never drifts between the two.
///
/// Returns `None` when the provider slug or the model slug does not match any
/// catalog entry.
fn with_provider_and_model_fact<T>(
    provider_slug: &str,
    model: &str,
    f: impl FnOnce(&ProviderEntry, &ModelEntry) -> T,
) -> Option<T> {
    let catalog = PROVIDER_CATALOG.load();
    let fact = catalog
        .iter()
        .find(|e| e.slug == provider_slug)
        .and_then(|provider| {
            provider
                .models
                .iter()
                .find(|m| m.model == model)
                .map(|m| f(provider, m))
        });
    trace!(
        provider = %provider_slug,
        model = %model,
        found = fact.is_some(),
        "model fact lookup (provider-scoped fallback)"
    );
    fact
}

/// Look up the recorded token prices for a model on a given provider, cloning
/// the [`ModelCost`] out of the catalog. Returns `None` for an unknown
/// provider or model, and `None` when the model records no cost object (the
/// snapshot simply carries no price fact for it — never a synthetic zero).
///
/// Consumed by the cache-warming cost gate — currently **recorded but
/// unwired** (no production caller yet, alongside `model_supports_temperature`).
#[must_use]
pub fn model_cost(provider_slug: &str, model: &str) -> Option<ModelCost> {
    // `ModelCost` is `Copy`, so the projection is a plain value clone.
    with_model_fact(provider_slug, model, |m| m.cost).flatten()
}

/// Resolve the effective prompt-cache TTL for a model on a given provider.
///
/// Precedence, highest wins: the model's own `prompt_cache` override, else the
/// provider-level `prompt_cache` default, else `None` (the provider declares no
/// TTL — the caller then treats cache-warming as unschedulable). Returns `None`
/// for an unknown provider or model too.
///
/// Consumed by cache-warming TTL scheduling — currently **recorded but
/// unwired** (no production caller yet).
#[must_use]
pub fn prompt_cache_ttl(provider_slug: &str, model: &str) -> Option<PromptCacheTtl> {
    // `PromptCacheTtl` is `Copy`, so `or` between the two levels needs no clone.
    with_provider_and_model_fact(provider_slug, model, |provider, model| {
        model.prompt_cache.or(provider.prompt_cache)
    })
    .flatten()
}

/// Look up the context window for a model on a given provider.
/// Matches the model slug exactly against known entries.
/// Returns `None` if no entry matches, the provider is unknown, or the
/// entry has no known window (`context_window == 0`, e.g. a model whose
/// window was never recorded — callers then fall back to the client config).
#[must_use]
pub fn lookup_context_window(provider_slug: &str, model: &str) -> Option<u32> {
    // `0` is the "unknown" sentinel shared with `max_output_tokens`; map it
    // back to `None` so callers keep their config fallback.
    with_model_fact(provider_slug, model, |m| m.context_window).and_then(|w| (w != 0).then_some(w))
}

/// Look up the maximum output tokens for a model on a given provider,
/// mirroring [`lookup_context_window`]: `None` for an unknown provider or
/// model, and `None` for a recorded `0` (models.dev omits `limit.output`
/// when it has no fact — 0 means unknown, exactly like `context_window`).
#[must_use]
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
#[must_use]
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
#[must_use]
pub fn model_supports_vision(provider_slug: &str, model: &str) -> bool {
    with_model_fact(provider_slug, model, |m| m.supports_vision).unwrap_or(false)
}

/// Whether the given model on the given provider can *produce* images
/// (image output), mirroring [`model_supports_vision`] on the output side of
/// the modalities pair. Unknown models and providers default to `false` —
/// the safe conservative choice: an image-capable capability gate must never
/// enable itself for an untracked model.
#[must_use]
pub fn model_supports_image_output(provider_slug: &str, model: &str) -> bool {
    with_model_fact(provider_slug, model, |m| m.supports_image_output).unwrap_or(false)
}

/// Return the model IDs on the given provider that can produce images
/// (`supports_image_output == true`), in the catalog's natural order (the
/// snapshot/overlay insertion order). Unknown providers yield an empty list.
pub fn image_models_for_provider(provider_slug: &str) -> Vec<String> {
    let catalog = PROVIDER_CATALOG.load();
    catalog
        .iter()
        .find(|e| e.slug == provider_slug)
        .map_or_default(|e| {
            e.models
                .iter()
                .filter(|m| m.supports_image_output)
                .map(|m| m.model.clone())
                .collect()
        })
}

/// Return the model IDs on the given provider that can *edit* an input image
/// (`supports_image_edit == true`), in the catalog's natural order. Unknown
/// providers yield an empty list.
///
/// Mirror of [`image_models_for_provider`] over the edit flag; the edit tool
/// uses it as its candidate set, so an un-pinned provider resolves to an empty
/// list and the tool fails closed (rather than offering a generation model
/// that has no edit endpoint).
#[must_use]
pub fn image_edit_models_for_provider(provider_slug: &str) -> Vec<String> {
    let catalog = PROVIDER_CATALOG.load();
    catalog
        .iter()
        .find(|e| e.slug == provider_slug)
        .map_or_default(|e| {
            e.models
                .iter()
                .filter(|m| m.supports_image_edit)
                .map(|m| m.model.clone())
                .collect()
        })
}

/// How a route wants replayed reasoning shaped on the chat-completions wire.
///
/// Two genuinely independent facts, both read from the per-model catalog entry:
///
/// - [`field`](Self::field) — the wire key the route wants (the *destination*).
///   When set it ALWAYS wins over the field a response was observed to stream
///   under: a gateway that answers under `reasoning` while rejecting any
///   history without `reasoning_content` exists in the wild, so the observation
///   cannot be trusted as the destination.
/// - [`require_present`](Self::require_present) — whether every assistant
///   message must carry the field, empty when there is nothing to echo. This is
///   the DeepSeek/GLM 5.x rule: an upstream 400s a tool-loop history whose
///   assistant message omits the field, even for a call that produced no
///   reasoning.
///
/// A route can declare a destination without demanding its presence (the
/// OpenRouter/Ollama `reasoning_details` resellers), so presence is an explicit
/// opt-in catalog fact — never inferred from a declared field.
///
/// The default (`field: None, require_present: false`) means "no declared
/// shape": the adapter then echoes whatever field the payload was observed
/// under, and omits the key entirely when there is nothing to echo.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ChatReasoningEcho {
    /// The wire field the route wants, when the catalog declares one.
    pub field: Option<ChatReasoningField>,
    /// Whether every assistant message must carry the field (empty when there
    /// is nothing to echo), resolved from the catalog's explicit
    /// `reasoning_echo_required` fact.
    pub require_present: bool,
}

/// Resolve the chat-completions reasoning-echo policy for one route.
///
/// Purely data-driven: the ingested models.dev fact (`interleaved` naming a
/// replayable chat field), the per-model overlay override, and the separate
/// presence requirement are the only sources, with NO name-based fallback. An
/// uncataloged model resolves to the default, so a catalog gap surfaces as the
/// upstream provider's own error instead of a silent heuristic guess that could
/// never be audited or overridden.
///
/// Guards, applied independently of the fact:
///
/// - a model on the `OpenAI` **Responses** API resolves to the default —
///   `reasoning_content` and friends are chat-completions wire concepts, and
///   continuity there flows through `previous_response_id`/input items, so a
///   mis-ingested entry cannot leak a chat field into a Responses request;
/// - a non-chat request format resolves to the default for the same reason.
///
/// Fixes for a catalog gap: add the model to the snapshot, or to the overlay
/// with `reasoning_echo_field = "…"` (and `reasoning_echo_required = true` when
/// the upstream also demands the key's presence).
#[must_use]
pub fn chat_reasoning_echo(provider_slug: &str, model: &str) -> ChatReasoningEcho {
    let echo = with_model_fact(provider_slug, model, |m| {
        // The destination and the presence requirement are separate catalog
        // facts, so project both here and let the guard below flatten the pair
        // to the default together — a guarded route must resolve to the whole
        // default, never to a half-answer carrying a field with no presence.
        (m.reasoning_echo, m.reasoning_echo_required.unwrap_or(false))
    })
    .filter(|(field, _)| {
        field.is_some()
            && model_request_format(provider_slug, model) != Some(RequestFormat::Responses)
    })
    .map_or_else(ChatReasoningEcho::default, |(field, require_present)| {
        ChatReasoningEcho {
            field,
            require_present,
        }
    });
    trace!(
        provider = %provider_slug,
        model = %model,
        field = ?echo.field,
        require_present = echo.require_present,
        "chat_reasoning_echo"
    );
    echo
}

/// Look up whether a model should use `OpenAI`'s Responses API.
/// Returns None for unknown models — caller falls back to `default_request_format`.
#[must_use]
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
        assert_eq!(cap.available_effort_levels, [] as [String; 0]);
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
        assert_eq!(cap.available_effort_levels, [] as [String; 0]);
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
        assert_eq!(cap.available_effort_levels, [] as [String; 0]);
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
    fn chat_reasoning_echo_default_without_the_fact() {
        // glm-4.5 is cataloged but carries no interleaved echo fact in the
        // snapshot → no declared shape: the adapter echoes the observed field.
        assert_eq!(
            chat_reasoning_echo("zhipuai", "glm-4.5"),
            ChatReasoningEcho::default()
        );
    }

    #[test]
    fn chat_reasoning_echo_unknown_model_is_default() {
        // The old deepseek/kimi NAME heuristic is gone: a plainly-named but
        // uncataloged model resolves to the default — no declared field, no
        // presence requirement. Catalog gaps surface as upstream errors (the
        // provider's own 400) instead of a silent unauditable guess.
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

    #[test]
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
    fn bundled_overlay_pins_max_tokens_field_for_the_zhipu_slugs() {
        // z.ai's chat-completions API documents only `max_tokens` (no
        // `max_completion_tokens`), but the models.dev derivation defaults
        // every OpenAI-protocol provider to `max_completion_tokens`. The
        // bundled overlay must win for both Zhipu slugs — this pins the
        // field-wise provider-scalar merge end to end for the real catalog.
        // Read-only (no catalog swap), so no restore guard is needed.
        for slug in ["zai", "zhipuai"] {
            let entry =
                lookup_provider(slug).unwrap_or_else(|| panic!("{slug} in bundled catalog"));
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
}
