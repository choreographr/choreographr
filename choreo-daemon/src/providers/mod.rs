//! Provider construction and the protocol-erased [`InferenceProvider`] facade.
//!
//! [`InferenceProvider`] is the only daemon type that dispatches by wire
//! protocol; all protocol knowledge lives in `choreo-ai-protocols`. The
//! `from_*` constructors wrap an already-built client, while
//! [`InferenceProvider::from_account_config`] resolves an account's provider
//! slug through the catalog and builds the matching client, registering its
//! sockets in the `SocketRegistry` of the unit of work that will use it (a
//! session's registry for session-scoped requests, otherwise the daemon-wide
//! one).

use std::io;
use std::sync::Arc;

use choreo_ai_protocols::openai::{OpenAiClient, ServiceConfig};
use choreo_ai_protocols::{
    AnthropicClient, AnthropicConfig, ChatTurnRequest, ChatTurnResult, FalImageClient,
    GoogleClient, GoogleConfig, GoogleImageClient, ImageGenerationClient, OpenAiImageClient,
    ProviderClient, ProviderEntry, ProviderProtocol, StreamEvent, ZaiImageClient, lookup_provider,
};
use choreo_proto::InferenceError;

mod auth;

pub use auth::ProviderAuth;

/// Whether a catalog swap changed anything [`InferenceProvider::from_account_config`]
/// bakes into a provider *client*.
///
/// The constructor reads exactly four catalog fields — the protocol (which
/// selects the client type and carries the `OpenAI` `max_tokens_field`), the base
/// URL, the request-body compression coding, and the cache-affinity dialect. A
/// change in any of them makes an already-built client for this slug stale, so a
/// catalog swap must rebuild it. Everything else on the entry (the curated model
/// list, contexts, costs, `default_model`, prompt-cache TTLs) is resolved live
/// per request and is deliberately excluded here: those change on an ordinary
/// models.dev refresh, and invalidating every session then would tear down warm
/// connection pools for nothing. The slug is the map key, so it is not compared
/// here.
#[must_use]
pub(crate) fn client_config_changed(before: &ProviderEntry, after: &ProviderEntry) -> bool {
    before.protocol != after.protocol
        || before.base_url != after.base_url
        || before.request_compression != after.request_compression
        || before.cache_affinity != after.cache_affinity
}

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
    /// return a generic value (e.g. `OpenAiClient` always says "openai"). Owned
    /// because the catalog lookup that supplies it returns a clone, not a
    /// `'static` reference.
    slug: String,
    /// Optional image-generation backend. `Some` for OpenAI-protocol
    /// providers — the plain OpenAI-protocol proxy families use the
    /// [`OpenAiImageClient`] default, while the two Zhipu slugs (`zai`, the
    /// z.ai coding gateway, and `zhipuai`, the mainland bigmodel endpoint)
    /// get the dedicated [`ZaiImageClient`] since their Images API is a
    /// different endpoint contract (URL-returning glm-image, hd/standard
    /// quality, no-n body). Gemini-protocol providers get the dedicated
    /// [`GoogleImageClient`], whose image models generate and edit through the
    /// `generateContent` API rather than a dedicated Images endpoint. Anthropic
    /// has no image-generation backend, so its constructor leaves this `None`
    /// and image requests against those accounts surface a precise "does not
    /// support" error instead of a half-working client.
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

/// User-Agent product string for both callers: every inference request and
/// the `http_request` tool. Both are raw `ureq` clients reaching servers that
/// use the User-Agent for metrics and allowlisting, so both must name the
/// daemon identically.
pub(crate) fn daemon_user_agent() -> String {
    format!("choreographr/{}", env!("CARGO_PKG_VERSION"))
}

impl InferenceProvider {
    /// Construct a provider from an already-built [`OpenAiClient`], tagged with
    /// the catalog slug `"openai"`. Image generation is left unconfigured —
    /// [`from_account_config`](Self::from_account_config) is what attaches an
    /// image backend based on the account's protocol.
    #[must_use]
    pub fn from_openai(client: OpenAiClient) -> Self {
        Self {
            client: Arc::new(client),
            slug: "openai".to_string(),
            image_client: None,
        }
    }

