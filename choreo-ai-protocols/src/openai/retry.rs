use std::sync::mpsc;

pub use crate::retry::{RetryCallback, RetryConfig, check_cancelled};

use super::{OpenAiError, ServiceConfig};
use crate::retry;

pub(crate) fn retry_config_from_config(config: &ServiceConfig) -> RetryConfig {
    RetryConfig {
        max_attempts: config.retry_max_attempts,
        initial_backoff_ms: config.retry_initial_backoff_ms,
        max_backoff_ms: config.retry_max_backoff_ms,
    }
}

pub(crate) fn retry_send(
    agent: &ureq::Agent,
    url: &str,
    api_key: &str,
    body: &serde_json::Value,
    retry_cfg: &RetryConfig,
    on_retry: &mut Option<RetryCallback>,
    cancel_rx: Option<&mpsc::Receiver<()>>,
) -> Result<ureq::http::Response<ureq::Body>, OpenAiError> {
    let auth_header = format!("Bearer {}", api_key.trim());
    retry::retry_loop(
        || {
            agent
                .post(url)
                .header("Authorization", &auth_header)
                .send_json(body.clone())
        },
        retry_cfg,
        on_retry,
        cancel_rx,
    )
    .map_err(OpenAiError::from)
}

pub(crate) fn retry_send_get(
    agent: &ureq::Agent,
    url: &str,
    api_key: &str,
    retry_cfg: &RetryConfig,
    on_retry: &mut Option<RetryCallback>,
    cancel_rx: Option<&mpsc::Receiver<()>>,
) -> Result<ureq::http::Response<ureq::Body>, OpenAiError> {
    let auth_header = format!("Bearer {}", api_key.trim());
    retry::retry_loop(
        || agent.get(url).header("Authorization", &auth_header).call(),
        retry_cfg,
        on_retry,
        cancel_rx,
    )
    .map_err(OpenAiError::from)
}
