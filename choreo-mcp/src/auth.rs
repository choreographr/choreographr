//! OAuth credential plumbing for remote MCP servers.
//!
//! The interactive sign-in (discovery, dynamic client registration, PKCE) is
//! owned by the frontend login library, which stores a
//! [`ServiceCredential::OAuth`] under a target-scoped key. This module is the
//! daemon half: a [`CredentialBackend`] trait the daemon implements over its
//! keystore, and a [`CredentialStoreAdapter`] that maps the durable credential
//! to and from `rmcp`'s [`StoredCredentials`] so `rmcp`'s
//! `AuthorizationManager` can refresh the access token on connect.
//!
//! Two invariants shape the adapter:
//!
//! - **The access token never reaches the keystore.** `save` strips it and
//!   serves it from an in-process cache; `load` synthesises a token response
//!   carrying the durable refresh token when the cache is cold, so `rmcp`
//!   refreshes on connect rather than trusting a persisted bearer.
//! - **No `rmcp` type crosses the crate boundary.** The [`CredentialBackend`]
//!   trait is expressed entirely in this crate's own types plus the shared
//!   [`ServiceCredential`]; the adapter (which does name `rmcp` types) lives
//!   here and never leaks out.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use choreo_shared::{OAuthKind, ServiceCredential};
use oauth2::basic::BasicTokenType;
use oauth2::{AccessToken, RefreshToken, StandardTokenResponse, TokenResponse};
use rmcp::transport::auth::{
    AuthError, CredentialRefreshGuard, CredentialStore, OAuthTokenResponse, StoredCredentials,
    VendorExtraTokenFields,
};
use tokio::sync::Mutex;

use crate::config::{McpServerConfig, McpTransport};
use crate::naming::short_hash;

/// An owned guard held across a credential refresh; dropping it releases the
/// single-flight lock.
///
/// A backend produces one from its per-credential refresh lock. It must be
/// `Send + 'static` (the guard is held across an `await` on the sidecar
/// runtime), so a backend wraps an owned lock guard rather than a borrow.
pub struct RefreshGuard {
    _inner: Box<dyn Send>,
}

impl RefreshGuard {
    /// Wrap an owned guard whose `Drop` releases the refresh lock.
    #[must_use]
    pub fn new(inner: impl Send + 'static) -> Self {
        Self {
            _inner: Box::new(inner),
        }
    }
}

/// A daemon-provided backing store for one MCP server's OAuth credential.
///
/// The daemon implements this over its keystore; the [`CredentialStoreAdapter`]
/// drives it. Every method is blocking — the adapter calls each on a
/// `spawn_blocking` task, so the daemon's keystore round-trip (which crosses to
/// the command loop) never blocks the sidecar runtime.
pub trait CredentialBackend: Send + Sync + 'static {
    /// Load the durable OAuth credential for this server, if one is stored.
    ///
    /// # Errors
    ///
    /// Returns a message when the backend cannot read its store.
    fn load(&self) -> Result<Option<ServiceCredential>, String>;
    /// Persist the durable OAuth credential (the access token stripped).
    ///
    /// # Errors
    ///
    /// Returns a message when the backend cannot write its store.
    fn save(&self, credential: ServiceCredential) -> Result<(), String>;
    /// Delete the stored credential (logout).
    ///
    /// # Errors
    ///
    /// Returns a message when the backend cannot delete from its store.
    fn clear(&self) -> Result<(), String>;
    /// Acquire the per-credential single-flight refresh guard, if the backend
    /// coordinates refreshes.
    ///
    /// # Errors
    ///
    /// Returns a message when the backend cannot acquire the guard.
    fn acquire_refresh_guard(&self) -> Result<Option<RefreshGuard>, String>;
    /// Record that this server's refresh token was terminally rejected, so the
    /// daemon surfaces a re-authentication request instead of retrying.
    ///
    /// # Errors
    ///
    /// Returns a message when the backend cannot record the rejection.
    fn mark_reauthentication(&self) -> Result<(), String>;
}

