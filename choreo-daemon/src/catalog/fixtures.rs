//! Shared fixtures for the catalog module's unit tests.
//!
//! Compiled only for test builds; keeps one definition of the tiny base so a
//! cache round-trip and a command-shape assertion cannot drift apart.

use choreo_ai_protocols::{
    MaxTokensField, ModelEntry, ProviderEntry, ProviderProtocol, RequestCompression,
};

/// A tiny two-provider base: small enough to assert on field-by-field, and
/// distinguishable from the 150+ provider embedded blob by its slugs.
pub(super) fn tiny_base() -> Vec<ProviderEntry> {
    vec![
        ProviderEntry {
            slug: "acme".into(),
            display_name: "Acme".into(),
            protocol: ProviderProtocol::OpenAi {
                max_tokens_field: MaxTokensField::MaxCompletionTokens,
            },
            base_url: "https://api.acme.dev/v1".into(),
            default_model: "acme-1".into(),
            prompt_cache: None,
            request_compression: RequestCompression::Identity,
            oauth: None,
            models: vec![ModelEntry {
                model: "acme-1".into(),
                context_window: 8192,
                reasoning_supported: true,
                openai_reasoning_levels: vec!["off".into(), "high".into()],
                max_output_tokens: 4096,
                ..Default::default()
            }],
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
            models: Vec::new(),
        },
    ]
}
