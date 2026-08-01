use std::io;
use std::sync::Arc;

use crate::anthropic::AnthropicClient;
use crate::openai::OpenAiClient;
use choreo_proto::InferenceError;

mod catalog;
pub(crate) mod context_window;
pub(crate) mod shared;
mod traits;
pub(crate) mod types;

pub use catalog::{
    ModelEntry, PROVIDER_CATALOG, ProviderEntry, ProviderProtocol, all_display_names, all_slugs,
    lookup_context_window, lookup_provider, model_reasoning_capability, model_request_format,
};
pub use context_window::ContextWindowConfig;
pub use traits::{ChatTurnRequest, ProviderClient, ToolResultItem};
pub use types::{CallerInfo, ChatAssistantToolUse, ChatToolCall, ChatTurnResult, FinalTextResult};

#[derive(Clone, Debug)]
pub struct InferenceProvider {
    client: Arc<dyn ProviderClient>,
    /// The provider slug from the catalog (e.g. "openai", "anthropic", "opencode").
    /// Used for catalog lookups instead of delegating to the client, which may
    /// return a generic value (e.g. OpenAiClient always says "openai").
    slug: &'static str,
}

impl InferenceProvider {
    pub fn from_openai(client: OpenAiClient) -> Self {
        Self {
            client: Arc::new(client),
            slug: "openai",
        }
    }

    pub fn from_anthropic(client: AnthropicClient) -> Self {
        Self {
            client: Arc::new(client),
            slug: "anthropic",
        }
    }

    /// Create a provider from an account config + credential key.
    /// Applies all account-level overrides (base_url, streaming, timeouts)
    /// onto the service config before constructing the client.
    pub fn from_account_config(
        config: &crate::accounts::AccountConfig,
        api_key: Option<String>,
    ) -> Result<Self, String> {
        let entry = lookup_provider(&config.provider)
            .ok_or_else(|| format!("unknown provider: {}", config.provider))?;

        match entry.protocol {
            ProviderProtocol::OpenAi { max_tokens_field } => {
                let mut svc_config = crate::openai::ServiceConfig {
                    base_url: entry.base_url.to_string(),
                    chat_completions_max_tokens_field: max_tokens_field,
                    provider_slug: entry.slug.as_str(),
                    ..Default::default()
                };
                config.apply_overrides(&mut svc_config);
                let key = api_key
                    .ok_or_else(|| format!("no API key for '{}' provider", config.provider))?;
                let client = OpenAiClient::new(svc_config, key)
                    .map_err(|e| format!("failed to create OpenAI client: {e}"))?;
                Ok(Self {
                    client: Arc::new(client),
                    slug: entry.slug.as_str(),
                })
            }
            ProviderProtocol::AnthropicMessages => {
                let key = api_key
                    .ok_or_else(|| format!("no API key for '{}' provider", config.provider))?;
                let mut anthro_cfg = crate::anthropic::AnthropicConfig::default();
                // If the account doesn't specify a base_url, use the catalog default.
                if config.base_url.is_none() {
                    anthro_cfg.base_url = entry.base_url.to_string();
                }
                anthro_cfg.apply_overrides(config);
                let client = AnthropicClient::new(anthro_cfg, key)
                    .map_err(|e| format!("failed to create Anthropic client: {e}"))?;
                Ok(Self {
                    client: Arc::new(client),
                    slug: entry.slug.as_str(),
                })
            }
            ProviderProtocol::GoogleGenerativeAi => {
                let key = api_key
                    .ok_or_else(|| format!("no API key for '{}' provider", config.provider))?;
                let mut google_cfg = crate::google::GoogleConfig::default();
                // If the account doesn't specify a base_url, use the catalog default.
                if config.base_url.is_none() {
                    google_cfg.base_url = entry.base_url.to_string();
                }
                google_cfg.apply_overrides(config);
                let client = crate::google::GoogleClient::new(google_cfg, key)
                    .map_err(|e| format!("failed to create Google client: {e}"))?;
                Ok(Self {
                    client: Arc::new(client),
                    slug: entry.slug.as_str(),
                })
            }
        }
    }

    pub fn chat_completion_turn(
        &self,
        params: ChatTurnRequest<'_>,
    ) -> Result<ChatTurnResult, InferenceError> {
        self.client.chat_completion_turn(params)
    }

    pub fn chat_completion_turn_streaming(
        &self,
        params: ChatTurnRequest<'_>,
        on_event: &mut dyn FnMut(StreamEvent) -> io::Result<()>,
    ) -> Result<ChatTurnResult, InferenceError> {
        self.client.chat_completion_turn_streaming(params, on_event)
    }

    /// Return the provider slug (e.g. "openai", "anthropic").
    pub fn provider_slug(&self) -> &'static str {
        self.slug
    }

    /// Resolve the context window for a model, using the client config first
    /// and falling back to the static catalog for known model slugs.
    pub fn resolve_context_window(&self, model: &str) -> Option<u32> {
        self.client
            .context_window_for_model(model)
            .or_else(|| catalog::lookup_context_window(self.slug, model))
    }

    pub fn list_models(&self) -> Result<Vec<String>, InferenceError> {
        self.client.list_models()
    }

    pub fn supports_programmatic_tool_calling(&self, model: &str) -> bool {
        self.client.supports_programmatic_tool_calling(model)
    }
}

/// A single event emitted during a streaming LLM response.
///
/// Replaces the old `(CompletionChunkKind, String)` tuple with a self-describing
/// enum so each variant carries its data inline.  The consumer receives these
/// through the `on_event` callback of [`chat_completion_turn_streaming`] and can
/// use them for real-time UI updates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamEvent {
    Answer(String),
    Reasoning(String),
}

