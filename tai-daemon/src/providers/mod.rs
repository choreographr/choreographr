use std::io;
use std::sync::Arc;
use std::sync::mpsc;

use crate::openai::{
    ChatRequestMessage, ChatToolDefinition, ChatTurnResult, CompletionChunkKind, OpenAiClient,
    RetryCallback,
};
use tai_proto::InferenceError;

#[derive(Debug, Clone)]
pub enum InferenceProvider {
    OpenAi(Arc<OpenAiClient>),
    Anthropic,
}

impl InferenceProvider {
    pub fn from_openai(client: OpenAiClient) -> Self {
        Self::OpenAi(Arc::new(client))
    }

    /// Create a provider from an account config + credential key.
    /// Applies all account-level overrides (base_url, streaming, timeouts)
    /// onto the service config before constructing the client.
    pub fn from_account_config(
        config: &crate::accounts::AccountConfig,
        api_key: Option<String>,
    ) -> Result<Self, String> {
        match config.provider.as_str() {
            "openai" | "openai_compatible" => {
                let mut svc_config = crate::openai::load_service_config().unwrap_or_default();
                config.apply_overrides(&mut svc_config);
                let key = api_key.ok_or_else(|| "no API key for OpenAI provider".to_string())?;
                let client = OpenAiClient::new(svc_config, key)
                    .map_err(|e| format!("failed to create OpenAI client: {e}"))?;
                Ok(Self::OpenAi(Arc::new(client)))
            }
            "anthropic" => Err("Anthropic provider is not yet implemented".to_string()),
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
            Self::Anthropic => Err(InferenceError::Io(
                "Anthropic provider is not yet implemented".into(),
            )),
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
            Self::Anthropic => Err(InferenceError::Io(
                "Anthropic provider is not yet implemented".into(),
            )),
        }
    }

    pub fn list_models(&self) -> Result<Vec<String>, InferenceError> {
        match self {
            Self::OpenAi(client) => client
                .validate_and_list_models()
                .map_err(inference_error_from_openai),
            Self::Anthropic => Err(InferenceError::Io(
                "Anthropic provider is not yet implemented".into(),
            )),
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
    fn from_account_config_unknown_provider_errors() {
        let cfg = AccountConfig {
            name: "unknown".to_string(),
            provider: "nonexistent".to_string(),
            model: None,
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
    fn from_account_config_anthropic_errors_not_implemented() {
        let cfg = AccountConfig {
            name: "claude".to_string(),
            provider: "anthropic".to_string(),
            model: None,
            base_url: None,
            streaming: None,
            retry_max_attempts: None,
            connect_timeout_secs: None,
            request_timeout_secs: None,
        };
        let err = InferenceProvider::from_account_config(&cfg, Some("key".into())).unwrap_err();
        assert!(
            err.contains("not yet implemented"),
            "expected 'not yet implemented', got: {err}"
        );
    }

    #[test]
    fn from_account_config_openai_missing_key_errors() {
        let cfg = AccountConfig {
            name: "openai".to_string(),
            provider: "openai".to_string(),
            model: None,
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
    fn anthropic_provider_returns_error_on_all_operations() {
        let provider = InferenceProvider::Anthropic;
        let err = provider.list_models().unwrap_err();
        assert!(err.to_string().contains("not yet implemented"));
    }
}
