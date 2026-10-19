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
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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
    /// The client id the durable grant was minted for. The token response never
    /// carries it, so the warm-cache fast path (`load`'s cache-first branch)
    /// needs this copy to build its `StoredCredentials` without a backend read.
    client_id: String,
    /// The scopes the durable grant was issued for, kept for the same reason as
    /// the client id: `rmcp` replays them onto the refresh request.
    granted_scopes: Vec<String>,
}

/// The pure inputs to [`project_durable_grant`]: everything a `save` needs to
/// decide the durable credential, lifted out of the `rmcp` [`StoredCredentials`]
/// so the projection is unit-testable without a runtime or a backend.
struct SaveInput {
    /// The refresh token the token response carried, if any. `rmcp` rewrites the
    /// previous token back onto the response, so a value here does NOT imply
    /// rotation; it is compared against the seed's own token.
    response_refresh_token: Option<String>,
    /// The access token the token response carried, if any (an empty string
    /// counts as absent).
    response_access_token: Option<String>,
    /// The scopes the token response reported, if any.
    response_scopes: Option<Vec<String>>,
    /// The scopes `rmcp`'s [`StoredCredentials`] carried (the fallback when the
    /// response reports none).
    stored_granted_scopes: Vec<String>,
    /// The client id `rmcp`'s [`StoredCredentials`] carried.
    stored_client_id: String,
    /// The issuer `rmcp`'s [`StoredCredentials`] carried, if any.
    stored_issuer: Option<String>,
}

impl SaveInput {
    /// Project the `rmcp` [`StoredCredentials`] a `save` receives onto the pure
    /// input struct, so the durable-grant decision is taken off `rmcp`'s types.
    fn from_stored(credentials: &StoredCredentials) -> Self {
        Self {
            response_refresh_token: credentials
                .token_response
                .as_ref()
                .and_then(|r| r.refresh_token().map(|t| t.secret().clone())),
            response_access_token: credentials
                .token_response
                .as_ref()
                .map(|r| r.access_token().secret().clone()),
            response_scopes: credentials.token_response.as_ref().and_then(|r| {
                r.scopes()
                    .map(|scopes| scopes.iter().map(|s| s.as_ref().to_owned()).collect())
            }),
            stored_granted_scopes: credentials.granted_scopes.clone(),
            stored_client_id: credentials.client_id.clone(),
            stored_issuer: credentials.issuer.clone(),
        }
    }
}

