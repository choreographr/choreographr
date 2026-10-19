//! MCP remote-server OAuth login library (discovery + DCR + PKCE).
//!
//! A remote MCP server's sign-in is a standard OAuth 2.1 authorization-code
//! flow with PKCE. What differs from a provider is that the authorization
//! server's endpoints are **discovered** rather than configured: the client
//! fetches the server's RFC 9728 protected-resource metadata, follows it to the
//! authorization server, reads the RFC 8414 / OIDC metadata for the endpoints,
//! and — when it has no pre-registered client id — dynamically registers
//! (SEP-837). The resulting [`ServiceCredential::OAuth`] is stored under the
//! daemon-supplied target-scoped key, exactly like a provider grant.
//!
//! Reuses the crate's PKCE, loopback callback server, and form-encoding helpers
//! from the provider login library; the flow itself runs on a worker thread and
//! streams the same `LoginEvent`s, so the frontend renders it with the same
//! modal. The interactive flow is **loopback + paste-URL only** (MCP has no
//! device-code flow).
//!
//! The module is split by responsibility: this file owns the public types
//! ([`McpLoginInfo`], [`McpLoginOptions`]) and the worker driver
//! ([`start_mcp_login`] / `run`); the `discovery` submodule reads the RFC 9728
//! and RFC 8414 / OIDC metadata; the `register` submodule performs dynamic
//! client registration; the `authorize` submodule runs the PKCE callback flow
//! and the code exchange.
//!
//! The scopes recorded on the MCP credential are one composition of two rules,
//! each owned by the module that applies it:
//!
//! - [`register::default_scopes`] resolves the **requested** set by precedence —
//!   the configured `scopes`, else the `WWW-Authenticate` challenge's `scope`,
//!   else the protected resource's `scopes_supported`, else the authorization
//!   server's advertised `scopes_supported` (an empty result means "request
//!   none", so the callers omit the `scope` parameter).
//! - [`credential::granted_scopes`] then prefers the token response's own
//!   `scope` when it is present and non-empty, falling back to that requested
//!   set.
//!
//! [`resolved_granted_scopes`] is exactly that composition, so the scopes
//! stored on the credential never disagree with what the authorize request and
//! dynamic registration asked for.

use std::sync::atomic::AtomicBool;

use choreo_keystore::{OAuthKind, ServiceCredential};
use choreo_proto::McpOAuthConfig;
use crossbeam_channel::Sender;
use zeroize::Zeroizing;

use super::{
    LoginEvent, LoginSession, OAuthLoginError, callback, credential, http, spawn_login_worker,
};
use crate::credentials::build_add_credential_from_credential;

mod authorize;
mod challenge;
mod discovery;
mod register;

use authorize::{AuthorizeContext, authorize};
use discovery::{AuthServer, discover};
use register::{default_scopes, register_client};

/// The server information a frontend derives from `McpStatus` to sign in.
///
/// `resource` is the server URL (the RFC 8707 resource indicator); `key` is the
/// daemon-computed target-scoped credential key the grant is stored under; the
/// `oauth` block supplies only what discovery cannot (a pre-registered client
/// id, a preferred callback port, the scopes to request).
#[derive(Debug, Clone)]
pub struct McpLoginInfo {
    /// The human-facing server slug (for the modal title).
    pub slug: String,
    /// The server URL, as the RFC 8707 `resource` indicator.
    pub resource: String,
    /// The daemon-computed target-scoped credential key (`mcp:<slug>|<hash>`).
    pub credential_key: String,
    /// The resolved `oauth` block (client id, callback port, scopes).
    pub oauth: McpOAuthConfig,
}

/// Options controlling how the MCP sign-in runs.
#[derive(Debug, Clone, Default)]
pub struct McpLoginOptions {
    /// Never open a browser; emit the authorize URL for the caller to surface.
    pub no_browser: bool,
    /// A callback URL (or authorize-response URL) pasted by the user for the
    /// paste-URL path, instead of waiting on the loopback server.
    pub callback_url: Option<String>,
}

/// The ports the loopback callback server tries, in order.
const CALLBACK_PORTS: [u16; 2] = [1455, 1457];

