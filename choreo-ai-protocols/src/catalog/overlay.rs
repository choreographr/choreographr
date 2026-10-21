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
//! prompt_cache_short = 300             # provider-level TTL default, seconds
//! prompt_cache_long = 3600
//!
//! [provider.opencode.models."gpt-5.4"]
//! responses = true
//! reasoning_passback = "response_id"
//! reasoning_echo_field = "reasoning_content"   # the wire key every assistant
//!                                              # message must carry (alias:
//!                                              # reasoning_content_required)
//! prompt_cache_short = 300             # per-model TTL override
//! prompt_cache_long = 3600
//!
//! # OAuth profile (overlay-only policy). All fields except `client_id` are
//! # required; a partial block is warned and skipped, leaving the provider's
//! # API-key default in place.
//! [provider.chatgpt.oauth]
//! issuer = "https://auth.openai.com"
//! authorize_path = "/oauth/authorize"
//! token_path = "/oauth/token"
//! scopes = ["openid", "profile", "email", "offline_access"]
//! redirect_ports = [1455, 1457]
//! code_exchange_encoding = "form"      # form | json
//! refresh_encoding = "json"
//! device_code = false
//! client_id = "app_EMoamEEZ73f0CkXaXp7hrann"   # public id only, optional
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
//! - **Prompt-cache TTL** (`prompt_cache_short` / `prompt_cache_long`, in
//!   seconds) is provider-level policy with a per-model override: the two
//!   provider keys set the default on `[provider.<slug>]`, and the same two
//!   keys on a model entry set that model's override (which wins over the
//!   provider default at lookup time — see `prompt_cache_ttl`). Each tier is
//!   optional and set independently.
//! - **New providers** (models.dev does not cover them) are defined
//!   wholesale: a `[provider.<slug>]` table that does not exist in the base
//!   creates the provider, and its `models` sub-table fills in the entries.
//!
//! Unknown keys at any level are **warned and skipped**, never fatal — a
//! user overlay typo must not brick the daemon.

use tracing::{debug, trace, warn};

use super::{
    GrantEncoding, ModelEntry, OAuthProfile, ProviderEntry, ProviderProtocol, ReasoningPassback,
};
use crate::retry::RequestCompression;
use crate::shared::MaxTokensField;
use choreo_proto::ChatReasoningField;

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
    // Aggregate counters for a single per-merge debug line: the per-provider
    // / per-model detail moved to `trace!` because a merge runs at startup, on
    // every overlay reload, and on every `/refresh-models` — per-entry debug
    // lines flooded the log with hundreds of one-line entries per merge.
    let mut overridden = 0usize;
    let mut added = 0usize;
    let mut models_touched = 0usize;
    for (slug, value) in providers {
        let Some(table) = value.as_table() else {
            warn!(slug, "overlay provider is not a table; skipping");
            continue;
        };
        if let Some(entry) = merged.iter_mut().find(|e| e.slug == *slug) {
            trace!(slug, "overlay: applying provider overrides");
            apply_provider_overlay(entry, table, &mut models_touched);
            overridden += 1;
        } else {
            trace!(slug, "overlay: adding new provider");
            let mut entry = ProviderEntry {
                slug: slug.clone(),
                display_name: slug.clone(),
                protocol: ProviderProtocol::OpenAi {
                    max_tokens_field: MaxTokensField::MaxCompletionTokens,
                },
                base_url: String::new(),
                default_model: String::new(),
                models: Vec::new(),
                prompt_cache: None,
                request_compression: RequestCompression::Identity,
                oauth: None,
            };
            apply_provider_overlay(&mut entry, table, &mut models_touched);
            merged.push(entry);
            added += 1;
        }
    }
    debug!(overridden, added, models_touched, "overlay merge complete",);
    merged
}

