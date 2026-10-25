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

use std::collections::HashSet;
use std::sync::{LazyLock, RwLock};

use tracing::{debug, warn};

/// Maximum length of an `OpenRouter` session id, per its documented constraint.
const OPENROUTER_SESSION_MAX_LEN: usize = 256;

/// The top-level body field that carries the [`CacheAffinity::PromptCacheKey`]
/// dialect. Named once so the rejection detector and the resolver agree on the
/// exact spelling a 400 must mention.
pub(crate) const PROMPT_CACHE_KEY_FIELD: &str = "prompt_cache_key";

/// The top-level body field that carries the [`CacheAffinity::OpenRouterSession`]
/// dialect.
const SESSION_ID_FIELD: &str = "session_id";

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

/// Provider slugs whose endpoint answered the affinity body field with an HTTP
/// 400 naming that field, i.e. reject the hint rather than ignore it.
///
/// [`effective_cache_affinity`] reads this before a request body is built and
/// [`note_cache_affinity_rejected`] inserts into it once a 400 is final, so one
/// rejected request permanently (for the process) stops that provider from
/// receiving the field — resending a parameter the server rejects can only
/// waste a turn. A provider that *ignores* an unknown field never appears here
/// (it answers 2xx), so only a genuine rejection downgrades.
///
/// This is a process-wide behavioral cache: every request of an
/// affinity-opted-in provider consults it (a shared read lock — the
/// [`CacheAffinity::Off`] default skips the lookup entirely), and only a
/// matching 400 writes it (a write lock, once per provider), so readers never
/// contend with one another.
/// It is single-purpose bookkeeping that carries no protocol data (a set of
/// provider slugs); the lock recovery follows AGENTS.md's poisoned-lock rule,
/// with a lost entry an acceptable failure mode for a downgrade hint.
static AFFINITY_REJECTED: LazyLock<RwLock<HashSet<String>>> =
    LazyLock::new(|| RwLock::new(HashSet::new()));

/// The cache affinity to actually use for `slug`: `configured`, unless this
/// process has already learned from a 400 that `slug` rejects the field.
///
/// The resolved precedence, lowest priority first, is the rejection registry
/// (which can only withhold a hint), then the catalog's `cache_affinity`
/// dialect carried in `configured`, then the per-account override already folded
/// into `configured` at construction. Because the override is applied to
/// `configured` before this lookup, a forced-on hint still reaches a provider
/// that has not rejected it, while a rejection keeps it off even when forced.
///
/// `Off` short-circuits **before** taking the lock, so a provider that never
/// opted in never pays for the lookup.
#[must_use]
pub(crate) fn effective_cache_affinity(slug: &str, configured: CacheAffinity) -> CacheAffinity {
    if configured == CacheAffinity::Off {
        return CacheAffinity::Off;
    }
    let rejected = AFFINITY_REJECTED
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .contains(slug);
    if rejected {
        CacheAffinity::Off
    } else {
        configured
    }
}

/// Whether the affinity hint `configured` for `slug` is being **withheld**
/// because this process already learned, from a 400 naming the field, that the
/// provider rejects it.
///
/// `true` requires both halves of the downgrade: the provider *intended* to
/// send a hint (`configured` is not [`CacheAffinity::Off`]) **and** the
/// request-time resolution comes back `Off` from the rejection registry. An
/// `Off` provider sent nothing to reject, so it is never downgraded and always
/// answers `false`.
///
/// This is the signal the cache-warming gate reads. A warm ping's economics
/// assume the re-send lands on a backend that still holds the prefix cache;
/// with the hint withheld the provider may route the ping anywhere, so it could
/// pay a full-price cache write the next real turn never reads.
#[must_use]
pub fn cache_affinity_downgraded(slug: &str, configured: CacheAffinity) -> bool {
    configured != CacheAffinity::Off
        && effective_cache_affinity(slug, configured) == CacheAffinity::Off
}

/// Record that `slug` rejected the affinity field `field`, so future requests
/// send no hint. Warns once per slug, naming only the provider and the field
/// (never the request or response body).
pub(crate) fn note_cache_affinity_rejected(slug: &str, field: &str) {
    let fresh = AFFINITY_REJECTED
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(slug.to_string());
    if fresh {
        warn!(
            provider = %slug,
            field = %field,
            "provider rejected the per-session cache-affinity field (HTTP 400); \
             omitting it for the rest of this process",
        );
    }
}

/// The affinity field name present in a built request body, if any.
///
/// Both wire spellings are mutually exclusive on a body, so the first match is
/// the one that was actually sent — the field a 400 must name to count as a
/// rejection of the hint rather than of something else in the request.
pub(crate) fn sent_affinity_field(body: &serde_json::Value) -> Option<&'static str> {
    if body.get(PROMPT_CACHE_KEY_FIELD).is_some() {
        Some(PROMPT_CACHE_KEY_FIELD)
    } else if body.get(SESSION_ID_FIELD).is_some() {
        Some(SESSION_ID_FIELD)
    } else {
        None
    }
}

/// The affinity field a terminal response rejects, or `None` when it is not a
/// genuine rejection of the hint that was sent.
///
/// A response counts only when it is an HTTP **400** *and* its extracted detail
/// mentions the very field carried by `body` (`prompt_cache_key`, or `session_id`
/// for the `OpenRouter` dialect). Any other status (429, 5xx, a different 4xx)
/// and any 400 that does not name the sent field — a malformed request, an
/// unknown model, an unrelated schema error — returns `None`, so nothing else
/// downgrades. `body` is inspected, never logged.
#[must_use]
pub(crate) fn affinity_rejection_field(
    body: &serde_json::Value,
    status: u16,
    detail: &str,
) -> Option<&'static str> {
    if status != 400 {
        return None;
    }
    let field = sent_affinity_field(body)?;
    detail.contains(field).then_some(field)
}

