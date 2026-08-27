use super::{MaxTokensField, OpenAiClient, RequestFormat};
use choreo_proto::ContextConfig;
use std::{collections::HashMap, io};

const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
const DEFAULT_MODEL_LIST_PATH: &str = "/models";
const DEFAULT_RESPONSES_PATH: &str = "/responses";
const DEFAULT_CHAT_COMPLETIONS_PATH: &str = "/chat/completions";

/// Fixed `x-opencode-session` value sent to opencode providers.
///
/// The opencode.ai zen/go gateway routes each request to one of several
/// weighted upstream providers by hashing a "sticky id": the
/// `x-opencode-session` header when present, otherwise the API key's
/// workspace id. Without the header, choreographr requests always landed on a
/// provider that reported "Model is unavailable". A fixed value keeps routing
/// on a stable, verified-good provider. The value only matters for its
/// last-4-chars hash (which provider gets selected), so it is deliberately a
/// constant rather than the session id.
pub(crate) const OPENCODE_SESSION_ID: &str = "choreographr";

/// Per-client configuration for an OpenAI-compatible service.
///
/// Provider-level settings (endpoints, timeouts, retry, token limits,
/// request format) live here; daemon-level settings live in
/// `choreographr`'s `config::DaemonConfig`.
#[derive(Debug, Clone)]
pub struct ServiceConfig {
    pub base_url: String,
    pub model_list_path: String,
    pub responses_path: String,
    pub chat_completions_path: String,
    pub default_request_format: RequestFormat,
    /// Catalog slug for this service (e.g. `"opencode"` for an OpenAI-format
    /// gateway). Owned: the catalog lookup that supplies it returns a clone,
    /// so the slug cannot be a `'static` reference anymore.
    pub provider_slug: String,
    pub chat_completions_max_tokens: Option<u32>,
    pub model_max_tokens: HashMap<String, u32>,
    pub context_window_config: crate::ContextWindowConfig,
    pub responses_max_output_tokens: Option<u32>,
    pub model_responses_max_output_tokens: HashMap<String, u32>,
    pub chat_completions_max_tokens_field: MaxTokensField,
    pub model_max_tokens_fields: HashMap<String, MaxTokensField>,
    pub streaming: bool,
    pub stream_options: bool,
    pub max_turns: Option<u32>,
    pub retry_max_attempts: u32,
    pub retry_initial_backoff_ms: u64,
    pub retry_max_backoff_ms: u64,
    pub connect_timeout_secs: u64,
    pub request_timeout_secs: u64,
    /// Hard wall-clock deadline for a single HTTP request attempt, including
    /// the streaming body read; 0 disables.  Unlike `request_timeout_secs` (an
    /// idle/no-progress timeout that resets per chunk), this fires even when a
    /// provider trickles keep-alive bytes, so it bounds a stalled SSE stream.
    /// It covers one attempt: each retry restarts the deadline, so retries
    /// plus their backoff can exceed this value in aggregate.
    pub total_timeout_secs: u64,
    pub context: ContextConfig,
    pub programmatic_tool_calling: bool,
}

impl Default for ServiceConfig {
    fn default() -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.to_string(),
            model_list_path: DEFAULT_MODEL_LIST_PATH.to_string(),
            responses_path: DEFAULT_RESPONSES_PATH.to_string(),
            chat_completions_path: DEFAULT_CHAT_COMPLETIONS_PATH.to_string(),
            default_request_format: RequestFormat::ChatCompletions,
            provider_slug: "openai".to_string(),
            chat_completions_max_tokens: None,
            model_max_tokens: HashMap::new(),
            context_window_config: crate::ContextWindowConfig::default(),
            responses_max_output_tokens: None,
            model_responses_max_output_tokens: HashMap::new(),
            chat_completions_max_tokens_field: MaxTokensField::MaxCompletionTokens,
            model_max_tokens_fields: HashMap::new(),
            streaming: true,
            stream_options: true,
            max_turns: None,
            retry_max_attempts: 5,
            retry_initial_backoff_ms: 1000,
            retry_max_backoff_ms: 30000,
            connect_timeout_secs: 30,
            request_timeout_secs: 120,
            total_timeout_secs: 3600,
            context: ContextConfig::default(),
            programmatic_tool_calling: false,
        }
    }
}

pub(crate) fn endpoint_url(base_url: &str, path: &str) -> io::Result<String> {
    if !path.starts_with('/') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "path must start with '/'",
        ));
    }
    Ok(format!("{}{}", base_url.trim_end_matches('/'), path))
}

