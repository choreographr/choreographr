use std::fmt;
use tracing::debug;

use choreo_proto::ReasoningCapability;

use crate::openai::RequestFormat;
use crate::providers::shared::MaxTokensField;

/// Per-model metadata in the provider catalog.
/// A single source of truth for context window, reasoning support,
/// and explicit effort levels.
#[derive(Debug)]
pub struct ModelEntry {
    pub model: &'static str,
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
    /// `model_reasoning_capability()`).  Non-OpenAi protocols always
    /// use their own default levels — see `protocol_default_levels()`.
    ///
    /// Resolution rules in `model_reasoning_capability()`:
    ///   • `reasoning_supported = false` → empty capability (no reasoning)
    ///   • `reasoning_supported = true` + `OpenAi` protocol + non-empty
    ///     levels → use these levels
    ///   • `reasoning_supported = true` + non-`OpenAi` protocol → use
    ///     protocol defaults (the field is ignored)
    ///   • `reasoning_supported = true` + any protocol + empty levels →
    ///     use protocol defaults
    pub openai_reasoning_levels: &'static [&'static str],
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

#[derive(Debug, Clone, Copy)]
pub struct ProviderEntry {
    pub slug: &'static str,
    pub display_name: &'static str,
    pub protocol: ProviderProtocol,
    pub base_url: &'static str,
    pub default_model: &'static str,
    pub models: &'static [ModelEntry],
}

