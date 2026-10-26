//! The process-wide registry of prompt-cache request fields a provider rejected.
//!
//! Adding a prompt-cache hint to a request body is only ever an optimisation:
//! `prompt_cache_key` / `session_id` (the per-session affinity dialects, see
//! [`crate::session_routing`]) and `prompt_cache_retention` /
//! `prompt_cache_options` (the extended-retention selectors, see
//! [`crate::cache_retention`]) are all optional additions. Providers differ in
//! how they treat an option they do not know: most **ignore** it, but some
//! answer an HTTP 400 naming it, which fails the whole turn. This module
//! remembers such a rejection for the process so the field is never sent to
//! that model again.
//!
//! # One registry for the whole prompt-cache surface
//!
//! The affinity hints and the retention selectors are one family of optional
//! additions, so they share one registry: a model that rejected an unknown
//! `prompt_cache_key` is not asked to accept a `prompt_cache_retention` either.
//! Every field name that family puts on a body is listed in
//! [`PROMPT_CACHE_FIELDS`], which is why the names live here rather than beside
//! the resolvers that emit them — the detector and the resolvers must agree on
//! the exact spelling a 400 has to mention.
//!
//! # Keyed by the exact `(provider, model)` pair
//!
//! Never a provider-wide or model-prefix judgment: a provider often accepts a
//! field on one model and rejects it on another (a newest model that does not
//! yet support `prompt_cache_key`, say), so a rejection recorded for one model
//! must not withhold the hint from a sibling model the provider still honours
//! it on. Membership is an exact test on the pair.

use std::collections::{HashMap, HashSet};
use std::sync::{LazyLock, RwLock};

use tracing::warn;

/// The top-level body field that carries the
/// [`CacheAffinity::PromptCacheKey`](crate::session_routing::CacheAffinity::PromptCacheKey)
/// dialect.
pub(crate) const PROMPT_CACHE_KEY_FIELD: &str = "prompt_cache_key";

/// The top-level body field that carries the
/// [`CacheAffinity::OpenRouterSession`](crate::session_routing::CacheAffinity::OpenRouterSession)
/// dialect.
pub(crate) const SESSION_ID_FIELD: &str = "session_id";

/// The top-level body field carrying the pre-GPT-5.6 retention selector.
pub(crate) const PROMPT_CACHE_RETENTION_FIELD: &str = "prompt_cache_retention";

/// The nested body field carrying the GPT-5.6+ retention selector.
pub(crate) const PROMPT_CACHE_OPTIONS_FIELD: &str = "prompt_cache_options";

/// Every optional prompt-cache field a request body can carry.
///
/// One list, consumed by both [`sent_cache_fields`] and
/// [`cache_field_rejection_field`], so a new spelling cannot be emitted by a
/// body builder and forgotten by the rejection detector. The affinity dialects
/// are mutually exclusive with each other, as are the retention dialects, but
/// the two families are independent — a body CAN carry `prompt_cache_key` and
/// `prompt_cache_retention` together — so the detector must consider every
/// entry, never just the first one present.
pub(crate) const PROMPT_CACHE_FIELDS: [&str; 4] = [
    PROMPT_CACHE_KEY_FIELD,
    SESSION_ID_FIELD,
    PROMPT_CACHE_RETENTION_FIELD,
    PROMPT_CACHE_OPTIONS_FIELD,
];

