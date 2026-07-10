use std::fmt;
use tracing::debug;

use crate::providers::shared::MaxTokensField;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderProtocol {
    OpenAiCompatible,
    AnthropicMessages,
    GoogleGenerativeAi,
    Mistral,
}

/// Declares which reasoning parameter protocol a provider speaks.
/// This is the wire-format level — model-level gating is done separately
/// via `effective_reasoning_support()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReasoningSupport {
    /// No reasoning parameter is supported by this provider.
    None,
    /// OpenAI-style `reasoning_effort` field in chat completions.
    ReasoningEffort,
    /// Anthropic-style `thinking` block with `budget_tokens`.
    AnthropicThinking,
    /// Google-style `thinkingConfig` with `includeThoughts`.
    GoogleThinkingConfig,
}

#[derive(Debug, Clone, Copy)]
pub struct ProviderEntry {
    pub slug: &'static str,
    pub display_name: &'static str,
    pub protocol: ProviderProtocol,
    pub default_base_url: &'static str,
    pub default_model: &'static str,
    pub reasoning: ReasoningSupport,
    pub max_tokens_field: MaxTokensField,
}

/// Static catalog of all known providers.
pub static PROVIDER_CATALOG: &[ProviderEntry] = &[
    ProviderEntry {
        slug: "openai",
        display_name: "OpenAI",
        protocol: ProviderProtocol::OpenAiCompatible,
        default_base_url: "https://api.openai.com/v1",
        default_model: "gpt-4.1",
        reasoning: ReasoningSupport::ReasoningEffort,
        max_tokens_field: MaxTokensField::MaxCompletionTokens,
    },
    ProviderEntry {
        slug: "openai_compatible",
        display_name: "OpenAI Compatible",
        protocol: ProviderProtocol::OpenAiCompatible,
        default_base_url: "https://api.openai.com/v1",
        default_model: "custom-model",
        reasoning: ReasoningSupport::ReasoningEffort,
        max_tokens_field: MaxTokensField::MaxCompletionTokens,
    },
    ProviderEntry {
        slug: "opencode",
        display_name: "OpenCode Zen",
        protocol: ProviderProtocol::OpenAiCompatible,
        default_base_url: "https://opencode.ai/zen/v1",
        default_model: "deepseek-v4-flash",
        reasoning: ReasoningSupport::ReasoningEffort,
        max_tokens_field: MaxTokensField::MaxTokens,
    },
    ProviderEntry {
        slug: "opencode-go",
        display_name: "OpenCode Go",
        protocol: ProviderProtocol::OpenAiCompatible,
        default_base_url: "https://opencode.ai/zen/go/v1",
        default_model: "deepseek-v4-pro",
        reasoning: ReasoningSupport::ReasoningEffort,
        max_tokens_field: MaxTokensField::MaxTokens,
    },
    ProviderEntry {
        slug: "deepseek",
        display_name: "DeepSeek",
        protocol: ProviderProtocol::OpenAiCompatible,
        default_base_url: "https://api.deepseek.com",
        default_model: "deepseek-chat",
        reasoning: ReasoningSupport::ReasoningEffort,
        max_tokens_field: MaxTokensField::MaxCompletionTokens,
    },
    ProviderEntry {
        slug: "xai",
        display_name: "xAI Grok",
        protocol: ProviderProtocol::OpenAiCompatible,
        default_base_url: "https://api.x.ai/v1",
        default_model: "grok-4",
        reasoning: ReasoningSupport::ReasoningEffort,
        max_tokens_field: MaxTokensField::MaxCompletionTokens,
    },
    ProviderEntry {
        slug: "groq",
        display_name: "Groq",
        protocol: ProviderProtocol::OpenAiCompatible,
        default_base_url: "https://api.groq.com/openai/v1",
        default_model: "llama-3.3-70b-versatile",
        reasoning: ReasoningSupport::ReasoningEffort,
        max_tokens_field: MaxTokensField::MaxCompletionTokens,
    },
    ProviderEntry {
        slug: "together",
        display_name: "Together AI",
        protocol: ProviderProtocol::OpenAiCompatible,
        default_base_url: "https://api.together.xyz/v1",
        default_model: "meta-llama/Llama-3.3-70B-Instruct-Turbo",
        reasoning: ReasoningSupport::ReasoningEffort,
        max_tokens_field: MaxTokensField::MaxCompletionTokens,
    },
    ProviderEntry {
        slug: "mistral",
        display_name: "Mistral",
        protocol: ProviderProtocol::Mistral,
        default_base_url: "https://api.mistral.ai/v1",
        default_model: "mistral-large-latest",
        reasoning: ReasoningSupport::ReasoningEffort,
        max_tokens_field: MaxTokensField::MaxTokens,
    },
    ProviderEntry {
        slug: "ollama",
        display_name: "Ollama (Local)",
        protocol: ProviderProtocol::OpenAiCompatible,
        default_base_url: "http://localhost:11434/v1",
        default_model: "llama3.1",
        reasoning: ReasoningSupport::ReasoningEffort,
        max_tokens_field: MaxTokensField::MaxCompletionTokens,
    },
    ProviderEntry {
        slug: "ollama-cloud",
        display_name: "Ollama Cloud",
        protocol: ProviderProtocol::OpenAiCompatible,
        default_base_url: "https://ollama.com/v1",
        default_model: "qwen3-coder:480b",
        reasoning: ReasoningSupport::ReasoningEffort,
        max_tokens_field: MaxTokensField::MaxCompletionTokens,
    },
    ProviderEntry {
        slug: "openrouter",
        display_name: "OpenRouter",
        protocol: ProviderProtocol::OpenAiCompatible,
        default_base_url: "https://openrouter.ai/api/v1",
        default_model: "openai/gpt-4.1",
        reasoning: ReasoningSupport::ReasoningEffort,
        max_tokens_field: MaxTokensField::MaxCompletionTokens,
    },
    ProviderEntry {
        slug: "huggingface",
        display_name: "Hugging Face",
        protocol: ProviderProtocol::OpenAiCompatible,
        default_base_url: "https://router.huggingface.co/v1",
        default_model: "meta-llama/Llama-3.3-70B-Instruct",
        reasoning: ReasoningSupport::ReasoningEffort,
        max_tokens_field: MaxTokensField::MaxCompletionTokens,
    },
    ProviderEntry {
        slug: "github",
        display_name: "GitHub Models",
        protocol: ProviderProtocol::OpenAiCompatible,
        default_base_url: "https://models.inference.ai.azure.com",
        default_model: "openai/gpt-4.1",
        reasoning: ReasoningSupport::ReasoningEffort,
        max_tokens_field: MaxTokensField::MaxCompletionTokens,
    },
    ProviderEntry {
        slug: "nvidia",
        display_name: "NVIDIA NIM",
        protocol: ProviderProtocol::OpenAiCompatible,
        default_base_url: "https://integrate.api.nvidia.com/v1",
        default_model: "nvidia/llama-3.1-nemotron-70b-instruct",
        reasoning: ReasoningSupport::ReasoningEffort,
        max_tokens_field: MaxTokensField::MaxCompletionTokens,
    },
    ProviderEntry {
        slug: "cerebras",
        display_name: "Cerebras",
        protocol: ProviderProtocol::OpenAiCompatible,
        default_base_url: "https://api.cerebras.ai/v1",
        default_model: "cerebras",
        reasoning: ReasoningSupport::ReasoningEffort,
        max_tokens_field: MaxTokensField::MaxCompletionTokens,
    },
    ProviderEntry {
        slug: "fireworks",
        display_name: "Fireworks AI",
        protocol: ProviderProtocol::OpenAiCompatible,
        default_base_url: "https://api.fireworks.ai/inference/v1",
        default_model: "accounts/fireworks/models/llama-v3p3-70b-instruct",
        reasoning: ReasoningSupport::ReasoningEffort,
        max_tokens_field: MaxTokensField::MaxCompletionTokens,
    },
    ProviderEntry {
        slug: "xiaomi-mimo",
        display_name: "Xiaomi MiMo",
        protocol: ProviderProtocol::OpenAiCompatible,
        default_base_url: "https://api.mimo.xiaomi.com/openai/v1",
        default_model: "mimo-vl",
        reasoning: ReasoningSupport::ReasoningEffort,
        max_tokens_field: MaxTokensField::MaxCompletionTokens,
    },
    ProviderEntry {
        slug: "dashscope",
        display_name: "DashScope (Alibaba)",
        protocol: ProviderProtocol::OpenAiCompatible,
        default_base_url: "https://dashscope-intl.aliyuncs.com/compatible-mode/v1",
        default_model: "qwen-plus",
        reasoning: ReasoningSupport::ReasoningEffort,
        max_tokens_field: MaxTokensField::MaxCompletionTokens,
    },
    ProviderEntry {
        slug: "moonshot",
        display_name: "Moonshot AI (Kimi)",
        protocol: ProviderProtocol::OpenAiCompatible,
        default_base_url: "https://api.moonshot.ai/v1",
        default_model: "kimi-k2",
        reasoning: ReasoningSupport::ReasoningEffort,
        max_tokens_field: MaxTokensField::MaxCompletionTokens,
    },
    ProviderEntry {
        slug: "perplexity",
        display_name: "Perplexity",
        protocol: ProviderProtocol::OpenAiCompatible,
        default_base_url: "https://api.perplexity.ai",
        default_model: "sonar-pro",
        reasoning: ReasoningSupport::ReasoningEffort,
        max_tokens_field: MaxTokensField::MaxCompletionTokens,
    },
    ProviderEntry {
        slug: "zai",
        display_name: "Z.ai (GLM)",
        protocol: ProviderProtocol::OpenAiCompatible,
        default_base_url: "https://api.z.ai/api/paas/v4",
        default_model: "glm-4.5",
        reasoning: ReasoningSupport::ReasoningEffort,
        max_tokens_field: MaxTokensField::MaxCompletionTokens,
    },
    ProviderEntry {
        slug: "venice",
        display_name: "Venice AI",
        protocol: ProviderProtocol::OpenAiCompatible,
        default_base_url: "https://api.venice.ai/api/v1",
        default_model: "qwen-2.5-qwq-32b",
        reasoning: ReasoningSupport::ReasoningEffort,
        max_tokens_field: MaxTokensField::MaxCompletionTokens,
    },
    ProviderEntry {
        slug: "novita",
        display_name: "Novita AI",
        protocol: ProviderProtocol::OpenAiCompatible,
        default_base_url: "https://api.novita.ai/v3/openai",
        default_model: "deepseek-v3",
        reasoning: ReasoningSupport::ReasoningEffort,
        max_tokens_field: MaxTokensField::MaxCompletionTokens,
    },
    ProviderEntry {
        slug: "lmstudio",
        display_name: "LM Studio",
        protocol: ProviderProtocol::OpenAiCompatible,
        default_base_url: "http://localhost:1234/v1",
        default_model: "local-model",
        reasoning: ReasoningSupport::ReasoningEffort,
        max_tokens_field: MaxTokensField::MaxCompletionTokens,
    },
    ProviderEntry {
        slug: "custom-openai",
        display_name: "Custom OpenAI-Compatible",
        protocol: ProviderProtocol::OpenAiCompatible,
        default_base_url: "https://api.openai.com/v1",
        default_model: "custom-model",
        reasoning: ReasoningSupport::ReasoningEffort,
        max_tokens_field: MaxTokensField::MaxCompletionTokens,
    },
    ProviderEntry {
        slug: "anthropic",
        display_name: "Anthropic",
        protocol: ProviderProtocol::AnthropicMessages,
        default_base_url: "https://api.anthropic.com",
        default_model: "claude-sonnet-4-20250514",
        reasoning: ReasoningSupport::AnthropicThinking,
        max_tokens_field: MaxTokensField::MaxTokens,
    },
    ProviderEntry {
        slug: "minimax",
        display_name: "MiniMax",
        protocol: ProviderProtocol::AnthropicMessages,
        default_base_url: "https://api.minimax.io/anthropic",
        default_model: "MiniMax-M3",
        reasoning: ReasoningSupport::AnthropicThinking,
        max_tokens_field: MaxTokensField::MaxTokens,
    },
    ProviderEntry {
        slug: "custom-anthropic",
        display_name: "Custom Anthropic-Compatible",
        protocol: ProviderProtocol::AnthropicMessages,
        default_base_url: "https://api.anthropic.com",
        default_model: "custom-model",
        reasoning: ReasoningSupport::AnthropicThinking,
        max_tokens_field: MaxTokensField::MaxTokens,
    },
    ProviderEntry {
        slug: "google",
        display_name: "Google Gemini",
        protocol: ProviderProtocol::GoogleGenerativeAi,
        default_base_url: "https://generativelanguage.googleapis.com/v1beta",
        default_model: "gemini-2.5-pro",
        reasoning: ReasoningSupport::GoogleThinkingConfig,
        max_tokens_field: MaxTokensField::MaxTokens,
    },
];

