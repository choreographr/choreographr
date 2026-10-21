//! Normalization of the models.dev snapshot into base `ProviderEntry` values.
//!
//! The local models.dev snapshot (`catalog/models.dev.json`, fetched
//! 2026-08-13) is the source of *facts*: provider slugs/names/base URLs and
//! per-model reasoning/context/cost facts. The snapshot is a **gitignored local
//! artifact** — `catalog-gen` fetches a fresh copy from models.dev when it is
//! missing and caches it at that path — so `catalog.bin` is the only
//! committed catalog data file. Policy (protocol selection, per-model passback
//! exceptions, the prompt-cache TTL, and the providers models.dev does not
//! cover) lives in `models-overlay.toml` and is merged at load time by
//! [`merge_overlay`].
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
use tracing::debug;

use super::{ModelCost, ModelEntry, ProviderEntry, ProviderProtocol};
use crate::retry::RequestCompression;
use crate::shared::MaxTokensField;
use choreo_proto::ChatReasoningField;

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
    /// How the model interleaves reasoning with tool calls. The snapshot
    /// encodes it three ways (see [`RawInterleaved`]): an object carrying the
    /// echo field name, a plain-string shorthand for the field name, or a
    /// bare `true` meaning "interleaved thinking supported" with NO field
    /// fact. Normalized to the field name, if any.
    #[serde(default)]
    interleaved: Option<RawInterleaved>,
    /// Whether the model accepts the `temperature` request parameter.
    /// Reasoning-first models (o-series, GPT-5.x, several Kimi/GLM lanes)
    /// reject it. Absent → `true` (models.dev only encodes the fact when the
    /// answer is interesting; the wire default stays permissive).
    #[serde(default)]
    temperature: Option<bool>,
    /// Lifecycle status straight from the snapshot (e.g.
    /// `"deprecated"`). Only the deprecated value is lifted into the
    /// catalog today; everything else is treated as "no fact".
    #[serde(default)]
    status: Option<String>,
    /// Input/output modalities (`{"input": ["text", "image"], "output": ["text"]}`).
    /// A model is vision-capable when `"image"` appears in `modalities.input`;
    /// absent → treated as text-only. This is the source of truth for
    /// [`ModelEntry::supports_vision`], and the `output` side feeds
    /// [`ModelEntry::supports_image_output`] the same way.
    #[serde(default)]
    modalities: Option<RawModalities>,
    /// Token prices, USD per million tokens. Absent → `None` (the model records
    /// no cost). The snapshot's `cost` object carries nested/aux keys
    /// (`tiers`, `context_over_200k`, `input_audio`, `output_audio`,
    /// `reasoning`) that this catalog does not model; serde ignores unknown
    /// fields, so only the four flat prices below are ingested.
    #[serde(default)]
    cost: Option<RawCost>,
}