/// The OAuth wiring for one remote server: its target-scoped credential key
/// and the daemon-provided backend.
///
/// Bundled so the connect path takes one argument rather than two, and kept out
/// of [`McpServerConfig`] (which derives `PartialEq` for the reload
/// comparison) because the backend is a trait object.
#[derive(Clone)]
pub struct McpAuthWiring {
    /// The credential key (`mcp:<slug>|<target-hash>`), computed by the daemon.
    pub key: String,
    /// The backend that stores the durable credential.
    pub backend: Arc<dyn CredentialBackend>,
}

impl std::fmt::Debug for McpAuthWiring {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The key is not secret, but keep the shape redacted-safe and omit the
        // backend (a trait object) — never print a credential-bearing value.
        f.debug_struct("McpAuthWiring")
            .field("key", &self.key)
            .finish_non_exhaustive()
    }
}

/// Canonicalize a remote server URL for credential keying.
///
/// Lowercases the scheme and host, drops a default port, strips a trailing
/// slash, and drops any fragment and userinfo. Canonicalization matters more
/// than the hash function: a hash cannot fix a non-canonical input, so two
/// spellings of one endpoint must reduce to the same string here. An
/// unparseable URL is returned trimmed and unchanged (a best effort).
#[must_use]
pub fn canonical_target(url: &str) -> String {
    let trimmed = url.trim();
    let Ok(mut parsed) = reqwest::Url::parse(trimmed) else {
        return trimmed.to_string();
    };
    // Drop the fragment and any userinfo; neither identifies the server.
    parsed.set_fragment(None);
    if !parsed.username().is_empty() {
        let _ = parsed.set_username("");
    }
    let _ = parsed.set_password(None);
    // Normalize the path's trailing slash: the url crate keeps `/` for an
    // empty path and preserves any other trailing slash, but `…/mcp` and
    // `…/mcp/` name one endpoint.
    let normalized = parsed.path().trim_end_matches('/').to_owned();
    if normalized.is_empty() {
        parsed.set_path("/");
    } else {
        parsed.set_path(&normalized);
    }
    // A known default port is already dropped by `Url::parse`; a non-default
    // port is meaningful and kept.
    let mut out = parsed.to_string();
    if out.ends_with('/') {
        out.pop();
    }
    out
}

/// Whether a stored MCP credential may be used for a server with the given
/// canonical resource and optional expected issuer (fail-closed, SEP-2352).
///
/// On load the stored `resource` (the server URL) is compared with the
/// currently-configured server URL and the stored `issuer` with the expected
/// issuer (when known): a `git pull` that re-points a repo-controlled server
/// URL must NOT receive the stored token, and an issuer that changed must not
/// either. Any mismatch — or a missing/!MCP credential — returns `false`, so
/// the caller treats it as no credential (quarantine → re-login).
#[must_use]
pub fn credential_matches_target(
    credential: &ServiceCredential,
    canonical_resource: &str,
    expected_issuer: Option<&str>,
) -> bool {
    let Some(view) = credential.as_oauth() else {
        return false;
    };
    if view.kind != OAuthKind::Mcp {
        return false;
    }
    let resource_matches = view
        .resource
        .is_some_and(|r| canonical_target(r) == canonical_resource);
    let issuer_matches = expected_issuer.is_none_or(|iss| view.issuer == Some(iss));
    resource_matches && issuer_matches
}

/// Build the target-scoped credential key for a canonical target identity and
/// slug: `mcp:<slug>|<short-hash>`.
///
/// The hash is the repo's stable FNV-1a ([`short_hash`]) of the canonical
/// identity joined to the slug with a `\0`, so two servers of one slug pointing
/// at different endpoints key distinctly while the same endpoint keys the same.
#[must_use]
pub fn credential_key(slug: &str, identity: &str) -> String {
    format!("mcp:{slug}|{}", short_hash(&format!("{identity}\0{slug}")))
}