#[cfg(test)]
mod tests {
    use super::*;

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

    // -- the rejection registry ---------------------------------------
    //
    // The static is process-wide, so every case below uses its OWN slug: cargo
    // nextest runs each test in a fresh process, but libtest does not, and a
    // shared slug would let one test's rejection leak into another's assertion.

    #[test]
    fn effective_affinity_stays_configured_before_any_rejection() {
        // A never-rejected slug keeps the configured dialect.
        assert_eq!(
            effective_cache_affinity("affinity-clean-slug", CacheAffinity::PromptCacheKey),
            CacheAffinity::PromptCacheKey,
        );
    }

    #[test]
    fn effective_affinity_is_off_after_a_rejection() {
        // A rejection recorded for this slug downgrades it for the rest of the
        // process, for every dialect it might have been configured with.
        note_cache_affinity_rejected("affinity-rejected-slug", PROMPT_CACHE_KEY_FIELD);
        assert_eq!(
            effective_cache_affinity("affinity-rejected-slug", CacheAffinity::PromptCacheKey),
            CacheAffinity::Off,
        );
        assert_eq!(
            effective_cache_affinity("affinity-rejected-slug", CacheAffinity::OpenRouterSession),
            CacheAffinity::Off,
        );
    }

    #[test]
    fn off_provider_is_never_downgraded() {
        // `Off` short-circuits before the registry lookup, so it stays `Off`
        // whether or not the slug has a recorded rejection.
        assert_eq!(
            effective_cache_affinity("affinity-off-slug", CacheAffinity::Off),
            CacheAffinity::Off,
        );
        note_cache_affinity_rejected("affinity-off-slug", PROMPT_CACHE_KEY_FIELD);
        assert_eq!(
            effective_cache_affinity("affinity-off-slug", CacheAffinity::Off),
            CacheAffinity::Off,
        );
        // Because nothing was ever intended, no downgrade is signalled either —
        // this is the regression guard for the cache-warming gate: an `Off`
        // provider must keep warming exactly as it would with no registry.
        assert!(!cache_affinity_downgraded(
            "affinity-off-slug",
            CacheAffinity::Off
        ));
    }

    #[test]
    fn downgrade_signal_needs_an_intent_and_a_rejection() {
        // Intended, never rejected: not a downgrade.
        assert!(!cache_affinity_downgraded(
            "affinity-intent-clean-slug",
            CacheAffinity::PromptCacheKey,
        ));

        // Intended and then rejected: a downgrade.
        let slug = "affinity-intent-rejected-slug";
        note_cache_affinity_rejected(slug, PROMPT_CACHE_KEY_FIELD);
        assert!(cache_affinity_downgraded(
            slug,
            CacheAffinity::PromptCacheKey
        ));
        // Every dialect is downgraded alike, and an unknown provider is
        // unaffected (`Off` from an empty catalog lookup).
        assert!(cache_affinity_downgraded(
            slug,
            CacheAffinity::OpenRouterSession
        ));
        assert!(!cache_affinity_downgraded(
            "affinity-never-recorded-slug",
            CacheAffinity::Off,
        ));
    }

    #[test]
    fn sent_affinity_field_reads_the_body() {
        let prompt = serde_json::json!({ "prompt_cache_key": "42" });
        assert_eq!(sent_affinity_field(&prompt), Some(PROMPT_CACHE_KEY_FIELD));
        let openrouter = serde_json::json!({ "session_id": "abc-123" });
        assert_eq!(sent_affinity_field(&openrouter), Some(SESSION_ID_FIELD));
        let neither = serde_json::json!({ "model": "gpt-5.4" });
        assert_eq!(sent_affinity_field(&neither), None);
    }

    #[test]
    fn a_400_naming_the_sent_field_is_a_rejection() {
        let body = serde_json::json!({ "prompt_cache_key": "42" });
        assert_eq!(
            affinity_rejection_field(
                &body,
                400,
                "Unrecognized request argument supplied: prompt_cache_key",
            ),
            Some(PROMPT_CACHE_KEY_FIELD),
        );
    }

    #[test]
    fn a_400_naming_the_wrong_field_is_not_a_rejection() {
        // The server names `session_id`, but `prompt_cache_key` is what was sent
        // — so this is not a rejection of the affinity hint.
        let body = serde_json::json!({ "prompt_cache_key": "42" });
        assert_eq!(
            affinity_rejection_field(&body, 400, "unsupported: session_id"),
            None,
        );
    }

    #[test]
    fn an_unrelated_400_is_not_a_rejection() {
        let body = serde_json::json!({ "prompt_cache_key": "42" });
        assert_eq!(
            affinity_rejection_field(&body, 400, "invalid model: gpt-9"),
            None,
        );
    }

    #[test]
    fn a_non_400_is_not_a_rejection() {
        // Even when the body prose mentions the field, only a 400 downgrades.
        let body = serde_json::json!({ "prompt_cache_key": "42" });
        assert_eq!(
            affinity_rejection_field(&body, 429, "rate limited: prompt_cache_key"),
            None,
        );
        assert_eq!(
            affinity_rejection_field(&body, 500, "server error: prompt_cache_key"),
            None,
        );
    }

    #[test]
    fn a_body_without_the_field_is_never_a_rejection() {
        // An `Off` provider's body carries no affinity field, so even a 400 that
        // happens to mention the name cannot downgrade it.
        let body = serde_json::json!({ "model": "gpt-5.4" });
        assert_eq!(
            affinity_rejection_field(&body, 400, "bad prompt_cache_key"),
            None,
        );
    }
}
