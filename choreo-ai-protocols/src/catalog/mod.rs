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

use serde::{Deserialize, Serialize};

use choreo_proto::ReasoningCapability;

use crate::openai::RequestFormat;
use crate::shared::MaxTokensField;

mod loader;

/// How reasoning is replayed back to the provider on subsequent turns.
/// Default derived from protocol in `model_reasoning_passback`.
/// `None` (the serde default) means "no explicit override — derive from
/// protocol", so TOMLs only set this field where nuance matters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningPassback {
    /// Never send reasoning back (display-only providers / fields).
    #[default]
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
    /// How reasoning is replayed back to the provider on subsequent turns.
    /// `None` means "no override — `model_reasoning_passback` derives the
    /// format from the provider protocol" (Responses → `ResponseId`,
    /// chat completions → `ToolLoop`, Anthropic → `AllTurns`, Google →
    /// `Signature`).
    pub reasoning_passback: ReasoningPassback,
}

/// Protocol variant — selects wire format and carries protocol-specific fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
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

/// Compute how reasoning is replayed back to the provider for a given
/// model. Mirrors `model_reasoning_capability`: an explicit per-model
/// `reasoning_passback` TOML override wins, otherwise the format is derived
/// from the provider protocol (falling back to protocol defaults for
/// unknown/new models, and `None` for unknown providers).
pub fn model_reasoning_passback(provider_slug: &str, model: &str) -> ReasoningPassback {
    let entry = lookup_provider(provider_slug);

    let passback = match entry {
        Some(e) => {
            let model_entry = e.models.iter().find(|m| m.model == model);
            match model_entry {
                // Known model: explicit TOML override wins; `None` means
                // unset, so derive from the protocol (and, for OpenAi
                // providers, whether the model uses Responses).
                Some(m) if m.reasoning_passback != ReasoningPassback::None => m.reasoning_passback,
                // Known model without an override → protocol default.
                Some(m) => protocol_default_passback(e.protocol, m.openai_responses),
                // Unknown model → protocol default (best-effort for new
                // models; OpenAi assumed chat-completions, matching
                // `ServiceConfig::default_request_format`).
                None => protocol_default_passback(e.protocol, false),
            }
        }
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
            // `context_window` is a `u32` where 0 means "unknown"; the previous
            // `== 0 || > 0` check was vacuously true for every model, so it is
            // dropped (clippy::double_comparisons).
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
        // openai.toml has no chat-completions models, so exercise the
        // protocol default on another OpenAI-protocol provider with a known
        // chat-completions model: Cerebras' gpt-oss-120b (responses = false)
        // → echo reasoning on tool-call turns.
        assert_eq!(
            model_reasoning_passback("cerebras", "gpt-oss-120b"),
            ReasoningPassback::ToolLoop
        );
    }

    #[test]
    fn model_reasoning_passback_deepseek_toml_override() {
        // DeepSeek overrides the protocol default explicitly (tool_loop) on
        // both models; matches the responses = false default, so the override
        // is documentation-grade here.
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
        // deepseek-v4-flash carries an explicit tool_loop override; even if
        // the responses flag flipped to true, the override would still win.
        let entry = lookup_provider("deepseek").unwrap();
        let model = entry
            .models
            .iter()
            .find(|m| m.model == "deepseek-v4-flash")
            .unwrap();
        assert_eq!(model.reasoning_passback, ReasoningPassback::ToolLoop);
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
}