impl ServiceConfig {
    /// Resolve the request format for a model: catalog lookup first,
    /// falling back to the configured default for unknown models.
    pub fn request_format_for_model(&self, model: &str) -> RequestFormat {
        crate::catalog::model_request_format(&self.provider_slug, model)
            .unwrap_or(self.default_request_format)
    }

    /// Clamp a requested output-token limit down to the catalog's per-model
    /// `max_output_tokens` fact (`lookup_max_output_tokens`), when one exists.
    ///
    /// Why clamp-only: the catalog fact is a *ceiling* — a request may be
    /// intentionally small (a low-turn budget, a tool-loop headroom split), so
    /// the clamp must never *raise* a lower request. `None` (unknown provider,
    /// unknown model, or the recorded `0` = unknown sentinel) leaves the
    /// request untouched: an unfactored model's own API default is a better
    /// guess than any heuristic here.
    fn clamp_output_to_catalog(&self, model: &str, requested: u32) -> u32 {
        match crate::catalog::lookup_max_output_tokens(&self.provider_slug, model) {
            Some(limit) if requested > limit => {
                tracing::info!(
                    provider = %self.provider_slug,
                    model = %model,
                    requested,
                    clamped_to = limit,
                    "clamped outgoing output-token limit to the catalog max_output_tokens fact"
                );
                limit
            }
            // No fact, or the request already fits under the ceiling: pass the
            // request through unchanged (clamp-down only, never clamp-up).
            _ => requested,
        }
    }

    pub fn max_output_tokens_for_model(&self, model: &str) -> Option<u32> {
        // Clamp-down against the catalog ceiling at the single resolution
        // point so every Responses-path caller inherits the clamp without
        // each body-builder having to remember it.
        self.model_responses_max_output_tokens
            .get(model)
            .copied()
            .or(self.responses_max_output_tokens)
            .map(|requested| self.clamp_output_to_catalog(model, requested))
    }

    pub fn max_tokens_for_model(&self, model: &str) -> Option<u32> {
        self.model_max_tokens
            .get(model)
            .copied()
            .or(self.chat_completions_max_tokens)
    }

    pub fn context_window_for_model(&self, model: &str) -> Option<u32> {
        self.context_window_config.context_window_for_model(model)
    }

    /// Returns whether programmatic tool calling should be enabled for a given model.
    ///
    /// Auto-enables for gpt-5.6+ models when the default is the Responses API.
    /// The account-level `programmatic_tool_calling` override takes precedence.
    pub fn programmatic_tool_calling_for_model(&self, model: &str) -> bool {
        if self.programmatic_tool_calling {
            return true;
        }
        // Auto-enable for gpt-5.6 models when using the Responses API
        if self.request_format_for_model(model) == RequestFormat::Responses
            && model.starts_with("gpt-5.6")
        {
            return true;
        }
        false
    }

    pub fn max_tokens_field_for_model(&self, model: &str) -> MaxTokensField {
        let field = self
            .model_max_tokens_fields
            .get(model)
            .copied()
            .unwrap_or(self.chat_completions_max_tokens_field);
        tracing::debug!(
            model = %model,
            ?field,
            "max_tokens_field_for_model"
        );
        field
    }

    /// Extra headers to attach to every request for opencode providers.
    ///
    /// The opencode.ai zen/go gateway picks a weighted upstream provider by
    /// hashing the `x-opencode-session` header (falling back to the API key's
    /// workspace id when absent). Sending a fixed, verified-good session id
    /// pins routing to a working provider instead of deterministically landing
    /// on a broken one. Only the two known opencode gateway slugs
    /// (`opencode`, `opencode-go`) get the header — matched exactly, not by
    /// prefix, so an unrelated `opencode-*` slug is never given routing
    /// behavior it wasn't configured for. Every other provider slug gets an
    /// empty list.
    pub(crate) fn opencode_request_headers(&self) -> &'static [(&'static str, &'static str)] {
        match self.provider_slug.as_str() {
            "opencode" | "opencode-go" => &[("x-opencode-session", OPENCODE_SESSION_ID)],
            _ => &[],
        }
    }

    /// Resolve the `(max_tokens, max_completion_tokens)` pair for a model.
    ///
    /// Which field to use depends on the model family (o-series uses
    /// `max_completion_tokens`, gpt-series uses `max_tokens`).
    /// Returns `(Some(n), None)` or `(None, Some(n))` depending on the
    /// resolved field.  If `max_tokens_for_model` returns `None`, both
    /// fields are `None` (the API will use its own default).
    pub(crate) fn max_tokens_field_pair(&self, model: &str) -> (Option<u32>, Option<u32>) {
        // Clamp-down against the catalog's `max_output_tokens` fact here, at
        // the single resolution point, so every chat-completions body-builder
        // (plain, turn, and both streaming variants) inherits the clamp.
        let max_tokens = self
            .max_tokens_for_model(model)
            .map(|requested| self.clamp_output_to_catalog(model, requested));
        match self.max_tokens_field_for_model(model) {
            MaxTokensField::MaxTokens => (max_tokens, None),
            MaxTokensField::MaxCompletionTokens => (None, max_tokens),
        }
    }
}