/// The `(provider slug, model id)` pairs whose endpoint answered a prompt-cache
/// request field with an HTTP 400 naming that field, i.e. reject it rather than
/// ignore it. A model that *ignores* an unknown field never appears here (it
/// answers 2xx), so only a genuine rejection downgrades.
///
/// [`effective_cache_affinity`](crate::session_routing::effective_cache_affinity)
/// and [`crate::cache_retention`] read this before a request body is built and
/// [`note_cache_field_rejected`] inserts into it once a 400 is final, so one
/// rejected request permanently (for the process) stops that model from
/// receiving any of them — resending a parameter the server rejects can only
/// waste a turn.
///
/// This is a process-wide behavioral cache: every request of an opted-in
/// provider consults it (a shared read lock — the `Off`/`None` default skips
/// the lookup entirely), and only a matching 400 writes it (a write lock, once
/// per pair), so readers never contend with one another.
///
/// Shaped as provider → models so a read is **allocation-free**: `get` on the
/// outer `String` map borrows the caller's `&str` slug, and `contains` on the
/// inner `String` set borrows the caller's `&str` model, so a lookup — which
/// happens on every request body build — allocates nothing. A flat
/// `HashSet<(String, String)>` would have to allocate both halves of a probe
/// key on every read because `(String, String)` cannot be borrowed as
/// `(&str, &str)`.
///
/// It is single-purpose bookkeeping that carries no protocol data (a set of
/// provider+model slugs); the lock recovery follows AGENTS.md's poisoned-lock
/// rule, with a lost entry an acceptable failure mode for a downgrade hint.
static CACHE_FIELD_REJECTED: LazyLock<RwLock<HashMap<String, HashSet<String>>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

/// Whether this process already learned, from an HTTP 400 naming the field,
/// that `slug`/`model` rejects a prompt-cache request field (affinity or
/// retention).
///
/// This is the single read side of [`CACHE_FIELD_REJECTED`], shared by the
/// affinity resolver and the extended-retention resolver so they can never
/// consult different memories. The lookup is an exact `(slug, model)` match and
/// allocates nothing.
#[must_use]
pub(crate) fn cache_field_rejected(slug: &str, model: &str) -> bool {
    CACHE_FIELD_REJECTED
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(slug)
        .is_some_and(|models| models.contains(model))
}

/// Record that `slug`/`model` rejected the prompt-cache request field `field`,
/// so future requests send none of them. Warns once per pair, naming only the
/// provider, the model, and the field (never the request or response body).
pub(crate) fn note_cache_field_rejected(slug: &str, model: &str, field: &str) {
    let fresh = CACHE_FIELD_REJECTED
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .entry(slug.to_string())
        .or_default()
        .insert(model.to_string());
    if fresh {
        warn!(
            provider = %slug,
            model = %model,
            field = %field,
            "provider rejected a prompt-cache request field (HTTP 400); \
             omitting it for the rest of this process",
        );
    }
}

/// The prompt-cache request fields a built request body actually carries.
///
/// A body can carry more than one: the two affinity dialects are mutually
/// exclusive with each other and so are the two retention dialects, but the
/// families are independent (a provider may be dialected for an affinity key
/// and its model pinned for extended retention), so this yields every field
/// present rather than the first.
pub(crate) fn sent_cache_fields(
    body: &serde_json::Value,
) -> impl Iterator<Item = &'static str> + '_ {
    PROMPT_CACHE_FIELDS
        .into_iter()
        .filter(move |field| body.get(*field).is_some())
}

