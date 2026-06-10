use std::io;
use tai_proto::socket_path;
use tracing::{info, warn};
use tracing_subscriber::{EnvFilter, fmt};

#[tokio::main]
async fn main() -> io::Result<()> {
    fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with_target(false)
        .init();

    let auth_config = tai_daemon::openai::load_auth_config()?;
    info!(base_url = %auth_config.base_url, model_list_path = %auth_config.model_list_path, responses_path = %auth_config.responses_path, chat_completions_path = %auth_config.chat_completions_path, default_request_format = ?auth_config.default_request_format, "validating OpenAI credentials on startup");
    let models = tai_daemon::openai::validate_and_list_models(&auth_config).await?;
    if models.is_empty() {
        warn!("startup validation succeeded but provider returned no models");
    } else {
        info!(model_count = models.len(), first_model = %models[0], "startup validation succeeded");
    }

    let socket_path = socket_path();
    tai_daemon::run_server(&socket_path, auth_config).await
}
