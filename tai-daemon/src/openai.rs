use serde::{Deserialize, Serialize};
use std::{collections::HashMap, fs, io, path::PathBuf};

const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
const DEFAULT_MODEL_LIST_PATH: &str = "/models";
const DEFAULT_RESPONSES_PATH: &str = "/responses";
const DEFAULT_CHAT_COMPLETIONS_PATH: &str = "/chat/completions";

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RequestFormat {
    Responses,
    ChatCompletions,
}

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

#[derive(Debug, Deserialize)]
struct ModelListResponse {
    data: Vec<ModelInfo>,
}

#[derive(Debug, Deserialize)]
struct ModelInfo {
    id: String,
}

#[derive(Debug, Serialize)]
struct ResponsesRequest<'a> {
    model: &'a str,
    input: &'a str,
}

#[derive(Debug, Deserialize)]
struct ResponsesResponse {
    output: Vec<OutputItem>,
}

#[derive(Debug, Deserialize)]
struct OutputItem {
    #[serde(default)]
    content: Vec<ContentItem>,
}

#[derive(Debug, Deserialize)]
struct ContentItem {
    text: Option<String>,
}

#[derive(Debug, Serialize)]
struct ChatCompletionsRequest<'a> {
    model: &'a str,
    messages: Vec<ChatMessage<'a>>,
}

#[derive(Debug, Serialize, Deserialize)]
struct ChatMessage<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Debug, Deserialize)]
struct ChatCompletionsResponse {
    choices: Vec<Choice>,
}

#[derive(Debug, Deserialize)]
struct Choice {
    message: AssistantMessage,
}

#[derive(Debug, Deserialize)]
struct AssistantMessage {
    content: String,
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
            format!("auth config at {} contains an empty api_key", path.display()),
        ));
    }

    Ok(config)
}

fn endpoint_url(base_url: &str, path: &str) -> io::Result<String> {
    if !path.starts_with('/') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "path must start with '/'",
        ));
    }
    Ok(format!("{}{}", base_url.trim_end_matches('/'), path))
}

async fn send_request<R>(request: reqwest::RequestBuilder) -> io::Result<R>
where
    R: for<'de> Deserialize<'de>,
{
    let response = request.send().await.map_err(io::Error::other)?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        let trimmed_body = body.trim();
        let detail = if trimmed_body.is_empty() {
            format!("request failed with status {status}")
        } else {
            format!("request failed with status {status}: {trimmed_body}")
        };
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, detail));
    }

    response.json().await.map_err(io::Error::other)
}

pub async fn validate_and_list_models(config: &AuthConfig) -> io::Result<Vec<String>> {
    let client = reqwest::Client::builder()
        .build()
        .map_err(io::Error::other)?;
    let url = endpoint_url(&config.base_url, &config.model_list_path)?;
    let payload: ModelListResponse = send_request(
        client.get(&url).bearer_auth(config.api_key.trim()),
    )
    .await?;
    Ok(payload.data.into_iter().map(|model| model.id).collect())
}

impl AuthConfig {
    pub fn request_format_for_model(&self, model: &str) -> RequestFormat {
        self.model_request_formats
            .get(model)
            .copied()
            .unwrap_or(self.default_request_format)
    }
}

async fn responses_request(client: &reqwest::Client, config: &AuthConfig, model: &str, prompt: &str) -> io::Result<String> {
    let url = endpoint_url(&config.base_url, &config.responses_path)?;
    let payload: ResponsesResponse = send_request(
        client
            .post(&url)
            .bearer_auth(config.api_key.trim())
            .json(&ResponsesRequest {
                model,
                input: prompt,
            }),
    )
    .await?;

    let content = payload
        .output
        .into_iter()
        .flat_map(|item| item.content.into_iter())
        .filter_map(|item| item.text)
        .map(|text| text.trim().to_string())
        .find(|text| !text.is_empty())
        .unwrap_or_default();

    if content.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "provider returned an empty response",
        ));
    }

    Ok(content)
}

async fn chat_completions_request(client: &reqwest::Client, config: &AuthConfig, model: &str, prompt: &str) -> io::Result<String> {
    let url = endpoint_url(&config.base_url, &config.chat_completions_path)?;
    let payload: ChatCompletionsResponse = send_request(
        client
            .post(&url)
            .bearer_auth(config.api_key.trim())
            .json(&ChatCompletionsRequest {
                model,
                messages: vec![ChatMessage {
                    role: "user",
                    content: prompt,
                }],
            }),
    )
    .await?;

    let content = payload
        .choices
        .into_iter()
        .next()
        .map(|choice| choice.message.content)
        .unwrap_or_default()
        .trim()
        .to_string();

    if content.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "provider returned an empty response",
        ));
    }

    Ok(content)
}

pub async fn completion(config: &AuthConfig, model: &str, prompt: &str) -> io::Result<String> {
    let client = reqwest::Client::builder()
        .build()
        .map_err(io::Error::other)?;

    match config.request_format_for_model(model) {
        RequestFormat::Responses => responses_request(&client, config, model, prompt).await,
        RequestFormat::ChatCompletions => chat_completions_request(&client, config, model, prompt).await,
    }
}
