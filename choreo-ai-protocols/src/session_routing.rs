//! Per-session cache-affinity routing hints for provider request bodies.
//!
//! Some OpenAI-compatible providers accept a *session identifier* in the request
//! body as a routing/affinity hint: requests that share the identifier are more
//! likely to be served by the same backend, whose prompt cache is then warm for
//! their shared prefix. The hint's shape is not universal, so it is modelled as
//! a per-provider capability ([`CacheAffinity`]) selected by the catalog overlay
//! (`cache_affinity = "…"`), and this module is the one place that turns the
//! configured dialect plus the turn's real session id into the body fields to
//! add.
//!
//! # Model-generation semantics
//!
//! The hint means different things across `OpenAI` model generations, and that
//! difference is exactly why the value must be the real per-session id and never
//! a constant:
//!
//! - On **older `OpenAI` models** the key only optimises **routing** — it biases
//!   which server a request lands on, so more of a session's requests reach a
//!   machine that already holds its prefix.
//! - On **GPT-5.6+** models the key additionally **separates cache accounting**
//!   (cache hits are bucketed by key), while routing itself is decided by a hash
//!   of the prompt tokens. A constant key there would merely partition the
//!   accounting rather than improve routing.
//!
//! Either way a *fixed* key is wrong: it would pin every session to one bucket
//! and, on the newer models, split cache accounting apart. Only the daemon's
//! real session id gives each conversation its own affinity.
//!
//! # Rejections are withheld by a shared registry
//!
//! A provider that answers a *prompt-cache request field* with an HTTP 400 —
//! these affinity fields, or the extended-retention selectors in
//! [`crate::cache_retention`] — is remembered by
//! [`crate::cache_field_rejection`], which withholds every such field from that
//! `(provider, model)` pair for the rest of the process. The affinity resolution
//! here is the read side of that registry.

use tracing::debug;

use crate::cache_field_rejection::cache_field_rejected;

/// Maximum length of an `OpenRouter` session id, per its documented constraint.
const OPENROUTER_SESSION_MAX_LEN: usize = 256;

/// Which request-body field (if any) a provider accepts as a per-session
/// cache-affinity hint.
///
/// Selected per provider by the catalog overlay's `cache_affinity` scalar; the
/// default is [`CacheAffinity::Off`], which sends no hint. The mapping from the
/// enum to the actual body fields lives in the crate-internal resolver
/// `cache_affinity_fields`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum CacheAffinity {
    /// Send no session hint (the default).
    #[default]
    Off,
    /// Send the session id as the top-level `prompt_cache_key` body field, the
    /// hint OpenAI-compatible providers document (`OpenAI`, Venice, …).
    PromptCacheKey,
    /// Send the session id as `OpenRouter`'s top-level `session_id` body field.
    /// `OpenRouter` prefers this over `prompt_cache_key`; it ignores an id that
    /// does not match its charset, so the resolver validates rather
    /// than sending a rejected value.
    OpenRouterSession,
}

impl CacheAffinity {
    /// Parse an overlay dialect token — the lowercase spelling the
    /// `cache_affinity` overlay scalar uses (`"off"`, `"prompt-cache-key"`,
    /// `"openrouter-session"`).  Returns `None` for an unrecognized value so the
    /// caller can warn and keep the safe [`Self::Off`] default rather than guess
    /// a dialect the provider may not understand.
    #[must_use]
    pub(crate) fn from_overlay_str(value: &str) -> Option<Self> {
        match value {
            "off" => Some(Self::Off),
            "prompt-cache-key" => Some(Self::PromptCacheKey),
            "openrouter-session" => Some(Self::OpenRouterSession),
            _ => None,
        }
    }
}

