use super::{MaxTokensField, OpenAiClient, RequestFormat};
use serde::Deserialize;
use std::{collections::HashMap, fs, io, path::PathBuf};
use tai_proto::ContextConfig;

const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
const DEFAULT_MODEL_LIST_PATH: &str = "/models";
const DEFAULT_RESPONSES_PATH: &str = "/responses";
const DEFAULT_CHAT_COMPLETIONS_PATH: &str = "/chat/completions";

/// Daemon-level configuration from config.toml.
///
/// Only truly global settings belong here.  All provider-level
/// configuration (endpoints, timeouts, retry, etc.) belongs in
/// accounts.toml.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct DaemonConfig {
    #[serde(default)]
    pub max_turns: Option<u32>,
    #[serde(default)]
    pub context: ContextConfig,
}

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
    pub context_window_config: crate::providers::ContextWindowConfig,
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
            context_window_config: crate::providers::ContextWindowConfig::default(),
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
            context: ContextConfig::default(),
            programmatic_tool_calling: false,
        }
    }
}

pub fn config_path() -> io::Result<PathBuf> {
    let config_dir = dirs::config_dir().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "could not determine standard config directory",
        )
    })?;
    Ok(config_dir.join("tai-daemon").join("config.toml"))
}

/// Load daemon-level configuration from config.toml.
///
/// Emits `tracing::warn!` for any provider-level fields that are still
/// present in config.toml (they should be in accounts.toml instead).
pub fn load_daemon_config() -> io::Result<DaemonConfig> {
    let path = config_path()?;
    if !path.exists() {
        return Ok(DaemonConfig::default());
    }
    let raw = fs::read_to_string(&path).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("failed to read config at {}: {error}", path.display()),
        )
    })?;
    // Parse only the daemon-level fields (unknown fields are silently
    // ignored thanks to #[serde(default)]).
    let config: DaemonConfig = toml::from_str(&raw).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("failed to parse config at {}: {error}", path.display()),
        )
    })?;
    Ok(config)
}

/// Deprecated.  Use [`load_daemon_config`] instead.
///
/// Provider-level fields in config.toml are no longer read.  This function
/// returns default provider settings; configure those in accounts.toml.
#[deprecated(
    since = "0.1.0",
    note = "provider-level config has moved to accounts.toml; use load_daemon_config() for daemon settings"
)]
pub fn load_service_config() -> io::Result<ServiceConfig> {
    tracing::warn!(
        "load_service_config() is deprecated.  Provider-level config is no longer read from \
         config.toml; configure providers in accounts.toml instead."
    );
    // Also surface deprecation warnings for any stale fields.
    if let Err(e) = load_daemon_config() {
        tracing::warn!("error reading config.toml while checking for deprecated fields: {e}");
    }
    Ok(ServiceConfig::default())
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
        crate::providers::model_request_format(self.provider_slug, model)
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

    /// Resolve which JSON field to use for the token limit for a given model.
    /// Per-model overrides take precedence over the default.
    /// Returns whether programmatic tool calling should be enabled for a given model.
    ///
    /// Auto-enables for gpt-5.6+ models when the default is Responses API.
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
        let mut config = ServiceConfig::default();
        config.programmatic_tool_calling = true;
        // Override takes precedence regardless of model.
        assert!(config.programmatic_tool_calling_for_model("gpt-4.1"));
        assert!(config.programmatic_tool_calling_for_model("claude-3"));
    }

    #[test]
    fn programmatic_tool_calling_auto_enables_for_gpt_5_6_responses() {
        let mut config = ServiceConfig::default();
        config.default_request_format = RequestFormat::Responses;
        // Models not in the catalog fall back to default_request_format.
        // When the default is Responses, gpt-5.6 models auto-enable.
        assert!(config.programmatic_tool_calling_for_model("gpt-5.6-chat"));
        // Known models with openai_responses: false use ChatCompletions
        // instead, so auto-enable does not fire.
        assert!(!config.programmatic_tool_calling_for_model("gpt-5.6-sol"));
    }

    #[test]
    fn programmatic_tool_calling_not_auto_enabled_for_gpt_5_6_chat_completions() {
        let mut config = ServiceConfig::default();
        config.default_request_format = RequestFormat::ChatCompletions;
        // No auto-enable when using Chat Completions format.
        assert!(!config.programmatic_tool_calling_for_model("gpt-5.6-sol"));
    }

    #[test]
    fn programmatic_tool_calling_not_auto_enabled_for_other_models() {
        let mut config = ServiceConfig::default();
        config.default_request_format = RequestFormat::Responses;
        assert!(!config.programmatic_tool_calling_for_model("gpt-4.1"));
        assert!(!config.programmatic_tool_calling_for_model("gpt-5.5"));
        assert!(!config.programmatic_tool_calling_for_model("claude-3-opus"));
    }

    #[test]
    fn request_format_for_model_uses_catalog_lookup() {
        let mut config = ServiceConfig::default();
        config.default_request_format = RequestFormat::Responses;
        // Known model with openai_responses: false should return ChatCompletions.
        config.provider_slug = "openai";
        assert_eq!(
            config.request_format_for_model("gpt-4.1"),
            RequestFormat::ChatCompletions
        );
        // Unknown model falls back to default_request_format.
        assert_eq!(
            config.request_format_for_model("totally-unknown-xyz"),
            RequestFormat::Responses
        );
    }

    #[test]
    fn programmatic_tool_calling_override_wins_over_auto_disable() {
        let mut config = ServiceConfig::default();
        config.default_request_format = RequestFormat::ChatCompletions;
        config.programmatic_tool_calling = true;
        // Account-level override takes precedence even when auto-enable
        // would not trigger (ChatCompletions format).
        assert!(config.programmatic_tool_calling_for_model("gpt-5.6-sol"));
        assert!(config.programmatic_tool_calling_for_model("gpt-4.1"));
    }
}
