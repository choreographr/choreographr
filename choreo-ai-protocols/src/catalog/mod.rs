//! Provider catalog — types, aggregation, and lookups.
//!
//! The catalog is a **two-layer pipeline**: a normalized snapshot of the
//! models.dev API (a *local, gitignored* `catalog/models.dev.json` artifact —
//! `catalog-gen` fetches a fresh copy from models.dev when it is missing and
//! caches it there — preprocessed by the `catalog-gen` binary into an embedded
//! postcard blob `catalog/catalog.bin`, the only committed catalog data file)
//! supplies the *facts* — provider slugs/names, base URLs, and per-model
//! reasoning/context data — and the bundled `catalog/models-overlay.toml`
//! policy layer (`include_str!`, merged at load time) supplies everything
//! models.dev cannot express: wire-protocol selection, endpoint policy,
//! per-model passback exceptions, and the providers models.dev does not cover.
//! The merge entry points ([`normalize_modelsdev`], [`merge_overlay`], and
//! [`load_bundled_base`]) are the public seam S4 reuses for the runtime user
//! overlay + cache.
//!
//! The catalog lives behind a process-wide [`ArcSwap`] (see
//! [`PROVIDER_CATALOG`]): readers are lock-free, and a runtime refresh can
//! atomically replace the whole catalog with [`replace_catalog`]. Single-writer
//! invariant: only the daemon command loop calls [`replace_catalog`] (after a
//! refresh, overlay change, or `/refresh-models`); every change *request*
//! travels over a channel, and only the atomic store mutates (documented as a
//! thread-communication exception in ARCHITECTURE.md).
//!
//! The per-model fact **lookups** ([`lookup_context_window`],
//! [`lookup_max_output_tokens`], [`model_request_format`], …) live in the
//! sibling `lookup` module and are re-exported here so the public surface of
//! the crate is unchanged; the entry types those lookups read live in `entry`,
//! and the OAuth login descriptors assembled from an entry's profile live in
//! `oauth`.

mod entry;
mod loader;
mod lookup;
mod modelsdev;
mod oauth;
mod overlay;
mod persist;
pub mod refresh;

pub use entry::{
    AuthKind, GrantEncoding, ModelCost, ModelEntry, OAuthProfile, PromptCacheTtl, ProviderEntry,
    ProviderProtocol, ReasoningPassback,
};
pub use loader::{bundled_overlay_src, load_bundled_base};
pub use lookup::{
    ChatReasoningEcho, chat_reasoning_echo, image_edit_models_for_provider,
    image_models_for_provider, lookup_context_window, lookup_max_output_tokens, model_cost,
    model_reasoning_capability, model_reasoning_passback, model_request_format,
    model_supports_image_output, model_supports_temperature, model_supports_vision,
    prompt_cache_ttl,
};
pub use modelsdev::normalize_modelsdev;
pub use oauth::oauth_provider_descriptors;
pub use overlay::merge_overlay;
pub use persist::write_file_atomic;
pub use refresh::{RefreshError, RefreshOutcome, fetch_modelsdev};

use std::sync::{Arc, LazyLock};
use tracing::debug;

use arc_swap::ArcSwap;

/// Process-wide catalog of all known providers, backed by an [`ArcSwap`] so a
/// runtime refresh can atomically swap it while readers stay lock-free.
///
/// `ArcSwap::from_pointee` is not a const fn and the TOML load is a runtime
/// parse, so the `ArcSwap` itself is lazily initialized behind a [`LazyLock`]:
/// the first access parses the bundled data once, and every later access goes
/// straight to the `ArcSwap` (an atomic load, then lock-free reads / atomic
/// `store` on swap). The lazy init keeps the first-access parse out of static
/// initialization while making the catalog runtime-swappable.
pub static PROVIDER_CATALOG: LazyLock<ArcSwap<Vec<ProviderEntry>>> =
    LazyLock::new(|| ArcSwap::from_pointee(loader::load_catalog()));

/// Atomically replace the process-wide catalog. Single-writer invariant:
/// only the daemon command loop calls this (after a refresh, overlay
/// change, or `/refresh-models`). Readers are lock-free.
pub fn replace_catalog(entries: Vec<ProviderEntry>) {
    debug!(
        providers = entries.len(),
        "replacing process-wide provider catalog",
    );
    PROVIDER_CATALOG.store(Arc::new(entries));
}

