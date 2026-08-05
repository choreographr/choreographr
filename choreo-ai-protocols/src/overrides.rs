use std::collections::HashMap;

/// Protocol-agnostic account overrides applied to a provider config.
///
/// The daemon converts its `AccountConfig` into this carrier before applying
/// overrides, so this crate never depends on daemon types (keeping it
/// independently consumable).  `None` fields mean "use the provider default".
#[derive(Debug, Clone, Default)]
pub struct ProviderOverrides {
    /// Override the provider's well-known base URL.
    pub base_url: Option<String>,
    /// Force streaming on/off.
    pub streaming: Option<bool>,
    /// Maximum HTTP retry attempts.
    pub retry_max_attempts: Option<u32>,
    /// Connection timeout in seconds.
    pub connect_timeout_secs: Option<u64>,
    /// Request (idle) timeout in seconds.
    pub request_timeout_secs: Option<u64>,
    /// Hard wall-clock deadline for a single HTTP request attempt, including
    /// the streaming body read, in seconds; `None` = provider default.  Unlike
    /// `request_timeout_secs` (an idle/no-progress timeout), this fires even
    /// when a provider trickles keep-alive bytes, bounding a stalled SSE
    /// stream.  It covers one attempt: each retry restarts the deadline, so
    /// retries plus their backoff can exceed this value in aggregate.
    pub total_timeout_secs: Option<u64>,
    /// Initial retry backoff in milliseconds.
    pub retry_initial_backoff_ms: Option<u64>,
    /// Maximum retry backoff in milliseconds.
    pub retry_max_backoff_ms: Option<u64>,
    /// Global fallback context window size.
    pub context_window: Option<u32>,
    /// Per-model context window overrides.
    pub model_context_windows: Option<HashMap<String, u32>>,
}
