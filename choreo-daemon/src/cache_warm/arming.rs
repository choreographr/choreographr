//! Arming the cache warmer for the blocking tool window after a `ToolUse` turn.
//!
//! The agent loop holds no policy knowledge: it hands the just-sent request's
//! shape here and this module resolves every [`WarmFacts`] field from the live
//! catalog and the provider's configured cache-affinity dialect, then arms the
//! [`WarmHandle`]. Keeping the resolution in one place is what makes the
//! affinity safety gate correct — the dialect fed to
//! [`cache_affinity_downgraded`] must be the **same value the request actually
//! sends** (catalog dialect folded with the account override, minus a field the
//! provider's dialect never takes), so a forced-on hint that the provider then
//! rejected is detected, and a forced-off hint is not mistaken for one that will
//! be sent.

use choreo_ai_protocols::openai::{ChatRequestMessage, ChatToolDefinition};
use choreo_ai_protocols::{CacheAffinity, cache_affinity_downgraded, model_cost, prompt_cache_ttl};

use super::driver::{WarmHandle, WarmRequest};
use super::policy::{RetentionTier, WarmFacts, is_replayable};

/// Everything the agent loop must supply to arm the warmer for the blocking
/// tool window. Bundled into a struct so the arm call site reads as one
/// statement and a future fact is a one-line addition rather than a wider
/// argument list.
pub(crate) struct ToolWindowWarm<'a> {
    /// The provider slug of the just-sent turn (catalog lookups + the registry
    /// key's provider half).
    pub(crate) provider_slug: &'a str,
    /// The model slug of the just-sent turn (catalog lookups + the registry
    /// key's model half).
    pub(crate) model: &'a str,
    /// The affinity the request bodies send: the catalog dialect folded with the
    /// account override, minus a field the provider's dialect never takes, and
    /// **before** the rejection registry.
    pub(crate) affinity: CacheAffinity,
    /// Whether the request bodies ask for the model's **long** prompt-cache
    /// tier (see `ProviderClient::extended_cache_retention`), resolved by the
    /// same rules the body builder applies.
    pub(crate) extended_retention: bool,
    /// The estimated prompt-prefix size, in tokens.
    pub(crate) estimated_prompt_tokens: u32,
    /// The message prefix sent on the just-sent turn (replayed by the ping).
    pub(crate) messages: &'a [ChatRequestMessage],
    /// The tool definitions sent on the just-sent turn.
    pub(crate) tools: &'a [ChatToolDefinition],
    /// The daemon session id (the ping's gateway routing identity).
    pub(crate) session_id: u64,
    /// The per-turn stream id (the ping's gateway request id).
    pub(crate) stream_id: u64,
    /// Whether the turn's protocol is Anthropic (its `max_tokens: 0` pre-warm).
    pub(crate) is_anthropic: bool,
    /// Whether extended thinking is on (an Anthropic thinking request is not
    /// replayable with a capped output).
    pub(crate) thinking_enabled: bool,
}

/// Resolve the [`WarmFacts`] and arm the warmer for the tool window that
/// follows a `ToolUse` turn.
///
/// The request just sent is the prefix the next real turn re-reads, so a capped
/// re-send mid-tool refreshes its provider cache TTL. The payload is cloned
/// here (O(conversation), the same order the next iteration's payload rebuild
/// already costs) because the warmer thread runs while the loop rebuilds the
/// next turn.
///
/// The affinity gate is resolved from `affinity` exactly as a real request
/// would send it: `cache_affinity_downgraded` returns `true` only when the
/// request intended a hint and the process-wide rejection registry withheld it
/// for this `(provider, model)` pair. A provider the catalog leaves `Off` never
/// sets the fact and warms exactly as before.
///
/// The retention tier is `Long` when the request bodies actually ask the
/// provider for an extended cache — the same resolver the body builder uses, so
/// the ping schedule is computed against the lifetime the cache will really
/// have. Scheduling a 24 h cache on the short tier's minutes-long cadence would
/// re-ping (and pay for a write) long before the cache would have expired.
pub(crate) fn arm_for_tool_window(warmer: &WarmHandle, spec: &ToolWindowWarm<'_>) {
    let facts = WarmFacts {
        cache_ttl: prompt_cache_ttl(spec.provider_slug, spec.model),
        retention: RetentionTier::for_extended_retention(spec.extended_retention),
        prefix_tokens: spec.estimated_prompt_tokens,
        cost: model_cost(spec.provider_slug, spec.model),
        replayable: is_replayable(spec.is_anthropic, spec.thinking_enabled),
        affinity_downgraded: cache_affinity_downgraded(
            spec.provider_slug,
            spec.model,
            spec.affinity,
        ),
    };
    warmer.arm(WarmRequest {
        model: spec.model.to_string(),
        messages: spec.messages.to_vec(),
        tools: spec.tools.to_vec(),
        facts,
        session_id: spec.session_id.to_string(),
        stream_id: spec.stream_id.to_string(),
        // Anthropic accepts `max_tokens: 0` as its documented cache pre-warm
        // (writes the cache, bills no output); every other protocol needs at
        // least one output token.
        max_output_tokens: u32::from(!spec.is_anthropic),
    });
}
