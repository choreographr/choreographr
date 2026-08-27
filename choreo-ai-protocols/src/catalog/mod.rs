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
//! sibling [`lookup`] module and are re-exported here so the public surface of
//! the crate is unchanged.

use std::fmt;
use std::sync::{Arc, LazyLock};
use tracing::debug;

use arc_swap::ArcSwap;
use serde::{Deserialize, Serialize};

use crate::shared::MaxTokensField;

mod loader;
mod lookup;
mod modelsdev;
mod overlay;
mod persist;
pub mod refresh;

pub use loader::{bundled_overlay_src, load_bundled_base};
pub use lookup::{
    lookup_context_window, lookup_max_output_tokens, model_reasoning_capability,
    model_reasoning_passback, model_request_format, model_supports_temperature,
    model_supports_vision, requires_reasoning_content,
};
pub use modelsdev::normalize_modelsdev;
pub use overlay::merge_overlay;
pub use persist::write_file_atomic;
pub use refresh::{RefreshError, RefreshOutcome, fetch_modelsdev};

/// How reasoning is replayed back to the provider on subsequent turns.
/// The "unset" meaning lives in the outer `Option` on [`ModelEntry`]
/// (`None` → derive from protocol); `None` *here* is an explicit
/// "never replay" override. [`model_reasoning_passback`] resolves the
/// effective format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningPassback {
    /// Never send reasoning back (display-only providers / fields).
    None,
    /// Echo reasoning on assistant messages that had tool calls
    /// (DeepSeek/Kimi chat, and the minimum for Anthropic tool loops).
    ToolLoop,
    /// Echo reasoning across all turns of the session (Anthropic keep-all,
    /// GPT-5.6 all_turns).
    AllTurns,
    /// Send back encrypted thought signatures (Gemini).
    Signature,
    /// Chain via previous_response_id / opaque reasoning items
    /// (OpenAI/xAI Responses).
    ResponseId,
}

/// Per-model metadata in the provider catalog.
/// A single source of truth for context window, reasoning support,
/// explicit effort levels, and reasoning round-trip format.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelEntry {
    pub model: String,
    pub context_window: u32,
    /// Whether this model supports reasoning/thinking at all.
    /// Applicable across all protocols — OpenAi, AnthropicMessages,
    /// and GoogleGenerativeAi all use this flag to enable/disable
    /// their respective reasoning features per model.
    /// When `false`, the model entry's `openai_reasoning_levels`
    /// (if any) is ignored and the model is treated as non-reasoning.
    pub reasoning_supported: bool,
    /// Explicit reasoning effort level slugs for this model.
    /// Only meaningful when `reasoning_supported` is true AND the
    /// provider protocol is `OpenAi` (enforced by
    /// `model_reasoning_capability()`). Non-OpenAi protocols always
    /// use their own default levels — see `protocol_default_levels()`.
    pub openai_reasoning_levels: Vec<String>,
    /// Whether this model uses OpenAI's Responses API vs Chat Completions.
    /// Only relevant for OpenAi protocol providers.
    pub openai_responses: bool,
    /// How reasoning is replayed back to the provider on subsequent turns.
    /// `None` means "no override — `model_reasoning_passback` derives the
    /// format from the provider protocol" (Responses → `ResponseId`,
    /// chat completions → `ToolLoop`, Anthropic → `AllTurns`, Google →
    /// `Signature`); `Some(...)` is an explicit per-model override,
    /// including `Some(ReasoningPassback::None)` for "never replay this
    /// model's reasoning".
    pub reasoning_passback: Option<ReasoningPassback>,
    /// Whether this model requires `reasoning_content` to be *present* on
    /// every assistant message sent back (e.g. DeepSeek/GLM 5.x chat: the
    /// upstream rejects a history whose assistant tool-call message omits
    /// `reasoning_content`, even when the model produced no reasoning on
    /// that call). Ingested from the models.dev snapshot as a FACT — the
    /// model's `interleaved` value names `"reasoning_content"` as the echo
    /// field — and overridable per-model by the overlay
    /// (`reasoning_content_required = true|false`). `None` means "no explicit
    /// fact" and resolves to `false` in [`requires_reasoning_content`] —
    /// there is NO name-based fallback anymore; a model not covered by the
    /// snapshot needs an explicit overlay flag.
    #[serde(default)]
    pub reasoning_content_required: Option<bool>,
    /// Maximum output tokens the model can produce (`0` = unknown), ingested
    /// from the snapshot's `limit.output` and resolvable via
    /// [`lookup_max_output_tokens`] (which maps 0 back to `None`).
    #[serde(default)]
    pub max_output_tokens: u32,
    /// Whether the model accepts the `temperature` request parameter,
    /// ingested from the snapshot's `temperature` flag (absent → `true`,
    /// the permissive wire default). Resolvable via
    /// [`model_supports_temperature`]. Currently **recorded but unwired**:
    /// no request builder sends a `temperature` parameter today, so the
    /// resolver has no production caller — the fact is kept so the gate
    /// exists the moment temperature sending is added (see ARCHITECTURE.md,
    /// the catalog-facts paragraph).
    #[serde(default = "default_true")]
    pub supports_temperature: bool,
    /// Whether the snapshot marks the model deprecated (`status ==
    /// "deprecated"`). Purely informational today; surfaced so UIs and
    /// diagnostics can flag stale model picks without a second lookup.
    #[serde(default)]
    pub deprecated: bool,
    /// Whether the model accepts image input (vision). Derived from the
    /// models.dev `modalities.input` array (`"image"` present) at ingestion;
    /// the overlay can override it where the snapshot is wrong or a model is
    /// not covered. `false` = text-only — images must be gated out of the
    /// request and replaced with a text placeholder (see the vision gate).
    /// `false` is the safe default for unknown models.
    #[serde(default)]
    pub supports_vision: bool,
}

