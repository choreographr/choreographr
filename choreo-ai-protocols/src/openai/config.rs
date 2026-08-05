use super::{MaxTokensField, OpenAiClient, RequestFormat};
use choreo_proto::ContextConfig;
use std::{collections::HashMap, io};

const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
const DEFAULT_MODEL_LIST_PATH: &str = "/models";
const DEFAULT_RESPONSES_PATH: &str = "/responses";
const DEFAULT_CHAT_COMPLETIONS_PATH: &str = "/chat/completions";

/// Per-client configuration for an OpenAI-compatible service.
///
/// Provider-level settings (endpoints, timeouts, retry, token limits,
/// request format) live here; daemon-level settings live in
/// `choreo-daemon`'s `config::DaemonConfig`.
#[derive(Debug, Clone)]
pub struct ServiceConfig {
    pub base_url: String,
    pub model_list_path: String,
    pub responses_path: String,
    pub chat_completions_path: String,
    pub default_request_format: RequestFormat,
    pub provider_slug: &'static str,
    pub chat_completions_max_tokens: Option<u32>,
    pub model_max_tokens: HashMap<String, u32>,
    pub context_window_config: crate::ContextWindowConfig,
    pub responses_max_output_tokens: Option<u32>,
    pub model_responses_max_output_tokens: HashMap<String, u32>,
    pub chat_completions_max_tokens_field: MaxTokensField,
    pub model_max_tokens_fields: HashMap<String, MaxTokensField>,
    pub streaming: bool,
    pub stream_options: bool,
    pub max_turns: Option<u32>,
    pub retry_max_attempts: u32,
    pub retry_initial_backoff_ms: u64,
    pub retry_max_backoff_ms: u64,
    pub connect_timeout_secs: u64,
    pub request_timeout_secs: u64,
    /// Hard wall-clock deadline for a single HTTP request attempt, including
    /// the streaming body read; 0 disables.  Unlike `request_timeout_secs` (an
    /// idle/no-progress timeout that resets per chunk), this fires even when a
    /// provider trickles keep-alive bytes, so it bounds a stalled SSE stream.
    /// It covers one attempt: each retry restarts the deadline, so retries
    /// plus their backoff can exceed this value in aggregate.
    pub total_timeout_secs: u64,
    pub context: ContextConfig,
    pub programmatic_tool_calling: bool,
}

impl Default for ServiceConfig {
    fn default() -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.to_string(),
            model_list_path: DEFAULT_MODEL_LIST_PATH.to_string(),
            responses_path: DEFAULT_RESPONSES_PATH.to_string(),
            chat_completions_path: DEFAULT_CHAT_COMPLETIONS_PATH.to_string(),
            default_request_format: RequestFormat::ChatCompletions,
            provider_slug: "openai",
            chat_completions_max_tokens: None,
            model_max_tokens: HashMap::new(),
            context_window_config: crate::ContextWindowConfig::default(),
            responses_max_output_tokens: None,
            model_responses_max_output_tokens: HashMap::new(),
            chat_completions_max_tokens_field: MaxTokensField::MaxCompletionTokens,
            model_max_tokens_fields: HashMap::new(),
            streaming: true,
            stream_options: true,
            max_turns: None,
            retry_max_attempts: 5,
            retry_initial_backoff_ms: 1000,
            retry_max_backoff_ms: 30000,
            connect_timeout_secs: 30,
            request_timeout_secs: 120,
            total_timeout_secs: 3600,
            context: ContextConfig::default(),
            programmatic_tool_calling: false,
        }
    }
}

pub(crate) fn endpoint_url(base_url: &str, path: &str) -> io::Result<String> {
    if !path.starts_with('/') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "path must start with '/'",
        ));
    }
    Ok(format!("{}{}", base_url.trim_end_matches('/'), path))
}

impl ServiceConfig {
    /// Resolve the request format for a model: catalog lookup first,
    /// falling back to the configured default for unknown models.
    pub fn request_format_for_model(&self, model: &str) -> RequestFormat {
        crate::catalog::model_request_format(self.provider_slug, model)
            .unwrap_or(self.default_request_format)
    }

    pub fn max_output_tokens_for_model(&self, model: &str) -> Option<u32> {
        self.model_responses_max_output_tokens
            .get(model)
            .copied()
            .or(self.responses_max_output_tokens)
    }

    pub fn max_tokens_for_model(&self, model: &str) -> Option<u32> {
        self.model_max_tokens
            .get(model)
            .copied()
            .or(self.chat_completions_max_tokens)
    }

    pub fn context_window_for_model(&self, model: &str) -> Option<u32> {
        self.context_window_config.context_window_for_model(model)
    }

    /// Returns whether programmatic tool calling should be enabled for a given model.
    ///
    /// Auto-enables for gpt-5.6+ models when the default is the Responses API.
    /// The account-level `programmatic_tool_calling` override takes precedence.
    pub fn programmatic_tool_calling_for_model(&self, model: &str) -> bool {
        if self.programmatic_tool_calling {
            return true;
        }
        // Auto-enable for gpt-5.6 models when using the Responses API
        if self.request_format_for_model(model) == RequestFormat::Responses
            && model.starts_with("gpt-5.6")
        {
            return true;
        }
        false
    }