/// Return an immutable snapshot of the current catalog.
///
/// The returned `Arc` pins one version of the catalog independently of any
/// later swap, so a caller can iterate it (or clone entries out of it)
/// without holding an `ArcSwap` guard open.
pub fn catalog_snapshot() -> Arc<Vec<ProviderEntry>> {
    PROVIDER_CATALOG.load_full()
}

/// Look up a provider entry by slug. Returns `None` if not found.
///
/// Returns an *owned* clone taken out of the `ArcSwap` guard, so the entry
/// stays valid (and cheap to keep) even if the catalog is swapped underneath
/// the caller. Callers only read the fields transiently (`slug`, `base_url`,
/// `protocol`, `models`) and call this rarely (provider construction, not the
/// hot path), so the clone is fine.
pub fn lookup_provider(slug: &str) -> Option<ProviderEntry> {
    PROVIDER_CATALOG
        .load()
        .iter()
        .find(|e| e.slug == slug)
        .cloned()
}

/// Return all provider slugs as owned strings.
pub fn all_slugs() -> Vec<String> {
    PROVIDER_CATALOG
        .load()
        .iter()
        .map(|e| e.slug.clone())
        .collect()
}

/// Return all display names as owned strings.
pub fn all_display_names() -> Vec<String> {
    PROVIDER_CATALOG
        .load()
        .iter()
        .map(|e| e.display_name.clone())
        .collect()
}

/// Find which provider in the catalog owns a given model id (exact match on
/// the model's name).
///
/// This is the catalog's source of truth for the model→provider mapping
/// (ingested from models.dev + the overlay), so a caller that only has a model
/// id — e.g. a diagnostic with no recorded producer — can resolve its provider
/// without re-implementing name-prefix heuristics here that would inevitably
/// drift from the catalog and guess the wrong slug.
///
/// Returns `None` when the model id is not known to any provider; the caller
/// should treat that as "provider unresolvable" rather than guessing a slug
/// that would feed a wrong `model_reasoning_passback` / `chat_reasoning_echo`.
pub fn provider_slug_for_model(model: &str) -> Option<String> {
    let catalog = PROVIDER_CATALOG.load();
    catalog
        .iter()
        .find(|e| e.models.iter().any(|m| m.model == model))
        .map(|e| e.slug.clone())
}

#[cfg(test)]
// Shared test fixtures for the catalog tests. The resolver tests moved to
// `lookup.rs` but still need to swap the process-global catalog and restore
// it afterwards, and both modules' swap tests build the same synthetic
// one-provider catalog — so the helpers live here (next to
// `replace_catalog`/`loader`) and are shared via `pub(crate)`.
pub(crate) mod test_util {
    use super::ProviderEntry;
    use super::loader;
    use super::replace_catalog;
    use crate::retry::RequestCompression;
    use crate::shared::MaxTokensField;

    /// Restores the bundled catalog when dropped, so a failing swap test can
    /// never leave the process-global catalog swapped for later tests (the
    /// libtest fallback shares one process).
    pub(crate) struct RestoreBundledOnDrop;

    impl Drop for RestoreBundledOnDrop {
        fn drop(&mut self) {
            replace_catalog(loader::load_catalog());
        }
    }

    /// Build a minimal one-provider/one-model catalog for the swap tests.
    pub(crate) fn tiny_catalog() -> Vec<ProviderEntry> {
        vec![ProviderEntry {
            slug: "tiny-test".into(),
            display_name: "Tiny Test".into(),
            protocol: crate::catalog::ProviderProtocol::OpenAi {
                max_tokens_field: MaxTokensField::MaxTokens,
            },
            base_url: "https://tiny-test.example/v1".into(),
            default_model: "tiny-model".into(),
            prompt_cache: None,
            request_compression: RequestCompression::Identity,
            cache_affinity: crate::session_routing::CacheAffinity::Off,
            oauth: None,
            models: vec![crate::catalog::ModelEntry {
                model: "tiny-model".into(),
                context_window: 4096,
                reasoning_supported: true,
                openai_reasoning_levels: vec!["off".into(), "high".into()],
                max_output_tokens: 1024,
                ..Default::default()
            }],
        }]
    }
}

#[cfg(test)]
// All tests in this module read (and the `replace_catalog` tests mutate) the
// process-wide `PROVIDER_CATALOG`, so serialize them under one key: nextest
// gives per-process isolation anyway, but the libtest fallback shares one
// process across parallel threads and a mid-flight swap would race the readers.
#[serial_test::serial(catalog)]
mod tests {
    use super::*;

