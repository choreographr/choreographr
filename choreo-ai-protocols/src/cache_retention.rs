//! Extended prompt-cache **retention selectors** for `OpenAI` request bodies.
//!
//! A prompt cache is ephemeral by default: an idle session loses its warm
//! prefix after the provider's default tier (a few minutes). Some providers
//! expose a longer retention tier a request can ask for, which is what makes
//! cache warming useful across a longer idle window. The wire shape is **not**
//! stable across a provider's model generations, so it is modelled as a
//! per-*model* capability ([`CacheRetentionDialect`]) rather than a
//! per-provider one: `openai` serves both a pre-5.6 generation that takes a
//! top-level `prompt_cache_retention` field and a GPT-5.6+ generation that
//! takes a nested `prompt_cache_options.ttl`. A provider-level setting would
//! force one generation's spelling onto the other and 400 the request.
//!
//! # Opt-in, and gated on the curated TTL
//!
//! Nothing is emitted unless both halves hold:
//!
//! 1. the model's overlay entry declares a [`CacheRetentionDialect`] other than
//!    [`CacheRetentionDialect::None`], and
//! 2. the model declares a **long** tier in its curated
//!    [`PromptCacheTtl`](crate::PromptCacheTtl) data
//!    (`ModelEntry::prompt_cache` / `ProviderEntry::prompt_cache`).
//!
//! The second gate is deliberate: the curated TTL data is the one place that
//! records *which* tier a model actually exposes, so the selector reuses that
//! curation instead of introducing a parallel source of truth. The dialect is
//! never inferred from a model-id string — that kind of guessing breaks the
//! moment a provider adds a model.
//!
//! # Rejected fields fail safe
//!
//! A provider that answers a 400 naming one of these fields is remembered by
//! the process-wide cache-field rejection registry
//! ([`crate::session_routing`]), and the selector is withheld for the rest of
//! the process. It shares that registry with the per-session cache-affinity
//! hint: both are additions to the same prompt-cache request surface, so a
//! provider that rejects one unknown field is not asked to accept the other.

use serde::{Deserialize, Serialize};
use tracing::debug;

/// The top-level body field carrying the pre-GPT-5.6 retention selector.
///
/// Named once so the rejection detector and the resolver agree on the exact
/// spelling a 400 must mention.
pub(crate) const PROMPT_CACHE_RETENTION_FIELD: &str = "prompt_cache_retention";

/// The nested body field carrying the GPT-5.6+ retention selector.
pub(crate) const PROMPT_CACHE_OPTIONS_FIELD: &str = "prompt_cache_options";

/// The extended-tier wire value for [`CacheRetentionDialect::PromptCacheRetention`]
/// (the model keeps its prompt cache for 24 hours).
const RETENTION_LONG_VALUE: &str = "24h";

/// The extended-tier `ttl` for [`CacheRetentionDialect::PromptCacheOptions`].
///
/// This is the only value worth sending for the newer dialect: `"30m"` is the
/// field's default, so emitting it would change nothing. It is also
/// **Responses-API only** — see [`resolve_retention`].
const OPTIONS_LONG_TTL: &str = "12h";

/// Which request-body spelling (if any) a model's provider accepts for
/// **extended** prompt-cache retention.
///
/// Declared per model on [`ModelEntry`](crate::ModelEntry) by the catalog
/// overlay's `cache_retention` model scalar; the default is
/// [`CacheRetentionDialect::None`], which emits nothing. It is per-model
/// because `openai` serves both a pre-GPT-5.6 generation (top-level
/// `prompt_cache_retention`) and a GPT-5.6+ generation (nested
/// `prompt_cache_options.ttl`), and a provider-level value cannot be right for
/// both.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum CacheRetentionDialect {
    /// Emit no retention selector (the default).
    #[default]
    None,
    /// Pre-GPT-5.6 `OpenAI` models: the top-level `prompt_cache_retention`
    /// field, whose extended value is `"24h"`.
    PromptCacheRetention,
    /// GPT-5.6+ `OpenAI` models (`gpt-6*`, `gpt-5.6*`): the nested
    /// `prompt_cache_options.ttl` object, whose extended value `"12h"` is
    /// accepted on the Responses API only.
    PromptCacheOptions,
}

