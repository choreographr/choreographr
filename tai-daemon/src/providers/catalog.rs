use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderProtocol {
    OpenAiCompatible,
    AnthropicMessages,
    GoogleGenerativeAi,
}

#[derive(Debug, Clone, Copy)]
pub struct ProviderEntry {
    pub slug: &'static str,
    pub display_name: &'static str,
    pub protocol: ProviderProtocol,
    pub default_base_url: &'static str,
    pub default_model: &'static str,
}

/// Static catalog of all known providers.
pub static PROVIDER_CATALOG: &[ProviderEntry] = &[
    ProviderEntry {
        slug: "openai",
        display_name: "OpenAI",
        protocol: ProviderProtocol::OpenAiCompatible,
        default_base_url: "https://api.openai.com/v1",
        default_model: "gpt-4.1",
    },
    ProviderEntry {
        slug: "openai_compatible",
        display_name: "OpenAI Compatible",
        protocol: ProviderProtocol::OpenAiCompatible,
        default_base_url: "https://api.openai.com/v1",
        default_model: "custom-model",
    },
    ProviderEntry {
        slug: "opencode",
        display_name: "OpenCode Zen",
        protocol: ProviderProtocol::OpenAiCompatible,
        default_base_url: "https://opencode.ai/zen/v1",
        default_model: "deepseek-v4-flash",
    },
    ProviderEntry {
        slug: "opencode-go",
        display_name: "OpenCode Go",
        protocol: ProviderProtocol::OpenAiCompatible,
        default_base_url: "https://opencode.ai/zen/go/v1",
        default_model: "deepseek-v4-pro",
    },
    ProviderEntry {
        slug: "deepseek",
        display_name: "DeepSeek",
        protocol: ProviderProtocol::OpenAiCompatible,
        default_base_url: "https://api.deepseek.com",
        default_model: "deepseek-chat",
    },
    ProviderEntry {
        slug: "xai",
        display_name: "xAI Grok",
        protocol: ProviderProtocol::OpenAiCompatible,
        default_base_url: "https://api.x.ai/v1",
        default_model: "grok-4",
    },
    ProviderEntry {
        slug: "groq",
        display_name: "Groq",
        protocol: ProviderProtocol::OpenAiCompatible,
        default_base_url: "https://api.groq.com/openai/v1",
        default_model: "llama-3.3-70b-versatile",
    },
    ProviderEntry {
        slug: "together",
        display_name: "Together AI",
        protocol: ProviderProtocol::OpenAiCompatible,
        default_base_url: "https://api.together.xyz/v1",
        default_model: "meta-llama/Llama-3.3-70B-Instruct-Turbo",
    },
    ProviderEntry {
        slug: "mistral",
        display_name: "Mistral",
        protocol: ProviderProtocol::OpenAiCompatible,
        default_base_url: "https://api.mistral.ai/v1",
        default_model: "mistral-large-latest",
    },
    ProviderEntry {
        slug: "ollama",
        display_name: "Ollama (Local)",
        protocol: ProviderProtocol::OpenAiCompatible,
        default_base_url: "http://localhost:11434/v1",
        default_model: "llama3.1",
    },
    ProviderEntry {
        slug: "ollama-cloud",
        display_name: "Ollama Cloud",
        protocol: ProviderProtocol::OpenAiCompatible,
        default_base_url: "https://ollama.com/v1",
        default_model: "qwen3-coder:480b",
    },
    ProviderEntry {
        slug: "openrouter",
        display_name: "OpenRouter",
        protocol: ProviderProtocol::OpenAiCompatible,
        default_base_url: "https://openrouter.ai/api/v1",
        default_model: "openai/gpt-4.1",
    },
    ProviderEntry {
        slug: "huggingface",
        display_name: "Hugging Face",
        protocol: ProviderProtocol::OpenAiCompatible,
        default_base_url: "https://router.huggingface.co/v1",
        default_model: "meta-llama/Llama-3.3-70B-Instruct",
    },
    ProviderEntry {
        slug: "github",
        display_name: "GitHub Models",
        protocol: ProviderProtocol::OpenAiCompatible,
        default_base_url: "https://models.inference.ai.azure.com",
        default_model: "openai/gpt-4.1",
    },
    ProviderEntry {
        slug: "nvidia",
        display_name: "NVIDIA NIM",
        protocol: ProviderProtocol::OpenAiCompatible,
        default_base_url: "https://integrate.api.nvidia.com/v1",
        default_model: "nvidia/llama-3.1-nemotron-70b-instruct",
    },
    ProviderEntry {
        slug: "cerebras",
        display_name: "Cerebras",
        protocol: ProviderProtocol::OpenAiCompatible,
        default_base_url: "https://api.cerebras.ai/v1",
        default_model: "cerebras",
    },
    ProviderEntry {
        slug: "fireworks",
        display_name: "Fireworks AI",
        protocol: ProviderProtocol::OpenAiCompatible,
        default_base_url: "https://api.fireworks.ai/inference/v1",
        default_model: "accounts/fireworks/models/llama-v3p3-70b-instruct",
    },
    ProviderEntry {
        slug: "xiaomi-mimo",
        display_name: "Xiaomi MiMo",
        protocol: ProviderProtocol::OpenAiCompatible,
        default_base_url: "https://api.mimo.xiaomi.com/openai/v1",
        default_model: "mimo-vl",
    },
    ProviderEntry {
        slug: "dashscope",
        display_name: "DashScope (Alibaba)",
        protocol: ProviderProtocol::OpenAiCompatible,
        default_base_url: "https://dashscope-intl.aliyuncs.com/compatible-mode/v1",
        default_model: "qwen-plus",
    },
    ProviderEntry {
        slug: "moonshot",
        display_name: "Moonshot AI (Kimi)",
        protocol: ProviderProtocol::OpenAiCompatible,
        default_base_url: "https://api.moonshot.ai/v1",
        default_model: "kimi-k2",
    },
    ProviderEntry {
        slug: "perplexity",
        display_name: "Perplexity",
        protocol: ProviderProtocol::OpenAiCompatible,
        default_base_url: "https://api.perplexity.ai",
        default_model: "sonar-pro",
    },
    ProviderEntry {
        slug: "zai",
        display_name: "Z.ai (GLM)",
        protocol: ProviderProtocol::OpenAiCompatible,
        default_base_url: "https://api.z.ai/api/paas/v4",
        default_model: "glm-4.5",
    },
    ProviderEntry {
        slug: "venice",
        display_name: "Venice AI",
        protocol: ProviderProtocol::OpenAiCompatible,
        default_base_url: "https://api.venice.ai/api/v1",
        default_model: "qwen-2.5-qwq-32b",
    },
    ProviderEntry {
        slug: "novita",
        display_name: "Novita AI",
        protocol: ProviderProtocol::OpenAiCompatible,
        default_base_url: "https://api.novita.ai/v3/openai",
        default_model: "deepseek-v3",
    },
    ProviderEntry {
        slug: "lmstudio",
        display_name: "LM Studio",
        protocol: ProviderProtocol::OpenAiCompatible,
        default_base_url: "http://localhost:1234/v1",
        default_model: "local-model",
    },
    ProviderEntry {
        slug: "custom-openai",
        display_name: "Custom OpenAI-Compatible",
        protocol: ProviderProtocol::OpenAiCompatible,
        default_base_url: "https://api.openai.com/v1",
        default_model: "custom-model",
    },
    ProviderEntry {
        slug: "anthropic",
        display_name: "Anthropic",
        protocol: ProviderProtocol::AnthropicMessages,
        default_base_url: "https://api.anthropic.com",
        default_model: "claude-sonnet-4-20250514",
    },
    ProviderEntry {
        slug: "minimax",
        display_name: "MiniMax",
        protocol: ProviderProtocol::AnthropicMessages,
        default_base_url: "https://api.minimax.io/anthropic",
        default_model: "MiniMax-M3",
    },
    ProviderEntry {
        slug: "custom-anthropic",
        display_name: "Custom Anthropic-Compatible",
        protocol: ProviderProtocol::AnthropicMessages,
        default_base_url: "https://api.anthropic.com",
        default_model: "custom-model",
    },
    ProviderEntry {
        slug: "google",
        display_name: "Google Gemini",
        protocol: ProviderProtocol::GoogleGenerativeAi,
        default_base_url: "https://generativelanguage.googleapis.com/v1beta",
        default_model: "gemini-2.5-pro",
    },
];

impl fmt::Display for ProviderProtocol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProviderProtocol::OpenAiCompatible => write!(f, "OpenAI-compatible"),
            ProviderProtocol::AnthropicMessages => write!(f, "Anthropic Messages"),
            ProviderProtocol::GoogleGenerativeAi => write!(f, "Google Generative AI"),
        }
    }
}

/// Look up a provider entry by slug. Returns `None` if not found.
pub fn lookup_provider(slug: &str) -> Option<&'static ProviderEntry> {
    PROVIDER_CATALOG.iter().find(|e| e.slug == slug)
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
    fn all_display_names_are_non_empty() {
        for name in all_display_names() {
            assert!(!name.is_empty());
        }
    }
}