/// Apply a `[provider.<slug>]` overlay table onto an existing entry.
/// Recognized scalar keys are field-wise replaced; the `models` sub-table is
/// applied per model; anything else is warned and skipped.
fn apply_provider_overlay(
    entry: &mut ProviderEntry,
    table: &toml::Table,
    models_touched: &mut usize,
) {
    for (key, value) in table {
        match key.as_str() {
            "protocol" => {
                if let Some(protocol) = parse_protocol(value) {
                    entry.protocol = protocol;
                } else {
                    warn!(slug = %entry.slug, "overlay: unknown protocol; skipping");
                }
            }
            "max_tokens_field" => {
                if let Some(field) = parse_max_tokens_field(value) {
                    set_max_tokens_field(entry, field);
                } else {
                    warn!(slug = %entry.slug, "overlay: unknown max_tokens_field; skipping");
                }
            }
            "base_url" => {
                if let Some(url) = value.as_str() {
                    entry.base_url = url.to_string();
                } else {
                    warn!(slug = %entry.slug, "overlay: base_url is not a string; skipping");
                }
            }
            "default_model" => match value.as_str() {
                Some(model) => entry.default_model = model.to_string(),
                None => {
                    warn!(slug = %entry.slug, "overlay: default_model is not a string; skipping");
                }
            },
            "display_name" => match value.as_str() {
                Some(name) => entry.display_name = name.to_string(),
                None => {
                    warn!(slug = %entry.slug, "overlay: display_name is not a string; skipping");
                }
            },
            // Prompt-cache TTL default: provider policy, set tier-by-tier. Both
            // keys are optional and independent, so mentioning either one
            // creates the `PromptCacheTtl` and only that tier is set.
            "prompt_cache_short" => {
                if let Some(n) = value.as_integer() {
                    entry
                        .prompt_cache
                        .get_or_insert_with(Default::default)
                        .short_secs = Some(u32_len(n));
                } else {
                    warn!(
                        slug = %entry.slug,
                        "overlay: prompt_cache_short is not an integer; skipping",
                    );
                }
            }
            "prompt_cache_long" => {
                if let Some(n) = value.as_integer() {
                    entry
                        .prompt_cache
                        .get_or_insert_with(Default::default)
                        .long_secs = Some(u32_len(n));
                } else {
                    warn!(
                        slug = %entry.slug,
                        "overlay: prompt_cache_long is not an integer; skipping",
                    );
                }
            }
            "models" => apply_models_overlay(entry, value, models_touched),
            // OAuth profile: overlay-only policy (models.dev carries no OAuth
            // facts). Parsed all-or-nothing — a partial block is warned and
            // skipped, leaving the provider's API-key default in place.
            "oauth" => {
                if let Some(profile) = parse_oauth_profile(&entry.slug, value) {
                    entry.oauth = Some(profile);
                }
            }
            // Request-body compression: provider policy models.dev cannot
            // express (it carries no encoding facts).  The coding tokens are the
            // `RequestCompression` overlay spellings; `identity` (or any
            // unrecognized value) leaves the body uncompressed, the safe
            // default since the server must decompress.
            "request_compression" => {
                if let Some(coding) = value
                    .as_str()
                    .and_then(RequestCompression::from_overlay_str)
                {
                    entry.request_compression = coding;
                } else {
                    warn!(
                        slug = %entry.slug,
                        "overlay: request_compression must be \"gzip\", \"zstd\", or \"identity\"; skipping",
                    );
                }
            }
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
fn apply_models_overlay(
    entry: &mut ProviderEntry,
    value: &toml::Value,
    models_touched: &mut usize,
) {
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
        if let Some(model) = entry.models.iter_mut().find(|m| m.model == *model_id) {
            apply_model_overlay(&mut *model, table);
            *models_touched += 1;
        } else {
            let mut model = ModelEntry {
                model: model_id.clone(),
                ..Default::default()
            };
            apply_model_overlay(&mut model, table);
            entry.models.push(model);
            *models_touched += 1;
        }
    }
}

/// Apply a single model overlay table onto a model entry, field-wise.
/// Overlay integer coercion, `n.max(0) as u32`. Negative values were
/// already rejected by the `max(0)` above; values above `u32::MAX` give the
/// same wrapped result as before the lint (behavior is pinned by tests), so
/// the raw cast is kept rather than clamping.
#[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn u32_len(n: i64) -> u32 {
    n.max(0) as u32
}

fn apply_model_overlay(model: &mut ModelEntry, table: &toml::Table) {
    for (key, value) in table {
        match key.as_str() {
            "context_window" => {
                if let Some(n) = value.as_integer() {
                    model.context_window = u32_len(n);
                } else {
                    warn!(
                        model = %model.model,
                        "overlay: context_window is not an integer; skipping",
                    );
                }
            }
            "reasoning_supported" => {
                if let Some(b) = value.as_bool() {
                    model.reasoning_supported = b;
                } else {
                    warn!(
                        model = %model.model,
                        "overlay: reasoning_supported is not a bool; skipping",
                    );
                }
            }
            "reasoning_levels" => {
                if let Some(levels) = value.as_array() {
                    model.openai_reasoning_levels = parse_string_array(levels);
                } else {
                    warn!(
                        model = %model.model,
                        "overlay: reasoning_levels is not an array; skipping",
                    );
                }
            }
            "responses" => {
                if let Some(b) = value.as_bool() {
                    model.openai_responses = b;
                } else {
                    warn!(
                        model = %model.model,
                        "overlay: responses is not a bool; skipping",
                    );
                }
            }
            "reasoning_passback" => {
                if let Some(passback) = value.as_str().and_then(parse_passback) {
                    model.reasoning_passback = Some(passback);
                } else {
                    warn!(
                        model = %model.model,
                        "overlay: unknown reasoning_passback; skipping",
                    );
                }
            }
            "reasoning_echo_field" => {
                if let Some(field) = value.as_str().and_then(parse_echo_field) {
                    model.reasoning_echo = Some(field);
                } else {
                    warn!(
                        model = %model.model,
                        "overlay: unknown reasoning_echo_field; skipping",
                    );
                }
            }
            // Older spelling of the same policy: "require `reasoning_content`
            // on every assistant message". Kept as an alias so existing
            // overlay files keep working; `reasoning_echo_field` is the
            // expressive form (it can name any scalar field). Both assign the
            // same field, so the key that appears LATER in the file wins —
            // set only one per model.
            "reasoning_content_required" => {
                if let Some(required) = value.as_bool() {
                    model.reasoning_echo = required.then_some(ChatReasoningField::ReasoningContent);
                } else {
                    warn!(
                        model = %model.model,
                        "overlay: reasoning_content_required is not a bool; skipping",
                    );
                }
            }
            "supports_vision" => {
                if let Some(support) = value.as_bool() {
                    model.supports_vision = support;
                } else {
                    warn!(
                        model = %model.model,
                        "overlay: supports_vision is not a bool; skipping",
                    );
                }
            }
            // Image-output override: same shape as supports_vision, for
            // pinning image generation where the snapshot's output
            // modalities are wrong or a new image model is not yet listed.
            "supports_image_output" => {
                if let Some(support) = value.as_bool() {
                    model.supports_image_output = support;
                } else {
                    warn!(
                        model = %model.model,
                        "overlay: supports_image_output is not a bool; skipping",
                    );
                }
            }
            // Image-EDIT override: models.dev carries no edit modality, so this
            // flag is set ONLY here — the overlay is the sole source of
            // "this model can edit an input image", by design (no name
            // heuristic). Same shape as supports_image_output.
            "supports_image_edit" => {
                if let Some(support) = value.as_bool() {
                    model.supports_image_edit = support;
                } else {
                    warn!(
                        model = %model.model,
                        "overlay: supports_image_edit is not a bool; skipping",
                    );
                }
            }
            // Output-token ceiling: overrides the ingested `limit.output`
            // (0 = unknown, same convention as `context_window`).
            "max_output_tokens" => {
                if let Some(n) = value.as_integer() {
                    model.max_output_tokens = u32_len(n);
                } else {
                    warn!(
                        model = %model.model,
                        "overlay: max_output_tokens is not an integer; skipping",
                    );
                }
            }
            // Temperature acceptance: models.dev records the fact only when
            // it is "no", so the overlay is where a "yes" can be pinned for
            // a gateway that proxies a temperature-less base model.
            "temperature" => {
                if let Some(supports) = value.as_bool() {
                    model.supports_temperature = supports;
                } else {
                    warn!(
                        model = %model.model,
                        "overlay: temperature is not a bool; skipping",
                    );
                }
            }
            // Lifecycle status override (e.g. pinning a snapshot entry the
            // upstream has since retired, or un-deprecating a re-listed one).
            "deprecated" => {
                if let Some(deprecated) = value.as_bool() {
                    model.deprecated = deprecated;
                } else {
                    warn!(
                        model = %model.model,
                        "overlay: deprecated is not a bool; skipping",
                    );
                }
            }
            // Per-model prompt-cache TTL override: same tier-by-tier shape as
            // the provider-level default; a set tier here wins over the
            // provider default at lookup time (see `prompt_cache_ttl`).
            "prompt_cache_short" => {
                if let Some(n) = value.as_integer() {
                    model
                        .prompt_cache
                        .get_or_insert_with(Default::default)
                        .short_secs = Some(u32_len(n));
                } else {
                    warn!(
                        model = %model.model,
                        "overlay: prompt_cache_short is not an integer; skipping",
                    );
                }
            }
            "prompt_cache_long" => {
                if let Some(n) = value.as_integer() {
                    model
                        .prompt_cache
                        .get_or_insert_with(Default::default)
                        .long_secs = Some(u32_len(n));
                } else {
                    warn!(
                        model = %model.model,
                        "overlay: prompt_cache_long is not an integer; skipping",
                    );
                }
            }
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

/// Convert an overlay `reasoning_echo_field` value into the scalar chat field
/// the route must carry. Only the three fields the chat adapter can echo text
/// into are accepted; anything else (including `reasoning_details`, which is a
/// structured array) is rejected so a typo cannot silently select a wrong
/// wire key.
fn parse_echo_field(value: &str) -> Option<ChatReasoningField> {
    match value {
        "reasoning_content" => Some(ChatReasoningField::ReasoningContent),
        "reasoning" => Some(ChatReasoningField::Reasoning),
        "reasoning_text" => Some(ChatReasoningField::ReasoningText),
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

/// Parse a `code_exchange_encoding` / `refresh_encoding` scalar.
fn parse_grant_encoding(value: &str) -> Option<GrantEncoding> {
    match value {
        "form" => Some(GrantEncoding::Form),
        "json" => Some(GrantEncoding::Json),
        _ => None,
    }
}

/// Convert a TOML array of integers into loopback redirect ports, dropping
/// non-integer and out-of-range entries (a bad port is worse than a missing
/// one).
fn parse_port_array(array: &[toml::Value]) -> Vec<u16> {
    array
        .iter()
        .filter_map(toml::Value::as_integer)
        .filter_map(|n| u16::try_from(n).ok())
        .collect()
}

/// The keys [`parse_oauth_profile`] recognizes; anything else is warned and
/// skipped, matching the overlay's unknown-key contract.
const OAUTH_KEYS: &[&str] = &[
    "issuer",
    "authorize_path",
    "token_path",
    "scopes",
    "redirect_ports",
    "code_exchange_encoding",
    "refresh_encoding",
    "device_code",
    "device_authorization_path",
    "device_token_path",
    "client_id",
];

/// Parse the `[provider.<slug>.oauth]` sub-table into an [`OAuthProfile`],
/// all-or-nothing: every field except `client_id` must be present and correctly
/// typed, or the whole block is warned and skipped so the provider keeps its
/// API-key default. Unknown keys inside the block are warned and skipped.
fn parse_oauth_profile(slug: &str, value: &toml::Value) -> Option<OAuthProfile> {
    let Some(table) = value.as_table() else {
        warn!(slug, "overlay: oauth is not a table; skipping");
        return None;
    };
    for key in table.keys() {
        if !OAUTH_KEYS.contains(&key.as_str()) {
            warn!(
                slug,
                key = key.as_str(),
                "overlay: unknown oauth key; skipping"
            );
        }
    }
    let issuer = table.get("issuer").and_then(toml::Value::as_str);
    let authorize_path = table.get("authorize_path").and_then(toml::Value::as_str);
    let token_path = table.get("token_path").and_then(toml::Value::as_str);
    let scopes = table
        .get("scopes")
        .and_then(toml::Value::as_array)
        .map(|array| parse_string_array(array));
    let redirect_ports = table
        .get("redirect_ports")
        .and_then(toml::Value::as_array)
        .map(|array| parse_port_array(array));
    let code_exchange_encoding = table
        .get("code_exchange_encoding")
        .and_then(toml::Value::as_str)
        .and_then(parse_grant_encoding);
    let refresh_encoding = table
        .get("refresh_encoding")
        .and_then(toml::Value::as_str)
        .and_then(parse_grant_encoding);
    let device_code = table.get("device_code").and_then(toml::Value::as_bool);
    let device_authorization_path = table
        .get("device_authorization_path")
        .and_then(toml::Value::as_str)
        .map(str::to_string);
    let device_token_path = table
        .get("device_token_path")
        .and_then(toml::Value::as_str)
        .map(str::to_string);
    let client_id = table
        .get("client_id")
        .and_then(toml::Value::as_str)
        .map(str::to_string);

    if let (
        Some(issuer),
        Some(authorize_path),
        Some(token_path),
        Some(scopes),
        Some(redirect_ports),
        Some(code_exchange_encoding),
        Some(refresh_encoding),
        Some(device_code),
    ) = (
        issuer,
        authorize_path,
        token_path,
        scopes,
        redirect_ports,
        code_exchange_encoding,
        refresh_encoding,
        device_code,
    ) {
        Some(OAuthProfile {
            issuer: issuer.to_string(),
            authorize_path: authorize_path.to_string(),
            token_path: token_path.to_string(),
            scopes,
            redirect_ports,
            code_exchange_encoding,
            refresh_encoding,
            device_code,
            device_authorization_path,
            device_token_path,
            client_id,
        })
    } else {
        warn!(
            slug,
            "overlay: oauth profile is missing a required field; skipping"
        );
        None
    }
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
                prompt_cache: None,
                request_compression: RequestCompression::Identity,
                oauth: None,
                models: vec![
                    ModelEntry {
                        model: "acme-base".into(),
                        context_window: 8192,
                        reasoning_supported: true,
                        openai_reasoning_levels: vec!["off".into(), "high".into()],
                        supports_vision: true,
                        ..Default::default()
                    },
                    ModelEntry {
                        model: "acme-lite".into(),
                        context_window: 4096,
                        ..Default::default()
                    },
                ],
            },
            ProviderEntry {
                slug: "zoocorp".into(),
                display_name: "Zoo Corp".into(),
                protocol: ProviderProtocol::AnthropicMessages,
                base_url: "https://api.zoocorp.dev".into(),
                default_model: "zoo-1".into(),
                prompt_cache: None,
                request_compression: RequestCompression::Identity,
                oauth: None,
                models: vec![ModelEntry {
                    model: "zoo-1".into(),
                    context_window: 200_000,
                    reasoning_supported: true,
                    ..Default::default()
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
    fn overlay_reasoning_echo_field_declares_the_wire_key() {
        // The expressive form: the overlay names the exact field the route
        // must carry, and it lands on the model entry as a field NAME (not a
        // boolean), so the adapter can pick the right wire key.
        let merged = merge_overlay(
            &base(),
            r#"
[provider.acme.models."acme-base"]
reasoning_echo_field = "reasoning"
"#,
        );
        let acme = merged.iter().find(|e| e.slug == "acme").expect("acme");
        let model = acme
            .models
            .iter()
            .find(|m| m.model == "acme-base")
            .expect("acme-base");
        assert_eq!(model.reasoning_echo, Some(ChatReasoningField::Reasoning));
    }

    #[test]
    fn overlay_reasoning_content_required_alias_sets_the_same_field() {
        // The pre-existing spelling keeps working and means exactly the same
        // thing (`reasoning_content`, present on every assistant message).
        let merged = merge_overlay(
            &base(),
            r#"
[provider.acme.models."acme-base"]
reasoning_content_required = true
"#,
        );
        let acme = merged.iter().find(|e| e.slug == "acme").expect("acme");
        let model = acme
            .models
            .iter()
            .find(|m| m.model == "acme-base")
            .expect("acme-base");
        assert_eq!(
            model.reasoning_echo,
            Some(ChatReasoningField::ReasoningContent)
        );

        // `false` clears a fact the base (or an earlier layer) declared.
        let cleared = merge_overlay(
            &merged,
            r#"
[provider.acme.models."acme-base"]
reasoning_content_required = false
"#,
        );
        let acme = cleared.iter().find(|e| e.slug == "acme").expect("acme");
        assert_eq!(
            acme.models
                .iter()
                .find(|m| m.model == "acme-base")
                .expect("acme-base")
                .reasoning_echo,
            None
        );
    }

    #[test]
    fn overlay_rejects_an_unknown_reasoning_echo_field() {
        // A typo (or `reasoning_details`, which is structured) must not select
        // an arbitrary wire key: it is warned and skipped, leaving no fact.
        let merged = merge_overlay(
            &base(),
            r#"
[provider.acme.models."acme-base"]
reasoning_echo_field = "reasoning_details"
"#,
        );
        let acme = merged.iter().find(|e| e.slug == "acme").expect("acme");
        assert_eq!(
            acme.models
                .iter()
                .find(|m| m.model == "acme-base")
                .expect("acme-base")
                .reasoning_echo,
            None
        );
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
context_window = 131_072
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
        assert_eq!(ollama.models[0].context_window, 131_072);
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
context_window = 100_000
reasoning_supported = true
"#,
        );
        let acme = merged.iter().find(|e| e.slug == "acme").expect("acme");
        let new_model = acme
            .models
            .iter()
            .find(|m| m.model == "brand-new")
            .expect("brand-new added");
        assert_eq!(new_model.context_window, 100_000);
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
    fn new_model_facts_are_overridable() {
        // The new ingestion fields (max_output_tokens / temperature /
        // deprecated) are overlay-overridable like every other fact, so a
        // user can fix a stale snapshot entry without regenerating catalog.bin.
        let merged = merge_overlay(
            &base(),
            r#"
[provider.acme.models."acme-base"]
max_output_tokens = 64000
temperature = false
deprecated = true
"#,
        );
        let acme = merged.iter().find(|e| e.slug == "acme").expect("acme");
        let base_model = acme
            .models
            .iter()
            .find(|m| m.model == "acme-base")
            .expect("acme-base");
        assert_eq!(base_model.max_output_tokens, 64_000);
        assert!(!base_model.supports_temperature);
        assert!(base_model.deprecated);
        // The untouched sibling keeps its ingested values.
        let lite = acme
            .models
            .iter()
            .find(|m| m.model == "acme-lite")
            .expect("acme-lite");
        assert!(lite.supports_temperature);
        assert!(!lite.deprecated);
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
    fn supports_image_output_overlay_override_flips_the_flag() {
        // Mirror of the supports_vision override: an overlay bool flips the
        // ingested fact in either direction (and can define it wholesale for
        // a model the snapshot does not cover).
        let merged = merge_overlay(
            &base(),
            r#"
[provider.acme.models."acme-base"]
supports_image_output = true

[provider.acme.models."brand-new-image"]
supports_image_output = true

[provider.acme.models."acme-lite"]
supports_image_output = true
"#,
        );
        let acme = merged.iter().find(|e| e.slug == "acme").expect("acme");
        // The default entry for acme-base carries image output false — the
        // overlay flips it.
        assert!(
            acme.models
                .iter()
                .find(|m| m.model == "acme-base")
                .unwrap()
                .supports_image_output
        );
        // A brand-new model is added wholesale with the flag set.
        assert!(
            acme.models
                .iter()
                .find(|m| m.model == "brand-new-image")
                .unwrap()
                .supports_image_output
        );
        // A different provider's model is untouched.
        let zoo = merged
            .iter()
            .find(|e| e.slug == "zoocorp")
            .expect("zoocorp");
        assert!(!zoo.models[0].supports_image_output);
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

    #[test]
    fn provider_level_prompt_cache_sets_the_default() {
        let merged = merge_overlay(
            &base(),
            r"
[provider.acme]
prompt_cache_short = 300
prompt_cache_long = 3600
",
        );
        let acme = merged.iter().find(|e| e.slug == "acme").expect("acme");
        let ttl = acme.prompt_cache.expect("provider default set");
        assert_eq!(ttl.short_secs, Some(300));
        assert_eq!(ttl.long_secs, Some(3_600));
        // A single tier may be declared on its own.
        let merged = merge_overlay(
            &base(),
            r"
[provider.acme]
prompt_cache_short = 300
",
        );
        let ttl = merged
            .iter()
            .find(|e| e.slug == "acme")
            .expect("acme")
            .prompt_cache
            .expect("provider default set");
        assert_eq!(ttl.short_secs, Some(300));
        assert_eq!(ttl.long_secs, None);
        // The untouched provider keeps no TTL.
        assert!(
            merged
                .iter()
                .find(|e| e.slug == "zoocorp")
                .expect("zoocorp")
                .prompt_cache
                .is_none()
        );
    }

    #[test]
    fn model_level_prompt_cache_sets_the_override() {
        let merged = merge_overlay(
            &base(),
            r#"
[provider.acme.models."acme-base"]
prompt_cache_short = 60
prompt_cache_long = 1800
"#,
        );
        let acme = merged.iter().find(|e| e.slug == "acme").expect("acme");
        let base_model = acme
            .models
            .iter()
            .find(|m| m.model == "acme-base")
            .expect("acme-base");
        let ttl = base_model.prompt_cache.expect("model override set");
        assert_eq!(ttl.short_secs, Some(60));
        assert_eq!(ttl.long_secs, Some(1_800));
        // The sibling model carries no override.
        let lite = acme
            .models
            .iter()
            .find(|m| m.model == "acme-lite")
            .expect("acme-lite");
        assert!(lite.prompt_cache.is_none());
    }

    #[test]
    fn prompt_cache_wrong_type_warns_and_is_skipped() {
        // A non-integer TTL is skipped (warn), while a valid sibling still
        // applies — same warn-and-skip contract as every other overlay key.
        let merged = merge_overlay(
            &base(),
            r#"
[provider.acme]
prompt_cache_short = "soon"

[provider.acme.models."acme-base"]
prompt_cache_long = true
"#,
        );
        let acme = merged.iter().find(|e| e.slug == "acme").expect("acme");
        // The bad provider key is skipped: no provider default is created.
        assert!(acme.prompt_cache.is_none());
        let base_model = acme
            .models
            .iter()
            .find(|m| m.model == "acme-base")
            .expect("acme-base");
        assert!(base_model.prompt_cache.is_none());
    }

    #[test]
    fn overlay_sets_request_compression_and_rejects_unknown_values() {
        // `gzip` opts the provider into compressed request bodies.
        let merged = merge_overlay(
            &base(),
            r#"
[provider.acme]
request_compression = "gzip"
"#,
        );
        let acme = merged.iter().find(|e| e.slug == "acme").expect("acme");
        assert_eq!(acme.request_compression, RequestCompression::Gzip);

        // `zstd` opts the provider into zstd request bodies.
        let merged = merge_overlay(
            &base(),
            r#"
[provider.acme]
request_compression = "zstd"
"#,
        );
        let acme = merged.iter().find(|e| e.slug == "acme").expect("acme");
        assert_eq!(acme.request_compression, RequestCompression::Zstd);

        // An unknown coding is warned-and-skipped, leaving the safe identity
        // default (the server must decompress, so never guess a coding).
        let merged = merge_overlay(
            &base(),
            r#"
[provider.acme]
request_compression = "deflate"
"#,
        );
        let acme = merged.iter().find(|e| e.slug == "acme").expect("acme");
        assert_eq!(acme.request_compression, RequestCompression::Identity);
    }

    #[test]
    fn oauth_profile_parsed_from_overlay() {
        let merged = merge_overlay(
            &base(),
            r#"
[provider.acme.oauth]
issuer = "https://auth.acme.dev"
authorize_path = "/oauth/authorize"
token_path = "/oauth/token"
scopes = ["openid", "offline_access"]
redirect_ports = [1455, 1457]
code_exchange_encoding = "form"
refresh_encoding = "json"
device_code = true
client_id = "acme-public-client"
"#,
        );
        let acme = merged.iter().find(|e| e.slug == "acme").expect("acme");
        let oauth = acme.oauth.as_ref().expect("oauth profile parsed");
        assert_eq!(oauth.issuer, "https://auth.acme.dev");
        assert_eq!(oauth.authorize_path, "/oauth/authorize");
        assert_eq!(oauth.token_path, "/oauth/token");
        assert_eq!(oauth.scopes, ["openid", "offline_access"]);
        assert_eq!(oauth.redirect_ports, [1455, 1457]);
        assert_eq!(oauth.code_exchange_encoding, GrantEncoding::Form);
        assert_eq!(oauth.refresh_encoding, GrantEncoding::Json);
        assert!(oauth.device_code);
        assert_eq!(oauth.client_id.as_deref(), Some("acme-public-client"));
        // The derived kind flips to OAuth with the profile present.
        assert_eq!(acme.auth_kind(), crate::catalog::AuthKind::OAuth);
        // The untouched provider keeps the API-key default.
        let zoo = merged
            .iter()
            .find(|e| e.slug == "zoocorp")
            .expect("zoocorp");
        assert!(zoo.oauth.is_none());
        assert_eq!(zoo.auth_kind(), crate::catalog::AuthKind::ApiKey);
    }

    #[test]
    fn partial_oauth_profile_is_skipped() {
        // Missing required fields (`token_path`, encodings, `device_code`) —
        // the whole block is dropped, leaving the API-key default.
        let merged = merge_overlay(
            &base(),
            r#"
[provider.acme.oauth]
issuer = "https://auth.acme.dev"
authorize_path = "/oauth/authorize"
"#,
        );
        let acme = merged.iter().find(|e| e.slug == "acme").expect("acme");
        assert!(
            acme.oauth.is_none(),
            "a partial oauth profile must be skipped, not half-applied"
        );
        assert_eq!(acme.auth_kind(), crate::catalog::AuthKind::ApiKey);
    }

    #[test]
    fn non_table_oauth_block_is_skipped() {
        let merged = merge_overlay(
            &base(),
            r#"
[provider.acme]
oauth = "nope"
"#,
        );
        let acme = merged.iter().find(|e| e.slug == "acme").expect("acme");
        assert!(acme.oauth.is_none());
    }
}
