//! Overlay parsing and merging over the normalized models.dev base.
//!
//! The overlay is a TOML document with two layers that share one schema (see
//! `catalog/models-overlay.toml` for the bundled layer; S4 adds a user layer
//! on top):
//!
//! ```toml
//! [provider.deepseek]
//! protocol = "openai"                  # openai | anthropic | google
//! max_tokens_field = "max_completion_tokens"   # or "max_tokens"
//! base_url = "https://api.deepseek.com"
//! default_model = "deepseek-v4-pro"
//! display_name = "DeepSeek"            # only needed for new providers
//!
//! [provider.opencode.models."gpt-5.4"]
//! responses = true
//! reasoning_passback = "response_id"
//! ```
//!
//! Merge semantics, lowest → highest wins:
//!
//! - **Provider scalars** (`protocol`, `max_tokens_field`, `base_url`,
//!   `default_model`, `display_name`): field-wise replace; omitted fields
//!   fall through to the base.
//! - **Per-model entries** keyed `(provider slug, model id)`: naming a model
//!   replaces that entry's fields with the overlay's values (field-wise onto
//!   the base entry, or onto a default entry when the model is new — "new
//!   keys add"). The models.dev base stays authoritative for every fact the
//!   overlay does not mention (context windows, reasoning support, levels),
//!   so a partial override like the Anthropic `tool_loop` passback pin never
//!   destroys the model's other facts.
//! - **New providers** (models.dev does not cover them) are defined
//!   wholesale: a `[provider.<slug>]` table that does not exist in the base
//!   creates the provider, and its `models` sub-table fills in the entries.
//!
//! Unknown keys at any level are **warned and skipped**, never fatal — a
//! user overlay typo must not brick the daemon.

use tracing::{debug, warn};

use super::{ModelEntry, ProviderEntry, ProviderProtocol, ReasoningPassback};
use crate::shared::MaxTokensField;

/// Merge an overlay TOML document over a base catalog, returning a new
/// catalog. The base is never mutated (S4 builds a fresh merged `Vec` per
/// overlay change and calls `replace_catalog` with it).
///
/// A document that is not TOML, or lacks a `provider` table, logs a warning
/// and returns the base unchanged.
pub fn merge_overlay(base: &[ProviderEntry], overlay_src: &str) -> Vec<ProviderEntry> {
    let doc: toml::Value = match toml::from_str(overlay_src) {
        Ok(doc) => doc,
        Err(e) => {
            warn!(error = %e, "failed to parse overlay document; ignoring it");
            return base.to_vec();
        }
    };
    let Some(providers) = doc.get("provider").and_then(toml::Value::as_table) else {
        warn!("overlay document has no [provider] table; ignoring it");
        return base.to_vec();
    };

    let mut merged: Vec<ProviderEntry> = base.to_vec();
    for (slug, value) in providers {
        let Some(table) = value.as_table() else {
            warn!(slug, "overlay provider is not a table; skipping");
            continue;
        };
        match merged.iter_mut().find(|e| e.slug == *slug) {
            Some(entry) => {
                debug!(slug, "overlay: applying provider overrides");
                apply_provider_overlay(entry, table);
            }
            None => {
                debug!(slug, "overlay: adding new provider");
                let mut entry = ProviderEntry {
                    slug: slug.clone(),
                    display_name: slug.clone(),
                    protocol: ProviderProtocol::OpenAi {
                        max_tokens_field: MaxTokensField::MaxCompletionTokens,
                    },
                    base_url: String::new(),
                    default_model: String::new(),
                    models: Vec::new(),
                };
                apply_provider_overlay(&mut entry, table);
                merged.push(entry);
            }
        }
    }
    merged
}

/// Apply a `[provider.<slug>]` overlay table onto an existing entry.
/// Recognized scalar keys are field-wise replaced; the `models` sub-table is
/// applied per model; anything else is warned and skipped.
fn apply_provider_overlay(entry: &mut ProviderEntry, table: &toml::Table) {
    for (key, value) in table {
        match key.as_str() {
            "protocol" => match parse_protocol(value) {
                Some(protocol) => entry.protocol = protocol,
                None => warn!(slug = %entry.slug, "overlay: unknown protocol; skipping"),
            },
            "max_tokens_field" => match parse_max_tokens_field(value) {
                Some(field) => set_max_tokens_field(entry, field),
                None => warn!(slug = %entry.slug, "overlay: unknown max_tokens_field; skipping"),
            },
            "base_url" => match value.as_str() {
                Some(url) => entry.base_url = url.to_string(),
                None => warn!(slug = %entry.slug, "overlay: base_url is not a string; skipping"),
            },
            "default_model" => match value.as_str() {
                Some(model) => entry.default_model = model.to_string(),
                None => {
                    warn!(slug = %entry.slug, "overlay: default_model is not a string; skipping")
                }
            },
            "display_name" => match value.as_str() {
                Some(name) => entry.display_name = name.to_string(),
                None => {
                    warn!(slug = %entry.slug, "overlay: display_name is not a string; skipping")
                }
            },
            "models" => apply_models_overlay(entry, value),
            other => warn!(
                slug = %entry.slug,
                key = other,
                "overlay: unknown provider key; skipping",
            ),
        }
    }
}

