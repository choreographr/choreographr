use std::io;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use super::{OpenAiError, ServiceConfig};

/// Called before each retry attempt with (current_attempt, max_attempts, delay).
pub type RetryCallback = Box<dyn FnMut(u32, u32, Duration) + Send>;

#[derive(Debug, Clone)]
pub(crate) struct RetryConfig {
    pub(crate) max_attempts: u32,
    pub(crate) initial_backoff_ms: u64,
    pub(crate) max_backoff_ms: u64,
}

impl RetryConfig {
    pub(crate) fn from_service_config(config: &ServiceConfig) -> Self {
        Self {
            max_attempts: config.retry_max_attempts,
            initial_backoff_ms: config.retry_initial_backoff_ms,
            max_backoff_ms: config.retry_max_backoff_ms,
        }
    }
}

pub(crate) fn backoff_duration(retry_number: u32, config: &RetryConfig) -> Duration {
    let multiplier = 2u64.saturating_pow(retry_number.saturating_sub(1));
    let base = config.initial_backoff_ms.saturating_mul(multiplier);
    let capped = base.min(config.max_backoff_ms);
    let jitter: f64 = rand::random_range(0.75..=1.25);
    Duration::from_millis((capped as f64 * jitter) as u64)
}

pub(crate) fn is_retryable_status(status: reqwest::StatusCode) -> bool {
    matches!(
        status,
        reqwest::StatusCode::TOO_MANY_REQUESTS
            | reqwest::StatusCode::INTERNAL_SERVER_ERROR
            | reqwest::StatusCode::BAD_GATEWAY
            | reqwest::StatusCode::SERVICE_UNAVAILABLE
            | reqwest::StatusCode::GATEWAY_TIMEOUT
    )
}

pub(crate) fn parse_retry_after_secs(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
}

/// Block for `delay`, returning `Cancelled` early if a signal arrives on
/// `cancel_rx`.  When no channel is provided, falls back to `thread::sleep`.
pub(crate) fn sleep_or_cancel(
    delay: Duration,
    cancel_rx: Option<&mpsc::Receiver<()>>,
) -> Result<(), OpenAiError> {
    if let Some(rx) = cancel_rx {
        match rx.recv_timeout(delay) {
            Ok(()) => return Err(OpenAiError::Cancelled),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {}
        }
    } else {
        thread::sleep(delay);
    }
    Ok(())
}

/// Invoke the retry callback (if any) then wait for the backoff duration.
/// Returns `Cancelled` if the user cancelled during the wait.
pub(crate) fn wait_before_retry(
    attempt: u32,
    max_attempts: u32,
    delay: Duration,
    on_retry: &mut Option<RetryCallback>,
    cancel_rx: Option<&mpsc::Receiver<()>>,
) -> Result<(), OpenAiError> {
    if let Some(cb) = on_retry.as_mut() {
        cb(attempt, max_attempts, delay);
    }
    sleep_or_cancel(delay, cancel_rx)
}

fn status_to_error(
    status: reqwest::StatusCode,
    detail: &str,
    headers: &reqwest::header::HeaderMap,
) -> OpenAiError {
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        return OpenAiError::Unauthorized {
            status: status.as_u16(),
            detail: detail.to_string(),
        };
    }
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        return OpenAiError::RateLimited {
            retry_after_secs: parse_retry_after_secs(headers),
            detail: detail.to_string(),
        };
    }
    if status.is_server_error() {
        return OpenAiError::ServerError {
            status: status.as_u16(),
            detail: detail.to_string(),
        };
    }
    if status.is_client_error() {
        return OpenAiError::ClientError {
            status: status.as_u16(),
            detail: detail.to_string(),
        };
    }
    OpenAiError::Io(io::Error::new(io::ErrorKind::Other, detail.to_string()))
}

