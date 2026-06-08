use serde::Deserialize;
use std::{fs, io, path::PathBuf};

const DEFAULT_BASE_URL: &str = "https://api.openai.com";
const DEFAULT_MODEL_LIST_PATH: &str = "/v1/models";

#[derive(Debug, Deserialize)]
pub struct AuthConfig {
    pub api_key: String,
    #[serde(default = "default_base_url")]
    pub base_url: String,
    #[serde(default = "default_model_list_path")]
    pub model_list_path: String,
}

fn default_base_url() -> String {
    DEFAULT_BASE_URL.to_string()
}

fn default_model_list_path() -> String {
    DEFAULT_MODEL_LIST_PATH.to_string()
}

#[derive(Debug, Deserialize)]
struct ModelListResponse {
    data: Vec<ModelInfo>,
}

#[derive(Debug, Deserialize)]
struct ModelInfo {
    id: String,
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

pub async fn validate_and_list_models(config: &AuthConfig) -> io::Result<Vec<String>> {
    let client = reqwest::Client::builder()
        .build()
        .map_err(io::Error::other)?;

    let base = config.base_url.trim_end_matches('/');
    let path = if config.model_list_path.starts_with('/') {
        config.model_list_path.as_str()
    } else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "model_list_path must start with '/'",
        ));
    };
    let url = format!("{base}{path}");

    let response = client
        .get(&url)
        .bearer_auth(config.api_key.trim())
        .send()
        .await
        .map_err(io::Error::other)?;

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

    let payload: ModelListResponse = response.json().await.map_err(io::Error::other)?;
    Ok(payload.data.into_iter().map(|model| model.id).collect())
}
