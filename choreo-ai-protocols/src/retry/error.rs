//! The normalized provider HTTP error and the response-body mapping.
//!
//! [`ProviderHttpError`] is the terminal error the retry layer produces for
//! every non-2xx outcome; each provider adapter maps it into its own error type
//! via `From` (see `shared::ProviderError`).  [`status_to_error`] applies the
//! status → variant contract, and [`extract_error_message`] unwraps the
//! provider's JSON error envelope into a concise human-readable detail string.

use std::io;

/// Generic HTTP error produced by the retry layer.  Each provider maps this to
/// its own error type via `From`.
#[derive(Debug)]
pub enum ProviderHttpError {
    /// Authentication/authorization failure (HTTP 401/403).
    Unauthorized {
        /// HTTP status code.
        status: u16,
        /// Provider's human-readable error detail.
        detail: String,
    },
    /// Rate limited (HTTP 429); the provider may name a cooldown.
    RateLimited {
        /// HTTP status code.
        status: u16,
        /// Parsed `Retry-After` cooldown, when the provider sent one.
        retry_after_secs: Option<u64>,
        /// Provider's human-readable error detail.
        detail: String,
    },
    /// Server-side failure (HTTP 5xx).
    ServerError {
        /// HTTP status code.
        status: u16,
        /// Provider's human-readable error detail.
        detail: String,
    },
    /// Non-retryable client error (4xx other than 401/403/429).
    ClientError {
        /// HTTP status code.
        status: u16,
        /// Provider's human-readable error detail.
        detail: String,
    },
    /// The provider returned a success status but an empty body.
    EmptyResponse,
    /// The request was cancelled during a retry wait or pre-attempt check.
    Cancelled,
    /// A transport error, or a terminal status with no structured wrapper.
    Io(io::Error),
}

impl std::fmt::Display for ProviderHttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unauthorized { status, detail } => {
                write!(f, "unauthorized ({status}): {detail}")
            }
            Self::RateLimited {
                status,
                retry_after_secs,
                detail,
            } => {
                if let Some(secs) = retry_after_secs {
                    write!(f, "rate limited ({status}, retry after {secs}s): {detail}")
                } else {
                    write!(f, "rate limited ({status}): {detail}")
                }
            }
            Self::ServerError { status, detail } => {
                write!(f, "server error ({status}): {detail}")
            }
            Self::ClientError { status, detail } => {
                write!(f, "client error ({status}): {detail}")
            }
            Self::EmptyResponse => f.write_str("empty response"),
            Self::Cancelled => f.write_str("cancelled"),
            Self::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ProviderHttpError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

/// Map a terminal HTTP status (plus the parsed `Retry-After`) to the variant
/// each provider adapter converts into its own error type.  The status ranges
/// mirror the retry contract: 401/403 auth, 429 rate limited, 5xx server, the
/// rest of 4xx client, and anything else folds into a transport-style `Io`.
pub(super) fn status_to_error(
    status: u16,
    detail: &str,
    retry_after_secs: Option<u64>,
) -> ProviderHttpError {
    match status {
        401 | 403 => ProviderHttpError::Unauthorized {
            status,
            detail: detail.to_string(),
        },
        429 => ProviderHttpError::RateLimited {
            status,
            retry_after_secs,
            detail: detail.to_string(),
        },
        _ if (500..600).contains(&status) => ProviderHttpError::ServerError {
            status,
            detail: detail.to_string(),
        },
        _ if (400..500).contains(&status) => ProviderHttpError::ClientError {
            status,
            detail: detail.to_string(),
        },
        _ => ProviderHttpError::Io(io::Error::other(detail.to_string())),
    }
}

/// Extract a concise human-readable message from a provider error body,
/// unwrapping the standard JSON error envelopes used across providers:
///
/// - OpenAI-compatible APIs (`OpenAI`, `DeepSeek`, `OpenRouter`, Groq, …):
///   `{"error": {"message": "…", "type": "…", "param": …,
///   "code": …}}` — `error.message` is the standard human-readable field.
/// - Anthropic: `{"type": "error", "error": {"type": "…",
///   "message": "…"}}` — same `error.message` shape.
/// - Gemini: `{"error": {"code": …, "message": "…", "status":
///   "…"}}` — same `error.message` shape.
/// - Some proxies: `{"message": "…"}` or `{"error": "plain string"}`.
///
/// Returns `None` (the caller keeps the verbatim body) when the body is not
/// JSON or carries no recognizable string `message` — an unknown or hostile
/// body is never mis-summarized, and the raw text still reaches the user.
pub(super) fn extract_error_message(body: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    let message = match value.get("error") {
        // `{"error": {"message": "…"}}` — the standard envelope.
        Some(serde_json::Value::Object(map)) => {
            map.get("message").and_then(serde_json::Value::as_str)
        }
        // `{"error": "plain string"}` — rare compat-layer shape.
        Some(serde_json::Value::String(s)) => Some(s.as_str()),
        _ => None,
    };
    // Top-level `{"message": "…"}` (some reverse-proxies unwrap for you).
    let message = message.or_else(|| value.get("message").and_then(serde_json::Value::as_str));
    let message = message.map(str::trim).filter(|m| !m.is_empty())?;
    Some(message.to_string())
}