/// Start the MCP sign-in for `info` on a worker thread.
///
/// Returns immediately with a [`LoginSession`]; progress streams on
/// `session.events` as [`LoginEvent`]s. On `Done`, `message` is the
/// `AddCredential` request carrying the credential under `info.credential_key`.
#[must_use]
pub fn start_mcp_login(info: McpLoginInfo, options: McpLoginOptions) -> LoginSession {
    // MCP has no device-code interval wait, so no stop channel is installed;
    // cancellation rides the flag the loopback accept loop polls.
    spawn_login_worker("mcp-oauth-login", false, move |tx, cancel, _stop| {
        run(&info, &options, tx, cancel)
    })
}

/// The worker entry point.
///
/// The flow reuses one HTTP agent built once here for every request it makes
/// (discovery, registration, authorize, code exchange) instead of rebuilding
/// one per request, because a new agent rebuilds the TLS configuration and
/// reloads the root store.
fn run(
    info: &McpLoginInfo,
    options: &McpLoginOptions,
    tx: &Sender<LoginEvent>,
    cancel: &AtomicBool,
) -> Result<(), OAuthLoginError> {
    // Reject a nonsensical confidential-client config before any network work.
    validate_oauth_config(&info.oauth)?;
    // The local allowances are available only when the flow already began at a
    // loopback (in-repo mock) or private IP-literal (LAN self-hosted) resource.
    // A resource reached by hostname is remote, so a document it served must
    // not steer any later hop — a metadata GET redirect, a discovered endpoint
    // — onto `http://127.0.0.1` or an internal private/link-local address, which
    // would let a hostile document probe the host's local services.
    let policy = http::HopPolicy::for_resource(&info.resource);
    // One agent for the whole sign-in: discovery, dynamic registration, the
    // authorize request, and the code exchange all reuse the same
    // TLS-configured connection pool. Building an agent per request would
    // rebuild the TLS configuration and reload the root store each time.
    let agent = http::agent();
    let _ = tx.send(LoginEvent::Progress {
        message: "Discovering the authorization server…".to_string(),
    });
    let server = discover(
        &agent,
        &info.resource,
        info.oauth.authorization_server.as_deref(),
        policy,
    )?;
    // Bind the loopback callback FIRST so the redirect URI is known, then use
    // that exact URI for both dynamic registration and the authorize request.
    // A server that validates `redirect_uri` against the registered value must
    // see the same string, including the port actually bound (which may be the
    // fallback, not the preferred one).
    let ports = preferred_ports(info.oauth.callback_port);
    let (callback_server, port) = callback::bind("127.0.0.1", &ports)?;
    let bound = BoundCallback {
        server: callback_server,
        redirect_uri: format!("http://127.0.0.1:{port}{}", callback::CALLBACK_PATH),
    };
    let client_id = match info.oauth.client_id.clone() {
        Some(id) => id,
        None => register_client(
            &agent,
            &server,
            &bound.redirect_uri,
            &info.oauth.scopes,
            policy,
            tx,
        )?,
    };
    // Resolve the confidential client's secret from the environment variable the
    // config names, if any. An unset variable is a terminal error here rather
    // than a later, less clear token-endpoint rejection.
    let client_secret = resolve_client_secret(info.oauth.client_secret_env.as_deref())?;
    let ctx = AuthorizeContext {
        info,
        options,
        server: &server,
        client_id: &client_id,
        client_secret: client_secret.as_ref().map(|s| s.as_str()),
        bound: &bound,
        policy,
        agent: &agent,
    };
    let tokens = authorize(&ctx, tx, cancel)?;
    finish(
        info,
        &server,
        &client_id,
        client_secret.as_ref().map(|s| s.as_str()),
        &tokens,
        tx,
    )?;
    Ok(())
}

/// The bound loopback callback server and the redirect URI it answers on, so
/// the authorize path takes one argument instead of two.
struct BoundCallback {
    server: tiny_http::Server,
    redirect_uri: String,
}

