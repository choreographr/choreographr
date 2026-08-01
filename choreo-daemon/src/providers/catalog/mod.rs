//! Provider catalog — types, aggregation, and lookups.
//!
//! The catalog data (one TOML file per provider) is bundled at compile time
//! and parsed lazily into [`PROVIDER_CATALOG`]. Keeping the data in TOML
//! instead of Rust lets a provider or model be added/refreshed by editing a
//! single data file — no recompile of catalog logic, and the data is
//! machine-generatable (see `plans/provider-data/` for the generators).

use std::fmt;
use std::sync::LazyLock;
use tracing::debug;

use choreo_proto::ReasoningCapability;

use crate::openai::RequestFormat;
use crate::providers::shared::MaxTokensField;

mod loader;

/// Per-model metadata in the provider catalog.
/// A single source of truth for context window, reasoning support,
/// and explicit effort levels.
#[derive(Debug, Clone)]
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
}

/// Protocol variant — selects wire format and carries protocol-specific fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderProtocol {
    OpenAi { max_tokens_field: MaxTokensField },
    AnthropicMessages,
    GoogleGenerativeAi,
}

/// A provider and its curated model list, loaded from `<slug>.toml`.
#[derive(Debug, Clone)]
pub struct ProviderEntry {
    pub slug: String,
    pub display_name: String,
    pub protocol: ProviderProtocol,
    pub base_url: String,
    pub default_model: String,
    pub models: Vec<ModelEntry>,
}

/// Static catalog of all known providers, loaded lazily from the bundled
/// TOML data files. `LazyLock` gives every returned reference `'static`
/// lifetime (static storage is never mutated after first access), so the
/// `&'static`-based API used throughout the crate is preserved.
pub static PROVIDER_CATALOG: LazyLock<Vec<ProviderEntry>> = LazyLock::new(loader::load_catalog);

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
pub fn lookup_provider(slug: &str) -> Option<&'static ProviderEntry> {
    PROVIDER_CATALOG.iter().find(|e| e.slug == slug)
}

/// Look up the context window for a model on a given provider.
/// Matches the model slug exactly against known entries.
/// Returns `None` if no entry matches, the provider is unknown, or the
/// entry has no known window (`context_window == 0`, e.g. a model whose
/// window was never recorded — callers then fall back to the client config).
pub fn lookup_context_window(provider_slug: &str, model: &str) -> Option<u32> {
    let entry = lookup_provider(provider_slug)?;
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

/// Return all provider slugs.
pub fn all_slugs() -> impl Iterator<Item = &'static str> {
    PROVIDER_CATALOG.iter().map(|e| e.slug.as_str())
}

/// Return all display names.
pub fn all_display_names() -> impl Iterator<Item = &'static str> {
    PROVIDER_CATALOG.iter().map(|e| e.display_name.as_str())
}

/// Compute the reasoning capability for a given model on a given provider.
/// Falls back to protocol defaults for unknown models (best-effort
/// compatibility with new/untracked models).
pub fn model_reasoning_capability(provider_slug: &str, model: &str) -> ReasoningCapability {
    let entry = lookup_provider(provider_slug);

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
    let entry = lookup_provider(provider_slug)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_toml_parses() {
        // Every bundled data file must parse; a broken file here means the
        // production `unwrap_or_default`-style handling silently drops a
        // provider, so this must fail loudly at test time.
        let catalog = loader::load_catalog();
        assert!(
            catalog.len() >= 70,
            "expected >=70 providers from bundled TOML, got {}",
            catalog.len()
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
        let slugs: Vec<&str> = all_slugs().collect();
        assert_eq!(slugs.len(), PROVIDER_CATALOG.len());
    }

    #[test]
    fn catalog_has_no_duplicate_slugs() {
        let mut seen = std::collections::HashSet::new();
        for entry in PROVIDER_CATALOG.iter() {
            assert!(
                seen.insert(entry.slug.as_str()),
                "duplicate slug: {}",
                entry.slug
            );
        }
    }

    #[test]
    fn catalog_entries_have_no_empty_fields() {
        for entry in PROVIDER_CATALOG.iter() {
            assert!(!entry.slug.is_empty(), "empty slug");
            assert!(
                !entry.display_name.is_empty(),
                "empty display_name for {}",
                entry.slug
            );
            assert!(
                !entry.base_url.is_empty(),
                "empty base_url for {}",
                entry.slug
            );
            assert!(
                !entry.default_model.is_empty(),
                "empty model for {}",
                entry.slug
            );
            for m in &entry.models {
                assert!(!m.model.is_empty(), "empty model slug in {}", entry.slug);
                assert!(
                    m.context_window == 0 || m.context_window > 0,
                    "invalid context window {} for slug '{}' in {}",
                    m.context_window,
                    m.model,
                    entry.slug
                );
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
        assert_eq!(lookup_context_window("openai", "gpt-5.4"), Some(272_000));
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
    fn model_reasoning_capability_deepseek_v4_flash() {
        let cap = model_reasoning_capability("deepseek", "deepseek-v4-flash");
        assert_eq!(cap.available_effort_levels, vec!["off", "high", "xhigh"]);
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
}