pub fn validate_and_list_models(config: &ServiceConfig, api_key: &str) -> io::Result<Vec<String>> {
    let client = OpenAiClient::new(config.clone(), api_key.to_string())?;
    Ok(client.validate_and_list_models()?)
}

pub fn completion(
    config: &ServiceConfig,
    api_key: &str,
    model: &str,
    prompt: &str,
) -> io::Result<String> {
    let client = OpenAiClient::new(config.clone(), api_key.to_string())?;
    Ok(client.completion(model, prompt)?)
}

#[cfg(test)]
// These tests read the process-wide provider catalog via
// `ServiceConfig::request_format_for_model`, so serialize them under the same
// key as the catalog swap tests (see `catalog/mod.rs`) for libtest determinism.
#[serial_test::serial(catalog)]
mod tests {
    use super::*;

    #[test]
    fn opencode_request_headers_present_for_opencode_slug() {
        for slug in ["opencode", "opencode-go"] {
            let config = ServiceConfig {
                provider_slug: slug.into(),
                ..Default::default()
            };
            let headers = config.opencode_request_headers();
            assert_eq!(
                headers,
                &[("x-opencode-session", OPENCODE_SESSION_ID)],
                "slug {slug} must send the session header"
            );
        }
    }

    #[test]
    fn opencode_request_headers_empty_for_other_providers() {
        for slug in ["openai", "deepseek", "anthropic", "custom"] {
            let config = ServiceConfig {
                provider_slug: slug.into(),
                ..Default::default()
            };
            assert!(
                config.opencode_request_headers().is_empty(),
                "slug {slug} must not send opencode headers"
            );
        }
    }

    #[test]
    fn opencode_request_headers_exact_slug_allowlist() {
        // Only the two known gateway slugs get the header. Prefix matches (e.g.
        // a hypothetical `opencode-mirror`) and slugs merely *containing*
        // "opencode" must not — an unknown opencode-* slug is not known to be a
        // gateway and must not get routing behavior it wasn't configured for.
        for slug in [
            "opencode-future-tier",
            "not-opencode-gateway",
            "my-opencode-proxy",
        ] {
            let config = ServiceConfig {
                provider_slug: slug.into(),
                ..Default::default()
            };
            assert!(
                config.opencode_request_headers().is_empty(),
                "slug {slug} must not send opencode headers"
            );
        }
    }

    #[test]
    fn catalog_clamp_downs_known_model() {
        // The bundled snapshot records gpt-5.4's limit.output as 128_000, so a
        // larger request must be clamped down to the catalog fact.
        let config = ServiceConfig::default();
        assert_eq!(config.clamp_output_to_catalog("gpt-5.4", 999_999), 128_000);
        // Responses-path resolution inherits the clamp.
        let config = ServiceConfig {
            responses_max_output_tokens: Some(200_000),
            ..Default::default()
        };
        assert_eq!(config.max_output_tokens_for_model("gpt-5.4"), Some(128_000));
        // Chat-completions pair resolution inherits the clamp too.
        let config = ServiceConfig {
            chat_completions_max_tokens: Some(200_000),
            ..Default::default()
        };
        // Default max_tokens_field is MaxCompletionTokens, so the pair puts
        // the clamped value in the second slot.
        assert_eq!(
            config.max_tokens_field_pair("gpt-5.4"),
            (None, Some(128_000))
        );
    }

    #[test]
    fn catalog_clamp_never_raises_a_lower_request() {
        // A request already under the catalog ceiling must pass through
        // unchanged: the request may be intentionally small.
        let config = ServiceConfig::default();
        assert_eq!(config.clamp_output_to_catalog("gpt-5.4", 128_000), 128_000);
        assert_eq!(config.clamp_output_to_catalog("gpt-5.4", 512), 512);
    }