/// One model's `cost` object as recorded by models.dev, restricted to the four
/// flat per-token prices this catalog models. `input`/`output` are present
/// whenever the object exists; `cache_read`/`cache_write` are frequently absent
/// (many OpenAI-compatible models record only the first three) → `Option`.
#[derive(Debug, Deserialize)]
struct RawCost {
    #[serde(default)]
    input: f64,
    #[serde(default)]
    output: f64,
    #[serde(default)]
    cache_read: Option<f64>,
    #[serde(default)]
    cache_write: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct RawModalities {
    #[serde(default)]
    input: Vec<String>,
    /// Output modalities the model can *produce*. Absent in older snapshot
    /// entries → empty → no image-output fact, the same safe-default pattern
    /// as `input`.
    #[serde(default)]
    output: Vec<String>,
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
    /// Maximum output tokens the model can produce (`0` = unknown — the
    /// fact is simply not recorded; lookups treat 0 like context's 0).
    #[serde(default)]
    output: u32,
}

/// The snapshot's `interleaved` value in any of its encodings.
///
/// The schema spells it `true | { field: "reasoning_content" | "reasoning_details" }`
/// ([schema](https://github.com/anomalyco/models.dev/blob/dev/packages/core/src/schema.ts)),
/// so serde tries each untagged variant in order: an object carries the field
/// name, and `true` — "supports interleaved thinking", the only other legal
/// form — matches only the bool variant, so it can never be mistaken for a
/// field name.
///
/// The plain-string variant is a **tolerance**, not a documented shape: a
/// string is read as the field name so a hand-written or newer snapshot cannot
/// fail the whole parse, which would otherwise cost the entire embedded catalog
/// over one forgotten wrapper object. It records the same fact an object does.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawInterleaved {
    /// `{ "field": "reasoning_content" }` — the echo field the model demands.
    Field { field: String },
    /// A plain string read as the field name (tolerance beyond the schema).
    Shorthand(String),
    /// `true` — interleaved thinking supported, but no echo-field fact. The
    /// payload is intentionally ignored (a capability flag, not a fact).
    Just(#[expect(dead_code)] bool),
}

impl RawInterleaved {
    /// The echo field name the snapshot records, if this encoding carries
    /// one (`true` does not — it is a capability flag, not a fact about
    /// which request field the upstream demands).
    fn field(&self) -> Option<&str> {
        match self {
            RawInterleaved::Field { field } | RawInterleaved::Shorthand(field) => Some(field),
            RawInterleaved::Just(_) => None,
        }
    }
}

/// Map a models.dev `interleaved` field name onto the catalog's echo field.
///
/// The four chat fields this catalog can replay are facts: the three scalar
/// fields (`reasoning_content`, `reasoning`, `reasoning_text`) and the structured
/// `reasoning_details` array. The snapshot schema itself only permits
/// `reasoning_content` and `reasoning_details`
/// ([schema](https://github.com/anomalyco/models.dev/blob/dev/packages/core/src/schema.ts));
/// the two scalar aliases are accepted alongside them because an overlay can
/// pin either. Any unknown name is deliberately "no fact" rather
/// than a guess: an unrecognised name is a snapshot vocabulary this catalog
/// does not model, and guessing would be unauditable — the requirement decides
/// which wire key every assistant message carries, so an unknown name must
/// degrade to "no declared field" (and an upstream 400) rather than to a silent
/// wrong key.
fn echo_field(name: &str) -> Option<ChatReasoningField> {
    match name {
        "reasoning_content" => Some(ChatReasoningField::ReasoningContent),
        "reasoning" => Some(ChatReasoningField::Reasoning),
        "reasoning_text" => Some(ChatReasoningField::ReasoningText),
        "reasoning_details" => Some(ChatReasoningField::ReasoningDetails),
        other => {
            debug!(interleaved_field = %other, "models.dev interleaved field is not a chat echo fact; ignoring");
            None
        }
    }
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
    // bundled overlay overrides this per provider where a different default is
    // wanted.
    let default_model = raw.models.keys().next().cloned().unwrap_or_default();
    // Responses-API default: only OpenAI's own SDK defaults to the Responses
    // endpoint; everything else is Chat Completions unless a per-model
    // overlay entry says otherwise.
    let openai_responses = raw.npm == "@ai-sdk/openai";

    let models = raw
        .models
        .into_iter()
        .map(|(model, m)| {
            // Reasoning echo: TWO facts from the same snapshot signal, and the
            // asymmetry between them is deliberate. The model's `interleaved`
            // value names a replayable chat field (`reasoning_echo`), and the
            // field NAME is preserved (never collapsed to a boolean) because
            // the adapter needs it to pick the wire key. Any other encoding
            // (bare `true`, absent, an unknown field) is "no declared field"
            // → `None`.
            let echo_field = m
                .interleaved
                .as_ref()
                .and_then(RawInterleaved::field)
                .and_then(echo_field);
            // Whether the field must also be PRESENT on every assistant
            // message (`reasoning_echo_required`) depends on WHICH field it is:
            // a scalar name (`reasoning_content`, `reasoning`, `reasoning_text`)
            // means "the upstream demands this key echoed" — DeepSeek returns a
            // 400 for a request carrying `tools` whose earlier turns omit it
            // (https://api-docs.deepseek.com/guides/thinking_mode), and GLM asks
            // for the blocks back unmodified
            // (https://docs.z.ai/guides/capabilities/thinking-mode) — so a
            // scalar sets `Some(true)`. The structured `reasoning_details` array
            // is a DESTINATION fact only: no upstream is known to reject a
            // history that omits it (OpenRouter only asks that a replayed
            // sequence be preserved, not that one be present —
            // https://openrouter.ai/docs/guides/best-practices/reasoning-tokens#preserving-reasoning),
            // and inventing an empty `reasoning_details: []` on every assistant
            // message sends an unverified array to gateways that are strict
            // about its contents, so that case leaves the requirement unset.
            // Presence stays opt-in per route, and the overlay can pin either
            // half independently.
            let echo_required = match echo_field {
                Some(ChatReasoningField::ReasoningDetails) => None,
                Some(_) => Some(true),
                None => None,
            };
            ModelEntry {
                model,
                context_window: m.limit.as_ref().map_or(0, |l| l.context),
                reasoning_supported: m.reasoning,
                openai_reasoning_levels: effort_levels(&m.reasoning_options),
                openai_responses,
                // Derived at lookup time (`model_reasoning_passback`): the base
                // carries no passback policy; per-model exceptions come from the
                // overlay.
                reasoning_passback: None,
                reasoning_echo: echo_field,
                reasoning_echo_required: echo_required,
                // Output-token ceiling (`0` = unknown, mirroring context_window).
                max_output_tokens: m.limit.as_ref().map_or(0, |l| l.output),
                // Temperature acceptance: absent means models.dev records no
                // constraint, so the permissive wire default stands.
                supports_temperature: m.temperature.unwrap_or(true),
                // Lifecycle status: only "deprecated" is lifted into the base.
                deprecated: m.status.as_deref() == Some("deprecated"),
                // Vision: `"image"` in `modalities.input`. Absent modalities
                // (older snapshot entries) default to text-only.
                supports_vision: m
                    .modalities
                    .as_ref()
                    .is_some_and(|mods| mods.input.iter().any(|m| m == "image")),
                // Image OUTPUT: same safe-default pattern, on the `output` side
                // of the modalities pair. `"image"` there means the model can
                // *produce* images (gpt-image-1, Gemini image models, …);
                // absent modalities → false (text-generation only).
                supports_image_output: m
                    .modalities
                    .as_ref()
                    .is_some_and(|mods| mods.output.iter().any(|m| m == "image")),
                // Image EDIT: overlay-only policy. models.dev records no "edit"
                // modality, so the base never sets this — the overlay pins it for
                // the models whose provider exposes an edit endpoint. Kept
                // explicit (not `..Default`) so a future fact can be added here
                // deliberately rather than silently inheriting false.
                supports_image_edit: false,
                // Token prices (USD per million tokens): a fact from the snapshot's
                // `cost` object when present; `None` when the model records no cost.
                // Only the four flat prices are modelled — the aux/nested cost keys
                // are out of scope (see RawCost).
                cost: m.cost.map(|c| ModelCost {
                    input: c.input,
                    output: c.output,
                    cache_read: c.cache_read,
                    cache_write: c.cache_write,
                }),
                // Prompt-cache TTL is overlay policy, never a models.dev fact.
                prompt_cache: None,
            }
        })
        .collect();

    ProviderEntry {
        slug,
        display_name: raw.name,
        protocol,
        base_url: raw.api.unwrap_or_default(),
        default_model,
        models,
        // Provider-level prompt-cache TTL is overlay policy, never a base
        // fact.
        prompt_cache: None,
        // Request compression is overlay policy, never a base fact — models.dev
        // carries nothing about request encodings, so every provider starts on
        // the identity default.
        request_compression: RequestCompression::Identity,
        // OAuth profiles are overlay policy, never a base fact — models.dev
        // carries nothing about authorization servers.
        oauth: None,
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
                    "limit": {"context": 202800, "output": 131072},
                    "cost": {"input": 0.6, "output": 2.2, "cache_read": 0.11, "cache_write": 0.3, "tiers": [], "reasoning": 0.9}
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
                    "limit": {"context": 400000, "output": 131072},
                    "modalities": {"input": ["text", "image"], "output": ["text"]},
                    "cost": {"input": 1.25, "output": 10.0, "cache_read": 0.125}
                },
                "gpt-image-1": {
                    "limit": {"context": 128000, "output": 4096},
                    "modalities": {"input": ["text"], "output": ["image"]}
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
        },
        "features": {
            "name": "Features",
            "npm": "@ai-sdk/openai-compatible",
            "api": "https://features.example/v1",
            "models": {
                "echo-field": {
                    "reasoning": true,
                    "interleaved": {"field": "reasoning_content"},
                    "limit": {"context": 100000, "output": 32000}
                },
                "shorthand-other-field": {
                    "reasoning": true,
                    "interleaved": "reasoning"
                },
                "structured-details-field": {
                    "reasoning": true,
                    "interleaved": {"field": "reasoning_details"}
                },
                "unknown-field": {
                    "reasoning": true,
                    "interleaved": {"field": "vendor_reasoning"}
                },
                "bare-true": {
                    "reasoning": true,
                    "interleaved": true
                },
                "no-facts": {
                    "reasoning": true
                },
                "frozen-no-temp": {
                    "reasoning": true,
                    "temperature": false,
                    "status": "deprecated"
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
            vec!["zai", "openai", "anthropic", "google", "chatty", "features"]
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
        assert_eq!(
            catalog[2].models[0].openai_reasoning_levels,
            [] as [String; 0]
        );
        assert_eq!(
            catalog[3].models[0].openai_reasoning_levels,
            [] as [String; 0]
        );
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
        assert_eq!(
            catalog[4].models[0].openai_reasoning_levels,
            [] as [String; 0]
        );
    }

    #[test]
    fn derives_vision_from_modalities_input() {
        let catalog = normalize_modelsdev(SNAPSHOT);
        // gpt-5.4 declares image in input modalities → vision.
        assert!(catalog[1].models[0].supports_vision);
        // glm-5 (text only) and chatty-1 (no modalities field) → not vision.
        assert!(!catalog[0].models[0].supports_vision);
        assert!(!catalog[4].models[0].supports_vision);
    }

    #[test]
    fn derives_image_output_from_modalities_output() {
        let catalog = normalize_modelsdev(SNAPSHOT);
        // gpt-image-1 declares image in OUTPUT modalities → image generation.
        assert!(catalog[1].models[1].supports_image_output);
        assert!(!catalog[1].models[1].supports_vision);
        // gpt-5.4 declares image INPUT only — that must not leak into the
        // output flag (input side and output side are independent facts).
        assert!(!catalog[1].models[0].supports_image_output);
        // glm-5 (no output modality recorded) and chatty-1 (no modalities
        // field at all) → false, the safe default.
        assert!(!catalog[0].models[0].supports_image_output);
        assert!(!catalog[4].models[0].supports_image_output);
    }

    #[test]
    fn malformed_snapshot_yields_empty_catalog() {
        assert!(normalize_modelsdev("not json").is_empty());
    }

    #[test]
    fn interleaved_names_the_echo_field() {
        let catalog = normalize_modelsdev(SNAPSHOT);
        let features = &catalog[5];
        let m = &features.models[0];
        assert_eq!(m.model, "echo-field");
        // The field NAME survives ingestion: the adapter needs it to pick the
        // wire key, so it is not collapsed to a boolean. A scalar name also
        // means the upstream demands the key echoed.
        assert_eq!(m.reasoning_echo, Some(ChatReasoningField::ReasoningContent));
        assert_eq!(m.reasoning_echo_required, Some(true));
        // limit.output is lifted into max_output_tokens.
        assert_eq!(m.max_output_tokens, 32_000);
    }

    #[test]
    fn interleaved_string_shorthand_is_a_field_fact() {
        let catalog = normalize_modelsdev(SNAPSHOT);
        // A plain string IS the field name, for any of the three scalar fields
        // — not only `reasoning_content`. A scalar always carries the presence
        // requirement with it.
        assert_eq!(
            catalog[5].models[1].reasoning_echo,
            Some(ChatReasoningField::Reasoning)
        );
        assert_eq!(catalog[5].models[1].reasoning_echo_required, Some(true));
    }

    #[test]
    fn interleaved_reasoning_details_is_a_structured_echo_fact() {
        let catalog = normalize_modelsdev(SNAPSHOT);
        let features = &catalog[5];
        // `reasoning_details` is a structured array, but the catalog records it
        // as the declared echo field so the adapter can replay it structurally
        // (the bytes are a JSON array, not text). It is a DESTINATION only: no
        // presence requirement is derived for the structured array, because
        // injecting an unverified empty array into every assistant message is
        // worse than omitting the key.
        assert_eq!(
            features.models[2].reasoning_echo,
            Some(ChatReasoningField::ReasoningDetails)
        );
        assert_eq!(features.models[2].reasoning_echo_required, None);
        // An unknown field name stays unknown (no guessing).
        assert_eq!(features.models[3].reasoning_echo, None);
        assert_eq!(features.models[3].reasoning_echo_required, None);
        // Bare `true` is a capability flag, not a field fact.
        assert_eq!(features.models[4].reasoning_echo, None);
        assert_eq!(features.models[4].reasoning_echo_required, None);
        // Absent interleaved → None (never Some(false); None means unknown).
        assert_eq!(features.models[5].reasoning_echo, None);
        assert_eq!(features.models[5].reasoning_echo_required, None);
    }

    #[test]
    fn temperature_and_status_are_lifted() {
        let catalog = normalize_modelsdev(SNAPSHOT);
        let features = &catalog[5];
        // temperature:false → supports_temperature false; status
        // "deprecated" → deprecated true.
        let frozen = &features.models[6];
        assert!(!frozen.supports_temperature);
        assert!(frozen.deprecated);
        // Absent temperature → permissive true; absent status → not deprecated.
        let plain = &features.models[0];
        assert!(plain.supports_temperature);
        assert!(!plain.deprecated);
    }

    #[test]
    fn absent_limit_yields_zero_max_output_tokens() {
        let catalog = normalize_modelsdev(SNAPSHOT);
        // "no-facts" carries no limit at all → 0 (= unknown).
        assert_eq!(catalog[5].models[3].max_output_tokens, 0);
    }

    #[test]
    fn ingests_model_cost_and_partial_cache_fields() {
        let catalog = normalize_modelsdev(SNAPSHOT);
        // Full cost object: all four prices are ingested, and the aux/nested
        // keys (`tiers`, `reasoning`) are ignored without error.
        let glm = catalog[0].models[0].cost.expect("glm-5.1 records a cost");
        assert_eq!(glm.input, 0.6);
        assert_eq!(glm.output, 2.2);
        assert_eq!(glm.cache_read, Some(0.11));
        assert_eq!(glm.cache_write, Some(0.3));
        // Partial cost object (OpenAI-style: input/output/cache_read only) →
        // the absent cache_write stays None.
        let gpt = catalog[1].models[0].cost.expect("gpt-5.4 records a cost");
        assert_eq!(gpt.input, 1.25);
        assert_eq!(gpt.output, 10.0);
        assert_eq!(gpt.cache_read, Some(0.125));
        assert_eq!(gpt.cache_write, None);
    }

    #[test]
    fn model_without_cost_is_none() {
        let catalog = normalize_modelsdev(SNAPSHOT);
        // gpt-image-1 records no cost object → None (never a zero-price fact).
        assert_eq!(catalog[1].models[1].cost, None);
    }
}