/// The prompt-cache request field a terminal response rejects, or `None` when
/// it is not a genuine rejection of a field that was sent.
///
/// A response counts only when it is an HTTP **400** *and* its extracted detail
/// mentions one of the fields carried by `body` (`prompt_cache_key`,
/// `session_id` for the `OpenRouter` dialect, `prompt_cache_retention`, or
/// `prompt_cache_options`). Any other status (429, 5xx, a different 4xx) and any
/// 400 that does not name a sent field — a malformed request, an unknown model,
/// an unrelated schema error — returns `None`, so nothing else downgrades.
///
/// Every sent field is considered, not only the first: one registry covers the
/// whole prompt-cache surface, so a 400 naming a retention selector must
/// downgrade a body that also carries an affinity key. `body` is inspected,
/// never logged.
#[must_use]
pub(crate) fn cache_field_rejection_field(
    body: &serde_json::Value,
    status: u16,
    detail: &str,
) -> Option<&'static str> {
    if status != 400 {
        return None;
    }
    sent_cache_fields(body).find(|field| detail.contains(*field))
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- the rejection registry ---------------------------------------
    //
    // The static is process-wide, so every case below uses its OWN (slug, model)
    // pair: cargo nextest runs each test in a fresh process, but libtest does
    // not, and a shared key would let one test's rejection leak into another's
    // assertion.

    #[test]
    fn a_recorded_rejection_is_an_exact_pair_match() {
        // A rejection on one model must not withhold the hint from a sibling
        // model on the same provider — the lookup is an exact (provider, model)
        // match, never a prefix or substring of the model id.
        let slug = "rejection-scope-slug";
        assert!(!cache_field_rejected(slug, "rejected-model"));
        note_cache_field_rejected(slug, "rejected-model", PROMPT_CACHE_KEY_FIELD);
        assert!(cache_field_rejected(slug, "rejected-model"));
        // A different model on the same provider is untouched...
        assert!(!cache_field_rejected(slug, "sibling-model"));
        // ...as is a model whose id merely has the rejected id as a prefix, and
        // vice versa (exact match, not substring).
        assert!(!cache_field_rejected(slug, "rejected-model-preview"));
        note_cache_field_rejected(slug, "long-model", PROMPT_CACHE_KEY_FIELD);
        assert!(!cache_field_rejected(slug, "long"));
        // A different provider with the same model id is untouched too.
        assert!(!cache_field_rejected(
            "rejection-other-slug",
            "rejected-model"
        ));
    }

    #[test]
    fn recording_the_same_pair_twice_is_idempotent() {
        let slug = "rejection-idempotent-slug";
        note_cache_field_rejected(slug, "m", PROMPT_CACHE_KEY_FIELD);
        note_cache_field_rejected(slug, "m", PROMPT_CACHE_RETENTION_FIELD);
        assert!(cache_field_rejected(slug, "m"));
        // The second note is not "fresh" (the pair is already recorded), so it
        // must not warn again — and the pair stays recorded.
        assert!(cache_field_rejected(slug, "m"));
    }

    #[test]
    fn every_prompt_cache_field_is_in_the_sent_field_list() {
        // The one list is what the detector consults, so a spelling that exists
        // as a constant but is missing from it would be un-downgradable.
        for field in [
            PROMPT_CACHE_KEY_FIELD,
            SESSION_ID_FIELD,
            PROMPT_CACHE_RETENTION_FIELD,
            PROMPT_CACHE_OPTIONS_FIELD,
        ] {
            assert!(
                PROMPT_CACHE_FIELDS.contains(&field),
                "{field} must be in PROMPT_CACHE_FIELDS",
            );
        }
    }

    // -- what a body carried ------------------------------------------

    #[test]
    fn sent_cache_fields_reads_every_present_field() {
        let prompt = serde_json::json!({ "prompt_cache_key": "42" });
        assert_eq!(
            sent_cache_fields(&prompt).collect::<Vec<_>>(),
            [PROMPT_CACHE_KEY_FIELD],
        );
        let openrouter = serde_json::json!({ "session_id": "abc-123" });
        assert_eq!(
            sent_cache_fields(&openrouter).collect::<Vec<_>>(),
            [SESSION_ID_FIELD],
        );
        let neither = serde_json::json!({ "model": "gpt-5.4" });
        assert_eq!(sent_cache_fields(&neither).count(), 0);
    }

    #[test]
    fn sent_cache_fields_sees_both_retention_selectors() {
        let older = serde_json::json!({ "prompt_cache_retention": "24h" });
        assert_eq!(
            sent_cache_fields(&older).collect::<Vec<_>>(),
            [PROMPT_CACHE_RETENTION_FIELD],
        );
        let newer = serde_json::json!({ "prompt_cache_options": { "ttl": "12h" } });
        assert_eq!(
            sent_cache_fields(&newer).collect::<Vec<_>>(),
            [PROMPT_CACHE_OPTIONS_FIELD],
        );
    }

    #[test]
    fn sent_cache_fields_reports_both_families_on_one_body() {
        // The affinity and retention families are independent, so one body can
        // carry one of each. The detector must see both.
        let body = serde_json::json!({
            "prompt_cache_key": "42",
            "prompt_cache_retention": "24h",
        });
        assert_eq!(
            sent_cache_fields(&body).collect::<Vec<_>>(),
            [PROMPT_CACHE_KEY_FIELD, PROMPT_CACHE_RETENTION_FIELD],
        );
    }

    // -- rejection detection ------------------------------------------

    #[test]
    fn a_400_naming_the_sent_field_is_a_rejection() {
        let body = serde_json::json!({ "prompt_cache_key": "42" });
        assert_eq!(
            cache_field_rejection_field(
                &body,
                400,
                "Unrecognized request argument supplied: prompt_cache_key",
            ),
            Some(PROMPT_CACHE_KEY_FIELD),
        );
    }

    #[test]
    fn a_400_naming_a_co_present_field_is_a_rejection() {
        // Regression guard: a body that carries an affinity key AND a retention
        // selector must downgrade on a 400 naming EITHER of them. Checking only
        // the first field present would leave the rejected selector in every
        // later request, failing every turn with the same 400.
        let body = serde_json::json!({
            "prompt_cache_key": "42",
            "prompt_cache_retention": "24h",
        });
        assert_eq!(
            cache_field_rejection_field(
                &body,
                400,
                "Unsupported parameter: 'prompt_cache_retention'",
            ),
            Some(PROMPT_CACHE_RETENTION_FIELD),
        );
        // The affinity field is still detected on the same body.
        assert_eq!(
            cache_field_rejection_field(&body, 400, "unknown field prompt_cache_key"),
            Some(PROMPT_CACHE_KEY_FIELD),
        );
    }

    #[test]
    fn a_400_naming_the_wrong_field_is_not_a_rejection() {
        // The server names `session_id`, but `prompt_cache_key` is what was sent
        // — so this is not a rejection of the affinity hint.
        let body = serde_json::json!({ "prompt_cache_key": "42" });
        assert_eq!(
            cache_field_rejection_field(&body, 400, "unsupported: session_id"),
            None,
        );
    }

    #[test]
    fn an_unrelated_400_is_not_a_rejection() {
        let body = serde_json::json!({ "prompt_cache_key": "42" });
        assert_eq!(
            cache_field_rejection_field(&body, 400, "invalid model: gpt-9"),
            None,
        );
    }

    #[test]
    fn a_non_400_is_not_a_rejection() {
        // Even when the body prose mentions the field, only a 400 downgrades.
        let body = serde_json::json!({ "prompt_cache_key": "42" });
        assert_eq!(
            cache_field_rejection_field(&body, 429, "rate limited: prompt_cache_key"),
            None,
        );
        assert_eq!(
            cache_field_rejection_field(&body, 500, "server error: prompt_cache_key"),
            None,
        );
    }

    #[test]
    fn a_body_without_the_field_is_never_a_rejection() {
        // An `Off` provider's body carries no affinity field, so even a 400 that
        // happens to mention the name cannot downgrade it.
        let body = serde_json::json!({ "model": "gpt-5.4" });
        assert_eq!(
            cache_field_rejection_field(&body, 400, "bad prompt_cache_key"),
            None,
        );
    }

    #[test]
    fn a_400_naming_either_retention_field_is_a_rejection() {
        let older = serde_json::json!({ "prompt_cache_retention": "24h" });
        assert_eq!(
            cache_field_rejection_field(
                &older,
                400,
                "Unsupported parameter: 'prompt_cache_retention'",
            ),
            Some(PROMPT_CACHE_RETENTION_FIELD),
        );
        let newer = serde_json::json!({ "prompt_cache_options": { "ttl": "12h" } });
        assert_eq!(
            cache_field_rejection_field(&newer, 400, "Unknown parameter: prompt_cache_options"),
            Some(PROMPT_CACHE_OPTIONS_FIELD),
        );
    }
}