impl CacheRetentionDialect {
    /// Parse an overlay dialect token — the lowercase spelling the
    /// `cache_retention` overlay scalar uses (`"none"`,
    /// `"prompt-cache-retention"`, `"prompt-cache-options"`).
    ///
    /// Returns `None` for an unrecognized value so the caller can warn and keep
    /// the safe [`Self::None`] default rather than guess a spelling the
    /// provider's model generation may not accept.
    #[must_use]
    pub(crate) fn from_overlay_str(value: &str) -> Option<Self> {
        match value {
            "none" => Some(Self::None),
            "prompt-cache-retention" => Some(Self::PromptCacheRetention),
            "prompt-cache-options" => Some(Self::PromptCacheOptions),
            _ => None,
        }
    }
}

/// The `prompt_cache_options` value object: the newer dialect's retention
/// selector nests under this key rather than sitting at the top level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub(crate) struct CacheRetentionOptions {
    /// The requested retention TTL (`"12h"` for the extended tier).
    pub(crate) ttl: &'static str,
}

/// The retention body fields resolved for one request.
///
/// At most one is `Some`, since the two are mutually exclusive wire spellings
/// of the same selection. `Default` (both `None`) is what a model that did not
/// opt in (or one without a long tier) yields.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct CacheRetentionFields {
    /// The top-level `prompt_cache_retention` field, for the older dialect.
    pub(crate) prompt_cache_retention: Option<&'static str>,
    /// The nested `prompt_cache_options` object, for the newer dialect.
    pub(crate) prompt_cache_options: Option<CacheRetentionOptions>,
}

impl CacheRetentionFields {
    /// Whether the resolution produced nothing to send.
    #[must_use]
    fn is_empty(&self) -> bool {
        self.prompt_cache_retention.is_none() && self.prompt_cache_options.is_none()
    }
}

/// The pure resolution of the retention body fields from a model's catalog
/// facts.
///
/// Split from the catalog lookup so the generation-boundary rules are testable
/// without swapping the process-global catalog:
///
/// - [`CacheRetentionDialect::None`] — nothing, regardless of the TTL.
/// - [`CacheRetentionDialect::PromptCacheRetention`] — `prompt_cache_retention:
///   "24h"` on both the Chat Completions and Responses APIs.
/// - [`CacheRetentionDialect::PromptCacheOptions`] — `prompt_cache_options: {
///   "ttl": "12h" }` on the Responses API **only**. On Chat Completions the
///   only extended value is unavailable and the remaining `"30m"` is the
///   field's default (no extension at all), so the builder omits the selector
///   rather than sending a value that changes nothing.
/// - No declared long tier — nothing, because the model does not expose the
///   extended retention this selector asks for.
#[must_use]
pub(crate) fn resolve_retention(
    dialect: CacheRetentionDialect,
    has_long_tier: bool,
    is_responses: bool,
) -> CacheRetentionFields {
    if !has_long_tier {
        return CacheRetentionFields::default();
    }
    match dialect {
        CacheRetentionDialect::None => CacheRetentionFields::default(),
        CacheRetentionDialect::PromptCacheRetention => CacheRetentionFields {
            prompt_cache_retention: Some(RETENTION_LONG_VALUE),
            prompt_cache_options: None,
        },
        // `"12h"` is a Responses-API value; Chat Completions omits the
        // selector entirely (see this function's docs).
        CacheRetentionDialect::PromptCacheOptions if is_responses => CacheRetentionFields {
            prompt_cache_retention: None,
            prompt_cache_options: Some(CacheRetentionOptions {
                ttl: OPTIONS_LONG_TTL,
            }),
        },
        CacheRetentionDialect::PromptCacheOptions => CacheRetentionFields::default(),
    }
}

