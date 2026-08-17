//! Normalization of the models.dev snapshot into base `ProviderEntry` values.
//!
//! The local models.dev snapshot (`catalog/models.dev.json`, fetched
//! 2026-08-13) is the source of *facts*: provider slugs/names/base URLs and
//! per-model reasoning/context facts. The snapshot is a **gitignored local
//! artifact** — `catalog-gen` fetches a fresh copy from models.dev when it is
//! missing and caches it at that path — so `catalog.bin` is the only
//! committed catalog data file. Policy (protocol selection, per-model passback
//! exceptions, and the providers models.dev does not cover) lives in
//! `models-overlay.toml` and is merged at load time by [`merge_overlay`].
//!
//! The generator binary (`src/bin/catalog-gen.rs`) runs [`normalize_modelsdev`]
//! over the snapshot and postcard-serializes the result into
//! `catalog/catalog.bin`, which the library embeds and deserializes at first
//! load ([`crate::catalog::loader::load_bundled_base`]).
//!
//! Normalization is **deterministic**: providers and models keep the
//! snapshot's JSON object order (models.dev's order is load-bearing — the
//! derived `default_model` is the *first* model id in a provider's models
//! map), so re-running the generator over the same snapshot yields a
//! byte-identical `catalog.bin`.

use indexmap::IndexMap;
use serde::Deserialize;

use super::{ModelEntry, ProviderEntry, ProviderProtocol};
use crate::shared::MaxTokensField;

/// One provider as recorded by the models.dev snapshot.
#[derive(Debug, Deserialize)]
struct RawProvider {
    /// Human-readable provider name (becomes `display_name`).
    name: String,
    /// The AI SDK npm package the provider integrates with. Selects the
    /// derived default wire protocol (see [`derive_protocol`]) and the
    /// derived Responses-API default (only `@ai-sdk/openai` defaults to
    /// Responses; every other OpenAI-compatible provider defaults to Chat
    /// Completions).
    #[serde(default)]
    npm: String,
    /// Base URL for OpenAI-compatible endpoints. Absent for providers whose
    /// API is reached through a non-OpenAI SDK (Anthropic/Google), a gateway,
    /// or a local tool — those keep `String::new()` and the bundled overlay
    /// supplies the real endpoint (the overlay is authoritative for endpoint
    /// policy; models.dev only carries it when the provider publishes one).
    #[serde(default)]
    api: Option<String>,
    /// Per-model facts, keyed by model id, in the snapshot's JSON order.
    #[serde(default)]
    models: IndexMap<String, RawModel>,
}

/// One model as recorded by the models.dev snapshot.
#[derive(Debug, Deserialize)]
struct RawModel {
    /// Whether the model supports reasoning/thinking at all. Absent → `false`
    /// via the serde default, matching "unknown = non-reasoning" until models.dev
    /// says otherwise.
    #[serde(default)]
    reasoning: bool,
    /// Options the API accepts for enabling reasoning. An entry with
    /// `type == "effort"` and a `values` list yields the model's explicit
    /// effort-level slugs; anything else (a plain toggle, `budget_tokens`,
    /// …) means the model has no explicit levels and falls back to the
    /// protocol defaults at lookup time.
    #[serde(default)]
    reasoning_options: Vec<ReasoningOption>,
    /// Token limits; `limit.context` becomes the model's `context_window`
    /// (`0` = unknown, as today). Absent → 0.
    #[serde(default)]
    limit: Option<ModelLimit>,
}

#[derive(Debug, Deserialize)]
struct ReasoningOption {
    #[serde(default, rename = "type")]
    kind: String,
    /// Some snapshots carry a `null` placeholder in the values array (e.g.
    /// Sarvam's `sarvam-30b`); tolerate it by keeping the slot as `None` and
    /// skipping it in [`effort_levels`].
    #[serde(default)]
    values: Vec<Option<String>>,
}

#[derive(Debug, Deserialize)]
struct ModelLimit {
    #[serde(default)]
    context: u32,
}