impl fmt::Display for ProviderProtocol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProviderProtocol::OpenAiCompatible => write!(f, "OpenAI-compatible"),
            ProviderProtocol::AnthropicMessages => write!(f, "Anthropic Messages"),
            ProviderProtocol::GoogleGenerativeAi => write!(f, "Google Generative AI"),
            ProviderProtocol::Mistral => write!(f, "Mistral"),
        }
    }
}

/// Look up a provider entry by slug. Returns `None` if not found.
pub fn lookup_provider(slug: &str) -> Option<&'static ProviderEntry> {
    PROVIDER_CATALOG.iter().find(|e| e.slug == slug)
}

/// Determine whether a specific model actually supports reasoning on a given
/// provider. Uses model-name heuristics because the static catalog can't
/// enumerate every model variant dynamically.
///
/// Returns `ReasoningSupport::None` when the model doesn't support reasoning,
/// even if the provider protocol would allow it.
pub fn effective_reasoning_support(model: &str, provider: ReasoningSupport) -> ReasoningSupport {
    let lower = model.to_lowercase();
    let result = match provider {
        ReasoningSupport::ReasoningEffort => {
            // OpenAI reasoning models: o-series (o1, o3, o4-mini, etc.)
            // GPT-5 series
            // DeepSeek reasoner
            // xAI Grok reasoning models
            if lower.starts_with("o")
                || lower.starts_with("gpt-5")
                || lower.contains("deepseek-reasoner")
                || lower.contains("grok") && lower.contains("reasoning")
                || lower.contains("mistral-large")
            {
                ReasoningSupport::ReasoningEffort
            } else {
                ReasoningSupport::None
            }
        }
        ReasoningSupport::AnthropicThinking => {
            // Claude Sonnet/Opus 4+ support extended thinking
            if lower.contains("sonnet-4")
                || lower.contains("opus-4")
                || lower.contains("sonnet-3-5")
            {
                ReasoningSupport::AnthropicThinking
            } else {
                ReasoningSupport::None
            }
        }
        ReasoningSupport::GoogleThinkingConfig => {
            // Gemini 2.5 series supports thinking
            if lower.contains("gemini-2.5") {
                ReasoningSupport::GoogleThinkingConfig
            } else {
                ReasoningSupport::None
            }
        }
        ReasoningSupport::None => ReasoningSupport::None,
    };
    debug!(
        model = %model,
        ?provider,
        ?result,
        "effective_reasoning_support"
    );
    result
}