/// The target-scoped credential key for a resolved server config, or `None`
/// for a stdio server (which never carries an OAuth credential).
#[must_use]
pub fn credential_key_for(config: &McpServerConfig) -> Option<String> {
    match &config.transport {
        McpTransport::Http { url, .. } => {
            let identity = format!("http\0{}", canonical_target(url));
            Some(credential_key(&config.slug, &identity))
        }
        McpTransport::Stdio { .. } => None,
    }
}

/// The durable (non-token) OAuth fields, remembered across a `load`/`save`
/// pair so a refresh can re-persist them (the token response `rmcp` hands back
/// carries no client secret, issuer, or resource).
#[derive(Clone)]
struct DurableSeed {
    issuer: Option<String>,
    resource: Option<String>,
    authorization_server: Option<String>,
    account_id: Option<String>,
    refresh_token: Option<String>,
    client_secret: Option<String>,
    token_type: Option<String>,
}

/// An access token served from memory between the keystore round-trips.
#[derive(Clone)]
struct CachedToken {
    response: OAuthTokenResponse,
    received_at: u64,
}

/// The `rmcp` [`CredentialStore`] over a [`CredentialBackend`].
///
/// One adapter per connection: it caches the access token in memory and
/// remembers the durable non-token fields so it can persist a rotated refresh
/// token without the secret ever leaving the process.
pub struct CredentialStoreAdapter {
    /// The target-scoped credential key this adapter serves.
    key: String,
    /// The daemon backend that owns the durable credential.
    backend: Arc<dyn CredentialBackend>,
    /// The authorization-server issuer re-discovered at connect, when known.
    /// A stored credential naming a different issuer is withheld on `load`.
    expected_issuer: Option<String>,
    /// The in-process access-token cache (never persisted).
    cache: Mutex<Option<CachedToken>>,
    /// The durable non-token fields from the last `load`, used by `save`.
    seed: Mutex<Option<DurableSeed>>,
}

impl CredentialStoreAdapter {
    /// Build an adapter for `key` over `backend`.
    #[must_use]
    pub fn new(key: String, backend: Arc<dyn CredentialBackend>) -> Self {
        Self {
            key,
            backend,
            expected_issuer: None,
            cache: Mutex::new(None),
            seed: Mutex::new(None),
        }
    }

    /// Fail the credential closed against a re-discovered authorization-server
    /// issuer.
    ///
    /// The issuer is only known after connect-time discovery, so the adapter is
    /// told it once discovery has run. With it set, a stored credential whose
    /// issuer differs (or is absent) is withheld on `load`, so a server whose
    /// `oauth` config — or the resource it points at — was re-pointed never
    /// receives a token minted by the previous authorization server. The
    /// resulting unauthenticated connect surfaces a re-login, never a silent
    /// reuse.
    #[must_use]
    pub fn with_expected_issuer(mut self, issuer: Option<String>) -> Self {
        self.expected_issuer = issuer;
        self
    }

    /// The credential key this adapter serves (for logging).
    #[must_use]
    pub fn key(&self) -> &str {
        &self.key
    }
}

/// A synthetic token response carrying only a refresh token and an empty
/// access token.
///
/// On a cold start the cache holds no access token, but the durable grant does
/// carry a refresh token. Returning a response with an empty bearer (and no
/// `expires_in`, so `rmcp` skips its expiry check) makes the first request go
/// out unauthenticated, which 401s and drives `rmcp`'s one-shot refresh — so
/// the durable refresh token is spent at connect time, not trusted as a
/// persisted bearer.
fn synthetic_refresh_only(refresh_token: &str) -> OAuthTokenResponse {
    let mut response: OAuthTokenResponse = StandardTokenResponse::new(
        AccessToken::new(String::new()),
        BasicTokenType::Bearer,
        VendorExtraTokenFields::default(),
    );
    response.set_refresh_token(Some(RefreshToken::new(refresh_token.to_string())));
    response
}

