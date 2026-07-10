use std::io;
use std::sync::Arc;
use std::sync::mpsc;

use crate::anthropic::AnthropicClient;
use crate::openai::{
    ChatRequestMessage, ChatToolDefinition, ChatTurnResult, CompletionChunkKind, OpenAiClient,
    RetryCallback,
};
use tai_proto::InferenceError;

#[derive(Debug, Clone)]
pub enum InferenceProvider {
    OpenAi(Arc<OpenAiClient>),
    Anthropic(Arc<AnthropicClient>),
}

impl InferenceProvider {
    pub fn from_openai(client: OpenAiClient) -> Self {
        Self::OpenAi(Arc::new(client))
    }

    pub fn from_anthropic(client: AnthropicClient) -> Self {
        Self::Anthropic(Arc::new(client))
    }

    /// Create a provider from an account config + credential key.
    /// Applies all account-level overrides (base_url, streaming, timeouts)
    /// onto the service config before constructing the client.
    pub fn from_account_config(
        config: &crate::accounts::AccountConfig,
        api_key: Option<String>,
    ) -> Result<Self, String> {
        match config.provider.as_str() {
            "openai" | "openai_compatible" | "opencode" | "opencode-go" => {
                let mut svc_config = crate::openai::load_service_config().unwrap_or_default();
                config.apply_overrides(&mut svc_config);
                let key = api_key.ok_or_else(|| "no API key for OpenAI provider".to_string())?;
                let client = OpenAiClient::new(svc_config, key)
                    .map_err(|e| format!("failed to create OpenAI client: {e}"))?;
                Ok(Self::OpenAi(Arc::new(client)))
            }
            "anthropic" => {
                let key = api_key.ok_or_else(|| "no API key for Anthropic provider".to_string())?;
                let mut anthropic_cfg = crate::anthropic::AnthropicConfig::default();
                anthropic_cfg.apply_overrides(config);
                let client = AnthropicClient::new(anthropic_cfg, key)
                    .map_err(|e| format!("failed to create Anthropic client: {e}"))?;
                Ok(Self::Anthropic(Arc::new(client)))
            }
            other => Err(format!("unknown provider: {other}")),
        }
    }

    pub fn chat_completion_turn(
        &self,
        model: &str,
        messages: &[ChatRequestMessage],
        tools: &[ChatToolDefinition],
        on_retry: &mut Option<RetryCallback>,
        cancel_rx: Option<&mpsc::Receiver<()>>,
    ) -> Result<ChatTurnResult, InferenceError> {
        match self {
            Self::OpenAi(client) => client
                .chat_completion_turn(model, messages, tools, on_retry, cancel_rx)
                .map_err(inference_error_from_openai),
            Self::Anthropic(client) => client
                .chat_completion_turn(model, messages, tools, on_retry, cancel_rx)
                .map_err(inference_error_from_anthropic),
        }
    }

    pub fn chat_completion_turn_streaming<F>(
        &self,
        model: &str,
        messages: &[ChatRequestMessage],
        tools: &[ChatToolDefinition],
        on_retry: &mut Option<RetryCallback>,
        cancel_rx: Option<&mpsc::Receiver<()>>,
        on_chunk: F,
    ) -> Result<ChatTurnResult, InferenceError>
    where
        F: FnMut(CompletionChunkKind, String) -> io::Result<()>,
    {
        match self {
            Self::OpenAi(client) => client
                .chat_completion_turn_streaming(
                    model, messages, tools, on_retry, cancel_rx, on_chunk,
                )
                .map_err(inference_error_from_openai),
            Self::Anthropic(client) => client
                .chat_completion_turn_streaming(
                    model, messages, tools, on_retry, cancel_rx, on_chunk,
                )
                .map_err(inference_error_from_anthropic),
        }
    }

    pub fn list_models(&self) -> Result<Vec<String>, InferenceError> {
        match self {
            Self::OpenAi(client) => client
                .validate_and_list_models()
                .map_err(inference_error_from_openai),
            Self::Anthropic(client) => client
                .validate_and_list_models()
                .map_err(inference_error_from_anthropic),
        }
    }
}

fn inference_error_from_openai(e: crate::openai::OpenAiError) -> InferenceError {
    match e {
        crate::openai::OpenAiError::Unauthorized { status, detail } => {
            InferenceError::Unauthorized { status, detail }
        }
        crate::openai::OpenAiError::RateLimited {
            retry_after_secs,
            detail,
        } => InferenceError::RateLimited {
            retry_after_secs,
            detail,
        },
        crate::openai::OpenAiError::ServerError { status, detail } => {
            InferenceError::ServerError { status, detail }
        }
        crate::openai::OpenAiError::ClientError { status, detail } => {
            InferenceError::ClientError { status, detail }
        }
        crate::openai::OpenAiError::EmptyResponse => InferenceError::EmptyResponse,
        crate::openai::OpenAiError::Cancelled => InferenceError::Cancelled,
        crate::openai::OpenAiError::Io(e) => InferenceError::Io(e.to_string()),
    }
}

