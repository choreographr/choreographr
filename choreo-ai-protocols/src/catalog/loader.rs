//! Loads the bundled TOML provider catalog into typed runtime entries.
//!
//! Every provider lives in its own `*.toml` data file under this directory
//! (one file per provider, filename = slug with `-` → `_`). The files are
//! embedded at compile time via `include_str!` so the daemon stays a single
//! self-contained binary — no runtime filesystem reads, no missing-file
//! failures.
//!
//! The catalog is parsed lazily into a `static LazyLock` (see `mod.rs`), so
//! the `&'static`-reference API the rest of the crate relies on is preserved:
//! static storage lives forever and is never mutated after first access.

use serde::Deserialize;

use crate::shared::MaxTokensField;

use super::{ModelEntry, ProviderEntry, ProviderProtocol, ReasoningPassback};

/// Every provider data file bundled into the binary.
pub(crate) const CATALOG_FILES: &[&str] = &[
    include_str!("aimlapi.toml"),
    include_str!("alibaba.toml"),
    include_str!("ant-ling.toml"),
    include_str!("anthropic.toml"),
    include_str!("arcee.toml"),
    include_str!("atlascloud.toml"),
    include_str!("atomic-chat.toml"),
    include_str!("bankr.toml"),
    include_str!("cerebras.toml"),
    include_str!("cloudflare-ai-gateway.toml"),
    include_str!("cloudflare-workers-ai.toml"),
    include_str!("custom-anthropic.toml"),
    include_str!("custom-openai.toml"),
    include_str!("deepinfra.toml"),
    include_str!("deepseek.toml"),
    include_str!("empiriolabs.toml"),
    include_str!("fireworks.toml"),
    include_str!("friendli.toml"),
    include_str!("futurmix.toml"),
    include_str!("github-copilot.toml"),
    include_str!("github.toml"),
    include_str!("gitlawb-opengateway.toml"),
    include_str!("gmi.toml"),
    include_str!("google.toml"),
    include_str!("groq.toml"),
    include_str!("huggingface.toml"),
    include_str!("iflytek-astron.toml"),
    include_str!("iflytek.toml"),
    include_str!("inception.toml"),
    include_str!("kilocode.toml"),
    include_str!("kimi-code.toml"),
    include_str!("llama-swap.toml"),
    include_str!("lmstudio.toml"),
    include_str!("longcat.toml"),
    include_str!("meta.toml"),
    include_str!("minimax-cn.toml"),
    include_str!("minimax.toml"),
    include_str!("mistral.toml"),
    include_str!("moonshotai-cn.toml"),
    include_str!("moonshotai.toml"),
    include_str!("nearai.toml"),
    include_str!("nous.toml"),
    include_str!("novita.toml"),
    include_str!("nvidia.toml"),
    include_str!("ollama-cloud.toml"),
    include_str!("ollama.toml"),
    include_str!("omlx.toml"),
    include_str!("openai-codex.toml"),
    include_str!("openai.toml"),
    include_str!("openai_compatible.toml"),
    include_str!("opencode-go-anthropic-compatible.toml"),
    include_str!("opencode-go.toml"),
    include_str!("opencode.toml"),
    include_str!("openrouter.toml"),
    include_str!("orcarouter.toml"),
    include_str!("ovhcloud.toml"),
    include_str!("perplexity.toml"),
    include_str!("qwen-token-plan-cn.toml"),
    include_str!("qwen-token-plan.toml"),
    include_str!("routstr.toml"),
    include_str!("sakana.toml"),
    include_str!("saladcloud.toml"),
    include_str!("scaleway.toml"),
    include_str!("stepfun.toml"),
    include_str!("tanzu.toml"),
    include_str!("tensorix.toml"),
    include_str!("together.toml"),
    include_str!("upstage.toml"),
    include_str!("venice.toml"),
    include_str!("vercel-ai-gateway.toml"),
    include_str!("xai.toml"),
    include_str!("xiaomi-token-plan-ams.toml"),
    include_str!("xiaomi-token-plan-cn.toml"),
    include_str!("xiaomi-token-plan-sgp.toml"),
    include_str!("xiaomi.toml"),
    include_str!("zai-cn.toml"),
    include_str!("zai-coding-cn.toml"),
    include_str!("zai.toml"),
    include_str!("zhipu.toml"),
];