// Manual `impl` rather than `#[derive(Default)]` + `#[default]` because
// `supports_temperature` must default to `true` (the permissive wire default
// shared with its `#[serde(default = "default_true")]`); derive can only
// express `false` for a `bool`. Every other field's natural zero-value default
// already matches its serde behavior (`0` for unknown windows/tokens, `None`
// for unset options), so this keeps the serde attributes untouched and the
// wire format byte-identical while letting tests say `..Default::default()`.
impl Default for ModelEntry {
    fn default() -> Self {
        Self {
            model: String::new(),
            context_window: 0,
            reasoning_supported: false,
            openai_reasoning_levels: Vec::new(),
            openai_responses: false,
            reasoning_passback: None,
            reasoning_content_required: None,
            max_output_tokens: 0,
            supports_temperature: true,
            deprecated: false,
            supports_vision: false,
        }
    }
}

/// Protocol variant — selects wire format and carries protocol-specific fields.
///
/// The serde representation is the default externally-tagged enum encoding
/// (e.g. `{"OpenAi": {"max_tokens_field": "max_completion_tokens"}}`); it only
/// needs to round-trip for the embedded-artifact pipeline, so the simplest
/// correct repr is used.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum ProviderProtocol {
    OpenAi { max_tokens_field: MaxTokensField },
    AnthropicMessages,
    GoogleGenerativeAi,
}

/// A provider and its curated model list, loaded from `<slug>.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderEntry {
    pub slug: String,
    pub display_name: String,
    pub protocol: ProviderProtocol,
    pub base_url: String,
    pub default_model: String,
    pub models: Vec<ModelEntry>,
}

/// Process-wide catalog of all known providers, backed by an [`ArcSwap`] so a
/// runtime refresh can atomically swap it while readers stay lock-free.
///
/// `ArcSwap::from_pointee` is not a const fn and the TOML load is a runtime
/// parse, so the `ArcSwap` itself is lazily initialized behind a [`LazyLock`]:
/// the first access parses the bundled data once, and every later access goes
/// straight to the `ArcSwap` (an atomic load, then lock-free reads / atomic
/// `store` on swap). This preserves the old lazy-init behavior exactly while
/// making the catalog runtime-swappable.
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

impl fmt::Display for ProviderProtocol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProviderProtocol::OpenAi { .. } => write!(f, "Protocol: OpenAI"),
            ProviderProtocol::AnthropicMessages => write!(f, "Protocol: Anthropic Messages"),
            ProviderProtocol::GoogleGenerativeAi => write!(f, "Protocol: Google Generative AI"),
        }
    }
}

/// serde default helper: `bool` fields that mean "true when absent"
/// (`supports_temperature`) cannot use plain `#[serde(default)]`, which
/// would default to `false` and flip the permissive wire default.
fn default_true() -> bool {
    true
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
/// that would feed a wrong `model_reasoning_passback` / `requires_reasoning_content`.
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
            assert!(!name.is_empty());
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
}
