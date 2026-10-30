//! Model-fact lookup resolvers over the process-wide [`PROVIDER_CATALOG`].
//!
//! These are the read-side resolvers that turn (provider slug, model slug)
//! pairs into per-model facts — context window, output-token ceiling, wire
//! flags, and reasoning round-trip policy. The catalog *types* and the
//! `ArcSwap` global itself live in [`super`] (mod.rs); this module holds only
//! the lookups, plus the tiny protocol-default helpers they share.
//!
//! Most resolvers read the catalog through [`with_provider`], which holds the
//! `ArcSwap` guard once and hands the matched [`ProviderEntry`] to a closure
//! (the model-level walkers [`with_model_fact`] and
//! [`with_provider_and_model_fact`] build on it) — the per-resolver differences
//! (conservative `None` vs permissive `true` defaults) stay in each resolver, so
//! the doc comments and defaults remain the single source of truth for that
//! behavior.
//!
//! Portable facts — the model's own capacities, which survive being resold by
//! another provider (context window, output ceiling) — resolve through
//! [`with_portable_model_fact`] instead, so an offering can inherit them from an
//! explicitly declared canonical identity. Everything a *route* decides (wire
//! protocol, request format, reasoning echo/passback, modality gates) stays
//! exact-match only.

use tracing::trace;

use choreo_proto::{ChatReasoningField, ReasoningCapability};

use crate::cache_retention::CacheRetentionDialect;
use crate::openai::RequestFormat;
use crate::session_routing::CacheAffinity;

use super::{
    ModelCost, ModelEntry, PROVIDER_CATALOG, PromptCacheTtl, ProviderEntry, ProviderProtocol,
    ReasoningPassback,
};

/// Shared projection: run `read` against the catalog entry for `slug` while the
/// `ArcSwap` guard is held, and hand back exactly what `read` returned.
///
/// Every read-only provider lookup goes through this one function, so the
/// traversal exists once and a lookup is just its own field projection. `R`
/// cannot borrow from the entry — the guard is released when this returns — so a
/// resolver that needs the entry to outlive the read has to clone what it needs
/// inside `read`.
///
/// Returns `None` when no provider carries `slug`; each caller decides what that
/// means for its own fact. [`super::lookup_provider`] deliberately does NOT route
/// through this: it hands back an owned clone that outlives the guard, which is a
/// different contract from a transient read.
fn with_provider<R>(slug: &str, read: impl FnOnce(&ProviderEntry) -> R) -> Option<R> {
    let catalog = PROVIDER_CATALOG.load();
    catalog.iter().find(|e| e.slug == slug).map(read)
}