/// Apply the `[provider.<slug>.models]` sub-table. Each model id names an
/// entry that is replaced field-wise (or created when new).
fn apply_models_overlay(entry: &mut ProviderEntry, value: &toml::Value) {
    let Some(models) = value.as_table() else {
        warn!(slug = %entry.slug, "overlay: models is not a table; skipping");
        return;
    };
    for (model_id, model_value) in models {
        let Some(table) = model_value.as_table() else {
            warn!(
                slug = %entry.slug,
                model = model_id,
                "overlay: model entry is not a table; skipping",
            );
            continue;
        };
        match entry.models.iter_mut().find(|m| m.model == *model_id) {
            Some(model) => apply_model_overlay(&mut *model, table),
            None => {
                let mut model = ModelEntry {
                    model: model_id.clone(),
                    context_window: 0,
                    reasoning_supported: false,
                    openai_reasoning_levels: Vec::new(),
                    openai_responses: false,
                    reasoning_passback: None,
                };
                apply_model_overlay(&mut model, table);
                entry.models.push(model);
            }
        }
    }
}

/// Apply a single model overlay table onto a model entry, field-wise.
fn apply_model_overlay(model: &mut ModelEntry, table: &toml::Table) {
    for (key, value) in table {
        match key.as_str() {
            "context_window" => match value.as_integer() {
                Some(n) => model.context_window = n.max(0) as u32,
                None => warn!(
                    model = %model.model,
                    "overlay: context_window is not an integer; skipping",
                ),
            },
            "reasoning_supported" => match value.as_bool() {
                Some(b) => model.reasoning_supported = b,
                None => warn!(
                    model = %model.model,
                    "overlay: reasoning_supported is not a bool; skipping",
                ),
            },
            "reasoning_levels" => match value.as_array() {
                Some(levels) => model.openai_reasoning_levels = parse_string_array(levels),
                None => warn!(
                    model = %model.model,
                    "overlay: reasoning_levels is not an array; skipping",
                ),
            },
            "responses" => match value.as_bool() {
                Some(b) => model.openai_responses = b,
                None => warn!(
                    model = %model.model,
                    "overlay: responses is not a bool; skipping",
                ),
            },
            "reasoning_passback" => match value.as_str().and_then(parse_passback) {
                Some(passback) => model.reasoning_passback = Some(passback),
                None => warn!(
                    model = %model.model,
                    "overlay: unknown reasoning_passback; skipping",
                ),
            },
            other => warn!(
                model = %model.model,
                key = other,
                "overlay: unknown model key; skipping",
            ),
        }
    }
}

/// Parse the `protocol` scalar ("openai" | "anthropic" | "google").
fn parse_protocol(value: &toml::Value) -> Option<ProviderProtocol> {
    match value.as_str()? {
        "openai" => Some(ProviderProtocol::OpenAi {
            max_tokens_field: MaxTokensField::MaxCompletionTokens,
        }),
        "anthropic" => Some(ProviderProtocol::AnthropicMessages),
        "google" => Some(ProviderProtocol::GoogleGenerativeAi),
        _ => None,
    }
}

/// Parse the `max_tokens_field` scalar.
fn parse_max_tokens_field(value: &toml::Value) -> Option<MaxTokensField> {
    match value.as_str()? {
        "max_tokens" => Some(MaxTokensField::MaxTokens),
        "max_completion_tokens" => Some(MaxTokensField::MaxCompletionTokens),
        _ => None,
    }
}

/// Set the max-tokens field on an OpenAI-protocol entry. Non-OpenAI protocols
/// have no such field, so setting one is a warning (the overlay author likely
/// mixed policy from another provider into this one).
fn set_max_tokens_field(entry: &mut ProviderEntry, field: MaxTokensField) {
    match &mut entry.protocol {
        ProviderProtocol::OpenAi { max_tokens_field } => *max_tokens_field = field,
        other => warn!(
            slug = %entry.slug,
            protocol = %other,
            "overlay: max_tokens_field ignored for a non-OpenAI protocol",
        ),
    }
}

