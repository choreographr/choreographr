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

use crate::cache_retention::CacheRetentionDialect;
use crate::openai::RequestFormat;
use crate::session_routing::CacheAffinity;

use super::{
    ModelCost, ModelEntry, PROVIDER_CATALOG, PromptCacheTtl, ProviderEntry, ProviderProtocol,
    ReasoningPassback,
};

/// Shared walker: find the model entry for `(provider_slug, model)` against
/// the live catalog, holding the `ArcSwap` guard only for the duration of the
/// call, and map it through `f`.
///
/// Every per-fact resolver walks the catalog through this one function: the
/// guard is acquired once here (readers are lock-free, but a shared walk means
/// the traversal logic can never drift between resolvers), and each resolver
/// supplies only its own field projection and default mapping via `f`.
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

/// The per-session cache-affinity dialect the catalog declares for
/// `provider_slug`, or [`CacheAffinity::Off`] for an unknown provider.
///
/// This is the provider-level `cache_affinity` overlay scalar, i.e. the hint the
/// catalog *intends* a request to carry. It deliberately reports the catalog
/// value and not a request-time resolution: callers that need the hint a real
/// body will send compare it against
/// [`ServiceConfig::effective_cache_affinity`](crate::openai::ServiceConfig::effective_cache_affinity)
/// (or ask [`cache_affinity_downgraded`](crate::cache_affinity_downgraded))
/// to tell "opted in" apart from "opted in but withheld by the rejection
/// registry".
#[must_use]
pub fn cache_affinity(provider_slug: &str) -> CacheAffinity {
    PROVIDER_CATALOG
        .load()
        .iter()
        .find(|e| e.slug == provider_slug)
        .map_or_default(|e| e.cache_affinity)
}

/// The extended prompt-cache retention dialect the catalog declares for a
/// **specific model**, or [`CacheRetentionDialect::None`] for an unknown
/// provider/model (or one the overlay did not opt in).
///
/// Deliberately per-model: the retention field's spelling depends on the
/// model's generation (pre-GPT-5.6 `prompt_cache_retention` vs GPT-5.6+
/// `prompt_cache_options`), and a provider that serves both cannot carry one
/// value. There is no provider-level fallback for that reason.
///
/// This reports only the declared spelling; the request-time resolver in
/// `cache_retention` additionally requires the model to declare a long TTL
/// tier before emitting anything.
#[must_use]
pub fn cache_retention(provider_slug: &str, model: &str) -> CacheRetentionDialect {
    with_model_fact(provider_slug, model, |m| m.cache_retention)
        .unwrap_or(CacheRetentionDialect::None)
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
///   the `DeepSeek`/`GLM` rule: `DeepSeek` returns a 400 for a request carrying
///   `tools` whose earlier turns do not pass `reasoning_content` back
///   ([docs](https://api-docs.deepseek.com/guides/thinking_mode)), and GLM asks
///   for the same blocks to be returned unmodified
///   ([docs](https://docs.z.ai/guides/capabilities/thinking-mode)).
///
/// A route can declare a destination without demanding its presence (the
/// OpenRouter/Ollama `reasoning_details` resellers, whose documentation only
/// asks for the blocks to be preserved when replaying them —
/// [docs](https://openrouter.ai/docs/guides/best-practices/reasoning-tokens#preserving-reasoning)),
/// so the two are stored separately and either can be pinned on its own. The
/// base ingestion derives presence only for a **scalar** destination, because a
/// scalar field name is the snapshot's way of recording the upstream's replayed
/// side channel; the structured array is destination-only there. Presence is a
/// per-route fact, never invented at lowering time.
///
/// The default (`field: None, require_present: false`) means "no declared
/// shape": the adapter then echoes whatever field the payload was observed
/// under, and omits the key entirely when there is nothing to echo.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ChatReasoningEcho {
    /// The wire field the route wants, when the catalog declares one.
    pub field: Option<ChatReasoningField>,
    /// Whether every assistant message must carry the field (empty when there
    /// is nothing to echo), resolved from the catalog's
    /// `reasoning_echo_required` fact — pinned by the overlay, or derived by
    /// ingestion for a scalar destination (see the type docs).
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
/// The two facts are independent, so each half can be pinned on its own: a
/// destination with no presence requirement, a presence requirement with no
/// destination (resolving to `field: None, require_present: true`, which the
/// adapter emits as an empty default `reasoning_content`), or both.
///
/// One guard applies to the whole policy, independently of the facts: a model on
/// the `OpenAI` **Responses** API resolves to the default — `reasoning_content`
/// and friends are chat-completions wire concepts, and continuity there flows
/// through `previous_response_id`/input items, so a mis-ingested entry cannot
/// leak a chat field into a Responses request. The guard lives inside the one
/// catalog walk, so the whole policy costs a single lock-free read.
///
/// Fixes for a catalog gap: add the model to the snapshot, or to the overlay
/// with `reasoning_echo_field = "…"` (and `reasoning_echo_required = true` when
/// the upstream also demands the key's presence).
#[must_use]
pub fn chat_reasoning_echo(provider_slug: &str, model: &str) -> ChatReasoningEcho {
    let echo = with_model_fact(provider_slug, model, |m| {
        if m.openai_responses {
            return ChatReasoningEcho::default();
        }
        // The destination and the presence requirement are separate catalog
        // facts, so both are projected here and neither is inferred from the
        // other: a presence-only pin (no declared destination) is a legitimate
        // catalog entry and must survive to the caller.
        ChatReasoningEcho {
            field: m.reasoning_echo,
            require_present: m.reasoning_echo_required.unwrap_or(false),
        }
    })
    .unwrap_or_default();
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
mod tests;