    /// Construct a provider from an already-built [`AnthropicClient`], tagged
    /// with the catalog slug `"anthropic"`. Anthropic has no image-generation
    /// backend, so the image client is left unconfigured.
    #[must_use]
    pub fn from_anthropic(client: AnthropicClient) -> Self {
        Self {
            client: Arc::new(client),
            slug: "anthropic".to_string(),
            image_client: None,
        }
    }

    /// Construct a provider from an already-built [`GoogleClient`], tagged with
    /// the catalog slug `"google"`. The image backend is left unconfigured —
    /// [`from_account_config`](Self::from_account_config) attaches the
    /// [`GoogleImageClient`] based on the account's protocol.
    #[must_use]
    pub fn from_google(client: GoogleClient) -> Self {
        Self {
            client: Arc::new(client),
            slug: "google".to_string(),
            image_client: None,
        }
    }

    /// Create a provider from an account config + resolved auth grant.
    /// Applies all account-level overrides (`base_url`, streaming, timeouts)
    /// onto the service config before constructing the client.
    ///
    /// # Errors
    ///
    /// Returns Err if the account's provider protocol is not supported, the
    /// grant is missing/unsuitable (no credential, an OAuth grant with no
    /// access token yet), or the client construction fails.
    pub fn from_account_config(
        config: &crate::accounts::AccountConfig,
        auth: ProviderAuth,
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
                    base_url: entry.base_url.clone(),
                    chat_completions_max_tokens_field: max_tokens_field,
                    provider_slug: entry.slug.clone(),
                    // Every inference request identifies as the daemon
                    // ("choreographr/<version>"), not ureq's default — see
                    // `build_agent`. One construction point covers all
                    // accounts; tests and other callers keep `None`.
                    user_agent: Some(daemon_user_agent()),
                    // Compress the request body (gzip or zstd) when the catalog
                    // overlay marks this provider as accepting compressed
                    // request bodies (identity otherwise — the safe default).
                    request_compression: entry.request_compression,
                    // Attach the per-session cache-affinity body field the
                    // provider documents (none by default), filled with the
                    // turn's real session id at request time.
                    cache_affinity: entry.cache_affinity,
                    ..Default::default()
                };
                // The transport/endpoint knobs this account overrides on the
                // service config (base URL, streaming, timeouts, model limits).
                config.apply_to_service_config(&mut svc_config);
                // The protocol-agnostic carrier the Anthropic and Google adapters
                // also consume, applied exactly as they do: OpenAI's other shared
                // overrides are already on `svc_config` from the call above, and
                // this adds the carrier fields OpenAI alone reads — the
                // per-session cache-affinity override. Built once, like the other
                // two arms, so the carrier is not re-assembled per call.
                let overrides = config.provider_overrides();
                svc_config.apply_overrides(&overrides);
                // Image generation is API-key-only, so capture the key (if any)
                // BEFORE consuming the grant: an OAuth-only account simply has
                // no image backend. The daemon-side grant is then projected
                // into the client-side request material — this is where a
                // not-yet-signed-in OAuth account becomes a clean error before
                // any client is built.
                let image_key = auth.api_key_cloned();
                let request_auth = auth.into_request_auth()?;
                let client = OpenAiClient::new(svc_config.clone(), request_auth, registry)
                    .map_err(|e| format!("failed to create OpenAI client: {e}"))?;
                // Same account, same base_url/user_agent/slug as the chat
                // client — the image client is built from a clone of the
                // identical `ServiceConfig` (its constructor overrides only the
                // attempt deadline + retry budget the image path needs).
                // `config.apply_to_service_config` ran on `svc_config` BEFORE the
                // chat client was built, so the clone here sees every
                // account-level override (base_url, user agent, backoff knobs)
                // already applied. An image client is constructed even when the
                // account's models later fail a catalog image-capability
                // check — the tool gates model choice at request time.
                //
                // Image generation is API-key-only: an OAuth account carries no
                // key, so it gets no image backend and an image request against
                // it fails with a precise "does not support" error.
                //
                // Zhipu (z.ai coding + mainland zhipuai/bigmodel) accounts
                // route their Images API at a different endpoint than the
                // generic OpenAI one (and z.ai's chat base — the `/coding`
                // plan path — is not where images are served), so those two
                // slugs get the dedicated [`ZaiImageClient`]; fal.ai accounts
                // get the dedicated [`FalImageClient`] (its synchronous
                // `{base}/{model}` Images contract has no OpenAI shape at all);
                // every other OpenAI-protocol provider keeps the default
                // [`OpenAiImageClient`]. `ZaiImageClient` itself rewrites the
                // coding base to the plain PaaS-v4 base, so both Zhipu slugs
                // share one adapter with one documented endpoint convention.
                let image_client: Option<Arc<dyn ImageGenerationClient>> =
                    image_key.map(|key| -> Arc<dyn ImageGenerationClient> {
                        // The client crate owns the provider-family knowledge
                        // (which slugs speak the Zhipu vs fal Images contract) —
                        // see images::is_zhipu_image_provider_slug /
                        // is_fal_image_provider_slug.
                        if choreo_ai_protocols::images::is_fal_image_provider_slug(&entry.slug) {
                            Arc::new(FalImageClient::new(svc_config, key, registry))
                        } else if choreo_ai_protocols::images::is_zhipu_image_provider_slug(
                            &entry.slug,
                        ) {
                            Arc::new(ZaiImageClient::new(svc_config, key, registry))
                        } else {
                            Arc::new(OpenAiImageClient::new(svc_config, key, registry))
                        }
                    });
                Ok(Self {
                    client: Arc::new(client),
                    slug: entry.slug,
                    image_client,
                })
            }
            ProviderProtocol::AnthropicMessages => {
                // Anthropic is API-key-only in this stack (no OAuth path), so a
                // grant that is not an API key is rejected with the existing
                // "no API key" guidance.
                let key = auth
                    .api_key_cloned()
                    .ok_or_else(|| format!("no API key for '{}' provider", config.provider))?;
                let mut anthro_cfg = AnthropicConfig::default();
                // If the account doesn't specify a base_url, use the catalog default.
                if config.base_url.is_none() {
                    anthro_cfg.base_url.clone_from(&entry.base_url);
                }
                // Catalog slug (not hardcoded "anthropic") so gateway header
                // gating and metrics see e.g. "opencode-go-anthropic-compatible".
                anthro_cfg.provider_slug.clone_from(&entry.slug);
                anthro_cfg.user_agent = Some(daemon_user_agent());
                // See the OpenAI arm: overlay-driven request-body compression.
                anthro_cfg.request_compression = entry.request_compression;
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
                // Google is API-key-only in this stack (no OAuth path), so a
                // grant that is not an API key is rejected with the existing
                // "no API key" guidance.
                let key = auth
                    .api_key_cloned()
                    .ok_or_else(|| format!("no API key for '{}' provider", config.provider))?;
                let mut google_cfg = GoogleConfig::default();
                // If the account doesn't specify a base_url, use the catalog default.
                if config.base_url.is_none() {
                    google_cfg.base_url.clone_from(&entry.base_url);
                }
                google_cfg.user_agent = Some(daemon_user_agent());
                // See the OpenAI arm: overlay-driven request-body compression.
                // The catalog slug keys the compression downgrade registry so a
                // gateway provider keeps its own state.
                google_cfg.provider_slug.clone_from(&entry.slug);
                google_cfg.request_compression = entry.request_compression;
                let overrides = config.provider_overrides();
                google_cfg.apply_overrides(&overrides);
                let client = GoogleClient::new(google_cfg.clone(), key.clone(), registry)
                    .map_err(|e| format!("failed to create Google client: {e}"))?;
                // Same account, same key, same base_url/user_agent/slug as the
                // chat client — the image client is built from a clone of the
                // identical `GoogleConfig` (its constructor overrides only the
                // attempt deadline + retry budget the image path needs).
                // `config.provider_overrides` ran on `google_cfg` BEFORE the
                // chat client was built, so the clone here sees every
                // account-level override already applied. Gemini image models
                // generate and edit through the `generateContent` API, so this
                // is the dedicated [`GoogleImageClient`], not the OpenAI-shaped
                // default.
                let image_client: Arc<dyn ImageGenerationClient> =
                    Arc::new(GoogleImageClient::new(google_cfg, key, registry));
                Ok(Self {
                    client: Arc::new(client),
                    slug: entry.slug,
                    image_client: Some(image_client),
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

    /// Run a single non-streaming chat-completion turn and record API
    /// latency metrics for it.
    ///
    /// # Errors
    ///
    /// Returns Err when the underlying client reports an
    /// [`InferenceError`] (transport, auth, or API failure).
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

    /// Run a single streaming chat-completion turn, forwarding events to
    /// `on_event`, and record API latency metrics for it.
    ///
    /// # Errors
    ///
    /// Returns Err when the underlying client reports an
    /// [`InferenceError`] (transport, auth, or API failure) or the event
    /// callback fails with an I/O error.
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
    #[must_use]
    pub fn image_client(&self) -> Option<Arc<dyn ImageGenerationClient>> {
        self.image_client.clone()
    }

    /// Return the provider slug (e.g. "openai", "anthropic").
    #[must_use]
    pub fn provider_slug(&self) -> &str {
        self.slug.as_str()
    }

    /// The per-session cache-affinity dialect this provider's request bodies
    /// carry, if any — the catalog dialect folded with the account override,
    /// BEFORE the process-wide rejection registry. Deferred to the underlying
    /// client (only the OpenAI-compatible one has the capability; the rest
    /// answer [`choreo_ai_protocols::CacheAffinity::Off`]). The cache-warming
    /// gate reads this to tell an intentional hint from one the registry
    /// withheld for a model that rejected it.
    #[must_use]
    pub fn cache_affinity(&self) -> choreo_ai_protocols::CacheAffinity {
        self.client.cache_affinity()
    }

    /// Whether a request this provider sends for `model` will carry an extended
    /// prompt-cache retention selector, i.e. whether its prompt cache will live
    /// for the model's LONG tier rather than the default short one. Deferred to
    /// the underlying client (only the OpenAI-compatible one has the capability;
    /// the rest answer `false`). The cache warmer reads this to pick the tier its
    /// ping schedule is computed against.
    #[must_use]
    pub fn extended_cache_retention(&self, model: &str) -> bool {
        self.client.extended_cache_retention(model)
    }

    /// Resolve the context window for a model, using the client config first
    /// and falling back to the static catalog for known model slugs.
    #[must_use]
    pub fn resolve_context_window(&self, model: &str) -> Option<u32> {
        self.client
            .context_window_for_model(model)
            .or_else(|| choreo_ai_protocols::lookup_context_window(&self.slug, model))
    }

    /// List the model slugs available from this provider.
    ///
    /// # Errors
    ///
    /// Returns Err when the underlying client reports an
    /// [`InferenceError`] (transport, auth, or API failure).
    pub fn list_models(&self) -> Result<Vec<String>, InferenceError> {
        self.client.list_models()
    }

    /// Whether the underlying client reports that `model` can be driven with
    /// programmatic (model-issued) tool calling; the answer is protocol-
    /// specific.
    #[must_use]
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
    use choreo_ai_protocols::openai::{RequestAuth, ServiceConfig};

    /// An API-key grant for the happy-path constructions.
    fn api_key_auth() -> ProviderAuth {
        ProviderAuth::ApiKey {
            key: zeroize::Zeroizing::new("key".to_string()),
        }
    }

    /// An OAuth grant with no cached access token — the not-signed-in case.
    fn oauth_without_token_auth() -> ProviderAuth {
        ProviderAuth::OAuth {
            credential: Box::new(choreo_keystore::ServiceCredential::OAuth {
                kind: choreo_keystore::OAuthKind::Provider,
                issuer: None,
                resource: None,
                authorization_server: None,
                token_endpoint: None,
                refresh_encoding: None,
                refresh_token: None,
                access_token: None,
                account_id: None,
                client_id: None,
                client_secret: None,
                granted_scopes: Vec::new(),
                token_type: None,
            }),
            access_token: None,
        }
    }
    #[test]
    fn from_openai_constructs_provider() {
        let config = ServiceConfig::default();
        let client = OpenAiClient::new(
            config,
            RequestAuth::api_key("test-key"),
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
            api_key_auth(),
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
            oauth_without_token_auth(),
            &choreo_ai_protocols::SocketRegistry::new(),
        )
        .unwrap_err();
        assert!(err.contains("no API key"), "{err}");
    }

    #[test]
    fn from_account_config_openai_not_signed_in_errors() {
        // An OAuth grant with no cached access token: the OpenAI path fails
        // with the not-signed-in guidance, not a generic missing-credential
        // message.
        let cfg = AccountConfig::simple("openai", "openai");
        let err = InferenceProvider::from_account_config(
            &cfg,
            oauth_without_token_auth(),
            &choreo_ai_protocols::SocketRegistry::new(),
        )
        .unwrap_err();
        assert!(err.contains("sign in first"), "{err}");
    }

    #[test]
    fn from_account_config_openai_succeeds() {
        let cfg = AccountConfig::simple("openai", "openai");
        let result = InferenceProvider::from_account_config(
            &cfg,
            api_key_auth(),
            &choreo_ai_protocols::SocketRegistry::new(),
        );
        assert!(result.is_ok(), "{:?}", result.err());
    }

    #[test]
    fn from_account_config_anthropic_succeeds() {
        let cfg = AccountConfig::simple("claude", "anthropic");
        let result = InferenceProvider::from_account_config(
            &cfg,
            api_key_auth(),
            &choreo_ai_protocols::SocketRegistry::new(),
        );
        assert!(result.is_ok(), "{:?}", result.err());
    }

    #[test]
    fn from_account_config_google_succeeds() {
        let cfg = AccountConfig::simple("gemini", "google");
        let result = InferenceProvider::from_account_config(
            &cfg,
            api_key_auth(),
            &choreo_ai_protocols::SocketRegistry::new(),
        );
        assert!(result.is_ok(), "{:?}", result.err());
    }

    #[test]
    fn from_account_config_google_missing_key_errors() {
        let cfg = AccountConfig::simple("gemini", "google");
        let err = InferenceProvider::from_account_config(
            &cfg,
            oauth_without_token_auth(),
            &choreo_ai_protocols::SocketRegistry::new(),
        )
        .unwrap_err();
        assert!(err.contains("no API key"), "{err}");
    }

    #[test]
    fn from_account_config_google_routes_to_dedicated_image_client() {
        // Gemini image models generate and edit via the `generateContent` API,
        // not a dedicated Images endpoint, so the image backend must be the
        // dedicated GoogleImageClient — not the generic OpenAI adapter. Asserted
        // via the redacted Debug (which names the concrete struct), because the
        // trait object carries no type shape.
        let cfg = AccountConfig::simple("gemini", "google");
        let provider = InferenceProvider::from_account_config(
            &cfg,
            api_key_auth(),
            &choreo_ai_protocols::SocketRegistry::new(),
        )
        .expect("google account constructs");
        let image_client = provider
            .image_client()
            .expect("Google protocol gets an image client");
        let debug = format!("{image_client:?}");
        assert!(debug.starts_with("GoogleImageClient"), "{debug}");
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
            api_key_auth(),
            &choreo_ai_protocols::SocketRegistry::new(),
        )
        .expect("zai account constructs");
        let image_client = provider
            .image_client()
            .expect("OpenAI protocol gets an image client");
        let debug = format!("{image_client:?}");
        assert!(debug.starts_with("ZaiImageClient"), "{debug}");
        // Mainland zhipuai resolves to the same Zhipu image adapter.
        let cfg = AccountConfig::simple("zhipu", "zhipuai");
        let provider = InferenceProvider::from_account_config(
            &cfg,
            api_key_auth(),
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
            api_key_auth(),
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
    fn from_account_config_fal_routes_to_dedicated_image_client() {
        // fal is a wholesale overlay provider with no chat API but a dedicated
        // synchronous `{base}/{model}` Images contract, so its image backend
        // must be the FalImageClient — not the generic OpenAI adapter.
        let cfg = AccountConfig::simple("fal", "fal");
        let provider = InferenceProvider::from_account_config(
            &cfg,
            api_key_auth(),
            &choreo_ai_protocols::SocketRegistry::new(),
        )
        .expect("fal account constructs");
        let debug = format!(
            "{:?}",
            provider.image_client().expect("image client present")
        );
        assert!(debug.starts_with("FalImageClient"), "{debug}");
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
        assert_ne!(models, [] as [std::string::String; 0]);
        assert!(models.contains(&"claude-sonnet-4-20250514".to_string()));
    }

    #[test]
    fn resolve_context_window_uses_client_then_catalog() {
        let mut cfg = ServiceConfig::default();
        cfg.context_window_config.per_model = [("gpt-4.1-nano".into(), 1_048_576)].into();
        cfg.context_window_config.context_window = Some(128_000);
        let client = OpenAiClient::new(
            cfg,
            RequestAuth::api_key("test-key"),
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

#[cfg(test)]
pub(crate) mod test_util;