/// Parse a `reasoning_passback` scalar into the typed enum.
fn parse_passback(value: &str) -> Option<ReasoningPassback> {
    match value {
        "none" => Some(ReasoningPassback::None),
        "tool_loop" => Some(ReasoningPassback::ToolLoop),
        "all_turns" => Some(ReasoningPassback::AllTurns),
        "signature" => Some(ReasoningPassback::Signature),
        "response_id" => Some(ReasoningPassback::ResponseId),
        _ => None,
    }
}

/// Convert a TOML array of strings into a `Vec<String>` (silently dropping
/// non-string entries — a malformed level slug is worse than a missing one).
fn parse_string_array(array: &[toml::Value]) -> Vec<String> {
    array
        .iter()
        .filter_map(toml::Value::as_str)
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal one-provider base for the merge tests.
    fn base() -> Vec<ProviderEntry> {
        vec![
            ProviderEntry {
                slug: "acme".into(),
                display_name: "Acme".into(),
                protocol: ProviderProtocol::OpenAi {
                    max_tokens_field: MaxTokensField::MaxCompletionTokens,
                },
                base_url: "https://api.acme.dev/v1".into(),
                default_model: "acme-base".into(),
                models: vec![
                    ModelEntry {
                        model: "acme-base".into(),
                        context_window: 8192,
                        reasoning_supported: true,
                        openai_reasoning_levels: vec!["off".into(), "high".into()],
                        openai_responses: false,
                        reasoning_passback: None,
                    },
                    ModelEntry {
                        model: "acme-lite".into(),
                        context_window: 4096,
                        reasoning_supported: false,
                        openai_reasoning_levels: Vec::new(),
                        openai_responses: false,
                        reasoning_passback: None,
                    },
                ],
            },
            ProviderEntry {
                slug: "zoocorp".into(),
                display_name: "Zoo Corp".into(),
                protocol: ProviderProtocol::AnthropicMessages,
                base_url: "https://api.zoocorp.dev".into(),
                default_model: "zoo-1".into(),
                models: vec![ModelEntry {
                    model: "zoo-1".into(),
                    context_window: 200_000,
                    reasoning_supported: true,
                    openai_reasoning_levels: Vec::new(),
                    openai_responses: false,
                    reasoning_passback: None,
                }],
            },
        ]
    }

    #[test]
    fn provider_scalars_are_field_wise_replaced() {
        let merged = merge_overlay(
            &base(),
            r#"
[provider.acme]
base_url = "https://overridden.acme.dev/v1"
default_model = "acme-lite"
max_tokens_field = "max_tokens"
"#,
        );
        let acme = merged.iter().find(|e| e.slug == "acme").expect("acme");
        assert_eq!(acme.base_url, "https://overridden.acme.dev/v1");
        assert_eq!(acme.default_model, "acme-lite");
        assert!(matches!(
            acme.protocol,
            ProviderProtocol::OpenAi {
                max_tokens_field: MaxTokensField::MaxTokens
            }
        ));
        // Unmentioned fields fall through.
        assert_eq!(acme.display_name, "Acme");
        // zoocorp untouched.
        let zoo = merged
            .iter()
            .find(|e| e.slug == "zoocorp")
            .expect("zoocorp");
        assert_eq!(zoo.base_url, "https://api.zoocorp.dev");
    }

    #[test]
    fn model_named_in_overlay_replaces_that_entry() {
        // A fully-specified overlay model table replaces the base entry's
        // values wholesale (every field it names wins).
        let merged = merge_overlay(
            &base(),
            r#"
[provider.acme.models."acme-lite"]
context_window = 65536
reasoning_supported = true
reasoning_levels = ["off", "low", "medium", "high"]
responses = true
reasoning_passback = "response_id"
"#,
        );
        let acme = merged.iter().find(|e| e.slug == "acme").expect("acme");
        let lite = acme
            .models
            .iter()
            .find(|m| m.model == "acme-lite")
            .expect("acme-lite");
        assert_eq!(lite.context_window, 65536);
        assert!(lite.reasoning_supported);
        assert_eq!(
            lite.openai_reasoning_levels,
            vec!["off", "low", "medium", "high"]
        );
        assert!(lite.openai_responses);
        assert_eq!(lite.reasoning_passback, Some(ReasoningPassback::ResponseId));
        // The base entry that was not named is untouched.
        let base_model = acme
            .models
            .iter()
            .find(|m| m.model == "acme-base")
            .expect("acme-base");
        assert_eq!(base_model.context_window, 8192);
    }

    #[test]
    fn partial_model_override_keeps_base_facts() {
        // A partial overlay (only the passback) must not destroy the base's
        // other facts — this is what the Anthropic tool_loop pins rely on.
        let merged = merge_overlay(
            &base(),
            r#"
[provider.acme.models."acme-base"]
reasoning_passback = "tool_loop"
"#,
        );
        let acme = merged.iter().find(|e| e.slug == "acme").expect("acme");
        let base_model = acme
            .models
            .iter()
            .find(|m| m.model == "acme-base")
            .expect("acme-base");
        assert_eq!(
            base_model.reasoning_passback,
            Some(ReasoningPassback::ToolLoop)
        );
        assert_eq!(base_model.context_window, 8192);
        assert!(base_model.reasoning_supported);
        assert_eq!(base_model.openai_reasoning_levels, vec!["off", "high"]);
    }

    #[test]
    fn new_provider_is_added_wholesale() {
        let merged = merge_overlay(
            &base(),
            r#"
[provider.ollama]
display_name = "Ollama (Local)"
protocol = "openai"
base_url = "http://localhost:11434/v1"
default_model = "llama3.1"

[provider.ollama.models."llama3.1"]
context_window = 131072
reasoning_supported = false
reasoning_levels = []
responses = false
reasoning_passback = "none"
"#,
        );
        let ollama = merged
            .iter()
            .find(|e| e.slug == "ollama")
            .expect("ollama added");
        assert_eq!(ollama.display_name, "Ollama (Local)");
        assert_eq!(ollama.base_url, "http://localhost:11434/v1");
        assert_eq!(ollama.default_model, "llama3.1");
        assert_eq!(ollama.models.len(), 1);
        assert_eq!(ollama.models[0].model, "llama3.1");
        assert_eq!(ollama.models[0].context_window, 131072);
        assert_eq!(
            ollama.models[0].reasoning_passback,
            Some(ReasoningPassback::None)
        );
        // Base providers still present.
        assert_eq!(merged.len(), 3);
    }

    #[test]
    fn new_model_on_existing_provider_is_added() {
        let merged = merge_overlay(
            &base(),
            r#"
[provider.acme.models."brand-new"]
context_window = 100000
reasoning_supported = true
"#,
        );
        let acme = merged.iter().find(|e| e.slug == "acme").expect("acme");
        let new_model = acme
            .models
            .iter()
            .find(|m| m.model == "brand-new")
            .expect("brand-new added");
        assert_eq!(new_model.context_window, 100000);
        assert!(new_model.reasoning_supported);
        assert_eq!(acme.models.len(), 3);
    }

    #[test]
    fn unknown_keys_warn_and_are_skipped() {
        // None of these are fatal; the rest of the overlay still applies.
        let merged = merge_overlay(
            &base(),
            r#"
[provider.acme]
bogus_provider_key = 42

[provider.acme.models."acme-base"]
bogus_model_key = true
reasoning_passback = "signature"
"#,
        );
        let acme = merged.iter().find(|e| e.slug == "acme").expect("acme");
        // Unknown keys were skipped; the valid one applied.
        let base_model = acme
            .models
            .iter()
            .find(|m| m.model == "acme-base")
            .expect("acme-base");
        assert_eq!(
            base_model.reasoning_passback,
            Some(ReasoningPassback::Signature)
        );
        assert_eq!(acme.base_url, "https://api.acme.dev/v1");
    }

    #[test]
    fn malformed_overlay_returns_base_unchanged() {
        let merged = merge_overlay(&base(), "not [[ valid toml");
        assert_eq!(merged.len(), base().len());
        assert_eq!(merged[0].slug, "acme");
    }

    #[test]
    fn empty_overlay_returns_base_unchanged() {
        let merged = merge_overlay(&base(), "");
        assert_eq!(merged.len(), base().len());
    }

    #[test]
    fn protocol_override_and_max_tokens_field_are_independent() {
        let merged = merge_overlay(
            &base(),
            r#"
[provider.zoocorp]
protocol = "anthropic"
"#,
        );
        let zoo = merged
            .iter()
            .find(|e| e.slug == "zoocorp")
            .expect("zoocorp");
        assert!(matches!(zoo.protocol, ProviderProtocol::AnthropicMessages));
        // Setting max_tokens_field on a non-OpenAI protocol warns + skips.
        let merged = merge_overlay(
            &base(),
            r#"
[provider.zoocorp]
max_tokens_field = "max_tokens"
"#,
        );
        let zoo = merged
            .iter()
            .find(|e| e.slug == "zoocorp")
            .expect("zoocorp");
        assert!(matches!(zoo.protocol, ProviderProtocol::AnthropicMessages));
    }
}