/// The per-session affinity body fields resolved for one request.
///
/// Both fields borrow the turn's session id (never a copy), and at most one is
/// `Some` for any [`CacheAffinity`] dialect — the two are mutually exclusive
/// wire spellings of the same hint. A `Default` value (both `None`) is what an
/// [`CacheAffinity::Off`] provider, or an invalid `OpenRouter` id, yields.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct CacheAffinityFields<'a> {
    /// The `prompt_cache_key` body field, when the provider's dialect uses it.
    pub(crate) prompt_cache_key: Option<&'a str>,
    /// The `session_id` body field, when the provider's dialect uses it.
    pub(crate) session_id: Option<&'a str>,
}

/// Resolve the body fields to add for `affinity` under the real `session_id` of
/// the turn.
///
/// Returns an all-`None` [`CacheAffinityFields`] for [`CacheAffinity::Off`], and
/// — for [`CacheAffinity::OpenRouterSession`] — for a `session_id` that fails
/// `OpenRouter`'s charset/length validation, so an id the provider would ignore
/// is omitted (and logged) rather than sent as garbage.
///
/// `model` is used only for the diagnostic line: it records which model the hint
/// is being sent for, so a routing-versus-accounting surprise on a
/// generation-boundary model is traceable without re-running the request.
#[must_use]
pub(crate) fn cache_affinity_fields<'a>(
    affinity: CacheAffinity,
    session_id: &'a str,
    model: &str,
) -> CacheAffinityFields<'a> {
    match affinity {
        CacheAffinity::Off => CacheAffinityFields::default(),
        CacheAffinity::PromptCacheKey => {
            debug!(
                model = %model,
                session_id_len = session_id.len(),
                "attaching prompt_cache_key session affinity",
            );
            CacheAffinityFields {
                prompt_cache_key: Some(session_id),
                session_id: None,
            }
        }
        CacheAffinity::OpenRouterSession => {
            // OpenRouter silently drops a session id that is empty, longer than
            // 256 bytes, or contains anything outside `[A-Za-z0-9_-]`; sending
            // one would be a no-op at best, so validate here and omit it.
            if !is_valid_openrouter_session_id(session_id) {
                debug!(
                    model = %model,
                    session_id_len = session_id.len(),
                    "session id is not a valid OpenRouter session id; omitting session_id",
                );
                return CacheAffinityFields::default();
            }
            debug!(
                model = %model,
                session_id_len = session_id.len(),
                "attaching OpenRouter session_id affinity",
            );
            CacheAffinityFields {
                prompt_cache_key: None,
                session_id: Some(session_id),
            }
        }
    }
}

/// Whether `id` satisfies `OpenRouter`'s documented session-id constraint:
/// non-empty, at most [`OPENROUTER_SESSION_MAX_LEN`] bytes, and drawn only from
/// `[A-Za-z0-9_-]`. An empty id is treated as invalid because it carries no
/// affinity even though it technically matches the character class.
fn is_valid_openrouter_session_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= OPENROUTER_SESSION_MAX_LEN
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// The cache affinity to actually use for `slug`/`model`: `configured`,
/// unless this process has already learned from a 400 that the pair rejects the
/// field.
///
/// The resolved precedence, lowest priority first, is the rejection registry
/// (which can only withhold a hint), then the catalog's `cache_affinity`
/// dialect carried in `configured`, then the per-account override already folded
/// into `configured` at construction. Because the override is applied to
/// `configured` before this lookup, a forced-on hint still reaches a pair
/// that has not rejected it, while a rejection keeps it off even when forced.
///
/// The registry is consulted for the exact `(slug, model)` pair, so a rejection
/// on one model never withholds the hint from a sibling model. `Off`
/// short-circuits **before** taking the lock, so a provider that never opted in
/// never pays for the lookup.
#[must_use]
pub(crate) fn effective_cache_affinity(
    slug: &str,
    model: &str,
    configured: CacheAffinity,
) -> CacheAffinity {
    if configured == CacheAffinity::Off {
        return CacheAffinity::Off;
    }
    if cache_field_rejected(slug, model) {
        CacheAffinity::Off
    } else {
        configured
    }
}