    pub fn max_tokens_field_for_model(&self, model: &str) -> MaxTokensField {
        let field = self
            .model_max_tokens_fields
            .get(model)
            .copied()
            .unwrap_or(self.chat_completions_max_tokens_field);
        tracing::debug!(
            model = %model,
            ?field,
            "max_tokens_field_for_model"
        );
        field
    }

    /// Resolve the `(max_tokens, max_completion_tokens)` pair for a model.
    ///
    /// Which field to use depends on the model family (o-series uses
    /// `max_completion_tokens`, gpt-series uses `max_tokens`).
    /// Returns `(Some(n), None)` or `(None, Some(n))` depending on the
    /// resolved field.  If `max_tokens_for_model` returns `None`, both
    /// fields are `None` (the API will use its own default).
    pub(crate) fn max_tokens_field_pair(&self, model: &str) -> (Option<u32>, Option<u32>) {
        let max_tokens = self.max_tokens_for_model(model);
        match self.max_tokens_field_for_model(model) {
            MaxTokensField::MaxTokens => (max_tokens, None),
            MaxTokensField::MaxCompletionTokens => (None, max_tokens),
        }
    }
}

pub fn validate_and_list_models(config: &ServiceConfig, api_key: &str) -> io::Result<Vec<String>> {
    let client = OpenAiClient::new(config.clone(), api_key.to_string())?;
    Ok(client.validate_and_list_models()?)
}

pub fn completion(
    config: &ServiceConfig,
    api_key: &str,
    model: &str,
    prompt: &str,
) -> io::Result<String> {
    let client = OpenAiClient::new(config.clone(), api_key.to_string())?;
    Ok(client.completion(model, prompt)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn programmatic_tool_calling_default_is_false() {
        let config = ServiceConfig::default();
        assert!(!config.programmatic_tool_calling_for_model("gpt-4.1"));
    }

    #[test]
    fn programmatic_tool_calling_account_override() {
        let config = ServiceConfig {
            programmatic_tool_calling: true,
            ..Default::default()
        };
        // Override takes precedence regardless of model.
        assert!(config.programmatic_tool_calling_for_model("gpt-4.1"));
        assert!(config.programmatic_tool_calling_for_model("claude-3"));
    }

    #[test]
    fn programmatic_tool_calling_auto_enables_for_gpt_5_6_responses() {
        let config = ServiceConfig {
            default_request_format: RequestFormat::Responses,
            ..Default::default()
        };
        // Models not in the catalog fall back to default_request_format.
        // When the default is Responses, gpt-5.6 models auto-enable.
        assert!(config.programmatic_tool_calling_for_model("gpt-5.6-chat"));
        // gpt-5.6-sol is Responses in the catalog (OpenAI's default API),
        // so auto-enable fires even without the account-level override.
        assert!(config.programmatic_tool_calling_for_model("gpt-5.6-sol"));
    }

    #[test]
    fn programmatic_tool_calling_not_auto_enabled_with_chat_completions_default() {
        let config = ServiceConfig {
            default_request_format: RequestFormat::ChatCompletions,
            ..Default::default()
        };
        // An unknown gpt-5.6 model falls back to the ChatCompletions default,
        // so auto-enable (Responses-only) does not fire.
        assert!(!config.programmatic_tool_calling_for_model("gpt-5.6-future-unknown"));
    }

    #[test]
    fn programmatic_tool_calling_not_auto_enabled_for_other_models() {
        let config = ServiceConfig {
            default_request_format: RequestFormat::Responses,
            ..Default::default()
        };
        assert!(!config.programmatic_tool_calling_for_model("gpt-4.1"));
        assert!(!config.programmatic_tool_calling_for_model("gpt-5.5"));
        assert!(!config.programmatic_tool_calling_for_model("claude-3-opus"));
    }

    #[test]
    fn request_format_for_model_uses_catalog_lookup() {
        let config = ServiceConfig {
            default_request_format: RequestFormat::Responses,
            provider_slug: "openai",
            ..Default::default()
        };
        assert_eq!(
            config.request_format_for_model("gpt-4.1"),
            RequestFormat::Responses
        );
        // Unknown model falls back to default_request_format.
        assert_eq!(
            config.request_format_for_model("totally-unknown-xyz"),
            RequestFormat::Responses
        );
    }

    #[test]
    fn programmatic_tool_calling_override_wins_over_auto_disable() {
        let config = ServiceConfig {
            default_request_format: RequestFormat::ChatCompletions,
            programmatic_tool_calling: true,
            ..Default::default()
        };
        // Account-level override takes precedence even when auto-enable
        // would not trigger (ChatCompletions format).
        assert!(config.programmatic_tool_calling_for_model("gpt-5.6-sol"));
        assert!(config.programmatic_tool_calling_for_model("gpt-4.1"));
    }
}