/// Project a save onto the durable credential, or `None` when nothing durable
/// changed (a non-rotating refresh lives only in the in-process cache and costs
/// no re-encryption and no blob write, matching the provider path).
///
/// The durable grant changes only when the exchange ROTATED the refresh token
/// (or, for a grant with no refresh token, when the access token changed).
/// `rmcp` rewrites the previous refresh token back onto the response on refresh,
/// so "the response carries a refresh token" does NOT imply rotation — the
/// adopted value is compared against the seed's, not its presence.
///
/// The durable access token is persisted ONLY for a grant with no refresh token
/// (there is nothing to refresh, so it is the only credential): the freshly
/// returned token when the response carried a non-empty one, else the seed's, so
/// a save that omits the token does not drop it. An empty token counts as
/// absent — a blank bearer is never persisted or served, and only when neither
/// source has a non-empty token does the field fail closed to `None`.
fn project_durable_grant(seed: &DurableSeed, input: &SaveInput) -> Option<ServiceCredential> {
    let seed_refresh = seed.refresh_token.as_ref().map(|t| t.as_str());
    // Rotate-if-present: adopt the returned refresh token, else keep the seed's
    // rather than dropping the durable grant root.
    let refresh_token: Option<String> = input
        .response_refresh_token
        .clone()
        .or_else(|| seed_refresh.map(str::to_string));
    let granted_scopes = input
        .response_scopes
        .clone()
        .unwrap_or_else(|| input.stored_granted_scopes.clone());
    let seed_access = seed.access_token.as_ref().map(|t| t.as_str());
    let durable_access_token: Option<String> = if refresh_token.is_none() {
        input
            .response_access_token
            .clone()
            .filter(|token| !token.is_empty())
            .or_else(|| {
                seed_access
                    .map(str::to_string)
                    .filter(|token| !token.is_empty())
            })
    } else {
        None
    };
    // A plain `==`/`!=` (not constant-time) is deliberate: both operands are
    // in-process secrets already resident in this task's memory, so there is no
    // timing oracle for an attacker who could not already read them.
    let refresh_changed = refresh_token.as_deref() != seed_refresh;
    let access_changed = durable_access_token.as_deref() != seed_access;
    if !refresh_changed && !access_changed {
        return None;
    }
    Some(ServiceCredential::OAuth {
        kind: OAuthKind::Mcp,
        issuer: input.stored_issuer.clone().or_else(|| seed.issuer.clone()),
        resource: seed.resource.clone(),
        authorization_server: seed.authorization_server.clone(),
        // MCP grants carry no provider token endpoint or refresh encoding; their
        // refresh re-discovers through `rmcp`.
        token_endpoint: None,
        refresh_encoding: None,
        refresh_token,
        access_token: durable_access_token,
        account_id: seed.account_id.clone(),
        client_id: Some(input.stored_client_id.clone()),
        client_secret: seed.client_secret.as_ref().map(|s| s.as_str().to_string()),
        granted_scopes,
        token_type: seed.token_type.clone(),
    })
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

    /// Serve the warm in-process cache when `rmcp` would use the token as-is,
    /// rather than paying a blocking backend read, the process-global snapshot
    /// lock, and a whole-credential clone on every request.
    ///
    /// Returns `None` whenever the full load must run: no cached token, a token
    /// inside [`CACHE_SERVE_MARGIN_SECS`], no established seed, or a seed that no
    /// longer describes the cached grant.
    ///
    /// The seed check is what makes the shortcut safe for the save that follows a
    /// refresh. A refresh rebuilds the seed from the durable store; serving from
    /// cache skips that rebuild, so the seed must already match the cached token
    /// (its refresh token is the one `rmcp` will present, and the one a later
    /// `save` compares against and hands the backend as its
    /// `expected_refresh_token`). The two agree exactly when the last full load
    /// observed the grant the cache holds — a rotation this adapter then saved
    /// leaves them apart until the next full load, which is what re-pins the
    /// seed. Mismatched, the load falls through and re-reads the backend.
    async fn load_from_cache(&self) -> Option<StoredCredentials> {
        // The two mutexes are taken one at a time, in `save`'s order and never
        // nested: only the value each guard covers is cloned out of it.
        let cached = self.cache.lock().await.clone()?;
        if !cache_is_usable(&cached.response, cached.received_at, now_secs()) {
            return None;
        }
        let seed = self.seed.lock().await.clone()?;
        // A plain `==` (not constant-time) is deliberate: both operands are
        // in-process secrets already resident in this task's memory, so there is
        // no timing oracle for an attacker who could not already read them.
        let cached_refresh = cached.response.refresh_token().map(|t| t.secret().as_str());
        let seed_refresh = seed.refresh_token.as_ref().map(|t| t.as_str());
        if cached_refresh != seed_refresh {
            return None;
        }
        tracing::trace!(key = %self.key, "served warm cached MCP token");
        Some(
            StoredCredentials::new(
                seed.client_id.clone(),
                Some(cached.response),
                seed.granted_scopes.clone(),
                Some(cached.received_at),
            )
            .with_issuer(seed.issuer.clone()),
        )
    }
}

/// A synthetic, **already-expired** refresh-only token response a cold `load`
/// builds when the durable grant carries a refresh token.
///
/// The bearer is left empty and the response is given a zero `expires_in`, so
/// `rmcp`'s expiry check fires and it refreshes the access token *proactively*
/// (via the durable refresh token) before the first request. Relying instead on
/// the server answering the empty bearer with a `401` challenge to trigger the
/// refresh is fragile: some authorization servers — GitHub's remote MCP
/// endpoint among them — reject an empty `Authorization: Bearer` with `400`
/// ("Authorization header is badly formatted") rather than `401`, and `rmcp`
/// treats only `401` as an auth challenge, so the refresh would never run and
/// the connect would fail. An immediate expiry makes the refresh independent of
/// the server's challenge status, and the durable refresh token is still spent
/// at connect time rather than trusted as a persisted bearer.
fn synthetic_refresh_response(refresh_token: &str) -> OAuthTokenResponse {
    let mut response: OAuthTokenResponse = StandardTokenResponse::new(
        AccessToken::new(String::new()),
        BasicTokenType::Bearer,
        VendorExtraTokenFields::default(),
    );
    response.set_refresh_token(Some(RefreshToken::new(refresh_token.to_string())));
    // A zero lifetime marks the response expired, so `rmcp` refreshes at once
    // (see above) instead of sending the empty bearer and awaiting a `401`.
    response.set_expires_in(Some(&Duration::from_secs(0)));
    response
}

