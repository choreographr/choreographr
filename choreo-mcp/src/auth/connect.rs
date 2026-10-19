//! The connect-time `rmcp` OAuth client construction.
//!
//! Discover the authorization server, wire the [`CredentialStoreAdapter`] to the
//! server's backend, and — when a durable credential is already stored —
//! configure the `rmcp` OAuth client (client id, optional secret, granted
//! scopes) so `rmcp` can refresh the access token on connect. This is the one
//! place in the crate that names `rmcp`'s `AuthClient`/`AuthorizationManager`
//! connect wiring; the durable↔`StoredCredentials` mapping lives beside it in
//! [`super::adapter`], so each module owns one concern.

use choreo_shared::{OAuthKind, ServiceCredential};
use rmcp::transport::auth::{
    AuthorizationManager, AuthorizationMetadata, OAuthHttpClient, default_oauth_http_client,
};
use std::sync::{Arc, OnceLock};

use super::McpAuthWiring;
use super::adapter::CredentialStoreAdapter;
use crate::config::{McpServerConfig, McpTransport};

/// The process-wide `rmcp` OAuth HTTP client, cached once it has been built.
///
/// `AuthorizationManager::new` builds a fresh default OAuth client on each call,
/// and that client reads the entire platform trust store — so every OAuth
/// connect (sign-in, token refresh, session reconnect) reloaded every system
/// root certificate. Building the SDK's default once and handing it to
/// `new_with_oauth_http_client` loads the roots a single time for the process;
/// both constructors use the same stop-redirect refresh policy, so behavior is
/// unchanged.
///
/// Only a **successful** build is stored. A built client is immutable and
/// expensive, so caching it is pure gain; a build failure is neither, and the
/// platform trust store it reads can be momentarily unreadable, so caching a
/// failure would pin a transient error for the life of the process — every later
/// connect would fail with it and nothing short of a restart could clear it. A
/// failed build therefore stores nothing, and the next connect retries it.
static OAUTH_HTTP_CLIENT: OnceLock<Arc<dyn OAuthHttpClient>> = OnceLock::new();

/// The shared `rmcp` OAuth HTTP client, building it on first use and after a
/// failed build.
///
/// # Errors
///
/// Returns [`McpError::InitializeFailed`](crate::error::McpError::InitializeFailed)
/// when the default client cannot be built (its platform trust store could not
/// be read). The failure is deliberately not cached, so a later connect retries
/// the build instead of inheriting a stale error.
fn shared_oauth_http_client() -> Result<Arc<dyn OAuthHttpClient>, crate::error::McpError> {
    // The fast path: a previous connect already stored a client.
    if let Some(client) = OAUTH_HTTP_CLIENT.get() {
        return Ok(Arc::clone(client));
    }
    let built: Arc<dyn OAuthHttpClient> = Arc::new(
        default_oauth_http_client()
            .map_err(|e| crate::error::McpError::InitializeFailed(e.to_string()))?,
    );
    // A concurrent connect may store its own client between the `get` above and
    // this `set`. Only a successful build ever reaches the cache, so the racing
    // build is equivalent and dropping it is harmless — but return whichever
    // value actually landed so every connect shares one client.
    let _ = OAUTH_HTTP_CLIENT.set(Arc::clone(&built));
    Ok(prefer_cached(OAUTH_HTTP_CLIENT.get(), built))
}