/// Static catalog of all known providers.
pub static PROVIDER_CATALOG: &[ProviderEntry] = &[
    ProviderEntry {
        slug: "openai",
        display_name: "OpenAI",
        protocol: ProviderProtocol::OpenAi {
            max_tokens_field: MaxTokensField::MaxCompletionTokens,
        },
        base_url: "https://api.openai.com/v1",
        default_model: "gpt-4.1",
        models: &[
            ModelEntry {
                model: "gpt-5.6-sol",
                context_window: 272_000,
                reasoning_supported: true,
                openai_reasoning_levels: &["off", "low", "medium", "high"],
                openai_responses: false,
            },
            ModelEntry {
                model: "gpt-5.6-terra",
                context_window: 272_000,
                reasoning_supported: true,
                openai_reasoning_levels: &["off", "low", "medium", "high"],
                openai_responses: false,
            },
            ModelEntry {
                model: "gpt-5.6-luna",
                context_window: 272_000,
                reasoning_supported: true,
                openai_reasoning_levels: &["off", "low", "medium", "high"],
                openai_responses: false,
            },
            ModelEntry {
                model: "gpt-5.5",
                context_window: 272_000,
                reasoning_supported: true,
                openai_reasoning_levels: &["off", "low", "medium", "high"],
                openai_responses: false,
            },
            ModelEntry {
                model: "gpt-5.5-pro",
                context_window: 1_050_000,
                reasoning_supported: true,
                openai_reasoning_levels: &["medium", "high"],
                openai_responses: false,
            },
            ModelEntry {
                model: "gpt-5.4",
                context_window: 272_000,
                reasoning_supported: true,
                openai_reasoning_levels: &["off", "low", "medium", "high"],
                openai_responses: false,
            },
            ModelEntry {
                model: "gpt-5.4-pro",
                context_window: 1_050_000,
                reasoning_supported: true,
                openai_reasoning_levels: &["low", "medium", "high"],
                openai_responses: false,
            },
            ModelEntry {
                model: "gpt-5.4-mini",
                context_window: 400_000,
                reasoning_supported: false,
                openai_reasoning_levels: &[],
                openai_responses: false,
            },
            ModelEntry {
                model: "gpt-5.4-nano",
                context_window: 400_000,
                reasoning_supported: false,
                openai_reasoning_levels: &[],
                openai_responses: false,
            },
            ModelEntry {
                model: "gpt-5.3-codex-spark",
                context_window: 128_000,
                reasoning_supported: false,
                openai_reasoning_levels: &[],
                openai_responses: false,
            },
            ModelEntry {
                model: "gpt-5.3-codex",
                context_window: 400_000,
                reasoning_supported: false,
                openai_reasoning_levels: &[],
                openai_responses: false,
            },
            ModelEntry {
                model: "gpt-5.2",
                context_window: 400_000,
                reasoning_supported: false,
                openai_reasoning_levels: &[],
                openai_responses: false,
            },
            ModelEntry {
                model: "gpt-5.2-codex",
                context_window: 400_000,
                reasoning_supported: false,
                openai_reasoning_levels: &[],
                openai_responses: false,
            },
            ModelEntry {
                model: "gpt-5.1",
                context_window: 400_000,
                reasoning_supported: false,
                openai_reasoning_levels: &[],
                openai_responses: false,
            },
            ModelEntry {
                model: "gpt-5.1-codex-max",
                context_window: 400_000,
                reasoning_supported: false,
                openai_reasoning_levels: &[],
                openai_responses: false,
            },
            ModelEntry {
                model: "gpt-5.1-codex",
                context_window: 400_000,
                reasoning_supported: false,
                openai_reasoning_levels: &[],
                openai_responses: false,
            },
            ModelEntry {
                model: "gpt-5.1-codex-mini",
                context_window: 400_000,
                reasoning_supported: false,
                openai_reasoning_levels: &[],
                openai_responses: false,
            },
            ModelEntry {
                model: "gpt-5",
                context_window: 400_000,
                reasoning_supported: false,
                openai_reasoning_levels: &[],
                openai_responses: false,
            },
            ModelEntry {
                model: "gpt-5-codex",
                context_window: 400_000,
                reasoning_supported: false,
                openai_reasoning_levels: &[],
                openai_responses: false,
            },
            ModelEntry {
                model: "gpt-5-nano",
                context_window: 400_000,
                reasoning_supported: false,
                openai_reasoning_levels: &[],
                openai_responses: false,
            },
            ModelEntry {
                model: "gpt-4.1-nano",
                context_window: 1_048_576,
                reasoning_supported: false,
                openai_reasoning_levels: &[],
                openai_responses: false,
            },
            ModelEntry {
                model: "gpt-4.1-mini",
                context_window: 1_048_576,
                reasoning_supported: false,
                openai_reasoning_levels: &[],
                openai_responses: false,
            },
            ModelEntry {
                model: "gpt-4.1",
                context_window: 1_048_576,
                reasoning_supported: false,
                openai_reasoning_levels: &[],
                openai_responses: false,
            },
            ModelEntry {
                model: "gpt-4o",
                context_window: 128_000,
                reasoning_supported: false,
                openai_reasoning_levels: &[],
                openai_responses: false,
            },
            ModelEntry {
                model: "gpt-4o-mini",
                context_window: 128_000,
                reasoning_supported: false,
                openai_reasoning_levels: &[],
                openai_responses: false,
            },
            ModelEntry {
                model: "o1",
                context_window: 200_000,
                reasoning_supported: true,
                openai_reasoning_levels: &["off", "low", "medium", "high"],
                openai_responses: false,
            },
            ModelEntry {
                model: "o3",
                context_window: 200_000,
                reasoning_supported: true,
                openai_reasoning_levels: &[],
                openai_responses: false,
            },
            ModelEntry {
                model: "o4-mini",
                context_window: 200_000,
                reasoning_supported: true,
                openai_reasoning_levels: &[],
                openai_responses: false,
            },
            ModelEntry {
                model: "o3-mini",
                context_window: 200_000,
                reasoning_supported: true,
                openai_reasoning_levels: &["off", "low", "medium", "high"],
                openai_responses: false,
            },
            ModelEntry {
                model: "o4-mini-2025-07-18",
                context_window: 200_000,
                reasoning_supported: true,
                openai_reasoning_levels: &["off", "low", "medium", "high"],
                openai_responses: false,
            },
        ],
    },
    ProviderEntry {
        slug: "openai_compatible",
        display_name: "OpenAI Compatible",
        protocol: ProviderProtocol::OpenAi {
            max_tokens_field: MaxTokensField::MaxCompletionTokens,
        },
        base_url: "https://api.openai.com/v1",
        default_model: "custom-model",
        models: &[],
    },
    ProviderEntry {
        slug: "opencode",
        display_name: "OpenCode Zen",
        protocol: ProviderProtocol::OpenAi {
            max_tokens_field: MaxTokensField::MaxTokens,
        },
        base_url: "https://opencode.ai/zen/v1",
        default_model: "deepseek-v4-flash",
        // Model → API-format mapping follows opencode.ai/docs/zen (Zen
        // gateway): the GPT 5.x family is served via the Responses API
        // (@ai-sdk/openai), while deepseek-v4 and the rest use Chat
        // Completions (@ai-sdk/openai-compatible). The daemon dispatches per
        // model on this flag via ServiceConfig::request_format_for_model().
        models: &[
            ModelEntry {
                model: "deepseek-v4-flash",
                context_window: 1_000_000,
                reasoning_supported: true,
                openai_reasoning_levels: &["high", "max"],
                openai_responses: false,
            },
            ModelEntry {
                model: "deepseek-v4-pro",
                context_window: 1_000_000,
                reasoning_supported: true,
                openai_reasoning_levels: &["high", "max"],
                openai_responses: false,
            },
            ModelEntry {
                model: "big-pickle",
                context_window: 200_000,
                reasoning_supported: false,
                openai_reasoning_levels: &[],
                openai_responses: false,
            },
            ModelEntry {
                model: "gpt-5.6-sol",
                context_window: 272_000,
                reasoning_supported: true,
                openai_reasoning_levels: &["off", "low", "medium", "high"],
                openai_responses: true,
            },
            ModelEntry {
                model: "gpt-5.6-terra",
                context_window: 272_000,
                reasoning_supported: true,
                openai_reasoning_levels: &["off", "low", "medium", "high"],
                openai_responses: true,
            },
            ModelEntry {
                model: "gpt-5.6-luna",
                context_window: 272_000,
                reasoning_supported: true,
                openai_reasoning_levels: &["off", "low", "medium", "high"],
                openai_responses: true,
            },
            ModelEntry {
                model: "gpt-5.5",
                context_window: 272_000,
                reasoning_supported: true,
                openai_reasoning_levels: &["off", "low", "medium", "high"],
                openai_responses: true,
            },
            ModelEntry {
                model: "gpt-5.5-pro",
                context_window: 1_050_000,
                reasoning_supported: true,
                openai_reasoning_levels: &["medium", "high"],
                openai_responses: true,
            },
            ModelEntry {
                model: "gpt-5.4",
                context_window: 272_000,
                reasoning_supported: true,
                openai_reasoning_levels: &["off", "low", "medium", "high"],
                openai_responses: true,
            },
            ModelEntry {
                model: "gpt-5.4-pro",
                context_window: 1_050_000,
                reasoning_supported: true,
                openai_reasoning_levels: &["low", "medium", "high"],
                openai_responses: true,
            },
            ModelEntry {
                model: "gpt-5.4-mini",
                context_window: 400_000,
                reasoning_supported: false,
                openai_reasoning_levels: &[],
                openai_responses: true,
            },
            ModelEntry {
                model: "gpt-5.4-nano",
                context_window: 400_000,
                reasoning_supported: false,
                openai_reasoning_levels: &[],
                openai_responses: true,
            },
            ModelEntry {
                model: "gpt-5.3-codex",
                context_window: 400_000,
                reasoning_supported: false,
                openai_reasoning_levels: &[],
                openai_responses: true,
            },
            ModelEntry {
                model: "gpt-5.3-codex-spark",
                context_window: 128_000,
                reasoning_supported: false,
                openai_reasoning_levels: &[],
                openai_responses: true,
            },
            ModelEntry {
                model: "gpt-5.2",
                context_window: 400_000,
                reasoning_supported: false,
                openai_reasoning_levels: &[],
                openai_responses: true,
            },
            ModelEntry {
                model: "gpt-5.2-codex",
                context_window: 400_000,
                reasoning_supported: false,
                openai_reasoning_levels: &[],
                openai_responses: true,
            },
            ModelEntry {
                model: "gpt-5.1",
                context_window: 400_000,
                reasoning_supported: false,
                openai_reasoning_levels: &[],
                openai_responses: true,
            },
            ModelEntry {
                model: "gpt-5.1-codex",
                context_window: 400_000,
                reasoning_supported: false,
                openai_reasoning_levels: &[],
                openai_responses: true,
            },
            ModelEntry {
                model: "gpt-5.1-codex-max",
                context_window: 400_000,
                reasoning_supported: false,
                openai_reasoning_levels: &[],
                openai_responses: true,
            },
            ModelEntry {
                model: "gpt-5.1-codex-mini",
                context_window: 400_000,
                reasoning_supported: false,
                openai_reasoning_levels: &[],
                openai_responses: true,
            },
            ModelEntry {
                model: "gpt-5",
                context_window: 400_000,
                reasoning_supported: false,
                openai_reasoning_levels: &[],
                openai_responses: true,
            },
            ModelEntry {
                model: "gpt-5-codex",
                context_window: 400_000,
                reasoning_supported: false,
                openai_reasoning_levels: &[],
                openai_responses: true,
            },
            ModelEntry {
                model: "gpt-5-nano",
                context_window: 400_000,
                reasoning_supported: false,
                openai_reasoning_levels: &[],
                openai_responses: true,
            },
        ],
    },
    ProviderEntry {
        slug: "opencode-go",
        display_name: "OpenCode Go",
        protocol: ProviderProtocol::OpenAi {
            max_tokens_field: MaxTokensField::MaxTokens,
        },
        base_url: "https://opencode.ai/zen/go/v1",
        default_model: "deepseek-v4-pro",
        // Model → API-format mapping follows opencode.ai/docs/go (Go gateway):
        // the deepseek-v4 family is served via Chat Completions
        // (@ai-sdk/openai-compatible), while GPT-5.6 Luna is the one model on
        // this gateway that uses the Responses API (@ai-sdk/openai). The
        // daemon dispatches per model on this flag via
        // ServiceConfig::request_format_for_model().
        models: &[
            ModelEntry {
                model: "deepseek-v4-flash",
                context_window: 1_000_000,
                reasoning_supported: true,
                openai_reasoning_levels: &["high", "max"],
                openai_responses: false,
            },
            ModelEntry {
                model: "deepseek-v4-pro",
                context_window: 1_000_000,
                reasoning_supported: true,
                openai_reasoning_levels: &["high", "max"],
                openai_responses: false,
            },
            ModelEntry {
                model: "gpt-5.6-luna",
                context_window: 272_000,
                reasoning_supported: true,
                openai_reasoning_levels: &["off", "low", "medium", "high"],
                openai_responses: true,
            },
        ],
    },
    ProviderEntry {
        slug: "deepseek",
        display_name: "DeepSeek",
        protocol: ProviderProtocol::OpenAi {
            max_tokens_field: MaxTokensField::MaxCompletionTokens,
        },
        base_url: "https://api.deepseek.com",
        default_model: "deepseek-chat",
        models: &[
            ModelEntry {
                model: "deepseek-v4-flash",
                context_window: 1_000_000,
                reasoning_supported: true,
                openai_reasoning_levels: &["high", "max"],
                openai_responses: false,
            },
            ModelEntry {
                model: "deepseek-v4-pro",
                context_window: 1_000_000,
                reasoning_supported: true,
                openai_reasoning_levels: &["high", "max"],
                openai_responses: false,
            },
            ModelEntry {
                model: "deepseek-chat",
                context_window: 64_000,
                reasoning_supported: false,
                openai_reasoning_levels: &[],
                openai_responses: false,
            },
            ModelEntry {
                model: "deepseek-reasoner",
                context_window: 64_000,
                reasoning_supported: true,
                openai_reasoning_levels: &["off", "low", "medium", "high"],
                openai_responses: false,
            },
        ],
    },
    ProviderEntry {
        slug: "xai",
        display_name: "xAI Grok",
        protocol: ProviderProtocol::OpenAi {
            max_tokens_field: MaxTokensField::MaxCompletionTokens,
        },
        base_url: "https://api.x.ai/v1",
        default_model: "grok-4",
        models: &[
            ModelEntry {
                model: "grok-build-0.1",
                context_window: 256_000,
                reasoning_supported: false,
                openai_reasoning_levels: &[],
                openai_responses: false,
            },
            ModelEntry {
                model: "grok-4.5",
                context_window: 500_000,
                reasoning_supported: false,
                openai_reasoning_levels: &[],
                openai_responses: false,
            },
            ModelEntry {
                model: "grok-4.3",
                context_window: 1_000_000,
                reasoning_supported: false,
                openai_reasoning_levels: &[],
                openai_responses: false,
            },
            ModelEntry {
                model: "grok-4",
                context_window: 131_072,
                reasoning_supported: false,
                openai_reasoning_levels: &[],
                openai_responses: false,
            },
            ModelEntry {
                model: "grok-3",
                context_window: 131_072,
                reasoning_supported: false,
                openai_reasoning_levels: &[],
                openai_responses: false,
            },
        ],
    },
    ProviderEntry {
        slug: "groq",
        display_name: "Groq",
        protocol: ProviderProtocol::OpenAi {
            max_tokens_field: MaxTokensField::MaxCompletionTokens,
        },
        base_url: "https://api.groq.com/openai/v1",
        default_model: "llama-3.3-70b-versatile",
        models: &[],
    },
    ProviderEntry {
        slug: "together",
        display_name: "Together AI",
        protocol: ProviderProtocol::OpenAi {
            max_tokens_field: MaxTokensField::MaxCompletionTokens,
        },
        base_url: "https://api.together.xyz/v1",
        default_model: "meta-llama/Llama-3.3-70B-Instruct-Turbo",
        models: &[],
    },
    ProviderEntry {
        slug: "mistral",
        display_name: "Mistral",
        protocol: ProviderProtocol::OpenAi {
            max_tokens_field: MaxTokensField::MaxTokens,
        },
        base_url: "https://api.mistral.ai/v1",
        default_model: "mistral-large-latest",
        models: &[],
    },
    ProviderEntry {
        slug: "ollama",
        display_name: "Ollama (Local)",
        protocol: ProviderProtocol::OpenAi {
            max_tokens_field: MaxTokensField::MaxCompletionTokens,
        },
        base_url: "http://localhost:11434/v1",
        default_model: "llama3.1",
        models: &[],
    },
    ProviderEntry {
        slug: "ollama-cloud",
        display_name: "Ollama Cloud",
        protocol: ProviderProtocol::OpenAi {
            max_tokens_field: MaxTokensField::MaxCompletionTokens,
        },
        base_url: "https://ollama.com/v1",
        default_model: "qwen3-coder:480b",
        models: &[],
    },
    ProviderEntry {
        slug: "openrouter",
        display_name: "OpenRouter",
        protocol: ProviderProtocol::OpenAi {
            max_tokens_field: MaxTokensField::MaxCompletionTokens,
        },
        base_url: "https://openrouter.ai/api/v1",
        default_model: "openai/gpt-4.1",
        models: &[],
    },
    ProviderEntry {
        slug: "huggingface",
        display_name: "Hugging Face",
        protocol: ProviderProtocol::OpenAi {
            max_tokens_field: MaxTokensField::MaxCompletionTokens,
        },
        base_url: "https://router.huggingface.co/v1",
        default_model: "meta-llama/Llama-3.3-70B-Instruct",
        models: &[],
    },
    ProviderEntry {
        slug: "github",
        display_name: "GitHub Models",
        protocol: ProviderProtocol::OpenAi {
            max_tokens_field: MaxTokensField::MaxCompletionTokens,
        },
        base_url: "https://models.inference.ai.azure.com",
        default_model: "openai/gpt-4.1",
        models: &[],
    },
    ProviderEntry {
        slug: "nvidia",
        display_name: "NVIDIA NIM",
        protocol: ProviderProtocol::OpenAi {
            max_tokens_field: MaxTokensField::MaxCompletionTokens,
        },
        base_url: "https://integrate.api.nvidia.com/v1",
        default_model: "nvidia/llama-3.1-nemotron-70b-instruct",
        models: &[],
    },
    ProviderEntry {
        slug: "cerebras",
        display_name: "Cerebras",
        protocol: ProviderProtocol::OpenAi {
            max_tokens_field: MaxTokensField::MaxCompletionTokens,
        },
        base_url: "https://api.cerebras.ai/v1",
        default_model: "cerebras",
        models: &[],
    },
    ProviderEntry {
        slug: "fireworks",
        display_name: "Fireworks AI",
        protocol: ProviderProtocol::OpenAi {
            max_tokens_field: MaxTokensField::MaxCompletionTokens,
        },
        base_url: "https://api.fireworks.ai/inference/v1",
        default_model: "accounts/fireworks/models/llama-v3p3-70b-instruct",
        models: &[],
    },
    ProviderEntry {
        slug: "xiaomi-mimo",
        display_name: "Xiaomi MiMo",
        protocol: ProviderProtocol::OpenAi {
            max_tokens_field: MaxTokensField::MaxCompletionTokens,
        },
        base_url: "https://api.mimo.xiaomi.com/openai/v1",
        default_model: "mimo-vl",
        models: &[],
    },
    ProviderEntry {
        slug: "dashscope",
        display_name: "DashScope (Alibaba)",
        protocol: ProviderProtocol::OpenAi {
            max_tokens_field: MaxTokensField::MaxCompletionTokens,
        },
        base_url: "https://dashscope-intl.aliyuncs.com/compatible-mode/v1",
        default_model: "qwen-plus",
        models: &[
            ModelEntry {
                model: "qwen3.6-plus",
                context_window: 1_000_000,
                reasoning_supported: false,
                openai_reasoning_levels: &[],
                openai_responses: false,
            },
            ModelEntry {
                model: "qwen3.5-plus",
                context_window: 1_000_000,
                reasoning_supported: false,
                openai_reasoning_levels: &[],
                openai_responses: false,
            },
            ModelEntry {
                model: "qwen-plus",
                context_window: 131_072,
                reasoning_supported: false,
                openai_reasoning_levels: &[],
                openai_responses: false,
            },
            ModelEntry {
                model: "qwen-max",
                context_window: 32_000,
                reasoning_supported: false,
                openai_reasoning_levels: &[],
                openai_responses: false,
            },
            ModelEntry {
                model: "qwen-turbo",
                context_window: 1_000_000,
                reasoning_supported: false,
                openai_reasoning_levels: &[],
                openai_responses: false,
            },
            ModelEntry {
                model: "qwen2.5",
                context_window: 131_072,
                reasoning_supported: false,
                openai_reasoning_levels: &[],
                openai_responses: false,
            },
            ModelEntry {
                model: "qwen2",
                context_window: 131_072,
                reasoning_supported: false,
                openai_reasoning_levels: &[],
                openai_responses: false,
            },
            ModelEntry {
                model: "qwen3",
                context_window: 131_072,
                reasoning_supported: false,
                openai_reasoning_levels: &[],
                openai_responses: false,
            },
            ModelEntry {
                model: "qwen-vl",
                context_window: 131_072,
                reasoning_supported: false,
                openai_reasoning_levels: &[],
                openai_responses: false,
            },
        ],
    },
    ProviderEntry {
        slug: "moonshot",
        display_name: "Moonshot AI (Kimi)",
        protocol: ProviderProtocol::OpenAi {
            max_tokens_field: MaxTokensField::MaxCompletionTokens,
        },
        base_url: "https://api.moonshot.ai/v1",
        default_model: "kimi-k2",
        models: &[
            ModelEntry {
                model: "kimi-k2.7-code",
                context_window: 262_144,
                reasoning_supported: false,
                openai_reasoning_levels: &[],
                openai_responses: false,
            },
            ModelEntry {
                model: "kimi-k2.6",
                context_window: 262_144,
                reasoning_supported: false,
                openai_reasoning_levels: &[],
                openai_responses: false,
            },
            ModelEntry {
                model: "kimi-k2.5",
                context_window: 262_144,
                reasoning_supported: false,
                openai_reasoning_levels: &[],
                openai_responses: false,
            },
            ModelEntry {
                model: "kimi-k2",
                context_window: 128_000,
                reasoning_supported: false,
                openai_reasoning_levels: &[],
                openai_responses: false,
            },
        ],
    },
    ProviderEntry {
        slug: "perplexity",
        display_name: "Perplexity",
        protocol: ProviderProtocol::OpenAi {
            max_tokens_field: MaxTokensField::MaxCompletionTokens,
        },
        base_url: "https://api.perplexity.ai",
        default_model: "sonar-pro",
        models: &[
            ModelEntry {
                model: "sonar-pro",
                context_window: 127_000,
                reasoning_supported: false,
                openai_reasoning_levels: &[],
                openai_responses: false,
            },
            ModelEntry {
                model: "sonar-reasoning",
                context_window: 127_000,
                reasoning_supported: false,
                openai_reasoning_levels: &[],
                openai_responses: false,
            },
            ModelEntry {
                model: "sonar-deep-research",
                context_window: 127_000,
                reasoning_supported: false,
                openai_reasoning_levels: &[],
                openai_responses: false,
            },
            ModelEntry {
                model: "sonar",
                context_window: 127_000,
                reasoning_supported: false,
                openai_reasoning_levels: &[],
                openai_responses: false,
            },
        ],
    },
    ProviderEntry {
        slug: "zai",
        display_name: "Z.ai (GLM)",
        protocol: ProviderProtocol::OpenAi {
            max_tokens_field: MaxTokensField::MaxCompletionTokens,
        },
        base_url: "https://api.z.ai/api/paas/v4",
        default_model: "glm-4.5",
        models: &[
            ModelEntry {
                model: "glm-5.2",
                context_window: 202_752,
                reasoning_supported: false,
                openai_reasoning_levels: &[],
                openai_responses: false,
            },
            ModelEntry {
                model: "glm-5.1",
                context_window: 202_752,
                reasoning_supported: false,
                openai_reasoning_levels: &[],
                openai_responses: false,
            },
            ModelEntry {
                model: "glm-5",
                context_window: 202_752,
                reasoning_supported: false,
                openai_reasoning_levels: &[],
                openai_responses: false,
            },
            ModelEntry {
                model: "glm-4.5",
                context_window: 128_000,
                reasoning_supported: false,
                openai_reasoning_levels: &[],
                openai_responses: false,
            },
            ModelEntry {
                model: "glm-4",
                context_window: 128_000,
                reasoning_supported: false,
                openai_reasoning_levels: &[],
                openai_responses: false,
            },
        ],
    },
    ProviderEntry {
        slug: "venice",
        display_name: "Venice AI",
        protocol: ProviderProtocol::OpenAi {
            max_tokens_field: MaxTokensField::MaxCompletionTokens,
        },
        base_url: "https://api.venice.ai/api/v1",
        default_model: "qwen-2.5-qwq-32b",
        models: &[],
    },
    ProviderEntry {
        slug: "novita",
        display_name: "Novita AI",
        protocol: ProviderProtocol::OpenAi {
            max_tokens_field: MaxTokensField::MaxCompletionTokens,
        },
        base_url: "https://api.novita.ai/v3/openai",
        default_model: "deepseek-v3",
        models: &[],
    },
    ProviderEntry {
        slug: "lmstudio",
        display_name: "LM Studio",
        protocol: ProviderProtocol::OpenAi {
            max_tokens_field: MaxTokensField::MaxCompletionTokens,
        },
        base_url: "http://localhost:1234/v1",
        default_model: "local-model",
        models: &[],
    },
    ProviderEntry {
        slug: "custom-openai",
        display_name: "Custom OpenAI-Compatible",
        protocol: ProviderProtocol::OpenAi {
            max_tokens_field: MaxTokensField::MaxCompletionTokens,
        },
        base_url: "https://api.openai.com/v1",
        default_model: "custom-model",
        models: &[],
    },
    ProviderEntry {
        slug: "anthropic",
        display_name: "Anthropic",
        protocol: ProviderProtocol::AnthropicMessages,
        base_url: "https://api.anthropic.com",
        default_model: "claude-sonnet-4-20250514",
        models: &[
            ModelEntry {
                model: "claude-fable-5",
                context_window: 1_000_000,
                reasoning_supported: true,
                openai_reasoning_levels: &[],
                openai_responses: false,
            },
            ModelEntry {
                model: "claude-opus-4-8",
                context_window: 1_000_000,
                reasoning_supported: true,
                openai_reasoning_levels: &[],
                openai_responses: false,
            },
            ModelEntry {
                model: "claude-opus-4-7",
                context_window: 1_000_000,
                reasoning_supported: true,
                openai_reasoning_levels: &[],
                openai_responses: false,
            },
            ModelEntry {
                model: "claude-opus-4-6",
                context_window: 1_000_000,
                reasoning_supported: true,
                openai_reasoning_levels: &[],
                openai_responses: false,
            },
            ModelEntry {
                model: "claude-opus-4-5",
                context_window: 200_000,
                reasoning_supported: true,
                openai_reasoning_levels: &[],
                openai_responses: false,
            },
            ModelEntry {
                model: "claude-opus-4-1",
                context_window: 200_000,
                reasoning_supported: true,
                openai_reasoning_levels: &[],
                openai_responses: false,
            },
            ModelEntry {
                model: "claude-sonnet-5",
                context_window: 1_000_000,
                reasoning_supported: true,
                openai_reasoning_levels: &[],
                openai_responses: false,
            },
            ModelEntry {
                model: "claude-sonnet-4-6",
                context_window: 1_000_000,
                reasoning_supported: true,
                openai_reasoning_levels: &[],
                openai_responses: false,
            },
            ModelEntry {
                model: "claude-sonnet-4-5",
                context_window: 1_000_000,
                reasoning_supported: true,
                openai_reasoning_levels: &[],
                openai_responses: false,
            },
            ModelEntry {
                model: "claude-sonnet-4",
                context_window: 200_000,
                reasoning_supported: true,
                openai_reasoning_levels: &[],
                openai_responses: false,
            },
            ModelEntry {
                model: "claude-haiku-4-5",
                context_window: 200_000,
                reasoning_supported: false,
                openai_reasoning_levels: &[],
                openai_responses: false,
            },
        ],
    },
    ProviderEntry {
        slug: "minimax",
        display_name: "MiniMax",
        protocol: ProviderProtocol::AnthropicMessages,
        base_url: "https://api.minimax.io/anthropic",
        default_model: "MiniMax-M3",
        models: &[
            ModelEntry {
                model: "minimax-m3",
                context_window: 128_000,
                reasoning_supported: false,
                openai_reasoning_levels: &[],
                openai_responses: false,
            },
            ModelEntry {
                model: "minimax-m2.7",
                context_window: 204_800,
                reasoning_supported: false,
                openai_reasoning_levels: &[],
                openai_responses: false,
            },
            ModelEntry {
                model: "minimax-m2.5",
                context_window: 204_800,
                reasoning_supported: false,
                openai_reasoning_levels: &[],
                openai_responses: false,
            },
        ],
    },
    ProviderEntry {
        slug: "custom-anthropic",
        display_name: "Custom Anthropic-Compatible",
        protocol: ProviderProtocol::AnthropicMessages,
        base_url: "https://api.anthropic.com",
        default_model: "custom-model",
        models: &[],
    },
    ProviderEntry {
        slug: "google",
        display_name: "Google Gemini",
        protocol: ProviderProtocol::GoogleGenerativeAi,
        base_url: "https://generativelanguage.googleapis.com/v1beta",
        default_model: "gemini-2.5-pro",
        models: &[
            ModelEntry {
                model: "gemini-3.5-flash",
                context_window: 1_048_576,
                reasoning_supported: false,
                openai_reasoning_levels: &[],
                openai_responses: false,
            },
            ModelEntry {
                model: "gemini-3.1-pro",
                context_window: 1_048_576,
                reasoning_supported: false,
                openai_reasoning_levels: &[],
                openai_responses: false,
            },
            ModelEntry {
                model: "gemini-3-flash",
                context_window: 1_048_576,
                reasoning_supported: false,
                openai_reasoning_levels: &[],
                openai_responses: false,
            },
            ModelEntry {
                model: "gemini-2.5-pro",
                context_window: 1_048_576,
                reasoning_supported: true,
                openai_reasoning_levels: &[],
                openai_responses: false,
            },
            ModelEntry {
                model: "gemini-2.5-flash",
                context_window: 1_048_576,
                reasoning_supported: true,
                openai_reasoning_levels: &[],
                openai_responses: false,
            },
            ModelEntry {
                model: "gemini-2.0-flash",
                context_window: 1_048_576,
                reasoning_supported: false,
                openai_reasoning_levels: &[],
                openai_responses: false,
            },
            ModelEntry {
                model: "gemini-1.5-pro",
                context_window: 2_097_152,
                reasoning_supported: false,
                openai_reasoning_levels: &[],
                openai_responses: false,
            },
            ModelEntry {
                model: "gemini-1.5-flash",
                context_window: 1_048_576,
                reasoning_supported: false,
                openai_reasoning_levels: &[],
                openai_responses: false,
            },
        ],
    },
];

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
/// Returns `None` if no entry matches or the provider is unknown.
pub fn lookup_context_window(provider_slug: &str, model: &str) -> Option<u32> {
    let entry = lookup_provider(provider_slug)?;
    for m in entry.models {
        if model == m.model {
            return Some(m.context_window);
        }
    }
    None
}