/// The current Unix time in seconds.
fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Build `rmcp`'s OAuth HTTP client for one remote server.
///
/// Discovers the authorization-server metadata, wires the
/// [`CredentialStoreAdapter`] to `wiring`, and — when a durable credential is
/// already stored — configures the OAuth client (client id, optional secret,
/// granted scopes) so `rmcp` can refresh the access token on connect. A cold
/// start (no stored credential) leaves the client unconfigured; the connection
/// then fails with an actionable authorization error so the user can sign in.
///
/// # Errors
///
/// Returns [`McpError::InitializeFailed`](crate::error::McpError::InitializeFailed)
/// when discovery, credential loading,
/// or client configuration fails.
pub(crate) async fn build_auth_client(
    config: &McpServerConfig,
    url: &str,
    wiring: &McpAuthWiring,
    client: reqwest::Client,
) -> Result<rmcp::transport::auth::AuthClient<reqwest::Client>, crate::error::McpError> {
    use rmcp::transport::auth::{AuthClient, AuthorizationManager, OAuthClientConfig};

    let mut manager = AuthorizationManager::new(url).await.map_err(|e| {
        crate::error::McpError::InitializeFailed(format!("OAuth discovery init failed: {e}"))
    })?;

    let backend = wiring.backend.clone();
    let loaded = tokio::task::spawn_blocking(move || backend.load())
        .await
        .map_err(|e| {
            crate::error::McpError::InitializeFailed(format!("credential load task failed: {e}"))
        })?
        .map_err(crate::error::McpError::InitializeFailed)?;

    if let Some(view) = loaded.as_ref().and_then(ServiceCredential::as_oauth)
        && view.kind == OAuthKind::Mcp
    {
        let client_id = view.client_id.map(str::to_string);
        let client_secret = view.client_secret.map(str::to_string);
        let granted_scopes = view.granted_scopes.to_vec();
        let metadata = manager
            .resolve_metadata()
            .await
            .map_err(|e| {
                crate::error::McpError::InitializeFailed(format!(
                    "OAuth metadata discovery failed: {e}"
                ))
            })?
            .metadata;
        // Arm the adapter to withhold the stored grant when its issuer no longer
        // matches the one discovered here (a re-pointed server), so the token is
        // never sent; the connect then surfaces a re-login.
        manager.set_credential_store(
            CredentialStoreAdapter::new(wiring.key.clone(), wiring.backend.clone())
                .with_expected_issuer(metadata.issuer.clone()),
        );
        manager.set_metadata(metadata);
        // The redirect URI only needs to be a valid loopback URL for a token
        // refresh; a full interactive login supplies its own callback.
        let port = match &config.transport {
            McpTransport::Http { oauth, .. } => oauth.as_ref().and_then(|o| o.callback_port),
            McpTransport::Stdio { .. } => None,
        }
        .unwrap_or(1455);
        let redirect = format!("http://127.0.0.1:{port}/callback");
        let mut oauth_config = OAuthClientConfig::new(client_id.unwrap_or_default(), redirect)
            .with_scopes(granted_scopes);
        if let Some(secret) = client_secret {
            oauth_config = oauth_config.with_client_secret(secret);
        }
        manager.configure_client(oauth_config).map_err(|e| {
            crate::error::McpError::InitializeFailed(format!(
                "OAuth client configuration failed: {e}"
            ))
        })?;
    } else {
        // No usable stored grant: install the adapter anyway so a later
        // connect-time `save` (a fresh interactive grant) is persisted.
        manager.set_credential_store(CredentialStoreAdapter::new(
            wiring.key.clone(),
            wiring.backend.clone(),
        ));
    }

    Ok(AuthClient::new(client, manager))
}