/// A synthetic token response a cold `load` builds when the durable grant
/// carries **no** refresh token: the durable access token is the only credential
/// there is, so it is served directly as the bearer, with no `expires_in` so
/// `rmcp` skips its expiry check and sends it as-is (there is nothing to refresh
/// it with).
fn synthetic_access_response(access_token: &str) -> OAuthTokenResponse {
    StandardTokenResponse::new(
        AccessToken::new(access_token.to_string()),
        BasicTokenType::Bearer,
        VendorExtraTokenFields::default(),
    )
}

/// How much remaining lifetime a cached access token must keep for [`load`] to
/// serve it without reading the backend.
///
/// The margin is deliberately larger than the 30-second buffer `rmcp` uses to
/// refresh a token before it expires. That buffer is the only expiry check that
/// starts a refresh from an otherwise-unexpired token, and every refresh re-reads
/// the durable store under the per-credential single-flight guard (that re-read
/// is what keeps a rotated single-use refresh token from being spent twice, see
/// [`CredentialStore::load`]). Serving from cache only beyond a wider margin
/// therefore keeps the shortcut to loads `rmcp` follows with a plain request, so
/// no refresh — and no durable re-read — is ever skipped.
///
/// [`load`]: CredentialStore::load
const CACHE_SERVE_MARGIN_SECS: u64 = 60;

/// Whether a cached token response is fresh enough to serve without reading the
/// backend (see [`CACHE_SERVE_MARGIN_SECS`]).
///
/// A response that reports no `expires_in` makes `rmcp` skip its expiry check and
/// use the token as-is, so there is no refresh to short-circuit and it is always
/// usable. Otherwise the remaining lifetime is `expires_in` minus the time since
/// the token was received; the subtraction saturates so a clock step backwards
/// can never extend a token's life.
fn cache_is_usable(response: &OAuthTokenResponse, received_at: u64, now: u64) -> bool {
    let Some(expires_in) = response.expires_in() else {
        return true;
    };
    let elapsed = now.saturating_sub(received_at);
    expires_in.as_secs().saturating_sub(elapsed) > CACHE_SERVE_MARGIN_SECS
}