    #[test]
    fn embedded_catalog_loads() {
        // The merged catalog (embedded `catalog.bin` base + bundled overlay)
        // must parse and be sane: a broken artifact or overlay here means the
        // daemon silently loads an empty catalog, so this must fail loudly at
        // test time.
        let catalog = loader::load_catalog();
        assert!(
            catalog.len() >= 150,
            "expected >=150 providers from the embedded catalog, got {}",
            catalog.len()
        );
        // No duplicate slugs (the zai/github-copilot merges collapse three
        // old TOML providers each into one models.dev key).
        let mut seen = std::collections::HashSet::new();
        for entry in &catalog {
            assert!(
                seen.insert(entry.slug.as_str()),
                "duplicate slug in merged catalog: {}",
                entry.slug
            );
        }
        // Every provider has a name and a default model. base_url is NOT
        // required to be non-empty: models.dev carries several providers we
        // never hand-curated (cohere, azure, amazon-bedrock, …) with no API
        // endpoint — they are catalogued for their model lists, and only the
        // overlay-only providers and the api-less providers we pin carry an
        // explicit endpoint.
        for entry in &catalog {
            assert!(!entry.slug.is_empty(), "empty slug");
            assert!(
                !entry.display_name.is_empty(),
                "empty display_name for {}",
                entry.slug
            );
            assert!(
                !entry.default_model.is_empty(),
                "empty default model for {}",
                entry.slug
            );
        }
        // The one-time slug migration: renamed slugs present, old slugs gone.
        for slug in [
            "fireworks-ai",
            "togetherai",
            "github-copilot",
            "novita-ai",
            "salad-cloud",
            "kilo",
            "gmicloud",
            "vercel",
            "zhipuai",
            "zai",
        ] {
            assert!(
                seen.contains(slug),
                "renamed slug {slug} must be present in the merged catalog"
            );
        }
        for slug in [
            "fireworks",
            "together",
            "github",
            "novita",
            "saladcloud",
            "kilocode",
            "gmi",
            "vercel-ai-gateway",
            "zhipu",
            "zai-cn",
            "zai-coding-cn",
        ] {
            assert!(
                !seen.contains(slug),
                "old slug {slug} must be absent after the one-time migration"
            );
        }
    }

    #[test]
    fn opencode_gateway_base_urls_serve_the_model_list() {
        // models.dev publishes the opencode gateways' OpenAI *chat* bases
        // (…/inference/go/openai/v1 and …/inference/openai/v1), which have no
        // /models endpoint, so the default `base_url + /models` model-list join
        // 404s there. Both the model list and inference are served off the
        // shorter base one segment up, which the bundled overlay pins; this
        // guards it against a models.dev refresh silently reverting base_url to
        // a list-less endpoint (the regression that broke model listing).
        let catalog = loader::load_catalog();
        for (slug, expected) in [
            ("opencode-go", "https://opencode.ai/inference/go/v1"),
            ("opencode", "https://opencode.ai/inference/v1"),
        ] {
            let entry = catalog
                .iter()
                .find(|e| e.slug == slug)
                .unwrap_or_else(|| panic!("{slug} missing from the merged catalog"));
            assert_eq!(
                entry.base_url, expected,
                "{slug} base_url must be the base that also serves /models",
            );
        }
    }

    #[test]
    fn overlay_only_providers_survive_the_merge() {
        // Providers models.dev does not cover are defined wholesale in the
        // bundled overlay; each keeps its slug and carries its policy.
        for (slug, expected_name) in [
            ("aimlapi", "aimlapi.com"),
            ("ant-ling", "Ant Ling"),
            ("arcee", "Arcee AI"),
            ("atlascloud", "Atlas Cloud"),
            ("bankr", "Bankr"),
            ("futurmix", "FuturMix"),
            ("gitlawb-opengateway", "GitLawb OpenGateway"),
            ("iflytek", "iFlytek Spark"),
            ("iflytek-astron", "iFlytek Astron MaaS"),
            ("kimi-code", "Kimi Code subscription"),
            ("nous", "Nous Research"),
            ("omlx", "oMLX"),
            ("qwen-token-plan", "Qwen Token Plan"),
            ("qwen-token-plan-cn", "Qwen Token Plan CN"),
            ("routstr", "Routstr"),
            ("tanzu", "VMware Tanzu Platform"),
            ("tensorix", "Tensorix"),
            ("custom-openai", "Custom OpenAI-Compatible"),
            ("custom-anthropic", "Custom Anthropic-Compatible"),
            ("openai_compatible", "OpenAI Compatible"),
            ("ollama", "Ollama (Local)"),
        ] {
            let entry =
                lookup_provider(slug).unwrap_or_else(|| panic!("overlay-only {slug} missing"));
            assert_eq!(entry.display_name, expected_name);
            assert!(!entry.base_url.is_empty(), "{slug} needs a base_url");
            assert!(
                !entry.default_model.is_empty(),
                "{slug} needs a default_model"
            );
        }
        // kimi-code carries its full model list.
        let kimi = lookup_provider("kimi-code").expect("kimi-code");
        assert_eq!(
            kimi.models
                .iter()
                .map(|m| m.model.as_str())
                .collect::<Vec<_>>(),
            vec!["k3", "kimi-for-coding"]
        );
    }

