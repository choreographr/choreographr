use std::sync::mpsc;

pub use crate::retry::{RetryCallback, RetryConfig};

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
    client: &reqwest::blocking::Client,
    url: &str,
    api_key: &str,
    body: &serde_json::Value,
    retry_cfg: &RetryConfig,
    on_retry: &mut Option<RetryCallback>,
    cancel_rx: Option<&mpsc::Receiver<()>>,
) -> Result<reqwest::blocking::Response, OpenAiError> {
    retry::retry_loop(
        || {
            client
                .post(url)
                .bearer_auth(api_key.trim())
                .json(body)
                .send()
        },
        retry_cfg,
        on_retry,
        cancel_rx,
    )
    .map_err(OpenAiError::from)
}

pub(crate) fn retry_send_get(
    client: &reqwest::blocking::Client,
    url: &str,
    api_key: &str,
    retry_cfg: &RetryConfig,
    on_retry: &mut Option<RetryCallback>,
    cancel_rx: Option<&mpsc::Receiver<()>>,
) -> Result<reqwest::blocking::Response, OpenAiError> {
    retry::retry_loop(
        || client.get(url).bearer_auth(api_key.trim()).send(),
        retry_cfg,
        on_retry,
        cancel_rx,
    )
    .map_err(OpenAiError::from)
}

/// Thin wrapper around [`retry_send`] that skips retry callbacks and
/// cancellation.
pub(crate) fn retry_send_simple(
    client: &reqwest::blocking::Client,
    url: &str,
    api_key: &str,
    body: &serde_json::Value,
    retry_cfg: &RetryConfig,
) -> Result<reqwest::blocking::Response, OpenAiError> {
    retry_send(client, url, api_key, body, retry_cfg, &mut None, None)
}

/// Thin wrapper around [`retry_send_get`] that skips retry callbacks and
/// cancellation.
pub(crate) fn retry_send_get_simple(
    client: &reqwest::blocking::Client,
    url: &str,
    api_key: &str,
    retry_cfg: &RetryConfig,
) -> Result<reqwest::blocking::Response, OpenAiError> {
    retry_send_get(client, url, api_key, retry_cfg, &mut None, None)
}