#[async_trait]
impl CredentialStore for CredentialStoreAdapter {
    async fn load(&self) -> Result<Option<StoredCredentials>, AuthError> {
        let backend = self.backend.clone();
        let loaded = tokio::task::spawn_blocking(move || backend.load())
            .await
            .map_err(|e| AuthError::CredentialStoreError(format!("load task failed: {e}")))?
            .map_err(AuthError::CredentialStoreError)?;
        let Some(loaded) = loaded else {
            return Ok(None);
        };
        // Borrow the durable fields (the credential zeroizes on drop, so it
        // cannot be destructured by value); only ever serve an MCP grant.
        let Some(view) = loaded.as_oauth() else {
            return Ok(None);
        };
        if view.kind != OAuthKind::Mcp {
            return Ok(None);
        }
        // Fail closed on an authorization-server change: when the issuer was
        // re-discovered at connect and the stored credential names a different
        // one (or none), withhold the grant. The caller then connects
        // unauthenticated, which 401s and surfaces a re-login, rather than
        // sending a token minted by the previous authorization server.
        if let Some(expected) = self.expected_issuer.as_deref()
            && view.issuer != Some(expected)
        {
            tracing::warn!(
                key = %self.key,
                "stored MCP credential issuer does not match discovery; withholding it (re-login required)"
            );
            return Ok(None);
        }
        let issuer = view.issuer.map(str::to_string);
        let client_id = view.client_id.map(str::to_string);
        let refresh_token = view.refresh_token.map(str::to_string);
        let granted_scopes = view.granted_scopes.to_vec();
        // Remember the durable non-token fields so a later `save` can re-persist
        // them (the token response carries none of these).
        *self.seed.lock().await = Some(DurableSeed {
            issuer: issuer.clone(),
            resource: view.resource.map(str::to_string),
            authorization_server: view.authorization_server.map(str::to_string),
            account_id: view.account_id.map(str::to_string),
            refresh_token: view.refresh_token.map(str::to_string),
            client_secret: view.client_secret.map(str::to_string),
            token_type: view.token_type.map(str::to_string),
        });
        let cached = self.cache.lock().await.clone();
        let (token_response, token_received_at) = match cached {
            Some(c) => (Some(c.response), Some(c.received_at)),
            // Cold start: serve a refresh-only response so `rmcp` refreshes.
            None => match refresh_token.as_deref() {
                Some(rt) => (Some(synthetic_refresh_only(rt)), None),
                None => (None, None),
            },
        };
        let Some(token_response) = token_response else {
            // No access token and no refresh token: nothing usable. `rmcp`
            // treats this as `AuthorizationRequired`, which surfaces a re-login.
            return Ok(None);
        };
        Ok(Some(
            StoredCredentials::new(
                client_id.unwrap_or_default(),
                Some(token_response),
                granted_scopes,
                token_received_at,
            )
            .with_issuer(issuer),
        ))
    }

    async fn save(&self, credentials: StoredCredentials) -> Result<(), AuthError> {
        let received_at = credentials.token_received_at.unwrap_or_else(now_secs);
        if let Some(response) = credentials.token_response.clone() {
            *self.cache.lock().await = Some(CachedToken {
                response,
                received_at,
            });
        }
        let seed = self.seed.lock().await.clone();
        let Some(seed) = seed else {
            // A save before any load: nothing durable to anchor it to.
            return Ok(());
        };
        // Strip the access token: the durable grant is the refresh token plus
        // the client registration and identity. A rotated refresh token is
        // adopted; when the exchange omitted one, the seed's is kept.
        let (refresh_token, granted_scopes) = match &credentials.token_response {
            Some(response) => (
                // Adopt a rotated refresh token; when the exchange returned
                // none (rmcp rewrites the previous one onto the response on
                // refresh), fall back to the seed's rather than dropping the
                // durable grant root.
                response
                    .refresh_token()
                    .map(|t| t.secret().clone())
                    .or(seed.refresh_token.clone()),
                response.scopes().map_or_else(
                    || credentials.granted_scopes.clone(),
                    |scopes| scopes.iter().map(|s| s.as_ref().to_owned()).collect(),
                ),
            ),
            None => (
                seed.refresh_token.clone(),
                credentials.granted_scopes.clone(),
            ),
        };
        let durable = ServiceCredential::OAuth {
            kind: OAuthKind::Mcp,
            issuer: credentials.issuer.clone().or(seed.issuer),
            resource: seed.resource,
            authorization_server: seed.authorization_server,
            refresh_token,
            account_id: seed.account_id,
            client_id: Some(credentials.client_id.clone()),
            client_secret: seed.client_secret,
            granted_scopes,
            token_type: seed.token_type,
        };
        let backend = self.backend.clone();
        tokio::task::spawn_blocking(move || backend.save(durable))
            .await
            .map_err(|e| AuthError::CredentialStoreError(format!("save task failed: {e}")))?
            .map_err(AuthError::CredentialStoreError)
    }