/// Normalize a models.dev snapshot document into the base provider catalog.
///
/// On a parse failure the snapshot is logged as an error and an empty catalog
/// is returned (a broken embedded snapshot must never take the daemon down —
/// it is caught at `cargo test` time by `embedded_catalog_loads` and at
/// generation time by `catalog-gen`, which refuses to write an empty base).
pub fn normalize_modelsdev(src: &str) -> Vec<ProviderEntry> {
    match serde_json::from_str::<IndexMap<String, RawProvider>>(src) {
        Ok(providers) => providers
            .into_iter()
            .map(|(slug, raw)| normalize_provider(slug, raw))
            .collect(),
        Err(e) => {
            tracing::error!(error = %e, "failed to parse models.dev snapshot");
            Vec::new()
        }
    }
}

/// Normalize a single models.dev provider entry into a base `ProviderEntry`.
fn normalize_provider(slug: String, raw: RawProvider) -> ProviderEntry {
    let protocol = derive_protocol(&raw.npm);
    // Derived default: the FIRST model in the snapshot's JSON order. The
    // bundled overlay may override this per provider where the old TOML
    // picked a different default.
    let default_model = raw.models.keys().next().cloned().unwrap_or_default();
    // Responses-API default: only OpenAI's own SDK defaults to the Responses
    // endpoint; everything else is Chat Completions unless a per-model
    // overlay entry says otherwise.
    let openai_responses = raw.npm == "@ai-sdk/openai";

    let models = raw
        .models
        .into_iter()
        .map(|(model, m)| ModelEntry {
            model,
            context_window: m.limit.as_ref().map(|l| l.context).unwrap_or(0),
            reasoning_supported: m.reasoning,
            openai_reasoning_levels: effort_levels(&m.reasoning_options),
            openai_responses,
            // Derived at lookup time (`model_reasoning_passback`): the base
            // carries no passback policy; per-model exceptions come from the
            // overlay.
            reasoning_passback: None,
            // DeepSeek/Kimi reasoning_content-echo is likewise derived from
            // the model name/family at lookup time, not baked into the base.
            reasoning_content_required: None,
        })
        .collect();

    ProviderEntry {
        slug,
        display_name: raw.name,
        protocol,
        base_url: raw.api.unwrap_or_default(),
        default_model,
        models,
    }
}

/// Map a models.dev `npm` package to the default wire protocol.
///
/// The AI SDK packages are the canonical protocol signal: `@ai-sdk/anthropic`
/// speaks Anthropic Messages, `@ai-sdk/google` speaks Gemini, and every other
/// package (including `@ai-sdk/openai-compatible` and all third-party
/// gateways) is OpenAI-compatible on the wire. The overlay overrides this
/// where a provider's real endpoint differs (Fireworks and Vercel run
/// Anthropic-mode gateways).
fn derive_protocol(npm: &str) -> ProviderProtocol {
    match npm {
        "@ai-sdk/anthropic" => ProviderProtocol::AnthropicMessages,
        "@ai-sdk/google" => ProviderProtocol::GoogleGenerativeAi,
        _ => ProviderProtocol::OpenAi {
            max_tokens_field: MaxTokensField::MaxCompletionTokens,
        },
    }
}

