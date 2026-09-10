use std::io;
use std::sync::Arc;

use choreo_ai_protocols::openai::{OpenAiClient, ServiceConfig};
use choreo_ai_protocols::{
    AnthropicClient, AnthropicConfig, ChatTurnRequest, ChatTurnResult, GoogleClient, GoogleConfig,
    ImageGenerationClient, OpenAiImageClient, ProviderClient, ProviderProtocol, StreamEvent,
    ZaiImageClient, lookup_provider,
};
use choreo_proto::InferenceError;

/// A concrete, protocol-erased provider facade.
///
/// Wraps any one [`ProviderClient`] behind a `dyn` pointer so the rest of
/// the daemon never sees which wire protocol is in use.  Also remembers the
/// catalog slug (e.g. `"opencode"` even when the client is an
/// `OpenAiClient`, which always reports `"openai"`), which is what catalog
/// lookups for reasoning/context windows need.
///
/// This is the only daemon type that knows about provider *protocols*; all
/// wire-format knowledge lives in `choreo-ai-protocols`.
#[derive(Clone, Debug)]
pub struct InferenceProvider {
    client: Arc<dyn ProviderClient>,
    /// The provider slug from the catalog (e.g. "openai", "anthropic", "opencode").
    /// Used for catalog lookups instead of delegating to the client, which may
    /// return a generic value (e.g. OpenAiClient always says "openai"). Owned
    /// because the catalog lookup that supplies it returns a clone, not a
    /// `'static` reference.
    slug: String,
    /// Optional image-generation backend. `Some` for OpenAI-protocol
    /// providers — the plain OpenAI-protocol proxy families use the
    /// [`OpenAiImageClient`] default, while the two Zhipu slugs (`zai`, the
    /// z.ai coding gateway, and `zhipuai`, the mainland bigmodel endpoint)
    /// get the dedicated [`ZaiImageClient`] since their Images API is a
    /// different endpoint contract (URL-returning glm-image, hd/standard
    /// quality, no-n body). Anthropic and Gemini have no image-generation
    /// backend in v1 (the Gemini/Responses-image paths are deferred), so
    /// their constructors leave this `None` and image requests against
    /// those accounts surface a precise "does not support" error instead of
    /// a half-working client.
    image_client: Option<Arc<dyn ImageGenerationClient>>,
}

/// Opaque handle returned to a tool thread by
/// `DaemonCommand::GetImageGenerationProvider`.
///
/// It deliberately carries ONLY the trait object and the catalog slug: the
/// daemon command loop stays the sole owner of the providers map, and the
/// tool never learns which wire protocol is behind the client (same
/// protocol-erasure rule as [`InferenceProvider`]). The slug travels
/// alongside the client because the tool does catalog-based model selection
/// (`model_supports_image_output` / `image_models_for_provider`), and the
/// client's own `provider_slug()` may be generic — same reasoning as the
/// `slug` field below.
#[derive(Clone, Debug)]
pub struct ImageProviderHandle {
    /// Catalog slug of the resolved account's provider (e.g. "openai",
    /// "opencode") — the key for image-capability catalog lookups.
    pub slug: String,
    /// The image-generation client. Arc-cloned out of the providers map: the
    /// command loop keeps its entry (so later requests resolve again) while
    /// the tool thread owns a share — no shared mutable state, the client is
    /// immutable.
    pub client: Arc<dyn ImageGenerationClient>,
}

/// User-Agent product string for every inference request: names the daemon
/// with its own crate version (the version users actually run), so providers'
/// metrics and any UA-based allowlisting see "choreographr/x.y.z" instead of
/// ureq's generic default.
fn daemon_user_agent() -> String {
    format!("choreographr/{}", env!("CARGO_PKG_VERSION"))
}

impl InferenceProvider {
    pub fn from_openai(client: OpenAiClient) -> Self {
        Self {
            client: Arc::new(client),
            slug: "openai".to_string(),
            image_client: None,
        }
    }

    pub fn from_anthropic(client: AnthropicClient) -> Self {
        Self {
            client: Arc::new(client),
            slug: "anthropic".to_string(),
            image_client: None,
        }
    }