/// Return all provider slugs.
pub fn all_slugs() -> impl Iterator<Item = &'static str> {
    PROVIDER_CATALOG.iter().map(|e| e.slug)
}

/// Return all display names.
pub fn all_display_names() -> impl Iterator<Item = &'static str> {
    PROVIDER_CATALOG.iter().map(|e| e.display_name)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        for entry in PROVIDER_CATALOG {
            assert!(seen.insert(entry.slug), "duplicate slug: {}", entry.slug);
        }
    }

    #[test]
    fn catalog_entries_have_no_empty_fields() {
        for entry in PROVIDER_CATALOG {
            assert!(!entry.slug.is_empty(), "empty slug");
            assert!(
                !entry.display_name.is_empty(),
                "empty display_name for {}",
                entry.slug
            );
            assert!(
                !entry.default_base_url.is_empty(),
                "empty base_url for {}",
                entry.slug
            );
            assert!(
                !entry.default_model.is_empty(),
                "empty model for {}",
                entry.slug
            );
        }
    }

    #[test]
    fn catalog_entries_have_reasoning_support() {
        for entry in PROVIDER_CATALOG {
            assert!(
                matches!(
                    entry.reasoning,
                    ReasoningSupport::None
                        | ReasoningSupport::ReasoningEffort
                        | ReasoningSupport::AnthropicThinking
                        | ReasoningSupport::GoogleThinkingConfig
                ),
                "missing reasoning for {}",
                entry.slug
            );
        }
    }

    #[test]
    fn reasoning_support_effort_models() {
        // OpenAI reasoning models
        assert_eq!(
            effective_reasoning_support("o1", ReasoningSupport::ReasoningEffort),
            ReasoningSupport::ReasoningEffort
        );
        assert_eq!(
            effective_reasoning_support("o3-mini", ReasoningSupport::ReasoningEffort),
            ReasoningSupport::ReasoningEffort
        );
        assert_eq!(
            effective_reasoning_support("o4-mini-2025-07-18", ReasoningSupport::ReasoningEffort),
            ReasoningSupport::ReasoningEffort
        );
        assert_eq!(
            effective_reasoning_support("gpt-5.4", ReasoningSupport::ReasoningEffort),
            ReasoningSupport::ReasoningEffort
        );

        // Non-reasoning models
        assert_eq!(
            effective_reasoning_support("gpt-4.1", ReasoningSupport::ReasoningEffort),
            ReasoningSupport::None
        );
        assert_eq!(
            effective_reasoning_support("gpt-4o", ReasoningSupport::ReasoningEffort),
            ReasoningSupport::None
        );
        assert_eq!(
            effective_reasoning_support("llama-3.3-70b", ReasoningSupport::ReasoningEffort),
            ReasoningSupport::None
        );
    }

    #[test]
    fn reasoning_support_anthropic_models() {
        assert_eq!(
            effective_reasoning_support(
                "claude-sonnet-4-20250514",
                ReasoningSupport::AnthropicThinking
            ),
            ReasoningSupport::AnthropicThinking
        );
        assert_eq!(
            effective_reasoning_support(
                "claude-opus-4-20250514",
                ReasoningSupport::AnthropicThinking
            ),
            ReasoningSupport::AnthropicThinking
        );

        // Non-reasoning Claude
        assert_eq!(
            effective_reasoning_support(
                "claude-haiku-3-5-20241022",
                ReasoningSupport::AnthropicThinking
            ),
            ReasoningSupport::None
        );
        assert_eq!(
            effective_reasoning_support(
                "claude-3-haiku-20240307",
                ReasoningSupport::AnthropicThinking
            ),
            ReasoningSupport::None
        );
    }

    #[test]
    fn reasoning_support_google_models() {
        assert_eq!(
            effective_reasoning_support("gemini-2.5-pro", ReasoningSupport::GoogleThinkingConfig),
            ReasoningSupport::GoogleThinkingConfig
        );
        assert_eq!(
            effective_reasoning_support("gemini-2.5-flash", ReasoningSupport::GoogleThinkingConfig),
            ReasoningSupport::GoogleThinkingConfig
        );

        // Non-reasoning Gemini
        assert_eq!(
            effective_reasoning_support("gemini-1.5-pro", ReasoningSupport::GoogleThinkingConfig),
            ReasoningSupport::None
        );
        assert_eq!(
            effective_reasoning_support("gemma-3-27b-it", ReasoningSupport::GoogleThinkingConfig),
            ReasoningSupport::None
        );
    }

    #[test]
    fn reasoning_support_none_always_none() {
        assert_eq!(
            effective_reasoning_support("anything", ReasoningSupport::None),
            ReasoningSupport::None
        );
    }

    #[test]
    fn all_display_names_are_non_empty() {
        for name in all_display_names() {
            assert!(!name.is_empty());
        }
    }
}
