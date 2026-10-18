//! The `rmcp`-facing [`CredentialStore`] adapter: the durable↔`StoredCredentials`
//! mapping over a [`CredentialBackend`].
//!
//! With [`connect`](super::connect) (which owns the connect-time `rmcp`
//! `AuthClient` wiring) this is one of the two modules that name `rmcp`'s OAuth
//! types; it maps the daemon's durable [`ServiceCredential`] to and from
//! `rmcp`'s `StoredCredentials` and never leaks an `rmcp` type past its own
//! boundary. The access token is served from an in-process cache and normally
//! stripped on `save`, so it never reaches the keystore — except when the
//! authorization server issued no refresh token, where the access token is the
//! only usable credential and is persisted deliberately.

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
use zeroize::Zeroizing;

use super::CredentialBackend;

/// The durable (non-token) OAuth fields, remembered across a `load`/`save`
/// pair so a refresh can re-persist them (the token response `rmcp` hands back
/// carries no client secret, issuer, or resource).
///
/// The refresh token, the durable access token, and the client secret are
/// wipe-on-drop (`Zeroizing`), matching the credential they are copied from:
/// this seed is a second copy of those secrets that lives for the connection's
/// lifetime, so it must not linger in freed memory when the adapter drops.
#[derive(Clone)]
struct DurableSeed {
    issuer: Option<String>,
    resource: Option<String>,
    authorization_server: Option<String>,
    account_id: Option<String>,
    refresh_token: Option<Zeroizing<String>>,
    /// The durable access token, present only for a grant whose server issued
    /// no refresh token.
    access_token: Option<Zeroizing<String>>,
    client_secret: Option<Zeroizing<String>>,
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

/// A synthetic token response a cold `load` builds from whatever the durable
/// grant offers: a bearer token and/or a refresh token, never an `expires_in`.
///
/// Two shapes, both without `expires_in` so `rmcp` skips its expiry check. With
/// a refresh token the bearer is left empty: the first request goes out
/// unauthenticated, 401s, and drives `rmcp`'s one-shot refresh — so the durable
/// refresh token is spent at connect time, not trusted as a persisted bearer.
/// Without one, the durable access token is the only credential there is, so it
/// is served directly as the bearer.
fn synthetic_response(access_token: &str, refresh_token: Option<&str>) -> OAuthTokenResponse {
    let mut response: OAuthTokenResponse = StandardTokenResponse::new(
        AccessToken::new(access_token.to_string()),
        BasicTokenType::Bearer,
        VendorExtraTokenFields::default(),
    );
    if let Some(refresh_token) = refresh_token {
        response.set_refresh_token(Some(RefreshToken::new(refresh_token.to_string())));
    }
    response
}

/// The current Unix time in seconds.
fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
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
        // sending a token minted by the previous authorization server. The
        // comparison tolerates a single trailing-slash difference (the issuer
        // is otherwise exact), so a server that spells its issuer either way is
        // not mistaken for a re-pointed one and does not force a needless
        // re-login.
        if let Some(expected) = self.expected_issuer.as_deref()
            && !view
                .issuer
                .is_some_and(|iss| choreo_shared::oauth_endpoint::issuers_match(iss, expected))
        {
            tracing::warn!(
                key = %self.key,
                "stored MCP credential issuer does not match discovery; withholding it (re-login required)"
            );
            return Ok(None);
        }
        let issuer = view.issuer.map(str::to_string);
        let client_id = view.client_id.map(str::to_string);
        // The refresh token and durable access token are secrets handed to
        // `rmcp` below; keep our own handles to them wipe-on-drop (rmcp's
        // `oauth2` token type does not zeroize, so that copy is out of our
        // hands).
        let refresh_token = view.refresh_token.map(|t| Zeroizing::new(t.to_string()));
        let access_token = view.access_token.map(|t| Zeroizing::new(t.to_string()));
        let granted_scopes = view.granted_scopes.to_vec();
        // Remember the durable non-token fields so a later `save` can re-persist
        // them (the token response carries none of these). The refresh token,
        // durable access token, and client secret are wipe-on-drop; the two
        // tokens reuse the handles cloned above, so this second copy does not
        // outlive the adapter's life in freed memory.
        *self.seed.lock().await = Some(DurableSeed {
            issuer: issuer.clone(),
            resource: view.resource.map(str::to_string),
            authorization_server: view.authorization_server.map(str::to_string),
            account_id: view.account_id.map(str::to_string),
            refresh_token: refresh_token.clone(),
            access_token: access_token.clone(),
            client_secret: view.client_secret.map(|t| Zeroizing::new(t.to_string())),
            token_type: view.token_type.map(str::to_string),
        });
        let cached = self.cache.lock().await.clone();
        let (token_response, token_received_at) = match cached {
            Some(c) => (Some(c.response), Some(c.received_at)),
            // Cold start. With a refresh token, serve a refresh-only response so
            // `rmcp` refreshes. Without one, serve the durable access token
            // directly (no `received_at`, so `rmcp` skips the expiry check and
            // sends it as the bearer) — the server issued no refresh token, so
            // it is the only credential there is.
            None => match (refresh_token.as_deref(), access_token.as_deref()) {
                // A refresh token wins: serve it with an empty bearer so the
                // connect-time refresh drives the exchange.
                (Some(rt), _) => (Some(synthetic_response("", Some(rt))), None),
                // No refresh token: the durable access token is the bearer.
                (None, Some(at)) => (Some(synthetic_response(at, None)), None),
                (None, None) => (None, None),
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
        // The in-process cache update is unconditional: the cache is the
        // working copy the adapter serves between refreshes, and the durable
        // grant only mirrors the access token in the no-refresh-token case
        // handled below.
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
        // the client registration and identity. A returned refresh token is
        // adopted; when the exchange omitted one, the seed's is kept rather than
        // dropping the durable grant root.
        let seed_refresh = seed.refresh_token.clone();
        let (refresh_token, granted_scopes) = match &credentials.token_response {
            Some(response) => (
                response
                    .refresh_token()
                    .map(|t| t.secret().clone())
                    .or_else(|| seed_refresh.as_ref().map(|t| t.as_str().to_string())),
                response.scopes().map_or_else(
                    || credentials.granted_scopes.clone(),
                    |scopes| scopes.iter().map(|s| s.as_ref().to_owned()).collect(),
                ),
            ),
            None => (
                seed_refresh.as_ref().map(|t| t.as_str().to_string()),
                credentials.granted_scopes.clone(),
            ),
        };
        // The durable grant changes only when the exchange ROTATED the refresh
        // token (or, for a grant with no refresh token, when the access token
        // changed). `rmcp` rewrites the previous refresh token back onto the
        // response on refresh, so "the response carries a refresh token" does
        // NOT imply rotation — compare the adopted value against the seed's, not
        // its presence. A non-rotating refresh (the access token lives only in
        // the in-process cache) is persisted nowhere, so it costs no
        // re-encryption and no blob write, matching the provider path. One
        // consequence: the daemon's `OAuthSave` handler also prunes the
        // single-flight refresh-lock registry, so a skipped save leaves that
        // key's entry lingering until the next save or clear — harmless, it is
        // an idle lock the next refresh reuses.
        // A plain `==` (not constant-time) is deliberate: both operands are
        // in-process secrets already resident in this task's memory, so there
        // is no timing oracle for an attacker who could not already read them.
        let refresh_changed = refresh_token.as_deref() != seed_refresh.as_ref().map(|t| t.as_str());
        // The durable access token is persisted ONLY for a grant with no refresh
        // token (there is nothing to refresh, so it is the only credential). It
        // is the freshly returned access token when the response carried a
        // non-empty one, else the seed's, so a save that omits the token does
        // not drop it. An empty token counts as absent: a blank bearer is never
        // persisted or served, and only when neither source has a non-empty
        // token does the field fail closed to `None`.
        let seed_access = seed.access_token.clone();
        let durable_access_token: Option<String> = if refresh_token.is_none() {
            credentials
                .token_response
                .as_ref()
                .map(|r| r.access_token().secret().clone())
                .filter(|token| !token.is_empty())
                .or_else(|| {
                    seed_access
                        .as_ref()
                        .map(|t| t.as_str().to_string())
                        .filter(|token| !token.is_empty())
                })
        } else {
            None
        };
        let access_changed =
            durable_access_token.as_deref() != seed_access.as_ref().map(|t| t.as_str());
        if !refresh_changed && !access_changed {
            tracing::debug!(
                key = %self.key,
                "MCP token refresh carried no durable change; skipping the durable re-encrypt"
            );
            return Ok(());
        }
        let durable = ServiceCredential::OAuth {
            kind: OAuthKind::Mcp,
            issuer: credentials.issuer.clone().or(seed.issuer),
            resource: seed.resource,
            authorization_server: seed.authorization_server,
            // MCP grants carry no provider token endpoint or refresh encoding;
            // their refresh re-discovers through `rmcp`.
            token_endpoint: None,
            refresh_encoding: None,
            refresh_token,
            access_token: durable_access_token,
            account_id: seed.account_id,
            client_id: Some(credentials.client_id.clone()),
            client_secret: seed.client_secret.as_ref().map(|s| s.as_str().to_string()),
            granted_scopes,
            token_type: seed.token_type,
        };
        let backend = self.backend.clone();
        tokio::task::spawn_blocking(move || {
            backend.save(
                durable,
                seed_refresh.as_ref().map(|t| t.as_str().to_string()),
            )
        })
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
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    /// A scripted backend holding one durable credential in memory, counting
    /// how many times `save` was called so a test can prove a non-rotating
    /// refresh does not re-encrypt.
    struct FakeBackend {
        stored: StdMutex<Option<ServiceCredential>>,
        rejected: AtomicBool,
        saves: AtomicUsize,
    }

    impl FakeBackend {
        fn new(credential: Option<ServiceCredential>) -> Arc<Self> {
            Arc::new(Self {
                stored: StdMutex::new(credential),
                rejected: AtomicBool::new(false),
                saves: AtomicUsize::new(0),
            })
        }
    }

    impl CredentialBackend for FakeBackend {
        fn load(&self) -> Result<Option<ServiceCredential>, String> {
            Ok(self.stored.lock().unwrap().clone())
        }
        fn save(
            &self,
            credential: ServiceCredential,
            _expected_refresh_token: Option<String>,
        ) -> Result<(), String> {
            self.saves.fetch_add(1, Ordering::SeqCst);
            *self.stored.lock().unwrap() = Some(credential);
            Ok(())
        }
        fn clear(&self) -> Result<(), String> {
            *self.stored.lock().unwrap() = None;
            Ok(())
        }
        fn acquire_refresh_guard(&self) -> Result<Option<super::super::RefreshGuard>, String> {
            Ok(None)
        }
        fn mark_reauthentication(&self) -> Result<(), String> {
            self.rejected.store(true, Ordering::SeqCst);
            Ok(())
        }
    }

    fn durable_oauth() -> ServiceCredential {
        ServiceCredential::OAuth {
            kind: OAuthKind::Mcp,
            issuer: Some("https://as.example".into()),
            resource: Some("https://example.com/mcp".into()),
            authorization_server: Some("https://as.example".into()),
            token_endpoint: None,
            refresh_encoding: None,
            refresh_token: Some("refresh-secret".into()),
            access_token: None,
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
        // A trailing-slash-only difference is the SAME issuer, so it is served
        // rather than withheld (no needless re-login).
        let tolerant = CredentialStoreAdapter::new("mcp:docs|x".into(), backend.clone())
            .with_expected_issuer(Some("https://as.example/".into()));
        assert!(
            crate::runtime::block_on(tolerant.load())
                .expect("block_on")
                .expect("load ok")
                .is_some(),
            "a trailing-slash-only issuer difference must not withhold the credential"
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
    fn cold_load_serves_the_durable_access_token_when_no_refresh_token() {
        crate::runtime::init().expect("runtime init");
        let mut cred = durable_oauth();
        if let ServiceCredential::OAuth {
            refresh_token,
            access_token,
            ..
        } = &mut cred
        {
            *refresh_token = None;
            *access_token = Some("durable-access".into());
        }
        let backend = FakeBackend::new(Some(cred));
        let adapter = CredentialStoreAdapter::new("mcp:docs|x".into(), backend);
        let stored = crate::runtime::block_on(adapter.load())
            .expect("block_on")
            .expect("load ok")
            .expect("some credentials");
        let response = stored.token_response.expect("a token response");
        // The durable access token is served as the bearer, with no refresh
        // token and no received_at (so `rmcp` skips the expiry check).
        assert_eq!(response.access_token().secret(), "durable-access");
        assert!(response.refresh_token().is_none());
        assert_eq!(stored.token_received_at, None);
        assert_eq!(stored.client_id, "client-1");
    }

    #[test]
    fn cold_load_without_refresh_or_access_token_returns_none() {
        crate::runtime::init().expect("runtime init");
        let mut cred = durable_oauth();
        if let ServiceCredential::OAuth {
            refresh_token,
            access_token,
            ..
        } = &mut cred
        {
            *refresh_token = None;
            *access_token = None;
        }
        let backend = FakeBackend::new(Some(cred));
        let adapter = CredentialStoreAdapter::new("mcp:docs|x".into(), backend);
        let loaded = crate::runtime::block_on(adapter.load())
            .expect("block_on")
            .expect("load ok");
        assert!(
            loaded.is_none(),
            "neither an access token nor a refresh token yields no credentials"
        );
    }

    #[test]
    fn save_persists_the_access_token_when_no_refresh_token() {
        crate::runtime::init().expect("runtime init");
        let mut cred = durable_oauth();
        if let ServiceCredential::OAuth {
            refresh_token,
            access_token,
            ..
        } = &mut cred
        {
            *refresh_token = None;
            *access_token = Some("first-access".into());
        }
        let backend = FakeBackend::new(Some(cred));
        let adapter = CredentialStoreAdapter::new("mcp:docs|x".into(), backend.clone());
        crate::runtime::block_on(async {
            let _ = adapter.load().await.expect("load ok");
            // A rotated access token with no refresh token must be persisted so
            // it survives the process (there is nothing to refresh it with).
            let response: OAuthTokenResponse = StandardTokenResponse::new(
                AccessToken::new("rotated-access".into()),
                BasicTokenType::Bearer,
                VendorExtraTokenFields::default(),
            );
            let stored = StoredCredentials::new(
                "client-1".into(),
                Some(response),
                vec!["openid".into()],
                Some(3_000),
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
        assert_eq!(view.access_token, Some("rotated-access"));
        assert_eq!(view.refresh_token, None);
        // The other durable fields survive the save.
        assert_eq!(view.client_secret, Some("client-secret"));
        assert_eq!(backend.saves.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn save_never_persists_an_empty_access_token() {
        crate::runtime::init().expect("runtime init");
        let mut cred = durable_oauth();
        if let ServiceCredential::OAuth {
            refresh_token,
            access_token,
            ..
        } = &mut cred
        {
            *refresh_token = None;
            *access_token = Some("first-access".into());
        }
        let backend = FakeBackend::new(Some(cred));
        let adapter = CredentialStoreAdapter::new("mcp:docs|x".into(), backend.clone());
        crate::runtime::block_on(async {
            let _ = adapter.load().await.expect("load ok");
            // A response carrying an EMPTY bearer (and no refresh token) must
            // not blank the grant: an empty token counts as absent, so the
            // seed's usable token is kept rather than persisted or served as an
            // empty bearer.
            let response: OAuthTokenResponse = StandardTokenResponse::new(
                AccessToken::new(String::new()),
                BasicTokenType::Bearer,
                VendorExtraTokenFields::default(),
            );
            let stored = StoredCredentials::new(
                "client-1".into(),
                Some(response),
                vec!["openid".into()],
                Some(3_000),
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
        assert_eq!(view.access_token, Some("first-access"));
        assert_ne!(view.access_token, Some(""));
        // The empty bearer was the only candidate durable change and it is
        // ignored, so the seed still governs and nothing is re-encrypted.
        assert_eq!(backend.saves.load(Ordering::SeqCst), 0);
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
        // The access token is never in the durable credential's fields when a
        // refresh token exists.
        assert_eq!(view.access_token, None);
        assert!(!format!("{stored:?}").contains("access-secret"));
        // The client secret survives the save (it is not in the token response).
        assert_eq!(view.client_secret, Some("client-secret"));
        // A rotated refresh token IS a durable change, so the blob is written.
        assert_eq!(backend.saves.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn save_skips_the_reencrypt_when_not_rotated() {
        crate::runtime::init().expect("runtime init");
        let backend = FakeBackend::new(Some(durable_oauth()));
        let adapter = CredentialStoreAdapter::new("mcp:docs|x".into(), backend.clone());
        crate::runtime::block_on(async {
            let _ = adapter.load().await.expect("load ok");
            // A response carrying the SAME refresh token as the loaded
            // credential (rmcp rewrites it back onto the response on refresh),
            // so only the access token changed.
            let mut response: OAuthTokenResponse = StandardTokenResponse::new(
                AccessToken::new("new-access".into()),
                BasicTokenType::Bearer,
                VendorExtraTokenFields::default(),
            );
            response.set_refresh_token(Some(RefreshToken::new("refresh-secret".into())));
            let stored = StoredCredentials::new(
                "client-1".into(),
                Some(response),
                vec!["openid".into()],
                Some(2_000),
            )
            .with_issuer(Some("https://as.example".into()));
            adapter.save(stored).await.expect("save ok");
            // The cache still reflects the fresh access token.
            let cached = adapter.cache.lock().await.clone().expect("a cached token");
            assert_eq!(cached.response.access_token().secret(), "new-access");
        })
        .expect("block_on");
        assert_eq!(
            backend.saves.load(Ordering::SeqCst),
            0,
            "a non-rotating refresh must not re-encrypt the durable grant"
        );
        // The durable credential is unchanged.
        let stored = backend
            .stored
            .lock()
            .unwrap()
            .clone()
            .expect("a durable credential");
        let view = stored.as_oauth().expect("oauth");
        assert_eq!(view.refresh_token, Some("refresh-secret"));
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
            backend.rejected.load(Ordering::SeqCst),
            "a rejected refresh must be recorded"
        );
    }
}