/// Shared walker: find the model entry for `(provider_slug, model)` against
/// the live catalog, holding the `ArcSwap` guard only for the duration of the
/// call, and map it through `f`.
///
/// Every per-fact resolver walks the catalog through this one function: the
/// guard is acquired once (in [`with_provider`]), where the traversal logic
/// cannot drift between resolvers, and each resolver supplies only its own field
/// projection and default mapping via `f`.
///
/// Returns `None` when the provider slug or the model slug does not match any
/// catalog entry — each resolver decides what that means for its own fact.
fn with_model_fact<T>(
    provider_slug: &str,
    model: &str,
    f: impl FnOnce(&ModelEntry) -> T,
) -> Option<T> {
    let fact = with_provider(provider_slug, |provider| {
        provider.models.iter().find(|m| m.model == model).map(f)
    })
    .flatten();
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

/// Split an explicit `provider/model` metadata link into its two segments.
///
/// Both segments must be non-empty, so a malformed or half-written link
/// resolves to "no link" instead of quietly matching the provider/model of the
/// same name. Only the FIRST separator is significant: a model id may itself
/// contain `/` (e.g. `openai/gpt-oss-120b` under another lab), so the tail is
/// kept whole rather than being split again.
fn parse_canonical_link(link: &str) -> Option<(&str, &str)> {
    let (provider, model) = link.split_once('/')?;
    (!provider.is_empty() && !model.is_empty()).then_some((provider, model))
}

/// Sibling of [`with_model_fact`] for a **portable** fact: one that describes the
/// underlying model rather than this provider's offering of it.
///
/// The selected offering always wins, so a curated local value is never
/// displaced. Only when the offering has no value of its own (`f` returns
/// `None`) may its explicit [`ModelEntry::canonical_model_id`] link supply the
/// same fact from that `provider/model` record.
///
/// The link is the ONLY way to cross a provider boundary: there is no fallback
/// on a matching model string, a stripped provider prefix, a date suffix, or any
/// other similarity, and it is followed at most one hop (the canonical record's
/// own link is never chased). A provider-facing model id is a wire contract,
/// while canonical identity is a separately curated fact — inferring one from
/// the other would attribute another offering's limits to this one.
///
/// Returns `None` when the provider slug or the model slug does not match any
/// catalog entry, or when neither record carries the fact.
fn with_portable_model_fact<T>(
    provider_slug: &str,
    model: &str,
    f: impl Fn(&ModelEntry) -> Option<T>,
) -> Option<T> {
    // Held for the whole resolution: both the selected and the canonical walk
    // read the same snapshot, so a concurrent swap cannot mix two revisions.
    let catalog = PROVIDER_CATALOG.load();
    let find = |slug: &str, model: &str| {
        catalog
            .iter()
            .find(|provider| provider.slug == slug)?
            .models
            .iter()
            .find(|entry| entry.model == model)
    };

    let (fact, canonical_hit) = match find(provider_slug, model) {
        Some(selected) => match f(selected) {
            Some(fact) => (Some(fact), false),
            None => match selected
                .canonical_model_id
                .as_deref()
                .and_then(parse_canonical_link)
            {
                Some((slug, canonical_model)) => (find(slug, canonical_model).and_then(&f), true),
                None => (None, false),
            },
        },
        None => (None, false),
    };
    // Trace the miss as well as the hit, and say which record answered: a
    // catalog gap or an unexpected canonical hop is exactly what is worth seeing
    // when diagnosing a surprising limit in the UI.
    trace!(
        provider = %provider_slug,
        model = %model,
        found = fact.is_some(),
        via_canonical = canonical_hit,
        "portable model fact lookup"
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
    let fact = with_provider(provider_slug, |provider| {
        provider
            .models
            .iter()
            .find(|m| m.model == model)
            .map(|m| f(provider, m))
    })
    .flatten();
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
    with_provider(provider_slug, |e| e.cache_affinity).unwrap_or_default()
}

/// Whether the given provider's Responses endpoint speaks the **stateless**
/// dialect, from the overlay's provider-level `responses_stateless` fact.
///
/// Unknown providers report `false`: the standard chaining Responses shape is
/// the permissive default, since a provider the catalog has never heard of is
/// far more likely to be an ordinary `OpenAI`-compatible endpoint than the one
/// non-standard backend. The `chatgpt`/Codex route is the only provider that
/// pins it (see the bundled overlay), which is what makes
/// [`ServiceConfig::responses_stateless`](crate::openai::ServiceConfig::responses_stateless)
/// and the daemon's `previous_response_id` policy two views of ONE catalog fact
/// rather than two hand-kept switches.
#[must_use]
pub fn provider_responses_stateless(provider_slug: &str) -> bool {
    with_provider(provider_slug, |e| e.responses_stateless).unwrap_or(false)
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
/// The selected offering's nonzero value wins. If it has no value, its explicit
/// `canonical_model_id` may supply this portable capacity fact; no identifier
/// similarity is inferred. Returns `None` if neither record has a known window.
#[must_use]
pub fn lookup_context_window(provider_slug: &str, model: &str) -> Option<u32> {
    // `0` means the selected offering has no fact, so its explicit canonical
    // identity may supply this provider-independent capacity value.
    with_portable_model_fact(provider_slug, model, |m| {
        (m.context_window != 0).then_some(m.context_window)
    })
}

/// Look up the maximum output tokens for a model on a given provider,
/// mirroring [`lookup_context_window`]: the exact offering wins, then its
/// explicit canonical identity may supply this portable capacity fact. `0` is
/// unknown because models.dev omits `limit.output` when it has no fact.
#[must_use]
pub fn lookup_max_output_tokens(provider_slug: &str, model: &str) -> Option<u32> {
    // Same 0-is-unknown convention as the context window: only an explicit
    // canonical identity may supply the missing portable capacity fact.
    with_portable_model_fact(provider_slug, model, |m| {
        (m.max_output_tokens != 0).then_some(m.max_output_tokens)
    })
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
#[must_use]
pub fn image_models_for_provider(provider_slug: &str) -> Vec<String> {
    with_provider(provider_slug, |e| {
        e.models
            .iter()
            .filter(|m| m.supports_image_output)
            .map(|m| m.model.clone())
            .collect()
    })
    .unwrap_or_default()
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
    with_provider(provider_slug, |e| {
        e.models
            .iter()
            .filter(|m| m.supports_image_edit)
            .map(|m| m.model.clone())
            .collect()
    })
    .unwrap_or_default()
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
    // The provider entry and the model walk it holds are one read, so the
    // capability and the protocol its defaults come from cannot be resolved
    // against two different catalog versions.
    let levels: Vec<String> = with_provider(provider_slug, |e| {
        match e.models.iter().find(|m| m.model == model) {
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
    })
    // Unknown provider — no protocol to infer defaults from.
    .unwrap_or_default();

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
    let passback = with_provider(provider_slug, |e| {
        match e.models.iter().find(|m| m.model == model) {
            // Known model: an explicit TOML override wins (`Some(..)`, incl.
            // `Some(ReasoningPassback::None)` for "never replay"); an unset
            // field (`None`) derives from the protocol (and, for OpenAi
            // providers, whether the model uses Responses).
            Some(m) => resolve_passback(m.reasoning_passback, e.protocol, m.openai_responses),
            // Unknown model → protocol default (best-effort for new models;
            // OpenAi assumed chat-completions, matching
            // `ServiceConfig::default_request_format`).
            None => protocol_default_passback(e.protocol, false),
        }
    })
    // Unknown provider — no protocol to infer a default from.
    .unwrap_or(ReasoningPassback::None);

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