    pub fn from_google(client: GoogleClient) -> Self {
        Self {
            client: Arc::new(client),
            slug: "google".to_string(),
            image_client: None,
        }
    }

    /// Create a provider from an account config + credential key.
    /// Applies all account-level overrides (base_url, streaming, timeouts)
    /// onto the service config before constructing the client.
    pub fn from_account_config(
        config: &crate::accounts::AccountConfig,
        api_key: Option<String>,
        // The daemon-wide registry (one per process, created in
        // `DaemonState::open`). Every client this account produces registers
        // its sockets here, so cancel/suspend force-closes reach all of them
        // — a per-account registry would leave other accounts' wedged
        // readers blocked, defeating the whole point of `shutdown_all`.
        registry: &choreo_ai_protocols::SocketRegistry,
    ) -> Result<Self, String> {
        let entry = lookup_provider(&config.provider)
            .ok_or_else(|| format!("unknown provider: {}", config.provider))?;

        match entry.protocol {
            ProviderProtocol::OpenAi { max_tokens_field } => {
                let mut svc_config = ServiceConfig {
                    base_url: entry.base_url.to_string(),
                    chat_completions_max_tokens_field: max_tokens_field,
                    provider_slug: entry.slug.clone(),
                    // Every inference request identifies as the daemon
                    // ("choreographr/<version>"), not ureq's default — see
                    // `build_agent`. One construction point covers all
                    // accounts; tests and other callers keep `None`.
                    user_agent: Some(daemon_user_agent()),
                    ..Default::default()
                };
                config.apply_overrides(&mut svc_config);
                let key = api_key
                    .ok_or_else(|| format!("no API key for '{}' provider", config.provider))?;
                let client = OpenAiClient::new(svc_config.clone(), key.clone(), registry)
                    .map_err(|e| format!("failed to create OpenAI client: {e}"))?;
                // Same account, same key, same base_url/user_agent/slug as the
                // chat client — the image client is built from a clone of the
                // identical `ServiceConfig` (its constructor overrides only the
                // attempt deadline + retry budget the image path needs).
                // `config.apply_overrides` ran on `svc_config` BEFORE the chat
                // client was built, so the clone here sees every account-level
                // override (base_url, user agent, backoff knobs) already
                // applied. An image client is constructed even when the
                // account's models later fail a catalog image-capability
                // check — the tool gates model choice at request time.
                //
                // Zhipu (z.ai coding + mainland zhipuai/bigmodel) accounts
                // route their Images API at a different endpoint than the
                // generic OpenAI one (and z.ai's chat base — the `/coding`
                // plan path — is not where images are served), so those two
                // slugs get the dedicated [`ZaiImageClient`]; every other
                // OpenAI-protocol provider keeps the default
                // [`OpenAiImageClient`]. `ZaiImageClient` itself rewrites the
                // coding base to the plain PaaS-v4 base, so both slugs share
                // one adapter with one documented endpoint convention.
                let image_client: Arc<dyn ImageGenerationClient> =
                    // The client crate owns the provider-family knowledge
                    // (which slugs speak the Zhipu Images contract) — see
                    // images::is_zhipu_image_provider_slug's doc comment.
                    if choreo_ai_protocols::images::is_zhipu_image_provider_slug(&entry.slug) {
                        Arc::new(ZaiImageClient::new(svc_config, key, registry))
                    } else {
                        Arc::new(OpenAiImageClient::new(svc_config, key, registry))
                    };
                Ok(Self {
                    client: Arc::new(client),
                    slug: entry.slug,
                    image_client: Some(image_client),
                })
            }
            ProviderProtocol::AnthropicMessages => {
                let key = api_key
                    .ok_or_else(|| format!("no API key for '{}' provider", config.provider))?;
                let mut anthro_cfg = AnthropicConfig::default();
                // If the account doesn't specify a base_url, use the catalog default.
                if config.base_url.is_none() {
                    anthro_cfg.base_url = entry.base_url.to_string();
                }
                // Catalog slug (not hardcoded "anthropic") so gateway header
                // gating and metrics see e.g. "opencode-go-anthropic-compatible".
                anthro_cfg.provider_slug = entry.slug.clone();
                anthro_cfg.user_agent = Some(daemon_user_agent());
                let overrides = config.provider_overrides();
                anthro_cfg.apply_overrides(&overrides);
                let client = AnthropicClient::new(anthro_cfg, key, registry)
                    .map_err(|e| format!("failed to create Anthropic client: {e}"))?;
                Ok(Self {
                    client: Arc::new(client),
                    slug: entry.slug,
                    // No image backend for Anthropic in v1 — the Messages API
                    // has no image-generation endpoint; deferred.
                    image_client: None,
                })
            }
            ProviderProtocol::GoogleGenerativeAi => {
                let key = api_key
                    .ok_or_else(|| format!("no API key for '{}' provider", config.provider))?;
                let mut google_cfg = GoogleConfig::default();
                // If the account doesn't specify a base_url, use the catalog default.
                if config.base_url.is_none() {
                    google_cfg.base_url = entry.base_url.to_string();
                }
                google_cfg.user_agent = Some(daemon_user_agent());
                let overrides = config.provider_overrides();
                google_cfg.apply_overrides(&overrides);
                let client = GoogleClient::new(google_cfg, key, registry)
                    .map_err(|e| format!("failed to create Google client: {e}"))?;
                Ok(Self {
                    client: Arc::new(client),
                    slug: entry.slug,
                    // No image backend for Gemini in v1 — the Gemini image
                    // backend is deferred; `None` keeps image requests against
                    // these accounts failing with a precise error.
                    image_client: None,
                })
            }
            // `ProviderProtocol` is #[non_exhaustive] — a new protocol added
            // in choreo-ai-protocols must not silently fall through to a
            // bogus provider; surface it as an explicit error instead.
            _ => Err(format!(
                "unsupported provider protocol for '{}'",
                config.provider
            )),
        }
    }

