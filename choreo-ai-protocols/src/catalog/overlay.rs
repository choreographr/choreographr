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
//! cache_affinity = "prompt-cache-key"  # off | prompt-cache-key | openrouter-session
//!                                      # per-session cache-affinity body field the
//!                                      # provider accepts (default off)
//!
//! [provider.opencode.models."gpt-5.4"]
//! responses = true
//! reasoning_passback = "response_id"
//! reasoning_echo_field = "reasoning_content"   # the wire key replayed reasoning is
//!                                             # shaped as (alias:
//!                                             # reasoning_content_required)
//! reasoning_echo_required = true      # …and that key must be PRESENT on every
//!                                     # assistant message, empty when there is
//!                                     # nothing to echo. Independent of
//!                                     # `reasoning_echo_field`; the legacy
//!                                     # `reasoning_content_required` alias sets
//!                                     # both halves.
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
use crate::session_routing::CacheAffinity;
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
                cache_affinity: CacheAffinity::Off,
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
            // Cache-affinity routing dialect: provider policy models.dev cannot
            // express. The tokens are the `CacheAffinity` overlay spellings;
            // `off` (or any unrecognized value) sends no session hint, the safe
            // default since the field is ignored by some providers.
            "cache_affinity" => {
                if let Some(affinity) = value.as_str().and_then(CacheAffinity::from_overlay_str) {
                    entry.cache_affinity = affinity;
                } else {
                    warn!(
                        slug = %entry.slug,
                        "overlay: cache_affinity must be \"off\", \"prompt-cache-key\", or \"openrouter-session\"; skipping",
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
            // The DESTINATION half of the echo policy: which wire key replayed
            // reasoning is shaped as. It never implies a presence requirement —
            // a route can want a specific destination without demanding its
            // presence; that is the separate `reasoning_echo_required` key.
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
            // The PRESENCE half: whether every assistant message must carry the
            // declared echo field (empty when there is nothing to echo).
            // Settable on its own where the snapshot already declares the
            // destination, or alongside `reasoning_echo_field` to pin a
            // DeepSeek/GLM-style upstream rule wholesale.
            "reasoning_echo_required" => {
                if let Some(required) = value.as_bool() {
                    model.reasoning_echo_required = Some(required);
                } else {
                    warn!(
                        model = %model.model,
                        "overlay: reasoning_echo_required is not a bool; skipping",
                    );
                }
            }
            // Older spelling of the whole policy: "require `reasoning_content`
            // on every assistant message". Kept as an alias so existing overlay
            // files keep working; it sets BOTH halves (the destination field and
            // its presence requirement), whereas `reasoning_echo_field` /
            // `reasoning_echo_required` set them independently. The key that
            // appears LATER in the file wins, so set only one spelling per model.
            "reasoning_content_required" => {
                if let Some(required) = value.as_bool() {
                    model.reasoning_echo = required.then_some(ChatReasoningField::ReasoningContent);
                    model.reasoning_echo_required = required.then_some(true);
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

/// Convert an overlay `reasoning_echo_field` value into the chat field the
/// route must carry.
///
/// The mapping is [`ChatReasoningField::from_wire_name`] — the same one the
/// snapshot parser and the chat lowering use — so an overlay cannot select a
/// key the adapter would spell differently. Anything it does not model is
/// rejected, so a typo cannot silently select a wrong wire key.
fn parse_echo_field(value: &str) -> Option<ChatReasoningField> {
    ChatReasoningField::from_wire_name(value)
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
mod tests;