/// Resolve the extended-retention body fields to add for one request.
///
/// This is the entry point both `OpenAI` body builders call. It reads the
/// model's dialect and curated long tier from the live catalog and withholds
/// the selector when the process already learned (from a 400) that the
/// provider rejects a prompt-cache request field.
///
/// `is_responses` selects the API whose body is being built, so the
/// Responses-only `"12h"` value is never emitted on Chat Completions.
#[must_use]
pub(crate) fn cache_retention_fields(
    provider_slug: &str,
    model: &str,
    is_responses: bool,
) -> CacheRetentionFields {
    let dialect = crate::catalog::cache_retention(provider_slug, model);
    if dialect == CacheRetentionDialect::None {
        return CacheRetentionFields::default();
    }
    let has_long_tier = crate::catalog::prompt_cache_ttl(provider_slug, model)
        .is_some_and(|ttl| ttl.long_secs.is_some());
    let fields = resolve_retention(dialect, has_long_tier, is_responses);
    if fields.is_empty() {
        return fields;
    }
    // The rejection registry can only withhold the selector: a provider that
    // answered a 400 naming the field is never sent it again.
    if crate::session_routing::cache_field_rejected(provider_slug) {
        debug!(
            provider = %provider_slug,
            model = %model,
            "withholding prompt-cache retention: the provider rejected a cache field",
        );
        return CacheRetentionFields::default();
    }
    debug!(
        provider = %provider_slug,
        model = %model,
        ?dialect,
        is_responses,
        "attaching extended prompt-cache retention",
    );
    fields
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlay_tokens_parse_and_unknown_is_none() {
        assert_eq!(
            CacheRetentionDialect::from_overlay_str("none"),
            Some(CacheRetentionDialect::None)
        );
        assert_eq!(
            CacheRetentionDialect::from_overlay_str("prompt-cache-retention"),
            Some(CacheRetentionDialect::PromptCacheRetention)
        );
        assert_eq!(
            CacheRetentionDialect::from_overlay_str("prompt-cache-options"),
            Some(CacheRetentionDialect::PromptCacheOptions)
        );
        // A typo or an unsupported token must not guess.
        assert_eq!(CacheRetentionDialect::from_overlay_str("24h"), None);
        assert_eq!(CacheRetentionDialect::from_overlay_str(""), None);
    }

    #[test]
    fn default_is_none() {
        assert_eq!(
            CacheRetentionDialect::default(),
            CacheRetentionDialect::None
        );
    }

    #[test]
    fn none_emits_nothing() {
        assert_eq!(
            resolve_retention(CacheRetentionDialect::None, true, true),
            CacheRetentionFields::default(),
        );
        assert_eq!(
            resolve_retention(CacheRetentionDialect::None, true, false),
            CacheRetentionFields::default(),
        );
    }

    #[test]
    fn without_a_long_tier_nothing_is_emitted() {
        // The selector asks for the extended tier, so a model that declares no
        // long tier is never sent one — the curated TTL data is the gate.
        for dialect in [
            CacheRetentionDialect::PromptCacheRetention,
            CacheRetentionDialect::PromptCacheOptions,
        ] {
            assert_eq!(
                resolve_retention(dialect, false, true),
                CacheRetentionFields::default(),
                "{dialect:?} must not emit without a long tier",
            );
            assert_eq!(
                resolve_retention(dialect, false, false),
                CacheRetentionFields::default(),
            );
        }
    }

    #[test]
    fn the_older_dialect_emits_the_top_level_field_on_both_apis() {
        for is_responses in [true, false] {
            let fields = resolve_retention(
                CacheRetentionDialect::PromptCacheRetention,
                true,
                is_responses,
            );
            assert_eq!(fields.prompt_cache_retention, Some(RETENTION_LONG_VALUE));
            assert!(fields.prompt_cache_options.is_none());
        }
    }

    #[test]
    fn the_newer_dialect_emits_the_options_object_on_responses() {
        let fields = resolve_retention(CacheRetentionDialect::PromptCacheOptions, true, true);
        assert_eq!(
            fields.prompt_cache_options,
            Some(CacheRetentionOptions {
                ttl: OPTIONS_LONG_TTL
            }),
        );
        assert!(fields.prompt_cache_retention.is_none());
    }

    #[test]
    fn the_newer_dialect_never_emits_on_chat_completions() {
        // "12h" is Responses-only, and the remaining "30m" is the field's
        // default, so Chat Completions gets no selector at all.
        let fields = resolve_retention(CacheRetentionDialect::PromptCacheOptions, true, false);
        assert_eq!(fields, CacheRetentionFields::default());
    }
}
