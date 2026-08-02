use std::io;
use std::sync::Arc;

use choreo_ai_protocols::openai::{OpenAiClient, ServiceConfig};
use choreo_ai_protocols::{
    AnthropicClient, AnthropicConfig, ChatTurnRequest, ChatTurnResult, GoogleClient, GoogleConfig,
    ProviderClient, ProviderProtocol, StreamEvent, lookup_provider,
};
use choreo_proto::InferenceError;

/// A concrete, protocol-erased provider facade.
///
/// Wraps any one [`ProviderClient`] behind a `dyn` pointer so the rest of
/// the daemon never sees which wire protocol is in use.  Also remembers the
/// catalog slug (e.g. `"opencode"` even when the client is an
/// `OpenAiClient`, which always reports `"openai"`), which is what catalog
/// lookups for reasoning/context windows need.
///
/// This is the only daemon type that knows about provider *protocols*; all
/// wire-format knowledge lives in `choreo-ai-protocols`.
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
                let mut svc_config = ServiceConfig {
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
                let mut anthro_cfg = AnthropicConfig::default();
                // If the account doesn't specify a base_url, use the catalog default.
                if config.base_url.is_none() {
                    anthro_cfg.base_url = entry.base_url.to_string();
                }
                let overrides = config.provider_overrides();
                anthro_cfg.apply_overrides(&overrides);
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
                let mut google_cfg = GoogleConfig::default();
                // If the account doesn't specify a base_url, use the catalog default.
                if config.base_url.is_none() {
                    google_cfg.base_url = entry.base_url.to_string();
                }
                let overrides = config.provider_overrides();
                google_cfg.apply_overrides(&overrides);
                let client = GoogleClient::new(google_cfg, key)
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
        let start = std::time::Instant::now();
        let model = params.model.to_string();
        let result = self.client.chat_completion_turn(params);
        self.record_api_metrics(&model, start, &result);
        result
    }

    pub fn chat_completion_turn_streaming(
        &self,
        params: ChatTurnRequest<'_>,
        on_event: &mut dyn FnMut(StreamEvent) -> io::Result<()>,
    ) -> Result<ChatTurnResult, InferenceError> {
        let start = std::time::Instant::now();
        let model = params.model.to_string();
        let result = self.client.chat_completion_turn_streaming(params, on_event);
        self.record_api_metrics(&model, start, &result);
        result
    }

    /// Record API-call metrics around a provider result.  Timing lives here
    /// (not inside `choreo-ai-protocols`) so the provider crates stay free of
    /// daemon concerns.  The metric label is the catalog slug — more precise
    /// than the protocol name (e.g. "opencode" rather than "openai").
    fn record_api_metrics<T>(
        &self,
        model: &str,
        start: std::time::Instant,
        result: &Result<T, InferenceError>,
    ) {
        let elapsed = start.elapsed().as_secs_f64();
        crate::metrics::record_api_call(model, self.slug, elapsed);
        if let Err(e) = result {
            crate::metrics::record_api_error(model, inference_error_label(e));
        }
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
            .or_else(|| choreo_ai_protocols::lookup_context_window(self.slug, model))
    }

    pub fn list_models(&self) -> Result<Vec<String>, InferenceError> {
        self.client.list_models()
    }

    pub fn supports_programmatic_tool_calling(&self, model: &str) -> bool {
        self.client.supports_programmatic_tool_calling(model)
    }
}

/// Map an [`InferenceError`] variant to a stable label string for metrics.
///
/// Mirrors the label set the provider crates used to emit internally; the
/// mapping moved here when metrics recording moved to the daemon boundary.
fn inference_error_label(e: &InferenceError) -> &'static str {
    match e {
        InferenceError::Unauthorized { .. } => "unauthorized",
        InferenceError::RateLimited { .. } => "rate_limited",
        InferenceError::ServerError { .. } => "server_error",
        InferenceError::ClientError { .. } => "client_error",
        InferenceError::EmptyResponse => "empty_response",
        InferenceError::Cancelled => "cancelled",
        InferenceError::TruncatedToolCall { .. } => "truncated_tool_call",
        InferenceError::Io(_) => "other",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accounts::AccountConfig;
    use choreo_ai_protocols::openai::ServiceConfig;

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
    fn from_account_config_google_succeeds() {
        let cfg = AccountConfig::simple("gemini", "google");
        let result = InferenceProvider::from_account_config(&cfg, Some("key".into()));
        assert!(result.is_ok(), "{:?}", result.err());
    }

    #[test]
    fn from_account_config_google_missing_key_errors() {
        let cfg = AccountConfig::simple("gemini", "google");
        let err = InferenceProvider::from_account_config(&cfg, None).unwrap_err();
        assert!(err.contains("no API key"), "{err}");
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
        let mut cfg = ServiceConfig::default();
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
