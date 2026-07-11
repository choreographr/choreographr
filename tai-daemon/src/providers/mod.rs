use std::io;
use std::sync::Arc;

use crate::anthropic::AnthropicClient;
use crate::mistral::MistralClient;
use crate::openai::{ChatTurnResult, CompletionChunkKind, OpenAiClient};
use tai_proto::InferenceError;

mod catalog;
pub(crate) mod shared;
mod traits;

pub use catalog::{
    PROVIDER_CATALOG, ProviderEntry, ProviderProtocol, ReasoningSupport, all_display_names,
    all_slugs, effective_reasoning_support, lookup_provider,
};
pub use traits::{ChatTurnRequest, ProviderClient};

#[derive(Clone, Debug)]
pub struct InferenceProvider {
    client: Arc<dyn ProviderClient>,
}

impl InferenceProvider {
    pub fn from_openai(client: OpenAiClient) -> Self {
        Self {
            client: Arc::new(client),
        }
    }

    pub fn from_anthropic(client: AnthropicClient) -> Self {
        Self {
            client: Arc::new(client),
        }
    }

    pub fn from_mistral(client: MistralClient) -> Self {
        Self {
            client: Arc::new(client),
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
            ProviderProtocol::OpenAiCompatible => {
                let mut svc_config = crate::openai::ServiceConfig {
                    base_url: entry.default_base_url.to_string(),
                    chat_completions_max_tokens_field: entry.max_tokens_field,
                    ..Default::default()
                };
                config.apply_overrides(&mut svc_config);
                let key = api_key
                    .ok_or_else(|| format!("no API key for '{}' provider", config.provider))?;
                let client = OpenAiClient::new(svc_config, key)
                    .map_err(|e| format!("failed to create OpenAI client: {e}"))?;
                Ok(Self {
                    client: Arc::new(client),
                })
            }
            ProviderProtocol::AnthropicMessages => {
                let key = api_key
                    .ok_or_else(|| format!("no API key for '{}' provider", config.provider))?;
                let mut anthro_cfg = crate::anthropic::AnthropicConfig::default();
                // If the account doesn't specify a base_url, use the catalog default.
                if config.base_url.is_none() {
                    anthro_cfg.base_url = entry.default_base_url.to_string();
                }
                anthro_cfg.apply_overrides(config);
                let client = AnthropicClient::new(anthro_cfg, key)
                    .map_err(|e| format!("failed to create Anthropic client: {e}"))?;
                Ok(Self {
                    client: Arc::new(client),
                })
            }
            ProviderProtocol::GoogleGenerativeAi => {
                let key = api_key
                    .ok_or_else(|| format!("no API key for '{}' provider", config.provider))?;
                let mut google_cfg = crate::google::GoogleConfig::default();
                // If the account doesn't specify a base_url, use the catalog default.
                if config.base_url.is_none() {
                    google_cfg.base_url = entry.default_base_url.to_string();
                }
                google_cfg.apply_overrides(config);
                let client = crate::google::GoogleClient::new(google_cfg, key)
                    .map_err(|e| format!("failed to create Google client: {e}"))?;
                Ok(Self {
                    client: Arc::new(client),
                })
            }
            ProviderProtocol::Mistral => {
                let key = api_key
                    .ok_or_else(|| format!("no API key for '{}' provider", config.provider))?;
                let mut mistral_cfg = crate::mistral::MistralConfig::default();
                if config.base_url.is_none() {
                    mistral_cfg.base_url = entry.default_base_url.to_string();
                }
                mistral_cfg.apply_overrides(config);
                let client = MistralClient::new(mistral_cfg, key)
                    .map_err(|e| format!("failed to create Mistral client: {e}"))?;
                Ok(Self {
                    client: Arc::new(client),
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
        on_chunk: &mut dyn FnMut(CompletionChunkKind, String) -> io::Result<()>,
    ) -> Result<ChatTurnResult, InferenceError> {
        self.client.chat_completion_turn_streaming(params, on_chunk)
    }

    /// Return the provider slug (e.g. "openai", "anthropic").
    pub fn provider_slug(&self) -> &'static str {
        self.client.provider_slug()
    }

    pub fn list_models(&self) -> Result<Vec<String>, InferenceError> {
        self.client.list_models()
    }
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
    use crate::mistral::MistralClient;
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
    fn from_mistral_constructs_provider() {
        let config = crate::mistral::MistralConfig::default();
        let client = MistralClient::new(config, "test-key".into()).unwrap();
        let _provider = InferenceProvider::from_mistral(client);
    }

    #[test]
    fn from_account_config_mistral_missing_key_errors() {
        let cfg = AccountConfig::simple("mistral", "mistral");
        let err = InferenceProvider::from_account_config(&cfg, None).unwrap_err();
        assert!(err.contains("no API key"), "{err}");
    }

    #[test]
    fn from_account_config_mistral_succeeds() {
        let cfg = AccountConfig::simple("mistral", "mistral");
        let result = InferenceProvider::from_account_config(&cfg, Some("key".into()));
        assert!(result.is_ok(), "{:?}", result.err());
    }
}
