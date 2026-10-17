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
use rmcp::transport::auth::{AuthorizationManager, AuthorizationMetadata};

use super::McpAuthWiring;
use super::adapter::CredentialStoreAdapter;
use crate::config::{McpServerConfig, McpTransport};

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
            let resolver = AuthorizationManager::new(as_url).await.map_err(|e| {
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