    pub fn chat_completion_turn(
        &self,
        params: ChatTurnRequest<'_>,
    ) -> Result<ChatTurnResult, InferenceError> {
        let start = std::time::Instant::now();
        // `params.model` is a `&'a str` (Copy) — capture it before moving
        // `params` into the client call so metrics recording needs no
        // per-turn heap allocation.
        let model = params.model;
        let result = self.client.chat_completion_turn(params);
        self.record_api_metrics(model, start, &result);
        result
    }

    pub fn chat_completion_turn_streaming(
        &self,
        params: ChatTurnRequest<'_>,
        on_event: &mut dyn FnMut(StreamEvent) -> io::Result<()>,
    ) -> Result<ChatTurnResult, InferenceError> {
        let start = std::time::Instant::now();
        // Same borrow-safe capture as in `chat_completion_turn`: the model
        // name outlives `params` (it borrows from the caller), so copying the
        // `&str` before the move avoids allocating a String per turn.
        let model = params.model;
        let result = self.client.chat_completion_turn_streaming(params, on_event);
        self.record_api_metrics(model, start, &result);
        result
    }

    /// Record API-call metrics around a provider result.  Timing lives here
    /// (not inside `choreo-ai-protocols`) so the provider crates stay free of
    /// daemon concerns.  The metric label is the catalog slug — more precise
    /// than the protocol name (e.g. "opencode" rather than "openai").
    fn record_api_metrics<T>(
        &self,
        model: &str,
        start: std::time::Instant,
        result: &Result<T, InferenceError>,
    ) {
        let elapsed = start.elapsed().as_secs_f64();
        crate::metrics::record_api_call(model, self.slug.as_str(), elapsed);
        if let Err(e) = result {
            // The error→label mapping lives with the error type in
            // choreo-proto (InferenceError::metric_label) so it can't drift
            // from the variant list; reuse it rather than duplicating here.
            crate::metrics::record_api_error(model, e.metric_label());
        }
    }

