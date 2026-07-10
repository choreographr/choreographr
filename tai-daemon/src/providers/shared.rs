use std::io;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tai_proto::InferenceError;

/// Determines which JSON field carries the token limit in a chat
/// completions request body.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MaxTokensField {
    /// Use the `max_tokens` field.
    MaxTokens,
    /// Use the `max_completion_tokens` field.
    MaxCompletionTokens,
}

/// Unified error type for all API providers.
/// Each provider module re-exports this as its own error type.
#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("unauthorized ({status}): {detail}")]
    Unauthorized { status: u16, detail: String },
    #[error("rate limited: {detail}")]
    RateLimited {
        retry_after_secs: Option<u64>,
        detail: String,
    },
    #[error("server error ({status}): {detail}")]
    ServerError { status: u16, detail: String },
    #[error("client error ({status}): {detail}")]
    ClientError { status: u16, detail: String },
    #[error("provider returned an empty response")]
    EmptyResponse,
    #[error("request cancelled during retry backoff")]
    Cancelled,
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

impl From<crate::retry::ProviderHttpError> for ProviderError {
    fn from(err: crate::retry::ProviderHttpError) -> Self {
        match err {
            crate::retry::ProviderHttpError::Unauthorized { status, detail } => {
                ProviderError::Unauthorized { status, detail }
            }
            crate::retry::ProviderHttpError::RateLimited {
                retry_after_secs,
                detail,
            } => ProviderError::RateLimited {
                retry_after_secs,
                detail,
            },
            crate::retry::ProviderHttpError::ServerError { status, detail } => {
                ProviderError::ServerError { status, detail }
            }
            crate::retry::ProviderHttpError::ClientError { status, detail } => {
                ProviderError::ClientError { status, detail }
            }
            crate::retry::ProviderHttpError::EmptyResponse => ProviderError::EmptyResponse,
            crate::retry::ProviderHttpError::Cancelled => ProviderError::Cancelled,
            crate::retry::ProviderHttpError::Io(e) => ProviderError::Io(e),
        }
    }
}

impl From<ProviderError> for io::Error {
    fn from(err: ProviderError) -> Self {
        io::Error::other(err.to_string())
    }
}

/// Map a ProviderError variant to a stable label string for metrics.
pub(crate) fn error_type_label(e: &ProviderError) -> &'static str {
    match e {
        ProviderError::Unauthorized { .. } => "unauthorized",
        ProviderError::RateLimited { .. } => "rate_limited",
        ProviderError::ServerError { .. } => "server_error",
        ProviderError::ClientError { .. } => "client_error",
        ProviderError::EmptyResponse => "empty_response",
        ProviderError::Cancelled => "cancelled",
        ProviderError::Io(_) => "other",
    }
}

/// Convert a ProviderError into the shared InferenceError type used
/// across the ProviderClient trait boundary.
pub(crate) fn provider_error_to_inference(e: ProviderError) -> InferenceError {
    match e {
        ProviderError::Unauthorized { status, detail } => {
            InferenceError::Unauthorized { status, detail }
        }
        ProviderError::RateLimited {
            retry_after_secs,
            detail,
        } => InferenceError::RateLimited {
            retry_after_secs,
            detail,
        },
        ProviderError::ServerError { status, detail } => {
            InferenceError::ServerError { status, detail }
        }
        ProviderError::ClientError { status, detail } => {
            InferenceError::ClientError { status, detail }
        }
        ProviderError::EmptyResponse => InferenceError::EmptyResponse,
        ProviderError::Cancelled => InferenceError::Cancelled,
        ProviderError::Io(e) => InferenceError::Io(e.to_string()),
    }
}

/// Build a reqwest blocking client with connect and request timeouts.
pub(crate) fn build_http_client(
    connect_timeout_secs: u64,
    request_timeout_secs: u64,
) -> io::Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(connect_timeout_secs))
        .timeout(Duration::from_secs(request_timeout_secs))
        .build()
        .map_err(io::Error::other)
}

/// Wrap the result of a provider API call with timing instrumentation and error
/// conversion.  Every provider uses this from its ProviderClient trait impl so
/// that metrics are recorded uniformly.
pub(crate) fn timed_result<T>(
    start: std::time::Instant,
    model: &str,
    label: &str,
    result: Result<T, ProviderError>,
) -> Result<T, InferenceError> {
    let elapsed = start.elapsed().as_secs_f64();
    match &result {
        Ok(_) => crate::metrics::record_api_call(model, label, elapsed),
        Err(e) => {
            crate::metrics::record_api_call(model, label, elapsed);
            crate::metrics::record_api_error(model, error_type_label(e));
        }
    }
    result.map_err(provider_error_to_inference)
}