/// Reject an `oauth` block that cannot describe a usable client: a confidential
/// client's secret (`clientSecretEnv`) is only meaningful with a pre-registered
/// `clientId`. A client with no id is created by dynamic registration, which
/// issues its own credentials, so supplying a secret for it could only ever
/// authenticate as the wrong client.
fn validate_oauth_config(oauth: &McpOAuthConfig) -> Result<(), OAuthLoginError> {
    if oauth.client_secret_env.is_some() && oauth.client_id.is_none() {
        return Err(OAuthLoginError::ClientSecretWithoutClientId);
    }
    Ok(())
}

/// Resolve a confidential client's secret from the environment variable the
/// config names, if any. Returning an error for an unset variable keeps the
/// failure at the clear "set the variable" point rather than a later, opaque
/// token-endpoint rejection.
fn resolve_client_secret(
    env_name: Option<&str>,
) -> Result<Option<Zeroizing<String>>, OAuthLoginError> {
    match env_name {
        None => Ok(None),
        Some(name) => std::env::var(name)
            .map(|value| Some(Zeroizing::new(value)))
            .map_err(|_| OAuthLoginError::ClientSecretEnvUnset(name.to_string())),
    }
}

/// Build the credential from the exchanged tokens and emit `Done`.
fn finish(
    info: &McpLoginInfo,
    server: &AuthServer,
    client_id: &str,
    client_secret: Option<&str>,
    tokens: &http::TokenSet,
    tx: &Sender<LoginEvent>,
) -> Result<(), OAuthLoginError> {
    let granted_scopes =
        resolved_granted_scopes(server, &info.oauth.scopes, tokens.scope.as_deref());
    let credential = ServiceCredential::OAuth {
        kind: OAuthKind::Mcp,
        issuer: Some(server.issuer.clone()),
        resource: Some(info.resource.clone()),
        authorization_server: Some(server.issuer.clone()),
        // MCP refresh re-discovers through `rmcp`, so no token endpoint or
        // refresh encoding is recorded (those are provider-grant fields).
        token_endpoint: None,
        refresh_encoding: None,
        refresh_token: tokens.refresh_token.as_ref().map(|t| t.to_string()),
        // Persist the access token ONLY when the server issued no refresh token:
        // with nothing to refresh, it is the only credential that can
        // authenticate the connect (and the only thing to survive a daemon
        // restart). Whenever a refresh token exists, the access token is
        // ephemeral and stays out of the durable grant.
        access_token: tokens
            .refresh_token
            .is_none()
            .then(|| tokens.access_token.to_string()),
        account_id: None,
        client_id: Some(client_id.to_string()),
        // Persisted only for a confidential client, so connect-time refresh can
        // re-authenticate the token endpoint (rmcp applies it via
        // `OAuthClientConfig::with_client_secret`).
        client_secret: client_secret.map(str::to_string),
        granted_scopes,
        token_type: credential::resolve_token_type(tokens.token_type.clone()),
    };
    let message = build_add_credential_from_credential(info.credential_key.clone(), credential)
        .map_err(|e| OAuthLoginError::Decode(e.to_string()))?;
    let _ = tx.send(LoginEvent::Done {
        service: info.credential_key.clone(),
        message: Box::new(message),
    });
    Ok(())
}

/// The scopes to record on the MCP credential: the token response's granted
/// `scope` when present, else the same resolved set the authorize request and
/// dynamic registration asked for (configured → challenge → resource → AS), so
/// the stored grant never disagrees with what was requested.
fn resolved_granted_scopes(
    server: &AuthServer,
    configured: &[String],
    token_scope: Option<&str>,
) -> Vec<String> {
    let requested = default_scopes(server, configured);
    credential::granted_scopes(token_scope, &requested)
}