    /// The optional image-generation backend for this provider.
    ///
    /// Returns a clone of the `Arc`: the providers map entry stays intact
    /// (subsequent requests resolve again) while the caller — typically a
    /// tool thread via `DaemonCommand::GetImageGenerationProvider` — owns a
    /// share. The client is immutable, so sharing it is safe with no
    /// additional synchronization.
    pub fn image_client(&self) -> Option<Arc<dyn ImageGenerationClient>> {
        self.image_client.clone()
    }

    /// Return the provider slug (e.g. "openai", "anthropic").
    pub fn provider_slug(&self) -> &str {
        self.slug.as_str()
    }

    /// Resolve the context window for a model, using the client config first
    /// and falling back to the static catalog for known model slugs.
    pub fn resolve_context_window(&self, model: &str) -> Option<u32> {
        self.client
            .context_window_for_model(model)
            .or_else(|| choreo_ai_protocols::lookup_context_window(&self.slug, model))
    }

    pub fn list_models(&self) -> Result<Vec<String>, InferenceError> {
        self.client.list_models()
    }

    pub fn supports_programmatic_tool_calling(&self, model: &str) -> bool {
        self.client.supports_programmatic_tool_calling(model)
    }
}

#[cfg(test)]
// Tests in this module read the process-wide `PROVIDER_CATALOG` ArcSwap
// (`resolve_context_window` falls back to `lookup_context_window`), which the
// daemon catalog-swap tests (`daemon.rs`, `#[serial(catalog)]`) mutate
// concurrently. Under libtest's in-process parallel execution a swap can land
// mid-assertion and the lookup resolves from the wrong catalog (nextest
// isolates each test in its own process, so this only bites the `cargo test`
// fallback). Sharing the `catalog` serial key with every catalog
// reader/mutator in this binary serializes them against each other.
#[serial_test::serial(catalog)]
mod tests {
    use super::*;
    use crate::accounts::AccountConfig;
    use choreo_ai_protocols::openai::ServiceConfig;

    #[test]
    fn from_openai_constructs_provider() {
        let config = ServiceConfig::default();
        let client = OpenAiClient::new(
            config,
            "test-key".into(),
            &choreo_ai_protocols::SocketRegistry::new(),
        )
        .unwrap();
        let _provider = InferenceProvider::from_openai(client);
        // Construction succeeds — no panic.
    }

    #[test]
    fn from_anthropic_constructs_provider() {
        let config = AnthropicConfig::default();
        let client = AnthropicClient::new(
            config,
            "test-key".into(),
            &choreo_ai_protocols::SocketRegistry::new(),
        )
        .unwrap();
        let _provider = InferenceProvider::from_anthropic(client);
        // Construction succeeds — no panic.
    }

    #[test]
    fn from_google_constructs_provider() {
        let config = GoogleConfig::default();
        let client = GoogleClient::new(
            config,
            "test-key".into(),
            &choreo_ai_protocols::SocketRegistry::new(),
        )
        .unwrap();
        let _provider = InferenceProvider::from_google(client);
        // Construction succeeds — no panic.
    }

    #[test]
    fn from_account_config_unknown_provider_errors() {
        let cfg = AccountConfig::simple("unknown", "nonexistent");
        let err = InferenceProvider::from_account_config(
            &cfg,
            Some("key".into()),
            &choreo_ai_protocols::SocketRegistry::new(),
        )
        .unwrap_err();
        assert!(err.contains("unknown provider"), "{err}");
    }

    #[test]
    fn from_account_config_anthropic_requires_key() {
        let cfg = AccountConfig::simple("claude", "anthropic");
        let err = InferenceProvider::from_account_config(
            &cfg,
            None,
            &choreo_ai_protocols::SocketRegistry::new(),
        )
        .unwrap_err();
        assert!(err.contains("no API key"), "{err}");
    }

    #[test]
    fn from_account_config_openai_missing_key_errors() {
        let cfg = AccountConfig::simple("openai", "openai");
        let err = InferenceProvider::from_account_config(
            &cfg,
            None,
            &choreo_ai_protocols::SocketRegistry::new(),
        )
        .unwrap_err();
        assert!(err.contains("no API key"), "{err}");
    }

