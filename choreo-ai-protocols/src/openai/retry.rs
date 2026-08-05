use std::sync::mpsc;

pub(crate) use crate::retry::AttemptDeadline;
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

#[expect(clippy::too_many_arguments)]
pub(crate) fn retry_send(
    agent: &ureq::Agent,
    url: &str,
    api_key: &str,
    body: &serde_json::Value,
    retry_cfg: &RetryConfig,
    on_retry: &mut Option<RetryCallback>,
    cancel_rx: Option<&mpsc::Receiver<()>>,
    attempt_deadline: Option<&mut crate::retry::AttemptDeadline>,
) -> Result<ureq::http::Response<ureq::Body>, OpenAiError> {
    let auth_header = zeroize::Zeroizing::new(format!("Bearer {}", api_key.trim()));
    // The closure captures `auth_header` by reference (it stays `Fn`); the
    // Zeroizing wrapper ensures the temporary `Bearer …` string is wiped when
    // it goes out of scope.
    retry::retry_loop(
        || {
            agent
                .post(url)
                .header("Authorization", auth_header.as_str())
                .send_json(body.clone())
        },
        retry_cfg,
        on_retry,
        cancel_rx,
        attempt_deadline,
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
    attempt_deadline: Option<&mut crate::retry::AttemptDeadline>,
) -> Result<ureq::http::Response<ureq::Body>, OpenAiError> {
    let auth_header = zeroize::Zeroizing::new(format!("Bearer {}", api_key.trim()));
    // The closure captures `auth_header` by reference (it stays `Fn`); the
    // Zeroizing wrapper ensures the temporary `Bearer …` string is wiped when
    // it goes out of scope.
    retry::retry_loop(
        || {
            agent
                .get(url)
                .header("Authorization", auth_header.as_str())
                .call()
        },
        retry_cfg,
        on_retry,
        cancel_rx,
        attempt_deadline,
    )
    .map_err(OpenAiError::from)
}