    #[test]
    fn lookup_openai_returns_entry() {
        let entry = lookup_provider("openai");
        assert!(entry.is_some());
        assert_eq!(entry.unwrap().slug, "openai");
    }

    #[test]
    fn lookup_anthropic_returns_entry() {
        let entry = lookup_provider("anthropic");
        assert!(entry.is_some());
        assert_eq!(entry.unwrap().slug, "anthropic");
    }

    #[test]
    fn lookup_nonexistent_returns_none() {
        assert!(lookup_provider("nonexistent").is_none());
        assert!(lookup_provider("").is_none());
    }

    #[test]
    fn all_slugs_returns_expected_count() {
        let slugs = all_slugs();
        assert_eq!(slugs.len(), catalog_snapshot().len());
    }

    #[test]
    fn catalog_has_no_duplicate_slugs() {
        let mut seen = std::collections::HashSet::new();
        for entry in catalog_snapshot().iter() {
            assert!(
                seen.insert(entry.slug.as_str()),
                "duplicate slug: {}",
                entry.slug
            );
        }
    }

    #[test]
    fn catalog_entries_have_no_empty_fields() {
        for entry in catalog_snapshot().iter() {
            assert!(!entry.slug.is_empty(), "empty slug");
            assert!(
                !entry.display_name.is_empty(),
                "empty display_name for {}",
                entry.slug
            );
            assert!(
                !entry.default_model.is_empty(),
                "empty model for {}",
                entry.slug
            );
            // `context_window` is a `u32` where 0 means "unknown"; the previous
            // `== 0 || > 0` check was vacuously true for every model, so it is
            // dropped (clippy::double_comparisons). `base_url` may be empty for
            // models.dev-only providers without an endpoint (cohere, azure,
            // bedrock, …) — only the overlay-pinned providers carry one.
            for m in &entry.models {
                assert!(!m.model.is_empty(), "empty model slug in {}", entry.slug);
            }
        }
    }

    #[test]
    fn all_display_names_are_non_empty() {
        for name in all_display_names() {
            assert_ne!(name, "");
        }
    }

    #[test]
    fn provider_slug_for_model_resolves_owner_and_unknown_is_none() {
        // A known model id resolves to the provider that owns it (exact match).
        // Pick a real id out of the bundled catalog rather than hardcoding a
        // slug, so the assertion tracks the catalog instead of the test.
        let _restore = test_util::RestoreBundledOnDrop;
        let bundled = catalog_snapshot();
        let (slug, model) = bundled
            .iter()
            .find_map(|e| e.models.first().map(|m| (e.slug.clone(), m.model.clone())))
            .expect("bundled catalog has at least one provider with models");
        assert_eq!(
            provider_slug_for_model(&model).as_deref(),
            Some(slug.as_str())
        );

        // The swap is visible to the new lookup through the same process-global.
        replace_catalog(test_util::tiny_catalog());
        assert_eq!(
            provider_slug_for_model("tiny-model").as_deref(),
            Some("tiny-test")
        );
        assert!(
            provider_slug_for_model("gpt-4o").is_none(),
            "tiny catalog has no gpt-4o"
        );

        // An id no provider knows is None — never a guessed slug.
        replace_catalog(bundled.to_vec());
        assert_eq!(provider_slug_for_model("no_such_model_exists_xyz"), None);
    }