/// The callback ports to try: the preferred one first, then the defaults.
fn preferred_ports(preferred: Option<u16>) -> Vec<u16> {
    let mut ports = Vec::new();
    if let Some(port) = preferred {
        ports.push(port);
    }
    for port in CALLBACK_PORTS {
        if !ports.contains(&port) {
            ports.push(port);
        }
    }
    ports
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preferred_ports_lead_with_the_configured_port() {
        assert_eq!(preferred_ports(Some(9000)), vec![9000, 1455, 1457]);
        assert_eq!(preferred_ports(Some(1457)), vec![1457, 1455]);
        assert_eq!(preferred_ports(None), vec![1455, 1457]);
    }

    fn server(challenge: Option<&[&str]>) -> AuthServer {
        AuthServer {
            issuer: "https://as.example".to_string(),
            authorization_endpoint: "https://as.example/authorize".to_string(),
            token_endpoint: "https://as.example/token".to_string(),
            registration_endpoint: Some("https://as.example/register".to_string()),
            scopes_supported: Vec::new(),
            token_endpoint_auth_methods_supported: Vec::new(),
            challenge_scope: challenge.map(|s| s.iter().map(ToString::to_string).collect()),
            resource_scopes_supported: Vec::new(),
            authorization_response_iss_parameter_supported: false,
        }
    }

    #[test]
    fn resolved_granted_scopes_match_the_requested_set() {
        // No token-response scope: fall back to the resolved set the authorize
        // request and DCR asked for, so a challenge-scoped grant is recorded.
        assert_eq!(
            resolved_granted_scopes(&server(Some(&["c"])), &[], None),
            ["c"]
        );
        // Configured scopes win in the resolution, so they are what is recorded.
        assert_eq!(
            resolved_granted_scopes(&server(Some(&["c"])), &["configured".to_string()], None),
            ["configured"]
        );
        // A token-response scope is recorded verbatim, regardless of the request.
        assert_eq!(
            resolved_granted_scopes(&server(Some(&["c"])), &[], Some("x y")),
            ["x", "y"]
        );
    }

    #[test]
    fn finish_records_the_resolved_token_type() {
        use choreo_proto::ClientMessageType;

        // The MCP path shares the provider credential's token-type rule: a reply
        // that omits `token_type` records `Bearer`, and a reported non-Bearer
        // type (e.g. `mac`) is preserved.
        for (reported, expected) in [(None, "Bearer"), (Some("mac"), "mac")] {
            let info = McpLoginInfo {
                slug: "docs".to_string(),
                resource: "https://example.com/mcp".to_string(),
                credential_key: "mcp:docs|x".to_string(),
                oauth: McpOAuthConfig {
                    client_id: Some("app-1".to_string()),
                    callback_port: None,
                    scopes: vec![],
                    authorization_server: None,
                    client_secret_env: None,
                },
            };
            let tokens = http::TokenSet {
                access_token: Zeroizing::new("access".to_string()),
                refresh_token: Some(Zeroizing::new("refresh".to_string())),
                id_token: None,
                token_type: reported.map(str::to_string),
                scope: None,
                expires_in: None,
            };
            let (tx, rx) = crossbeam_channel::unbounded();
            finish(&info, &server(None), "app-1", None, &tokens, &tx).expect("finish");

            let LoginEvent::Done { message, .. } = rx.try_recv().expect("a Done event") else {
                panic!("expected a Done event");
            };
            let ClientMessageType::AddCredential { credential, .. } = *message else {
                panic!("expected an AddCredential message");
            };
            assert_eq!(
                credential.as_oauth().expect("OAuth credential").token_type,
                Some(expected)
            );
        }
    }

    #[test]
    fn client_secret_env_requires_a_client_id() {
        let cfg = |client_id: Option<&str>, client_secret_env: Option<&str>| McpOAuthConfig {
            client_id: client_id.map(str::to_string),
            callback_port: None,
            scopes: vec![],
            authorization_server: None,
            client_secret_env: client_secret_env.map(str::to_string),
        };
        // No secret, no id: fine (public, dynamically registered).
        assert!(validate_oauth_config(&cfg(None, None)).is_ok());
        // A secret with a pre-registered id: fine (confidential client).
        assert!(validate_oauth_config(&cfg(Some("app-1"), Some("SHOPIFY_MCP_SECRET"))).is_ok());
        // A secret WITHOUT an id is rejected: dynamic registration would mint a
        // different client, so the configured secret can never match.
        assert!(matches!(
            validate_oauth_config(&cfg(None, Some("SHOPIFY_MCP_SECRET"))),
            Err(OAuthLoginError::ClientSecretWithoutClientId)
        ));
    }
}