fn retry_send_impl<F>(
    send_request: F,
    retry: &RetryConfig,
    on_retry: &mut Option<RetryCallback>,
    cancel_rx: Option<&mpsc::Receiver<()>>,
) -> Result<reqwest::blocking::Response, OpenAiError>
where
    F: Fn() -> Result<reqwest::blocking::Response, reqwest::Error>,
{
    let mut attempt: u32 = 0;
    loop {
        attempt += 1;

        let result = send_request();

        match result {
            Ok(response) => {
                let status = response.status();
                let headers = response.headers().clone();
                if status.is_success() {
                    return Ok(response);
                }

                if is_retryable_status(status) && attempt < retry.max_attempts {
                    let retry_after = parse_retry_after_secs(response.headers());
                    let body_text = response.text().unwrap_or_default();
                    let delay = if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                        retry_after
                            .map(Duration::from_secs)
                            .unwrap_or_else(|| backoff_duration(attempt, retry))
                    } else {
                        backoff_duration(attempt, retry)
                    };
                    tracing::warn!(
                        attempt,
                        max_attempts = retry.max_attempts,
                        ?status,
                        %body_text,
                        delay_ms = delay.as_millis(),
                        "retrying request"
                    );
                    wait_before_retry(attempt, retry.max_attempts, delay, on_retry, cancel_rx)?;
                    continue;
                }

                let body_text = response.text().unwrap_or_default();
                let trimmed_body = body_text.trim();
                let detail = if trimmed_body.is_empty() {
                    format!("request failed with status {status}")
                } else {
                    format!("request failed with status {status}: {trimmed_body}")
                };
                return Err(status_to_error(status, &detail, &headers));
            }
            Err(error) => {
                if (error.is_connect() || error.is_timeout()) && attempt < retry.max_attempts {
                    let delay = backoff_duration(attempt, retry);
                    tracing::warn!(
                        attempt,
                        max_attempts = retry.max_attempts,
                        ?error,
                        delay_ms = delay.as_millis(),
                        "retrying request after connection/timeout error"
                    );
                    wait_before_retry(attempt, retry.max_attempts, delay, on_retry, cancel_rx)?;
                    continue;
                }
                return Err(OpenAiError::Io(io::Error::other(error)));
            }
        }
    }
}

pub(crate) fn retry_send(
    client: &reqwest::blocking::Client,
    url: &str,
    api_key: &str,
    body: &serde_json::Value,
    retry: &RetryConfig,
    on_retry: &mut Option<RetryCallback>,
    cancel_rx: Option<&mpsc::Receiver<()>>,
) -> Result<reqwest::blocking::Response, OpenAiError> {
    retry_send_impl(
        || client.post(url).bearer_auth(api_key.trim()).json(body).send(),
        retry,
        on_retry,
        cancel_rx,
    )
}

pub(crate) fn retry_send_get(
    client: &reqwest::blocking::Client,
    url: &str,
    api_key: &str,
    retry: &RetryConfig,
    on_retry: &mut Option<RetryCallback>,
    cancel_rx: Option<&mpsc::Receiver<()>>,
) -> Result<reqwest::blocking::Response, OpenAiError> {
    retry_send_impl(
        || client.get(url).bearer_auth(api_key.trim()).send(),
        retry,
        on_retry,
        cancel_rx,
    )
}

/// Thin wrapper around [`retry_send`] that skips retry callbacks and
/// cancellation — used by callers that don't need interactive retry.
pub(crate) fn retry_send_simple(
    client: &reqwest::blocking::Client,
    url: &str,
    api_key: &str,
    body: &serde_json::Value,
    retry: &RetryConfig,
) -> Result<reqwest::blocking::Response, OpenAiError> {
    retry_send(client, url, api_key, body, retry, &mut None, None)
}

/// Thin wrapper around [`retry_send_get`] that skips retry callbacks and
/// cancellation.
pub(crate) fn retry_send_get_simple(
    client: &reqwest::blocking::Client,
    url: &str,
    api_key: &str,
    retry: &RetryConfig,
) -> Result<reqwest::blocking::Response, OpenAiError> {
    retry_send_get(client, url, api_key, retry, &mut None, None)
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum MaxTokensField {
    MaxTokens,
    MaxCompletionTokens,
}

pub(crate) fn chat_completions_max_tokens_field(config: &ServiceConfig, model: &str) -> MaxTokensField {
    if config.base_url.contains("opencode.ai") || model == "big-pickle" {
        MaxTokensField::MaxTokens
    } else {
        MaxTokensField::MaxCompletionTokens
    }
}