/// The current Unix time in seconds.
fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[async_trait]
impl CredentialStore for CredentialStoreAdapter {
    /// Serve the credential, from the warm in-process cache when it can be.
    ///
    /// `rmcp` loads the credential on every request, so this is the hot path.
    /// A cached token that `rmcp` would use as-is (see `cache_is_usable` and
    /// `CACHE_SERVE_MARGIN_SECS`) is served directly from the cache and the
    /// durable non-token fields remembered by the last load, with no backend
    /// read.
    ///
    /// That shortcut cannot reopen the rotation race: it only applies to a load
    /// `rmcp` follows with a plain request, never one it follows with a refresh.
    /// A refresh always starts by re-reading the durable store under the
    /// per-credential single-flight guard, so a second connection that acquires
    /// the guard after a winner rotated re-reads the rotated credential instead of
    /// spending an already-spent single-use refresh token. A token that is
    /// expired, about to expire, or of unknown freshness falls through to the
    /// full load below, which performs exactly that re-read; so does a cache whose
    /// grant the seed no longer describes.
    async fn load(&self) -> Result<Option<StoredCredentials>, AuthError> {
        if let Some(stored) = self.load_from_cache().await {
            return Ok(Some(stored));
        }
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
            // The token response carries neither the client id nor the scopes, so
            // the warm-cache fast path reads them from here rather than the
            // backend.
            client_id: client_id.clone().unwrap_or_default(),
            granted_scopes: granted_scopes.clone(),
        });
        let cached = self.cache.lock().await.clone();
        let (token_response, token_received_at) = match cached {
            Some(c) => (Some(c.response), Some(c.received_at)),
            // Cold start. With a refresh token, serve an already-expired
            // refresh-only response so `rmcp` refreshes the access token at once
            // (the durable refresh token is spent at connect time, never trusted
            // as a persisted bearer). Without one, serve the durable access token
            // directly with no `received_at`, so `rmcp` skips the expiry check
            // and sends it as the bearer — the server issued no refresh token, so
            // it is the only credential there is.
            None => match (refresh_token.as_deref(), access_token.as_deref()) {
                // A refresh token is served as an immediately-expired grant so
                // `rmcp`'s proactive refresh runs before the first request; the
                // `received_at` anchor is what lets its expiry check fire.
                (Some(rt), _) => (Some(synthetic_refresh_response(rt)), Some(now_secs())),
                // No refresh token: the durable access token is the bearer.
                (None, Some(at)) => (Some(synthetic_access_response(at)), None),
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
        // The pure projection decides whether anything durable changed. A
        // non-rotating refresh (the access token lives only in the in-process
        // cache) yields `None`, so it costs no re-encryption and no blob write,
        // matching the provider path — and a skipped save leaves the daemon's
        // `OAuthSave`-time refresh-lock prune unrun, which only leaves an idle
        // lock the next refresh reuses.
        let Some(durable) = project_durable_grant(&seed, &SaveInput::from_stored(&credentials))
        else {
            tracing::debug!(
                key = %self.key,
                "MCP token refresh carried no durable change; skipping the durable re-encrypt"
            );
            return Ok(());
        };
        // The refresh token the refresh actually loaded and presented, so the
        // backend can refuse to overwrite a credential replaced mid-refresh.
        let expected_refresh_token = seed.refresh_token.as_ref().map(|t| t.as_str().to_string());
        let backend = self.backend.clone();
        tokio::task::spawn_blocking(move || backend.save(durable, expected_refresh_token))
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
    /// how many times `load` and `save` were called so a test can prove a
    /// non-rotating refresh does not re-encrypt and a warm cache serves without a
    /// backend read.
    struct FakeBackend {
        stored: StdMutex<Option<ServiceCredential>>,
        rejected: AtomicBool,
        loads: AtomicUsize,
        saves: AtomicUsize,
    }

    impl FakeBackend {
        fn new(credential: Option<ServiceCredential>) -> Arc<Self> {
            Arc::new(Self {
                stored: StdMutex::new(credential),
                rejected: AtomicBool::new(false),
                loads: AtomicUsize::new(0),
                saves: AtomicUsize::new(0),
            })
        }
    }

    impl CredentialBackend for FakeBackend {
        fn load(&self) -> Result<Option<ServiceCredential>, String> {
            self.loads.fetch_add(1, Ordering::SeqCst);
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
    fn cold_load_serves_an_expired_refresh_only_response() {
        crate::runtime::init().expect("runtime init");
        let backend = FakeBackend::new(Some(durable_oauth()));
        let adapter = CredentialStoreAdapter::new("mcp:docs|x".into(), backend);
        let stored = crate::runtime::block_on(adapter.load())
            .expect("block_on")
            .expect("load ok")
            .expect("some credentials");
        let response = stored.token_response.expect("a token response");
        // No access token, the refresh token is present, and the response is
        // marked expired (a zero `expires_in` plus a `received_at` anchor), so
        // `rmcp`'s expiry check fires and it refreshes proactively before the
        // first request — rather than sending an empty bearer and awaiting a
        // `401` (which GitHub answers with `400`).
        assert_eq!(response.access_token().secret(), "");
        assert_eq!(
            response.refresh_token().map(|t| t.secret().as_str()),
            Some("refresh-secret")
        );
        assert_eq!(
            response.expires_in(),
            Some(std::time::Duration::from_secs(0))
        );
        assert!(
            stored.token_received_at.is_some(),
            "a `received_at` anchor is required for rmcp's expiry check to fire"
        );
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

    /// A seed carrying `refresh` (and, when set, `access`) and nothing else the
    /// projection reads, for the pure projection tests below.
    fn seed(refresh: Option<&str>, access: Option<&str>) -> DurableSeed {
        DurableSeed {
            issuer: Some("https://as.example".into()),
            resource: Some("https://example.com/mcp".into()),
            authorization_server: Some("https://as.example".into()),
            account_id: None,
            refresh_token: refresh.map(|r| Zeroizing::new(r.to_string())),
            access_token: access.map(|a| Zeroizing::new(a.to_string())),
            client_secret: None,
            token_type: Some("Bearer".into()),
            client_id: "client-1".into(),
            granted_scopes: vec!["openid".into()],
        }
    }

    /// A token response with `lifetime` as its `expires_in` (`None` for a
    /// response that reports no lifetime at all).
    fn response_with_lifetime(lifetime: Option<u64>) -> OAuthTokenResponse {
        let mut response: OAuthTokenResponse = StandardTokenResponse::new(
            AccessToken::new("access-token".into()),
            BasicTokenType::Bearer,
            VendorExtraTokenFields::default(),
        );
        if let Some(secs) = lifetime {
            response.set_expires_in(Some(&std::time::Duration::from_secs(secs)));
        }
        response
    }

    #[test]
    fn cache_is_usable_only_beyond_the_serve_margin() {
        let now = 10_000;
        // No reported lifetime: `rmcp` skips its expiry check and uses the token
        // as-is, so nothing could refresh and the cache is always servable.
        assert!(cache_is_usable(&response_with_lifetime(None), now, now));
        // A lifetime far beyond the margin is servable.
        assert!(cache_is_usable(
            &response_with_lifetime(Some(3_600)),
            now,
            now
        ));
        // A lifetime inside the margin is NOT: `rmcp` may refresh on the load
        // that follows, and that refresh must re-read the durable store.
        assert!(!cache_is_usable(
            &response_with_lifetime(Some(90)),
            now,
            now + 45
        ));
        // An already-expired token is NOT servable.
        assert!(!cache_is_usable(
            &response_with_lifetime(Some(3_600)),
            now,
            now + 3_600
        ));
        // A clock step backwards cannot resurrect a token: `elapsed` saturates.
        assert!(cache_is_usable(
            &response_with_lifetime(Some(3_600)),
            now,
            now - 500
        ));
    }

    #[test]
    fn a_fresh_cache_serves_without_a_backend_read() {
        crate::runtime::init().expect("runtime init");
        let backend = FakeBackend::new(Some(durable_oauth()));
        let adapter = CredentialStoreAdapter::new("mcp:docs|x".into(), backend.clone());
        let now = now_secs();
        crate::runtime::block_on(async {
            // Cold: no cache yet, so the credential comes from the backend.
            let _ = adapter.load().await.expect("load ok").expect("some");
            assert_eq!(backend.loads.load(Ordering::SeqCst), 1);
            // Warm the cache with a token that has an hour of life left and the
            // same refresh token as the durable grant (a non-rotating refresh).
            let mut response = response_with_lifetime(Some(3_600));
            response.set_refresh_token(Some(RefreshToken::new("refresh-secret".into())));
            adapter
                .save(
                    StoredCredentials::new(
                        "client-1".into(),
                        Some(response),
                        vec!["openid".into()],
                        Some(now),
                    )
                    .with_issuer(Some("https://as.example".into())),
                )
                .await
                .expect("save ok");
            // The steady-state load is served from the cache, unchanged, and the
            // backend is not read again.
            let stored = adapter.load().await.expect("load ok").expect("some");
            assert_eq!(stored.client_id, "client-1");
            assert_eq!(stored.granted_scopes, vec!["openid".to_string()]);
            assert_eq!(stored.issuer.as_deref(), Some("https://as.example"));
            assert_eq!(
                stored
                    .token_response
                    .expect("a response")
                    .access_token()
                    .secret(),
                "access-token"
            );
        })
        .expect("block_on");
        assert_eq!(
            backend.loads.load(Ordering::SeqCst),
            1,
            "a warm, comfortably unexpired cache must not read the backend"
        );
    }

    #[test]
    fn a_cache_inside_the_margin_falls_through_to_the_backend() {
        crate::runtime::init().expect("runtime init");
        let backend = FakeBackend::new(Some(durable_oauth()));
        let adapter = CredentialStoreAdapter::new("mcp:docs|x".into(), backend.clone());
        crate::runtime::block_on(async {
            let _ = adapter.load().await.expect("load ok").expect("some");
            // A token received 45 seconds ago with a 90-second lifetime has 45
            // seconds left — inside the 60-second serve margin, so `rmcp` could
            // refresh after this load and the durable store MUST be re-read.
            let mut response = response_with_lifetime(Some(90));
            response.set_refresh_token(Some(RefreshToken::new("refresh-secret".into())));
            adapter
                .save(
                    StoredCredentials::new(
                        "client-1".into(),
                        Some(response),
                        vec!["openid".into()],
                        Some(now_secs() - 45),
                    )
                    .with_issuer(Some("https://as.example".into())),
                )
                .await
                .expect("save ok");
            let stored = adapter.load().await.expect("load ok").expect("some");
            assert_eq!(
                stored
                    .token_response
                    .expect("a response")
                    .access_token()
                    .secret(),
                "access-token"
            );
        })
        .expect("block_on");
        assert_eq!(
            backend.loads.load(Ordering::SeqCst),
            2,
            "a token inside the margin must re-read the durable store, because a \
             refresh (which does exactly that) may follow the load"
        );
    }

    #[test]
    fn a_seed_that_does_not_match_the_cache_falls_through_to_the_backend() {
        crate::runtime::init().expect("runtime init");
        let backend = FakeBackend::new(Some(durable_oauth()));
        let adapter = CredentialStoreAdapter::new("mcp:docs|x".into(), backend.clone());
        crate::runtime::block_on(async {
            let _ = adapter.load().await.expect("load ok").expect("some");
            // A ROTATING refresh: the cache now holds a refresh token the seed
            // (built from the durable store at the previous load) does not know
            // about, so the shortcut must decline and re-read the backend rather
            // than let a later `save` compare against a stale seed.
            let mut response = response_with_lifetime(Some(3_600));
            response.set_refresh_token(Some(RefreshToken::new("rotated-refresh".into())));
            adapter
                .save(
                    StoredCredentials::new(
                        "client-1".into(),
                        Some(response),
                        vec!["openid".into()],
                        Some(now_secs()),
                    )
                    .with_issuer(Some("https://as.example".into())),
                )
                .await
                .expect("save ok");
            let _ = adapter.load().await.expect("load ok").expect("some");
        })
        .expect("block_on");
        assert_eq!(
            backend.loads.load(Ordering::SeqCst),
            2,
            "a cache whose grant the seed does not describe must re-read the backend \
             so the seed is re-pinned before any refresh"
        );
    }

    /// A `SaveInput` with every response field absent and the given stored
    /// scopes/client id, so each test varies only the field it checks.
    fn input() -> SaveInput {
        SaveInput {
            response_refresh_token: None,
            response_access_token: None,
            response_scopes: None,
            stored_granted_scopes: vec!["openid".into()],
            stored_client_id: "client-1".into(),
            stored_issuer: Some("https://as.example".into()),
        }
    }

    #[test]
    fn projection_rotates_a_returned_refresh_token_and_strips_the_access_token() {
        let mut inp = input();
        inp.response_refresh_token = Some("new".into());
        let durable = project_durable_grant(&seed(Some("old"), None), &inp)
            .expect("a rotation is a durable change");
        let view = durable.as_oauth().expect("oauth");
        assert_eq!(view.refresh_token, Some("new"));
        assert_eq!(
            view.access_token, None,
            "a grant with a refresh token never persists the access token"
        );
    }

    #[test]
    fn projection_skips_a_non_rotating_refresh() {
        // `rmcp` rewrites the previous refresh token back onto the response, so
        // an equal token is NOT a durable change.
        let mut inp = input();
        inp.response_refresh_token = Some("same".into());
        inp.response_access_token = Some("fresh-access".into());
        assert!(
            project_durable_grant(&seed(Some("same"), None), &inp).is_none(),
            "a non-rotating refresh must not re-encrypt"
        );
    }

    #[test]
    fn projection_persists_the_access_token_for_a_no_refresh_grant() {
        let mut inp = input();
        inp.response_access_token = Some("a2".into());
        let durable = project_durable_grant(&seed(None, Some("a1")), &inp)
            .expect("a changed access token is a durable change");
        let view = durable.as_oauth().expect("oauth");
        assert_eq!(view.refresh_token, None);
        assert_eq!(view.access_token, Some("a2"));
    }

    #[test]
    fn projection_never_persists_an_empty_access_token() {
        // An empty bearer counts as absent: the seed's usable token governs, so
        // there is no durable change and nothing is written.
        let mut inp = input();
        inp.response_access_token = Some(String::new());
        assert!(project_durable_grant(&seed(None, Some("a1")), &inp).is_none());
    }

    #[test]
    fn projection_is_a_noop_without_a_token_response() {
        // No response fields at all, seed carries a refresh token: nothing changed.
        assert!(project_durable_grant(&seed(Some("rt"), None), &input()).is_none());
    }
}