/// Return the cached client when the cache already holds one, else `built`.
///
/// A pure helper so the race-resolution rule — the first successful build wins
/// and a loser's equivalent client is dropped — stays testable without reading
/// the platform trust store.
fn prefer_cached<T: Clone>(cached: Option<&T>, built: T) -> T {
    cached.cloned().unwrap_or(built)
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
    use rmcp::transport::auth::{AuthClient, OAuthClientConfig};

    let mut manager =
        AuthorizationManager::new_with_oauth_http_client(url, shared_oauth_http_client()?)
            .await
            .map_err(|e| {
                crate::error::McpError::InitializeFailed(format!(
                    "OAuth discovery init failed: {e}"
                ))
            })?;

    let backend = wiring.backend.clone();
    let loaded = tokio::task::spawn_blocking(move || backend.load())
        .await
        .map_err(|e| {
            crate::error::McpError::InitializeFailed(format!("credential load task failed: {e}"))
        })?
        .map_err(crate::error::McpError::InitializeFailed)?;

    // A configured `authorizationServer` overrides discovery at connect exactly
    // as it does at sign-in: the server names its authorization server explicitly
    // rather than relying on the resource's well-known paths or the
    // `WWW-Authenticate` challenge.
    let explicit_as = match &config.transport {
        McpTransport::Http { oauth, .. } => {
            oauth.as_ref().and_then(|o| o.authorization_server.clone())
        }
        McpTransport::Stdio { .. } => None,
    };

    if let Some(view) = loaded.as_ref().and_then(ServiceCredential::as_oauth)
        && view.kind == OAuthKind::Mcp
    {
        let client_id = view.client_id.map(str::to_string);
        let client_secret = view.client_secret.map(str::to_string);
        let granted_scopes = view.granted_scopes.to_vec();
        let metadata = resolve_metadata(&manager, explicit_as.as_deref()).await?;
        // Arm the adapter to withhold the stored grant when its issuer no longer
        // matches the one discovered here (a re-pointed server), so the token is
        // never sent; the connect then surfaces a re-login.
        manager.set_credential_store(
            CredentialStoreAdapter::new(wiring.key.clone(), wiring.backend.clone())
                .with_expected_issuer(metadata.issuer.clone()),
        );
        manager.set_metadata(metadata);
        // The redirect URI is only used to build a token refresh; a refresh-token
        // grant does not send `redirect_uri`, but this matches the URI the
        // interactive login registers (`/auth/callback`) so the two never
        // disagree if a server does validate it.
        let port = match &config.transport {
            McpTransport::Http { oauth, .. } => oauth.as_ref().and_then(|o| o.callback_port),
            McpTransport::Stdio { .. } => None,
        }
        .unwrap_or(1455);
        let redirect = format!("http://127.0.0.1:{port}/auth/callback");
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

/// Resolve the authorization-server metadata the connect-time refresh uses.
///
/// With no `authorizationServer` override, rmcp discovers the authorization
/// server from the resource URL (RFC 9728 protected-resource metadata, the
/// `WWW-Authenticate` challenge, then the RFC 8414 / OIDC well-known paths).
/// rmcp's `AuthorizationManager` has no explicit-issuer setter, so a configured
/// `authorizationServer` — the case where the AS is not reachable through the
/// resource at all — is resolved by running rmcp's own discovery against the
/// override URL (its candidate set includes the `OpenID` Connect appended form)
/// and seeding the result back onto this manager. The manager's base URL stays
/// the resource, so the RFC 8707 `resource` parameter the refresh sends remains
/// the MCP endpoint rather than the authorization server.
async fn resolve_metadata(
    manager: &AuthorizationManager,
    explicit_as: Option<&str>,
) -> Result<AuthorizationMetadata, crate::error::McpError> {
    let resolution = match explicit_as {
        Some(as_url) => {
            let resolver = AuthorizationManager::new_with_oauth_http_client(
                as_url,
                shared_oauth_http_client()?,
            )
            .await
            .map_err(|e| {
                crate::error::McpError::InitializeFailed(format!(
                    "OAuth discovery init failed: {e}"
                ))
            })?;
            resolver.resolve_metadata().await
        }
        None => manager.resolve_metadata().await,
    }
    .map_err(|e| {
        crate::error::McpError::InitializeFailed(format!("OAuth metadata discovery failed: {e}"))
    })?;
    Ok(resolution.metadata)
}

#[cfg(test)]
mod tests {
    use super::prefer_cached;

    /// The stored client wins over a freshly built one, and a missing cache
    /// entry takes the fresh build. Resolving the race this way is what keeps
    /// every connect on a single shared client.
    #[test]
    fn prefer_cached_keeps_the_stored_value() {
        assert_eq!(prefer_cached(Some(&1u8), 2), 1, "the stored value wins");
        assert_eq!(prefer_cached(None, 2), 2, "nothing stored takes the build");
    }
}