/// Derive the explicit effort-level slugs from a model's `reasoning_options`.
///
/// `["off"]` is prepended (an "off" position is always offered — the old
/// TOML catalog shipped one on every reasoning model), `"none"` maps to
/// `"off"`, and duplicates are dropped preserving order. Models without an
/// `effort` entry get an empty list and fall back to the protocol defaults at
/// lookup time (e.g. Google → `["off","on"]`).
fn effort_levels(options: &[ReasoningOption]) -> Vec<String> {
    let Some(effort) = options
        .iter()
        .find(|o| o.kind == "effort" && !o.values.is_empty())
    else {
        return Vec::new();
    };
    let mut out = vec!["off".to_string()];
    for value in effort.values.iter().flatten() {
        let normalized = if value == "none" { "off" } else { value };
        if !out.iter().any(|v| v == normalized) {
            out.push(normalized.to_string());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared::MaxTokensField;

    const SNAPSHOT: &str = r#"{
        "zai": {
            "name": "Z.AI",
            "npm": "@ai-sdk/openai-compatible",
            "api": "https://api.z.ai/api/paas/v4",
            "models": {
                "glm-5.1": {
                    "reasoning": true,
                    "reasoning_options": [{"type": "effort", "values": ["high", "max"]}],
                    "limit": {"context": 202800, "output": 131072}
                },
                "glm-5": {
                    "reasoning": true,
                    "reasoning_options": [{"type": "toggle"}],
                    "limit": {"context": 202800, "output": 131072}
                }
            }
        },
        "openai": {
            "name": "OpenAI",
            "npm": "@ai-sdk/openai",
            "models": {
                "gpt-5.4": {
                    "reasoning": true,
                    "reasoning_options": [{"type": "effort", "values": ["none", "low", "medium", "high", "xhigh"]}],
                    "limit": {"context": 400000, "output": 131072}
                }
            }
        },
        "anthropic": {
            "name": "Anthropic",
            "npm": "@ai-sdk/anthropic",
            "models": {
                "claude-haiku-4-5": {
                    "reasoning": true,
                    "reasoning_options": [{"type": "toggle"}],
                    "limit": {"context": 200000, "output": 131072}
                }
            }
        },
        "google": {
            "name": "Google Gemini",
            "npm": "@ai-sdk/google",
            "models": {
                "gemini-2.5-pro": {
                    "reasoning": true,
                    "reasoning_options": [{"type": "budget_tokens", "min": 128, "max": 32768}],
                    "limit": {"context": 1048576, "output": 65536}
                }
            }
        },
        "chatty": {
            "name": "Chatty",
            "npm": "@ai-sdk/openai-compatible",
            "api": "https://chatty.example/v1",
            "models": {
                "chatty-1": {
                    "reasoning": false,
                    "limit": {"context": 8192, "output": 4096}
                }
            }
        }
    }"#;

    #[test]
    fn normalizes_providers_in_json_order_with_defaults() {
        let catalog = normalize_modelsdev(SNAPSHOT);
        let slugs: Vec<&str> = catalog.iter().map(|e| e.slug.as_str()).collect();
        assert_eq!(
            slugs,
            vec!["zai", "openai", "anthropic", "google", "chatty"]
        );
    }

    #[test]
    fn derives_protocol_and_responses_defaults() {
        let catalog = normalize_modelsdev(SNAPSHOT);
        let zai = &catalog[0];
        assert!(matches!(
            zai.protocol,
            ProviderProtocol::OpenAi {
                max_tokens_field: MaxTokensField::MaxCompletionTokens
            }
        ));
        // Only @ai-sdk/openai defaults to the Responses API.
        let openai = &catalog[1];
        assert!(openai.models[0].openai_responses);
        assert!(!zai.models[0].openai_responses);
        // Anthropic/Google protocols derive from their npm packages.
        assert!(matches!(
            catalog[2].protocol,
            ProviderProtocol::AnthropicMessages
        ));
        assert!(matches!(
            catalog[3].protocol,
            ProviderProtocol::GoogleGenerativeAi
        ));
    }

    #[test]
    fn default_model_is_first_model_in_json_order() {
        let catalog = normalize_modelsdev(SNAPSHOT);
        assert_eq!(catalog[0].default_model, "glm-5.1");
        assert_eq!(catalog[1].default_model, "gpt-5.4");
    }

    #[test]
    fn effort_levels_prepend_off_and_map_none() {
        let catalog = normalize_modelsdev(SNAPSHOT);
        // ["high","max"] → ["off","high","max"]
        assert_eq!(
            catalog[0].models[0].openai_reasoning_levels,
            vec!["off", "high", "max"]
        );
        // ["none","low","medium","high","xhigh"] → "none" maps to "off"
        assert_eq!(
            catalog[1].models[0].openai_reasoning_levels,
            vec!["off", "low", "medium", "high", "xhigh"]
        );
        // Toggle / budget_tokens options carry no explicit levels.
        assert!(catalog[2].models[0].openai_reasoning_levels.is_empty());
        assert!(catalog[3].models[0].openai_reasoning_levels.is_empty());
    }

    #[test]
    fn absent_api_and_limits_leave_empty_facts() {
        let catalog = normalize_modelsdev(SNAPSHOT);
        // anthropic has no `api` → empty base_url (the overlay supplies it).
        assert_eq!(catalog[2].base_url, "");
        // context window comes from limit.context.
        assert_eq!(catalog[2].models[0].context_window, 200_000);
        // reasoning=false model has no levels.
        assert!(!catalog[4].models[0].reasoning_supported);
        assert!(catalog[4].models[0].openai_reasoning_levels.is_empty());
    }

    #[test]
    fn malformed_snapshot_yields_empty_catalog() {
        assert!(normalize_modelsdev("not json").is_empty());
    }
}