    #[test]
    fn from_account_config_openai_succeeds() {
        let cfg = AccountConfig::simple("openai", "openai");
        let result = InferenceProvider::from_account_config(
            &cfg,
            Some("key".into()),
            &choreo_ai_protocols::SocketRegistry::new(),
        );
        assert!(result.is_ok(), "{:?}", result.err());
    }

    #[test]
    fn from_account_config_anthropic_succeeds() {
        let cfg = AccountConfig::simple("claude", "anthropic");
        let result = InferenceProvider::from_account_config(
            &cfg,
            Some("key".into()),
            &choreo_ai_protocols::SocketRegistry::new(),
        );
        assert!(result.is_ok(), "{:?}", result.err());
    }

    #[test]
    fn from_account_config_google_succeeds() {
        let cfg = AccountConfig::simple("gemini", "google");
        let result = InferenceProvider::from_account_config(
            &cfg,
            Some("key".into()),
            &choreo_ai_protocols::SocketRegistry::new(),
        );
        assert!(result.is_ok(), "{:?}", result.err());
    }

    #[test]
    fn from_account_config_google_missing_key_errors() {
        let cfg = AccountConfig::simple("gemini", "google");
        let err = InferenceProvider::from_account_config(
            &cfg,
            None,
            &choreo_ai_protocols::SocketRegistry::new(),
        )
        .unwrap_err();
        assert!(err.contains("no API key"), "{err}");
    }

    #[test]
    fn from_account_config_zai_routes_to_dedicated_image_client() {
        // The z.ai chat path is OpenAI-compatible, but its Images API is a
        // different endpoint shape (URL-returning glm-image, no-n body) —
        // the image backend must be the dedicated adapter, not the generic
        // OpenAI one. Asserted via the redacted Debug (which names the
        // concrete struct), because the trait object carries no type shape.
        let cfg = AccountConfig::simple("zai", "zai");
        let provider = InferenceProvider::from_account_config(
            &cfg,
            Some("key".into()),
            &choreo_ai_protocols::SocketRegistry::new(),
        )
        .expect("zai account constructs");
        let image_client = provider
            .image_client()
            .expect("OpenAI protocol gets an image client");
        let debug = format!("{:?}", image_client);
        assert!(debug.starts_with("ZaiImageClient"), "{debug}");
        // Mainland zhipuai resolves to the same Zhipu image adapter.
        let cfg = AccountConfig::simple("zhipu", "zhipuai");
        let provider = InferenceProvider::from_account_config(
            &cfg,
            Some("key".into()),
            &choreo_ai_protocols::SocketRegistry::new(),
        )
        .expect("zhipuai account constructs");
        let debug = format!(
            "{:?}",
            provider.image_client().expect("image client present")
        );
        assert!(debug.starts_with("ZaiImageClient"), "{debug}");
    }

    #[test]
    fn from_account_config_openai_keeps_default_image_client() {
        // Other OpenAI-protocol providers (and the plain openai slug) must
        // NOT have been switched to the z.ai adapter.
        let cfg = AccountConfig::simple("openai", "openai");
        let provider = InferenceProvider::from_account_config(
            &cfg,
            Some("key".into()),
            &choreo_ai_protocols::SocketRegistry::new(),
        )
        .expect("openai account constructs");
        let debug = format!(
            "{:?}",
            provider.image_client().expect("image client present")
        );
        assert!(debug.starts_with("OpenAiImageClient"), "{debug}");
    }

    #[test]
    fn anthropic_provider_list_models_returns_known() {
        let config = AnthropicConfig::default();
        let client = AnthropicClient::new(
            config,
            "test-key".into(),
            &choreo_ai_protocols::SocketRegistry::new(),
        )
        .unwrap();
        let provider = InferenceProvider::from_anthropic(client);
        let models = provider.list_models().unwrap();
        assert!(!models.is_empty());
        assert!(models.contains(&"claude-sonnet-4-20250514".to_string()));
    }

