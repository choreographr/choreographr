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

use std::fmt;
use std::sync::{Arc, LazyLock};
use tracing::debug;

use arc_swap::ArcSwap;
use serde::{Deserialize, Serialize};

use choreo_proto::ReasoningCapability;

use crate::openai::RequestFormat;
use crate::shared::MaxTokensField;

mod loader;
mod modelsdev;
mod overlay;
mod persist;
pub mod refresh;

pub use loader::{bundled_overlay_src, load_bundled_base};
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

/// Look up the context window for a model on a given provider.
/// Matches the model slug exactly against known entries.
/// Returns `None` if no entry matches, the provider is unknown, or the
/// entry has no known window (`context_window == 0`, e.g. a model whose
/// window was never recorded — callers then fall back to the client config).
pub fn lookup_context_window(provider_slug: &str, model: &str) -> Option<u32> {
    // Hold the ArcSwap guard for the duration of the lookup so we never clone
    // a whole provider entry just to read one model's window.
    let catalog = PROVIDER_CATALOG.load();
    let entry = catalog.iter().find(|e| e.slug == provider_slug)?;
    for m in &entry.models {
        if model == m.model {
            return if m.context_window == 0 {
                None
            } else {
                Some(m.context_window)
            };
        }
    }
    None
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

    debug!(
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

/// Look up whether a model should use OpenAI's Responses API.
/// Returns None for unknown models — caller falls back to default_request_format.
pub fn model_request_format(provider_slug: &str, model: &str) -> Option<RequestFormat> {
    let catalog = PROVIDER_CATALOG.load();
    let entry = catalog.iter().find(|e| e.slug == provider_slug)?;
    for m in &entry.models {
        if model == m.model {
            return if m.openai_responses {
                Some(RequestFormat::Responses)
            } else {
                Some(RequestFormat::ChatCompletions)
            };
        }
    }
    None
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

    debug!(
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
// All tests in this module read (and the `replace_catalog` tests mutate) the
// process-wide `PROVIDER_CATALOG`, so serialize them under one key: nextest
// gives per-process isolation anyway, but the libtest fallback shares one
// process across parallel threads and a mid-flight swap would race the readers.
#[serial_test::serial(catalog)]
mod tests {
    use super::*;

    /// Restores the bundled catalog when dropped, so a failing swap test can
    /// never leave the process-global catalog swapped for later tests (the
    /// libtest fallback shares one process).
    struct RestoreBundledOnDrop;

    impl Drop for RestoreBundledOnDrop {
        fn drop(&mut self) {
            replace_catalog(loader::load_catalog());
        }
    }

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
    fn all_display_names_are_non_empty() {
        for name in all_display_names() {
            assert!(!name.is_empty());
        }
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
        assert!(cap.available_effort_levels.is_empty());
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
        assert!(cap.available_effort_levels.is_empty());
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
        assert!(cap.available_effort_levels.is_empty());
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

    /// Build a minimal one-provider/one-model catalog for the swap tests.
    fn tiny_catalog() -> Vec<ProviderEntry> {
        vec![ProviderEntry {
            slug: "tiny-test".into(),
            display_name: "Tiny Test".into(),
            protocol: ProviderProtocol::OpenAi {
                max_tokens_field: MaxTokensField::MaxTokens,
            },
            base_url: "https://tiny-test.example/v1".into(),
            default_model: "tiny-model".into(),
            models: vec![ModelEntry {
                model: "tiny-model".into(),
                context_window: 4096,
                reasoning_supported: true,
                openai_reasoning_levels: vec!["off".into(), "high".into()],
                openai_responses: false,
                reasoning_passback: None,
            }],
        }]
    }

    #[test]
    fn replace_catalog_swaps_what_lookup_sees() {
        // Swapping the process-wide catalog must be visible to readers
        // immediately (the ArcSwap store is atomic). Restore the bundled
        // catalog on drop so a failure here cannot poison later tests.
        let _restore = RestoreBundledOnDrop;
        let bundled = catalog_snapshot();
        assert!(lookup_provider("openai").is_some());

        replace_catalog(tiny_catalog());

        let entry = lookup_provider("tiny-test").expect("swapped catalog has tiny-test");
        assert_eq!(entry.slug, "tiny-test");
        assert_eq!(entry.base_url, "https://tiny-test.example/v1");
        // The bundled catalog is gone while the tiny one is installed.
        assert!(lookup_provider("openai").is_none());
        assert_eq!(all_slugs(), vec!["tiny-test".to_string()]);

        // Sanity-check the lookup functions read through the same global.
        assert_eq!(lookup_context_window("tiny-test", "tiny-model"), Some(4096));
        assert_eq!(
            model_reasoning_capability("tiny-test", "tiny-model").available_effort_levels,
            vec!["off", "high"]
        );

        // Explicit restore (the guard also restores on panic) so the test is
        // self-documenting about returning the global to its initial state.
        replace_catalog(bundled.to_vec());
        assert!(lookup_provider("openai").is_some());
    }

    #[test]
    fn lookup_provider_returns_owned_clone_outliving_guard() {
        // The returned entry is owned (cloned out of the ArcSwap guard), so it
        // remains valid even after the catalog is swapped underneath it.
        let _restore = RestoreBundledOnDrop;
        let entry = lookup_provider("openai").expect("openai is in the bundled catalog");

        replace_catalog(tiny_catalog());

        // The clone outlives the swap: its data is intact and independent.
        assert_eq!(entry.slug, "openai");
        assert_eq!(entry.base_url, "https://api.openai.com/v1");
        assert_eq!(entry.default_model, "gpt-5.4");
        assert!(!entry.models.is_empty());
    }
}