    #[test]
    fn catalog_clamp_untouched_for_unknown_model_or_provider() {
        // Unknown model on a known provider, and unknown provider entirely:
        // no fact → no clamp, request passes through as-is.
        let config = ServiceConfig::default();
        assert_eq!(
            config.clamp_output_to_catalog("not-a-real-model", 200_000),
            200_000
        );
        let config = ServiceConfig {
            provider_slug: "no-such-provider".into(),
            ..Default::default()
        };
        assert_eq!(config.clamp_output_to_catalog("gpt-5.4", 200_000), 200_000);
    }

    #[test]
    fn catalog_clamp_untouched_when_fact_is_zero() {
        // A recorded `max_output_tokens` of `0` is the "unknown" sentinel and
        // resolves to `None` — the request must not be clamped to 0.
        let _restore = crate::catalog::test_util::RestoreBundledOnDrop;
        let bundled = crate::catalog::catalog_snapshot();
        crate::catalog::replace_catalog({
            let mut c = crate::catalog::test_util::tiny_catalog();
            c[0].models[0].max_output_tokens = 0;
            c
        });
        let config = ServiceConfig {
            provider_slug: "tiny-test".into(),
            chat_completions_max_tokens: Some(200_000),
            ..Default::default()
        };
        assert_eq!(config.max_tokens_field_pair("tiny-model").1, Some(200_000));
        crate::catalog::replace_catalog(bundled.to_vec());
    }

    #[test]
    fn programmatic_tool_calling_default_is_false() {
        let config = ServiceConfig::default();
        assert!(!config.programmatic_tool_calling_for_model("gpt-4.1"));
    }

    #[test]
    fn programmatic_tool_calling_account_override() {
        let config = ServiceConfig {
            programmatic_tool_calling: true,
            ..Default::default()
        };
        // Override takes precedence regardless of model.
        assert!(config.programmatic_tool_calling_for_model("gpt-4.1"));
        assert!(config.programmatic_tool_calling_for_model("claude-3"));
    }

    #[test]
    fn programmatic_tool_calling_auto_enables_for_gpt_5_6_responses() {
        let config = ServiceConfig {
            default_request_format: RequestFormat::Responses,
            ..Default::default()
        };
        // Models not in the catalog fall back to default_request_format.
        // When the default is Responses, gpt-5.6 models auto-enable.
        assert!(config.programmatic_tool_calling_for_model("gpt-5.6-chat"));
        // gpt-5.6-sol is Responses in the catalog (OpenAI's default API),
        // so auto-enable fires even without the account-level override.
        assert!(config.programmatic_tool_calling_for_model("gpt-5.6-sol"));
    }

    #[test]
    fn programmatic_tool_calling_not_auto_enabled_with_chat_completions_default() {
        let config = ServiceConfig {
            default_request_format: RequestFormat::ChatCompletions,
            ..Default::default()
        };
        // An unknown gpt-5.6 model falls back to the ChatCompletions default,
        // so auto-enable (Responses-only) does not fire.
        assert!(!config.programmatic_tool_calling_for_model("gpt-5.6-future-unknown"));
    }

    #[test]
    fn programmatic_tool_calling_not_auto_enabled_for_other_models() {
        let config = ServiceConfig {
            default_request_format: RequestFormat::Responses,
            ..Default::default()
        };
        assert!(!config.programmatic_tool_calling_for_model("gpt-4.1"));
        assert!(!config.programmatic_tool_calling_for_model("gpt-5.5"));
        assert!(!config.programmatic_tool_calling_for_model("claude-3-opus"));
    }

    #[test]
    fn request_format_for_model_uses_catalog_lookup() {
        let config = ServiceConfig {
            default_request_format: RequestFormat::Responses,
            provider_slug: "openai".to_string(),
            ..Default::default()
        };
        assert_eq!(
            config.request_format_for_model("gpt-4.1"),
            RequestFormat::Responses
        );
        // Unknown model falls back to default_request_format.
        assert_eq!(
            config.request_format_for_model("totally-unknown-xyz"),
            RequestFormat::Responses
        );
    }

    #[test]
    fn programmatic_tool_calling_override_wins_over_auto_disable() {
        let config = ServiceConfig {
            default_request_format: RequestFormat::ChatCompletions,
            programmatic_tool_calling: true,
            ..Default::default()
        };
        // Account-level override takes precedence even when auto-enable
        // would not trigger (ChatCompletions format).
        assert!(config.programmatic_tool_calling_for_model("gpt-5.6-sol"));
        assert!(config.programmatic_tool_calling_for_model("gpt-4.1"));
    }
}
