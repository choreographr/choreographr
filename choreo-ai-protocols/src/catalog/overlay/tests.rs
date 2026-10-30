//! Tests for the overlay parser/merger in [`super`].
//!
//! These cover the two-layer TOML overlay: field-wise replacement of provider
//! scalars, model-level overrides, OAuth profile parsing, and the rejection
//! paths. The suite lives in a sibling file so the parser module stays readable
//! at a glance; the module path is unchanged, so the moved tests reach their
//! subjects through the same `super` paths.

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
            cache_affinity: CacheAffinity::Off,
            responses_stateless: false,
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
            cache_affinity: CacheAffinity::Off,
            responses_stateless: false,
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
    // The expressive form: the overlay names the exact field replayed
    // reasoning is shaped as, and it lands on the model entry as a field
    // NAME (not a boolean), so the adapter can pick the right wire key. It
    // never implies presence — that is a separately settable fact.
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
    assert_eq!(model.reasoning_echo_required, None);
}

#[test]
fn overlay_reasoning_echo_required_sets_the_presence_fact() {
    // The presence half is its own key, so a route can demand the key on
    // every assistant message alongside a declared destination…
    let merged = merge_overlay(
        &base(),
        r#"
[provider.acme.models."acme-base"]
reasoning_echo_field = "reasoning_content"
reasoning_echo_required = true
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
    assert_eq!(model.reasoning_echo_required, Some(true));

    // …and a later layer can drop the requirement while keeping the
    // destination, which is exactly the "declare but do not force" shape.
    let relaxed = merge_overlay(
        &merged,
        r#"
[provider.acme.models."acme-base"]
reasoning_echo_required = false
"#,
    );
    let acme = relaxed.iter().find(|e| e.slug == "acme").expect("acme");
    let model = acme
        .models
        .iter()
        .find(|m| m.model == "acme-base")
        .expect("acme-base");
    assert_eq!(
        model.reasoning_echo,
        Some(ChatReasoningField::ReasoningContent)
    );
    assert_eq!(model.reasoning_echo_required, Some(false));
}

#[test]
fn overlay_reasoning_content_required_alias_sets_both_echo_facts() {
    // The pre-existing spelling keeps working and means the whole policy
    // (`reasoning_content`, present on every assistant message), so it
    // assigns BOTH the destination and the presence requirement.
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
    assert_eq!(model.reasoning_echo_required, Some(true));

    // `false` clears both facts a base (or an earlier layer) declared.
    let cleared = merge_overlay(
        &merged,
        r#"
[provider.acme.models."acme-base"]
reasoning_content_required = false
"#,
    );
    let acme = cleared.iter().find(|e| e.slug == "acme").expect("acme");
    let model = acme
        .models
        .iter()
        .find(|m| m.model == "acme-base")
        .expect("acme-base");
    assert_eq!(model.reasoning_echo, None);
    assert_eq!(model.reasoning_echo_required, None);
}

#[test]
fn overlay_accepts_a_structured_reasoning_echo_field() {
    // `reasoning_details` is structured, but it is a field the adapter can
    // replay; the overlay can name it (e.g. for a gateway model the
    // snapshot omits).
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
        Some(ChatReasoningField::ReasoningDetails)
    );
}

#[test]
fn overlay_rejects_an_unknown_reasoning_echo_field() {
    // A typo must not select an arbitrary wire key: it is warned and
    // skipped, leaving no fact.
    let merged = merge_overlay(
        &base(),
        r#"
[provider.acme.models."acme-base"]
reasoning_echo_field = "vendor_reasoning"
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