/// Wire-format mirror of a single provider's TOML file.
///
/// `protocol` and `max_tokens_field` are kept as strings here (TOML cannot
/// express a tagged enum directly) and converted to the typed runtime
/// `ProviderProtocol` in [`convert`]. `models` defaults to an empty list so a
/// dynamic provider file with no `[[models]]` section still parses.
#[derive(Debug, Deserialize)]
struct RawProvider {
    slug: String,
    display_name: String,
    protocol: String,
    #[serde(default)]
    max_tokens_field: Option<String>,
    base_url: String,
    default_model: String,
    #[serde(default)]
    models: Vec<RawModel>,
}

/// Wire-format mirror of a single model row.
#[derive(Debug, Deserialize)]
struct RawModel {
    model: String,
    context_window: u32,
    #[serde(default)]
    reasoning_supported: bool,
    #[serde(default)]
    reasoning_levels: Vec<String>,
    #[serde(default)]
    responses: bool,
    /// Explicit reasoning passback format; `None` (the serde default) means
    /// "no override — derive from protocol" in `model_reasoning_passback`;
    /// `Some(ReasoningPassback::None)` explicitly disables replay (a model
    /// that rejects replayed reasoning, e.g. Cerebras-style chat providers).
    #[serde(default)]
    reasoning_passback: Option<ReasoningPassback>,
}

/// Parse every bundled TOML file into typed `ProviderEntry` values.
///
/// Individual parse failures are logged and skipped rather than aborting the
/// daemon — a broken entry must never take the process down. The bundled TOML
/// is validated by a unit test (`catalog::tests::bundled_toml_parses`), so a
/// malformed file is caught at `cargo test` time, not in production.
pub(crate) fn load_catalog() -> Vec<ProviderEntry> {
    let mut out = Vec::with_capacity(CATALOG_FILES.len());
    for src in CATALOG_FILES {
        match toml::from_str::<RawProvider>(src) {
            Ok(raw) => out.push(convert(raw)),
            Err(e) => {
                tracing::error!(error = %e, "failed to parse bundled provider catalog entry");
            }
        }
    }
    out
}

/// Convert a raw wire-format provider into the typed runtime entry.
fn convert(raw: RawProvider) -> ProviderEntry {
    let protocol = match raw.protocol.as_str() {
        "anthropic" => ProviderProtocol::AnthropicMessages,
        "google" => ProviderProtocol::GoogleGenerativeAi,
        // Everything else is OpenAI-compatible on the wire. The max-tokens
        // field name differs per gateway; default to `max_completion_tokens`
        // (OpenAI standard) unless the file explicitly says `max_tokens`
        // (opencode / opencode-go / mistral).
        _ => ProviderProtocol::OpenAi {
            max_tokens_field: match raw.max_tokens_field.as_deref() {
                Some("max_tokens") => MaxTokensField::MaxTokens,
                _ => MaxTokensField::MaxCompletionTokens,
            },
        },
    };
    ProviderEntry {
        slug: raw.slug,
        display_name: raw.display_name,
        protocol,
        base_url: raw.base_url,
        default_model: raw.default_model,
        models: raw
            .models
            .into_iter()
            .map(|m| ModelEntry {
                model: m.model,
                context_window: m.context_window,
                reasoning_supported: m.reasoning_supported,
                openai_reasoning_levels: m.reasoning_levels,
                openai_responses: m.responses,
                reasoning_passback: m.reasoning_passback,
            })
            .collect(),
    }
}
