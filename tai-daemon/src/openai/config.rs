use super::{OpenAiClient, RequestFormat};
use serde::Deserialize;
use std::{collections::HashMap, fs, io, path::PathBuf};

const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
const DEFAULT_MODEL_LIST_PATH: &str = "/models";
const DEFAULT_RESPONSES_PATH: &str = "/responses";
const DEFAULT_CHAT_COMPLETIONS_PATH: &str = "/chat/completions";

#[derive(Debug, Clone, Deserialize)]
pub struct AuthConfig {
    pub api_key: String,
    #[serde(default = "default_base_url")]
    pub base_url: String,
    #[serde(default = "default_model_list_path")]
    pub model_list_path: String,
    #[serde(default = "default_responses_path")]
    pub responses_path: String,
    #[serde(default = "default_chat_completions_path")]
    pub chat_completions_path: String,
    #[serde(default = "default_request_format")]
    pub default_request_format: RequestFormat,
    #[serde(default)]
    pub model_request_formats: HashMap<String, RequestFormat>,
    #[serde(default)]
    pub chat_completions_max_tokens: Option<u32>,
    #[serde(default)]
    pub model_max_tokens: HashMap<String, u32>,
    #[serde(default = "default_streaming")]
    pub streaming: bool,
}

fn default_base_url() -> String {
    DEFAULT_BASE_URL.to_string()
}

fn default_model_list_path() -> String {
    DEFAULT_MODEL_LIST_PATH.to_string()
}

fn default_responses_path() -> String {
    DEFAULT_RESPONSES_PATH.to_string()
}

fn default_chat_completions_path() -> String {
    DEFAULT_CHAT_COMPLETIONS_PATH.to_string()
}

fn default_request_format() -> RequestFormat {
    RequestFormat::ChatCompletions
}

fn default_streaming() -> bool {
    true
}

pub fn auth_config_path() -> io::Result<PathBuf> {
    let config_dir = dirs::config_dir().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "could not determine standard config directory",
        )
    })?;
    Ok(config_dir.join("tai-daemon").join("auth.toml"))
}

pub fn load_auth_config() -> io::Result<AuthConfig> {
    let path = auth_config_path()?;
    let raw = fs::read_to_string(&path).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("failed to read auth config at {}: {error}", path.display()),
        )
    })?;

    let config: AuthConfig = toml::from_str(&raw).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("failed to parse auth config at {}: {error}", path.display()),
        )
    })?;

    if config.api_key.trim().is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "auth config at {} contains an empty api_key",
                path.display()
            ),
        ));
    }

    Ok(config)
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

impl AuthConfig {
    pub fn request_format_for_model(&self, model: &str) -> RequestFormat {
        self.model_request_formats
            .get(model)
            .copied()
            .unwrap_or(self.default_request_format)
    }

    pub fn max_tokens_for_model(&self, model: &str) -> Option<u32> {
        self.model_max_tokens
            .get(model)
            .copied()
            .or(self.chat_completions_max_tokens)
    }
}

pub async fn validate_and_list_models(config: &AuthConfig) -> io::Result<Vec<String>> {
    OpenAiClient::new(config.clone())?
        .validate_and_list_models()
        .await
}

pub async fn completion(config: &AuthConfig, model: &str, prompt: &str) -> io::Result<String> {
    OpenAiClient::new(config.clone())?
        .completion(model, prompt)
        .await
}