/// Try to list models via the API; fall back to the static known list on any
/// error.  Used by provider implementations to gracefully degrade when the
/// models endpoint is unreachable or the API key lacks permission.
pub(crate) fn list_models_with_fallback<F, E>(
    fetch: F,
    static_list: &[&str],
    provider_name: &str,
) -> Result<Vec<String>, E>
where
    F: FnOnce() -> Result<Vec<String>, E>,
    E: std::fmt::Display,
{
    match fetch() {
        Ok(models) => {
            tracing::info!("{provider_name} models returned: {}", models.len());
            Ok(models)
        }
        Err(e) => {
            tracing::warn!(
                "failed to list models from {provider_name} API, using static list: {e}"
            );
            Ok(static_list.iter().map(|s| s.to_string()).collect())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accounts::AccountConfig;
    use crate::anthropic::AnthropicConfig;

    use crate::openai::ServiceConfig;

    #[test]
    fn from_openai_constructs_provider() {
        let config = ServiceConfig::default();
        let client = OpenAiClient::new(config, "test-key".into()).unwrap();
        let _provider = InferenceProvider::from_openai(client);
        // Construction succeeds — no panic.
    }

    #[test]
    fn from_anthropic_constructs_provider() {
        let config = AnthropicConfig::default();
        let client = AnthropicClient::new(config, "test-key".into()).unwrap();
        let _provider = InferenceProvider::from_anthropic(client);
        // Construction succeeds — no panic.
    }

    #[test]
    fn from_account_config_unknown_provider_errors() {
        let cfg = AccountConfig::simple("unknown", "nonexistent");
        let err = InferenceProvider::from_account_config(&cfg, Some("key".into())).unwrap_err();
        assert!(err.contains("unknown provider"), "{err}");
    }

    #[test]
    fn from_account_config_anthropic_requires_key() {
        let cfg = AccountConfig::simple("claude", "anthropic");
        let err = InferenceProvider::from_account_config(&cfg, None).unwrap_err();
        assert!(err.contains("no API key"), "{err}");
    }

    #[test]
    fn from_account_config_openai_missing_key_errors() {
        let cfg = AccountConfig::simple("openai", "openai");
        let err = InferenceProvider::from_account_config(&cfg, None).unwrap_err();
        assert!(err.contains("no API key"), "{err}");
    }

    #[test]
    fn from_account_config_openai_succeeds() {
        let cfg = AccountConfig::simple("openai", "openai");
        let result = InferenceProvider::from_account_config(&cfg, Some("key".into()));
        assert!(result.is_ok(), "{:?}", result.err());
    }

    #[test]
    fn from_account_config_anthropic_succeeds() {
        let cfg = AccountConfig::simple("claude", "anthropic");
        let result = InferenceProvider::from_account_config(&cfg, Some("key".into()));
        assert!(result.is_ok(), "{:?}", result.err());
    }

    #[test]
    fn anthropic_provider_list_models_returns_known() {
        let config = AnthropicConfig::default();
        let client = AnthropicClient::new(config, "test-key".into()).unwrap();
        let provider = InferenceProvider::from_anthropic(client);
        let models = provider.list_models().unwrap();
        assert!(!models.is_empty());
        assert!(models.contains(&"claude-sonnet-4-20250514".to_string()));
    }

    #[test]
    fn resolve_context_window_uses_client_then_catalog() {
        let mut cfg = crate::openai::ServiceConfig::default();
        cfg.context_window_config.per_model = [("gpt-4.1-nano".into(), 1_048_576)].into();
        cfg.context_window_config.context_window = Some(128_000);
        let client = OpenAiClient::new(cfg, "test-key".into()).unwrap();
        let provider = InferenceProvider::from_openai(client);
        // Per-model from client config
        assert_eq!(
            provider.resolve_context_window("gpt-4.1-nano"),
            Some(1_048_576)
        );
        // Global fallback from client config
        assert_eq!(
            provider.resolve_context_window("unknown-model"),
            Some(128_000)
        );
    }

    #[test]
    fn resolve_context_window_falls_back_to_catalog() {
        // Anthropic provider with default config has no per-model map entries
        // and no global fallback, so it falls back to the catalog.
        let config = AnthropicConfig::default();
        let client = AnthropicClient::new(config, "test-key".into()).unwrap();
        let provider = InferenceProvider::from_anthropic(client);
        assert_eq!(
            provider.resolve_context_window("claude-sonnet-4-6"),
            Some(1_000_000)
        );
        // Unknown model — neither client nor catalog knows it
        assert_eq!(provider.resolve_context_window("completely-unknown"), None);
    }
}

/// Stub provider client and factory for use in daemon-level unit tests.
/// Only checks provider existence — all provider methods panic.
#[cfg(test)]
pub(crate) mod test_util {
    use super::*;

    #[derive(Debug)]
    pub(crate) struct StubProviderClient;

    impl ProviderClient for StubProviderClient {
        fn provider_slug(&self) -> &'static str {
            "test-stub"
        }

        fn chat_completion_turn(
            &self,
            _params: ChatTurnRequest<'_>,
        ) -> Result<ChatTurnResult, InferenceError> {
            panic!("StubProviderClient is not intended for real use");
        }

        fn chat_completion_turn_streaming(
            &self,
            _params: ChatTurnRequest<'_>,
            _on_event: &mut dyn FnMut(StreamEvent) -> io::Result<()>,
        ) -> Result<ChatTurnResult, InferenceError> {
            panic!("StubProviderClient is not intended for real use");
        }

        fn list_models(&self) -> Result<Vec<String>, InferenceError> {
            panic!("StubProviderClient is not intended for real use");
        }
    }

    pub(crate) fn make_test_provider() -> InferenceProvider {
        InferenceProvider {
            client: Arc::new(StubProviderClient),
            slug: "test-stub",
        }
    }
}
