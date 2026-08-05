pub(crate) use crate::retry::AttemptContext;
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

pub(crate) fn retry_send(
    agent: &ureq::Agent,
    url: &str,
    api_key: &str,
    body: &serde_json::Value,
    retry_cfg: &RetryConfig,
    ctx: &mut crate::retry::AttemptContext,
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
        ctx,
    )
    .map_err(OpenAiError::from)
}

pub(crate) fn retry_send_get(
    agent: &ureq::Agent,
    url: &str,
    api_key: &str,
    retry_cfg: &RetryConfig,
    ctx: &mut crate::retry::AttemptContext,
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
        ctx,
    )
    .map_err(OpenAiError::from)
}