fn inference_error_from_anthropic(e: crate::anthropic::AnthropicError) -> InferenceError {
    match e {
        crate::anthropic::AnthropicError::Unauthorized { status, detail } => {
            InferenceError::Unauthorized { status, detail }
        }
        crate::anthropic::AnthropicError::RateLimited {
            retry_after_secs,
            detail,
        } => InferenceError::RateLimited {
            retry_after_secs,
            detail,
        },
        crate::anthropic::AnthropicError::ServerError { status, detail } => {
            InferenceError::ServerError { status, detail }
        }
        crate::anthropic::AnthropicError::ClientError { status, detail } => {
            InferenceError::ClientError { status, detail }
        }
        crate::anthropic::AnthropicError::EmptyResponse => InferenceError::EmptyResponse,
        crate::anthropic::AnthropicError::Cancelled => InferenceError::Cancelled,
        crate::anthropic::AnthropicError::Io(e) => InferenceError::Io(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accounts::AccountConfig;
    use crate::openai::ServiceConfig;

    #[test]
    fn from_openai_constructs_provider() {
        let config = ServiceConfig::default();
        let client = OpenAiClient::new(config, "test-key".into()).unwrap();
        let provider = InferenceProvider::from_openai(client);
        assert!(matches!(provider, InferenceProvider::OpenAi(_)));
    }

    #[test]
    fn from_anthropic_constructs_provider() {
        let config = crate::anthropic::AnthropicConfig::default();
        let client = AnthropicClient::new(config, "test-key".into()).unwrap();
        let provider = InferenceProvider::from_anthropic(client);
        assert!(matches!(provider, InferenceProvider::Anthropic(_)));
    }

    #[test]
    fn from_account_config_unknown_provider_errors() {
        let cfg = AccountConfig {
            name: "unknown".to_string(),
            provider: "nonexistent".to_string(),
            base_url: None,
            streaming: None,
            retry_max_attempts: None,
            connect_timeout_secs: None,
            request_timeout_secs: None,
        };
        let err = InferenceProvider::from_account_config(&cfg, Some("key".into())).unwrap_err();
        assert!(err.contains("unknown provider"), "{err}");
    }

    #[test]
    fn from_account_config_anthropic_requires_key() {
        let cfg = AccountConfig {
            name: "claude".to_string(),
            provider: "anthropic".to_string(),
            base_url: None,
            streaming: None,
            retry_max_attempts: None,
            connect_timeout_secs: None,
            request_timeout_secs: None,
        };
        let err = InferenceProvider::from_account_config(&cfg, None).unwrap_err();
        assert!(err.contains("no API key"), "{err}");
    }

    #[test]
    fn from_account_config_openai_missing_key_errors() {
        let cfg = AccountConfig {
            name: "openai".to_string(),
            provider: "openai".to_string(),
            base_url: None,
            streaming: None,
            retry_max_attempts: None,
            connect_timeout_secs: None,
            request_timeout_secs: None,
        };
        let err = InferenceProvider::from_account_config(&cfg, None).unwrap_err();
        assert!(err.contains("no API key"), "{err}");
    }

    #[test]
    fn inference_error_from_openai_roundtrip() {
        let cases: Vec<(crate::openai::OpenAiError, &str)> = vec![
            (
                crate::openai::OpenAiError::Unauthorized {
                    status: 401,
                    detail: "bad key".into(),
                },
                "unauthorized",
            ),
            (crate::openai::OpenAiError::Cancelled, "cancelled"),
            (
                crate::openai::OpenAiError::Io(std::io::Error::other("wire error")),
                "wire error",
            ),
        ];
        for (openai_err, expected) in cases {
            let ie: InferenceError = inference_error_from_openai(openai_err);
            let msg = ie.to_string().to_lowercase();
            assert!(msg.contains(expected), "expected '{expected}' in '{msg}'");
        }
    }

    #[test]
    fn inference_error_from_anthropic_roundtrip() {
        let cases: Vec<(crate::anthropic::AnthropicError, &str)> = vec![
            (
                crate::anthropic::AnthropicError::Unauthorized {
                    status: 401,
                    detail: "bad key".into(),
                },
                "unauthorized",
            ),
            (crate::anthropic::AnthropicError::Cancelled, "cancelled"),
            (
                crate::anthropic::AnthropicError::Io(std::io::Error::other("wire error")),
                "wire error",
            ),
        ];
        for (err, expected) in cases {
            let ie: InferenceError = inference_error_from_anthropic(err);
            let msg = ie.to_string().to_lowercase();
            assert!(msg.contains(expected), "expected '{expected}' in '{msg}'");
        }
    }

    #[test]
    fn anthropic_provider_list_models_returns_known() {
        let config = crate::anthropic::AnthropicConfig::default();
        let client = AnthropicClient::new(config, "test-key".into()).unwrap();
        let provider = InferenceProvider::Anthropic(Arc::new(client));
        let models = provider.list_models().unwrap();
        assert!(!models.is_empty());
        assert!(models.contains(&"claude-sonnet-4-20250514".to_string()));
    }

    #[test]
    fn anthropic_provider_errors_on_empty_response() {
        let config = crate::anthropic::AnthropicConfig::default();
        let client = AnthropicClient::new(config, "test-key".into()).unwrap();
        let provider = InferenceProvider::Anthropic(Arc::new(client));
        // list_models should work fine with the static list.
        assert!(provider.list_models().is_ok());
    }
}