/// Return all provider slugs.
pub fn all_slugs() -> impl Iterator<Item = &'static str> {
    PROVIDER_CATALOG.iter().map(|e| e.slug)
}

/// Return all display names.
pub fn all_display_names() -> impl Iterator<Item = &'static str> {
    PROVIDER_CATALOG.iter().map(|e| e.display_name)
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
                    m.openai_reasoning_levels
                        .iter()
                        .map(|s| s.to_string())
                        .collect()
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
    for m in entry.models {
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
                !entry.base_url.is_empty(),
                "empty base_url for {}",
                entry.slug
            );
            assert!(
                !entry.default_model.is_empty(),
                "empty model for {}",
                entry.slug
            );
            for m in entry.models {
                assert!(!m.model.is_empty(), "empty model slug in {}", entry.slug);
                assert!(
                    m.context_window > 0,
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
            Some(1_048_576)
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
            lookup_context_window("anthropic", "claude-sonnet-4"),
            Some(200_000)
        );
        assert_eq!(
            lookup_context_window("anthropic", "claude-sonnet-4-6"),
            Some(1_000_000)
        );
        // Google exact slug matches
        assert_eq!(
            lookup_context_window("google", "gemini-3.5-flash"),
            Some(1_048_576)
        );
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
            vec!["off", "low", "medium", "high"]
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
        assert_eq!(cap.available_effort_levels, vec!["high", "max"]);
    }

    #[test]
    fn model_reasoning_capability_anthropic_supported() {
        let cap = model_reasoning_capability("anthropic", "claude-sonnet-4-20250514");
        assert_eq!(
            cap.available_effort_levels,
            vec!["off", "low", "medium", "high"]
        );
    }

    #[test]
    fn model_reasoning_capability_anthropic_unsupported() {
        let cap = model_reasoning_capability("anthropic", "claude-haiku-4-5");
        assert!(cap.available_effort_levels.is_empty());
    }

    #[test]
    fn model_reasoning_capability_google_supported() {
        let cap = model_reasoning_capability("google", "gemini-2.5-pro");
        assert_eq!(cap.available_effort_levels, vec!["off", "on"]);
    }

    #[test]
    fn model_reasoning_capability_google_unsupported() {
        let cap = model_reasoning_capability("google", "gemini-1.5-pro");
        assert!(cap.available_effort_levels.is_empty());
    }

    #[test]
    fn model_reasoning_capability_none_provider() {
        let cap = model_reasoning_capability("nonexistent", "any-model");
        assert!(cap.available_effort_levels.is_empty());
    }

    #[test]
    fn model_request_format_known_model() {
        let fmt = model_request_format("openai", "gpt-4.1");
        assert_eq!(fmt, Some(RequestFormat::ChatCompletions));
    }

    #[test]
    fn model_request_format_opencode_go_matches_gateway_docs() {
        // opencode.ai/docs/go: deepseek-v4 models are served via Chat
        // Completions (@ai-sdk/openai-compatible); gpt-5.6-luna is the Go
        // gateway's Responses API model (@ai-sdk/openai).
        assert_eq!(
            model_request_format("opencode-go", "deepseek-v4-flash"),
            Some(RequestFormat::ChatCompletions)
        );
        assert_eq!(
            model_request_format("opencode-go", "deepseek-v4-pro"),
            Some(RequestFormat::ChatCompletions)
        );
        assert_eq!(
            model_request_format("opencode-go", "gpt-5.6-luna"),
            Some(RequestFormat::Responses)
        );
    }

    #[test]
    fn model_request_format_opencode_zen_gpt_family_uses_responses() {
        // opencode.ai/docs/zen: the GPT 5.x family on the Zen gateway is
        // served via the Responses API, while deepseek-v4 uses Chat
        // Completions.
        for model in [
            "gpt-5",
            "gpt-5-codex",
            "gpt-5.1-codex-max",
            "gpt-5.4",
            "gpt-5.5-pro",
            "gpt-5.6-sol",
            "gpt-5.6-luna",
        ] {
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
        assert_eq!(
            model_request_format("opencode", "big-pickle"),
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