    #[test]
    fn lookup_provider_returns_owned_clone_outliving_guard() {
        // The returned entry is owned (cloned out of the ArcSwap guard), so it
        // remains valid even after the catalog is swapped underneath it.
        let _restore = test_util::RestoreBundledOnDrop;
        let entry = lookup_provider("openai").expect("openai is in the bundled catalog");

        replace_catalog(test_util::tiny_catalog());

        // The clone outlives the swap: its data is intact and independent.
        assert_eq!(entry.slug, "openai");
        assert_eq!(entry.base_url, "https://api.openai.com/v1");
        assert_eq!(entry.default_model, "gpt-5.4");
        assert!(!entry.models.is_empty());
    }

    #[test]
    fn model_cost_and_prompt_cache_round_trip_through_postcard() {
        // `catalog.bin` is postcard (positional, non-self-describing), so adding
        // the `f64`-bearing `ModelCost` and the `PromptCacheTtl` fields changes
        // the wire format. Pin that a catalog carrying both survives the exact
        // serialize/deserialize path the embedded artifact uses — `f64`
        // round-trips bit-exactly, and the optional tiers/fields survive too.
        let mut catalog = test_util::tiny_catalog();
        catalog[0].prompt_cache = Some(PromptCacheTtl {
            short_secs: Some(300),
            long_secs: None,
        });
        catalog[0].models[0].cost = Some(ModelCost {
            input: 3.0,
            output: 15.0,
            cache_read: Some(0.3),
            cache_write: None,
        });
        catalog[0].models[0].prompt_cache = Some(PromptCacheTtl {
            short_secs: Some(60),
            long_secs: Some(3_600),
        });

        let bytes = postcard::to_allocvec(&catalog).expect("postcard serialization succeeds");
        let decoded: Vec<ProviderEntry> =
            postcard::from_bytes(&bytes).expect("postcard deserialization succeeds");

        // `f64` round-trips bit-exactly and the optional tiers/fields survive;
        // compare the fields (ProviderEntry has no PartialEq).
        assert_eq!(decoded[0].models[0].cost, catalog[0].models[0].cost);
        assert_eq!(
            decoded[0].models[0].prompt_cache,
            catalog[0].models[0].prompt_cache
        );
        assert_eq!(decoded[0].prompt_cache, catalog[0].prompt_cache);
        let cost = decoded[0].models[0]
            .cost
            .expect("cost survived the round-trip");
        assert_eq!(cost.input, 3.0);
        assert_eq!(cost.output, 15.0);
        assert_eq!(cost.cache_read, Some(0.3));
        assert_eq!(cost.cache_write, None);
    }

    #[test]
    fn chatgpt_overlay_entry_is_the_oauth_provider() {
        // The bundled overlay's `chatgpt` entry is the first real OAuth
        // provider: its `oauth` block parses into an `OAuthProfile` (issuer,
        // paths, scopes, ports, per-endpoint encodings, device-code paths), the
        // derived auth kind flips to OAuth, and its curated model list — the
        // ChatGPT backend has no `/models` endpoint — is non-empty.
        let entry = lookup_provider("chatgpt").expect("chatgpt is in the bundled catalog");
        assert_eq!(entry.base_url, "https://chatgpt.com/backend-api/codex");
        let oauth = entry
            .oauth
            .as_ref()
            .expect("chatgpt carries an oauth profile");
        assert_eq!(oauth.issuer, "https://auth.openai.com");
        assert_eq!(oauth.authorize_path, "/oauth/authorize");
        assert_eq!(oauth.token_path, "/oauth/token");
        assert_eq!(
            oauth.scopes,
            [
                "openid",
                "profile",
                "email",
                "offline_access",
                "api.connectors.read",
                "api.connectors.invoke",
            ]
        );
        assert_eq!(oauth.redirect_ports, [1455, 1457]);
        assert_eq!(oauth.code_exchange_encoding, GrantEncoding::Form);
        assert_eq!(oauth.refresh_encoding, GrantEncoding::Json);
        assert!(oauth.device_code);
        assert_eq!(
            oauth.device_authorization_path.as_deref(),
            Some("/deviceauth/usercode")
        );
        assert_eq!(
            oauth.device_token_path.as_deref(),
            Some("/deviceauth/token")
        );
        // The borrowed vendor id lives in the compiled identity table, never in
        // the overlay — a profile `client_id` is for a registered client.
        assert!(oauth.client_id.is_none());
        assert_eq!(entry.auth_kind(), AuthKind::OAuth);
        assert!(
            !entry.models.is_empty(),
            "chatgpt's curated model list is its only model source"
        );
    }
}