    #[test]
    fn resolve_context_window_uses_client_then_catalog() {
        let mut cfg = ServiceConfig::default();
        cfg.context_window_config.per_model = [("gpt-4.1-nano".into(), 1_048_576)].into();
        cfg.context_window_config.context_window = Some(128_000);
        let client = OpenAiClient::new(
            cfg,
            "test-key".into(),
            &choreo_ai_protocols::SocketRegistry::new(),
        )
        .unwrap();
        let provider = InferenceProvider::from_openai(client);
        // Per-model from client config
        assert_eq!(
            provider.resolve_context_window("gpt-4.1-nano"),
            Some(1_048_576)
        );
        // Global fallback from client config
        assert_eq!(
            provider.resolve_context_window("unknown-model"),
            Some(128_000)
        );
    }

    #[test]
    fn resolve_context_window_falls_back_to_catalog() {
        // Anthropic provider with default config has no per-model map entries
        // and no global fallback, so it falls back to the catalog.
        let config = AnthropicConfig::default();
        let client = AnthropicClient::new(
            config,
            "test-key".into(),
            &choreo_ai_protocols::SocketRegistry::new(),
        )
        .unwrap();
        let provider = InferenceProvider::from_anthropic(client);
        assert_eq!(
            provider.resolve_context_window("claude-sonnet-4-6"),
            Some(1_000_000)
        );
        // Unknown model — neither client nor catalog knows it
        assert_eq!(provider.resolve_context_window("completely-unknown"), None);
    }
}

/// Stub provider client and factory for use in daemon-level unit tests.
/// Only checks provider existence — all provider methods panic.
#[cfg(test)]
pub(crate) mod test_util {
    use super::*;

    #[derive(Debug)]
    pub(crate) struct StubProviderClient;

    impl ProviderClient for StubProviderClient {
        fn provider_slug(&self) -> &str {
            "test-stub"
        }

        fn chat_completion_turn(
            &self,
            _params: ChatTurnRequest<'_>,
        ) -> Result<ChatTurnResult, InferenceError> {
            panic!("StubProviderClient is not intended for real use");
        }

        fn chat_completion_turn_streaming(
            &self,
            _params: ChatTurnRequest<'_>,
            _on_event: &mut dyn FnMut(StreamEvent) -> io::Result<()>,
        ) -> Result<ChatTurnResult, InferenceError> {
            panic!("StubProviderClient is not intended for real use");
        }

        fn list_models(&self) -> Result<Vec<String>, InferenceError> {
            panic!("StubProviderClient is not intended for real use");
        }
    }

    pub(crate) fn make_test_provider() -> InferenceProvider {
        InferenceProvider {
            client: Arc::new(StubProviderClient),
            slug: "test-stub".to_string(),
            image_client: None,
        }
    }

    /// Provider client that fails every streaming turn with a 4xx client
    /// error — used to exercise the agent loop's request-failure path
    /// (turn error marking + finalize) without touching the network.
    #[derive(Debug)]
    pub(crate) struct FailingProviderClient;

    impl ProviderClient for FailingProviderClient {
        fn provider_slug(&self) -> &str {
            "test-failing"
        }

        fn chat_completion_turn(
            &self,
            _params: ChatTurnRequest<'_>,
        ) -> Result<ChatTurnResult, InferenceError> {
            Err(InferenceError::ClientError {
                status: 402,
                detail: "Insufficient Balance".to_string(),
            })
        }

        fn chat_completion_turn_streaming(
            &self,
            _params: ChatTurnRequest<'_>,
            _on_event: &mut dyn FnMut(StreamEvent) -> io::Result<()>,
        ) -> Result<ChatTurnResult, InferenceError> {
            Err(InferenceError::ClientError {
                status: 402,
                detail: "Insufficient Balance".to_string(),
            })
        }

        fn list_models(&self) -> Result<Vec<String>, InferenceError> {
            Err(InferenceError::ClientError {
                status: 402,
                detail: "Insufficient Balance".to_string(),
            })
        }
    }

    pub(crate) fn make_failing_provider() -> InferenceProvider {
        InferenceProvider {
            client: Arc::new(FailingProviderClient),
            slug: "test-failing".to_string(),
            image_client: None,
        }
    }
}