/// Whether the affinity hint `configured` for `slug`/`model` is being
/// **withheld** because this process already learned, from a 400 naming the
/// field, that the pair rejects it.
///
/// `true` requires both halves of the downgrade: the request *intended* to
/// send a hint (`configured` is not [`CacheAffinity::Off`]) **and** the
/// request-time resolution comes back `Off` from the rejection registry. An
/// `Off` provider sent nothing to reject, so it is never downgraded and always
/// answers `false`.
///
/// This is the signal the cache-warming gate reads. A warm ping's economics
/// assume the re-send lands on a backend that still holds the prefix cache;
/// with the hint withheld the provider may route the ping anywhere, so it could
/// pay a full-price cache write the next real turn never reads. Callers pass the
/// affinity the *request* would send (catalog + account override), so a forced-on
/// hint that was then rejected is detected too.
#[must_use]
pub fn cache_affinity_downgraded(slug: &str, model: &str, configured: CacheAffinity) -> bool {
    configured != CacheAffinity::Off
        && effective_cache_affinity(slug, model, configured) == CacheAffinity::Off
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache_field_rejection::{PROMPT_CACHE_KEY_FIELD, note_cache_field_rejected};

    #[test]
    fn overlay_tokens_parse_and_unknown_is_none() {
        assert_eq!(
            CacheAffinity::from_overlay_str("off"),
            Some(CacheAffinity::Off)
        );
        assert_eq!(
            CacheAffinity::from_overlay_str("prompt-cache-key"),
            Some(CacheAffinity::PromptCacheKey)
        );
        assert_eq!(
            CacheAffinity::from_overlay_str("openrouter-session"),
            Some(CacheAffinity::OpenRouterSession)
        );
        // A typo or an unsupported dialect must not guess.
        assert_eq!(CacheAffinity::from_overlay_str("prompt_cache_key"), None);
        assert_eq!(CacheAffinity::from_overlay_str(""), None);
    }

    #[test]
    fn default_is_off() {
        assert_eq!(CacheAffinity::default(), CacheAffinity::Off);
    }

    #[test]
    fn off_yields_no_fields() {
        let fields = cache_affinity_fields(CacheAffinity::Off, "42", "gpt-5.4");
        assert_eq!(fields, CacheAffinityFields::default());
        assert!(fields.prompt_cache_key.is_none());
        assert!(fields.session_id.is_none());
    }

    #[test]
    fn prompt_cache_key_carries_the_real_session_id() {
        let fields = cache_affinity_fields(CacheAffinity::PromptCacheKey, "1844", "gpt-5.4");
        assert_eq!(fields.prompt_cache_key, Some("1844"));
        assert!(fields.session_id.is_none());
    }

    #[test]
    fn openrouter_session_carries_session_id_and_not_prompt_cache_key() {
        let fields = cache_affinity_fields(CacheAffinity::OpenRouterSession, "abc-123", "model");
        assert_eq!(fields.session_id, Some("abc-123"));
        assert!(fields.prompt_cache_key.is_none());
    }

    #[test]
    fn openrouter_session_rejects_an_invalid_id() {
        for bad in ["", "has space", "slash/y", "emoji🙂"] {
            let fields = cache_affinity_fields(CacheAffinity::OpenRouterSession, bad, "model");
            assert_eq!(
                fields,
                CacheAffinityFields::default(),
                "id {bad:?} must be rejected",
            );
        }
        // Over the 256-byte limit.
        let long = "a".repeat(OPENROUTER_SESSION_MAX_LEN + 1);
        let fields = cache_affinity_fields(CacheAffinity::OpenRouterSession, &long, "model");
        assert!(fields.session_id.is_none());
    }

    #[test]
    fn openrouter_session_accepts_the_character_class() {
        assert!(is_valid_openrouter_session_id("abc-123_XYZ"));
        // Exactly at the limit is still valid.
        let at_limit = "a".repeat(OPENROUTER_SESSION_MAX_LEN);
        assert!(is_valid_openrouter_session_id(&at_limit));
    }

    // -- the rejection registry's effect on affinity -------------------
    //
    // The registry is process-wide (`crate::cache_field_rejection`), so every
    // case below uses its OWN (slug, model) pair: cargo nextest runs each test
    // in a fresh process, but libtest does not, and a shared key would let one
    // test's rejection leak into another's assertion.

    #[test]
    fn effective_affinity_stays_configured_before_any_rejection() {
        // A never-rejected pair keeps the configured dialect.
        assert_eq!(
            effective_cache_affinity(
                "affinity-clean-slug",
                "clean-model",
                CacheAffinity::PromptCacheKey
            ),
            CacheAffinity::PromptCacheKey,
        );
    }

    #[test]
    fn effective_affinity_is_off_after_a_rejection() {
        // A rejection recorded for this pair downgrades it for the rest of the
        // process, for every dialect it might have been configured with.
        note_cache_field_rejected(
            "affinity-rejected-slug",
            "some-model",
            PROMPT_CACHE_KEY_FIELD,
        );
        assert_eq!(
            effective_cache_affinity(
                "affinity-rejected-slug",
                "some-model",
                CacheAffinity::PromptCacheKey
            ),
            CacheAffinity::Off,
        );
        assert_eq!(
            effective_cache_affinity(
                "affinity-rejected-slug",
                "some-model",
                CacheAffinity::OpenRouterSession
            ),
            CacheAffinity::Off,
        );
    }

    #[test]
    fn a_rejection_on_one_model_leaves_a_sibling_model_untouched() {
        // The registry is keyed by the exact pair, so the affinity resolution for
        // a sibling model on the same provider is unaffected.
        let slug = "affinity-model-scope-slug";
        note_cache_field_rejected(slug, "rejected-model", PROMPT_CACHE_KEY_FIELD);
        assert_eq!(
            effective_cache_affinity(slug, "rejected-model", CacheAffinity::PromptCacheKey),
            CacheAffinity::Off,
        );
        assert_eq!(
            effective_cache_affinity(slug, "sibling-model", CacheAffinity::PromptCacheKey),
            CacheAffinity::PromptCacheKey,
        );
    }

    #[test]
    fn off_provider_is_never_downgraded() {
        // `Off` short-circuits before the registry lookup, so it stays `Off`
        // whether or not the pair has a recorded rejection.
        assert_eq!(
            effective_cache_affinity("affinity-off-slug", "m", CacheAffinity::Off),
            CacheAffinity::Off,
        );
        note_cache_field_rejected("affinity-off-slug", "m", PROMPT_CACHE_KEY_FIELD);
        assert_eq!(
            effective_cache_affinity("affinity-off-slug", "m", CacheAffinity::Off),
            CacheAffinity::Off,
        );
        // Because nothing was ever intended, no downgrade is signalled either —
        // this is the regression guard for the cache-warming gate: an `Off`
        // provider must keep warming exactly as it would with no registry.
        assert!(!cache_affinity_downgraded(
            "affinity-off-slug",
            "m",
            CacheAffinity::Off
        ));
    }

    #[test]
    fn downgrade_signal_needs_an_intent_and_a_rejection() {
        // Intended, never rejected: not a downgrade.
        assert!(!cache_affinity_downgraded(
            "affinity-intent-clean-slug",
            "m",
            CacheAffinity::PromptCacheKey,
        ));

        // Intended and then rejected: a downgrade.
        let slug = "affinity-intent-rejected-slug";
        note_cache_field_rejected(slug, "m", PROMPT_CACHE_KEY_FIELD);
        assert!(cache_affinity_downgraded(
            slug,
            "m",
            CacheAffinity::PromptCacheKey
        ));
        // Every dialect is downgraded alike for the rejected pair, and an
        // unrelated pair is unaffected.
        assert!(cache_affinity_downgraded(
            slug,
            "m",
            CacheAffinity::OpenRouterSession
        ));
        assert!(!cache_affinity_downgraded(
            "affinity-never-recorded-slug",
            "m",
            CacheAffinity::Off,
        ));
    }
}