    async fn clear(&self) -> Result<(), AuthError> {
        *self.cache.lock().await = None;
        *self.seed.lock().await = None;
        let backend = self.backend.clone();
        tokio::task::spawn_blocking(move || backend.clear())
            .await
            .map_err(|e| AuthError::CredentialStoreError(format!("clear task failed: {e}")))?
            .map_err(AuthError::CredentialStoreError)
    }

    async fn on_refresh_token_rejected(
        &self,
        _credentials: &StoredCredentials,
    ) -> Result<(), AuthError> {
        // A definitive rejection means the grant is dead: mark it so the daemon
        // surfaces a re-authentication request rather than a silent retry loop.
        let backend = self.backend.clone();
        if let Err(e) = tokio::task::spawn_blocking(move || backend.mark_reauthentication()).await {
            tracing::warn!(error = %e, "failed to record MCP re-authentication");
        }
        Ok(())
    }

    async fn acquire_refresh_guard(&self) -> Result<Option<CredentialRefreshGuard>, AuthError> {
        let backend = self.backend.clone();
        let guard = tokio::task::spawn_blocking(move || backend.acquire_refresh_guard())
            .await
            .map_err(|e| AuthError::CredentialStoreError(format!("guard task failed: {e}")))?
            .map_err(AuthError::CredentialStoreError)?;
        Ok(guard.map(CredentialRefreshGuard::new))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex as StdMutex;

    #[test]
    fn credential_matches_target_is_fail_closed() {
        let cred = durable_oauth();
        let resource = canonical_target("https://example.com/mcp");
        // A matching resource (in any spelling) and no expected issuer passes.
        assert!(credential_matches_target(&cred, &resource, None));
        assert!(credential_matches_target(
            &cred,
            &canonical_target("HTTPS://Example.com/mcp/"),
            None
        ));
        // A re-pointed resource is rejected.
        assert!(!credential_matches_target(
            &cred,
            &canonical_target("https://other.example/mcp"),
            None
        ));
        // A matching expected issuer passes; a mismatched one is rejected.
        assert!(credential_matches_target(
            &cred,
            &resource,
            Some("https://as.example")
        ));
        assert!(!credential_matches_target(
            &cred,
            &resource,
            Some("https://evil.example")
        ));
        // A non-MCP credential never matches.
        assert!(!credential_matches_target(
            &ServiceCredential::ApiKey { key: "k".into() },
            &resource,
            None
        ));
    }

    #[test]
    fn canonical_target_normalizes_scheme_host_port_and_slash() {
        assert_eq!(
            canonical_target("HTTPS://Example.COM/mcp/"),
            "https://example.com/mcp"
        );
        assert_eq!(
            canonical_target("https://example.com:443/mcp"),
            "https://example.com/mcp"
        );
        assert_eq!(
            canonical_target("https://example.com/"),
            "https://example.com"
        );
        assert_eq!(
            canonical_target("https://example.com"),
            "https://example.com"
        );
        // Fragment and userinfo are dropped.
        assert_eq!(
            canonical_target("https://u:p@example.com/mcp#frag"),
            "https://example.com/mcp"
        );
        // A non-default port is meaningful and kept.
        assert_eq!(
            canonical_target("http://127.0.0.1:8765/mcp"),
            "http://127.0.0.1:8765/mcp"
        );
    }

    #[test]
    fn canonical_target_is_spelling_insensitive() {
        assert_eq!(
            canonical_target("https://MCP.Example.com:443/tools/"),
            canonical_target("https://mcp.example.com/tools")
        );
    }

    #[test]
    fn credential_key_is_stable_and_target_scoped() {
        let a = credential_key("docs", "http\0https://example.com/mcp");
        let b = credential_key("docs", "http\0https://example.com/mcp");
        assert_eq!(a, b, "same identity keys the same");
        let other = credential_key("docs", "http\0https://other.example/mcp");
        assert_ne!(a, other, "different targets key distinctly");
        assert!(a.starts_with("mcp:docs|"));
    }

    #[test]
    fn credential_key_for_http_only() {
        let http = McpServerConfig {
            slug: "docs".into(),
            transport: McpTransport::Http {
                url: "https://example.com/mcp".into(),
                headers: HashMap::new(),
                oauth: None,
            },
            enabled: true,
            timeout: None,
            protocol: crate::McpProtocolMode::Auto,
            max_concurrent_calls: None,
            max_restarts: None,
            disabled_tools: Vec::new(),
        };
        let key = credential_key_for(&http).expect("http has a key");
        assert!(key.starts_with("mcp:docs|"));

        let stdio = McpServerConfig {
            slug: "docs".into(),
            transport: McpTransport::Stdio {
                command: "npx".into(),
                args: Vec::new(),
                env: HashMap::new(),
                cwd: None,
                log_path: None,
            },
            enabled: true,
            timeout: None,
            protocol: crate::McpProtocolMode::Auto,
            max_concurrent_calls: None,
            max_restarts: None,
            disabled_tools: Vec::new(),
        };
        assert!(credential_key_for(&stdio).is_none());
    }

    /// A scripted backend holding one durable credential in memory.
    struct FakeBackend {
        stored: StdMutex<Option<ServiceCredential>>,
        rejected: std::sync::atomic::AtomicBool,
    }

    impl FakeBackend {
        fn new(credential: Option<ServiceCredential>) -> Arc<Self> {
            Arc::new(Self {
                stored: StdMutex::new(credential),
                rejected: std::sync::atomic::AtomicBool::new(false),
            })
        }
    }

    impl CredentialBackend for FakeBackend {
        fn load(&self) -> Result<Option<ServiceCredential>, String> {
            Ok(self.stored.lock().unwrap().clone())
        }
        fn save(&self, credential: ServiceCredential) -> Result<(), String> {
            *self.stored.lock().unwrap() = Some(credential);
            Ok(())
        }
        fn clear(&self) -> Result<(), String> {
            *self.stored.lock().unwrap() = None;
            Ok(())
        }
        fn acquire_refresh_guard(&self) -> Result<Option<RefreshGuard>, String> {
            Ok(None)
        }
        fn mark_reauthentication(&self) -> Result<(), String> {
            self.rejected
                .store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
    }

    fn durable_oauth() -> ServiceCredential {
        ServiceCredential::OAuth {
            kind: OAuthKind::Mcp,
            issuer: Some("https://as.example".into()),
            resource: Some("https://example.com/mcp".into()),
            authorization_server: Some("https://as.example".into()),
            refresh_token: Some("refresh-secret".into()),
            account_id: None,
            client_id: Some("client-1".into()),
            client_secret: Some("client-secret".into()),
            granted_scopes: vec!["openid".into()],
            token_type: Some("Bearer".into()),
        }
    }

    #[test]
    fn issuer_mismatch_withholds_the_stored_credential() {
        crate::runtime::init().expect("runtime init");
        let backend = FakeBackend::new(Some(durable_oauth()));
        // An expected issuer that differs from the stored one withholds the
        // grant, so a re-pointed server never receives the stored token.
        let mismatched = CredentialStoreAdapter::new("mcp:docs|x".into(), backend.clone())
            .with_expected_issuer(Some("https://evil.example".into()));
        assert!(
            crate::runtime::block_on(mismatched.load())
                .expect("block_on")
                .expect("load ok")
                .is_none(),
            "a mismatched issuer must withhold the credential"
        );
        // The matching issuer serves it.
        let matched = CredentialStoreAdapter::new("mcp:docs|x".into(), backend.clone())
            .with_expected_issuer(Some("https://as.example".into()));
        assert!(
            crate::runtime::block_on(matched.load())
                .expect("block_on")
                .expect("load ok")
                .is_some(),
            "a matching issuer must serve the credential"
        );
        // With no issuer expected (discovery found none), the credential is
        // served unchanged — there is nothing to compare against.
        let unchecked = CredentialStoreAdapter::new("mcp:docs|x".into(), backend);
        assert!(
            crate::runtime::block_on(unchecked.load())
                .expect("block_on")
                .expect("load ok")
                .is_some()
        );
    }

    #[test]
    fn cold_load_serves_a_refresh_only_response() {
        crate::runtime::init().expect("runtime init");
        let backend = FakeBackend::new(Some(durable_oauth()));
        let adapter = CredentialStoreAdapter::new("mcp:docs|x".into(), backend);
        let stored = crate::runtime::block_on(adapter.load())
            .expect("block_on")
            .expect("load ok")
            .expect("some credentials");
        let response = stored.token_response.expect("a token response");
        // No access token, but a refresh token is present, and no received_at
        // so `rmcp` skips the expiry check and drives a connect-time refresh.
        assert_eq!(response.access_token().secret(), "");
        assert_eq!(
            response.refresh_token().map(|t| t.secret().as_str()),
            Some("refresh-secret")
        );
        assert_eq!(stored.token_received_at, None);
        assert_eq!(stored.client_id, "client-1");
    }

    #[test]
    fn save_strips_the_access_token() {
        crate::runtime::init().expect("runtime init");
        let backend = FakeBackend::new(Some(durable_oauth()));
        let adapter = CredentialStoreAdapter::new("mcp:docs|x".into(), backend.clone());
        crate::runtime::block_on(async {
            // Prime the seed, then save a response carrying a fresh access
            // token and a rotated refresh token.
            let _ = adapter.load().await.expect("load ok");
            let mut response: OAuthTokenResponse = StandardTokenResponse::new(
                AccessToken::new("access-secret".into()),
                BasicTokenType::Bearer,
                VendorExtraTokenFields::default(),
            );
            response.set_refresh_token(Some(RefreshToken::new("rotated-refresh".into())));
            let stored = StoredCredentials::new(
                "client-1".into(),
                Some(response),
                vec!["openid".into()],
                Some(1_000),
            )
            .with_issuer(Some("https://as.example".into()));
            adapter.save(stored).await.expect("save ok");
        })
        .expect("block_on");

        let stored = backend
            .stored
            .lock()
            .unwrap()
            .clone()
            .expect("a durable credential");
        let view = stored.as_oauth().expect("oauth");
        assert_eq!(view.refresh_token, Some("rotated-refresh"));
        // The access token is never in the durable credential's fields.
        assert!(!format!("{stored:?}").contains("access-secret"));
        // The client secret survives the save (it is not in the token response).
        assert_eq!(view.client_secret, Some("client-secret"));
    }

    #[test]
    fn clear_removes_the_credential() {
        crate::runtime::init().expect("runtime init");
        let backend = FakeBackend::new(Some(durable_oauth()));
        let adapter = CredentialStoreAdapter::new("mcp:docs|x".into(), backend.clone());
        crate::runtime::block_on(adapter.clear())
            .expect("block_on")
            .expect("clear ok");
        assert!(backend.stored.lock().unwrap().is_none());
    }

    #[test]
    fn rejected_refresh_marks_reauthentication() {
        crate::runtime::init().expect("runtime init");
        let backend = FakeBackend::new(Some(durable_oauth()));
        let adapter = CredentialStoreAdapter::new("mcp:docs|x".into(), backend.clone());
        let stored = StoredCredentials::new("client-1".into(), None, vec![], None);
        crate::runtime::block_on(adapter.on_refresh_token_rejected(&stored))
            .expect("block_on")
            .expect("ok");
        assert!(
            backend.rejected.load(std::sync::atomic::Ordering::SeqCst),
            "a rejected refresh must be recorded"
        );
    }
}
