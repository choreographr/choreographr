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

use std::sync::atomic::AtomicBool;

use choreo_keystore::{OAuthKind, ServiceCredential};
use choreo_proto::McpOAuthConfig;
use crossbeam_channel::Sender;

use super::{
    LoginEvent, LoginSession, OAuthLoginError, callback, credential, http, spawn_login_worker,
};
use crate::credentials::build_add_credential_from_credential;

mod authorize;
mod discovery;
mod register;

use authorize::{AuthorizeContext, authorize};
use discovery::{AuthServer, discover};
use register::register_client;

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
fn run(
    info: &McpLoginInfo,
    options: &McpLoginOptions,
    tx: &Sender<LoginEvent>,
    cancel: &AtomicBool,
) -> Result<(), OAuthLoginError> {
    // The local allowances are available only when the flow already began at a
    // loopback (in-repo mock) or private IP-literal (LAN self-hosted) resource.
    // A resource reached by hostname is remote, so a document it served must
    // not steer any later hop — a metadata GET redirect, a discovered endpoint
    // — onto `http://127.0.0.1` or an internal private/link-local address, which
    // would let a hostile document probe the host's local services.
    let policy = http::HopPolicy::for_resource(&info.resource);
    let _ = tx.send(LoginEvent::Progress {
        message: "Discovering the authorization server…".to_string(),
    });
    let server = discover(&http::agent(), &info.resource, policy)?;
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
            &http::agent(),
            &server,
            &bound.redirect_uri,
            &info.oauth.scopes,
            policy,
            tx,
        )?,
    };
    let ctx = AuthorizeContext {
        info,
        options,
        server: &server,
        client_id: &client_id,
        bound: &bound,
        policy,
    };
    let tokens = authorize(&ctx, tx, cancel)?;
    finish(info, &server, &client_id, &tokens, tx)?;
    Ok(())
}

/// The bound loopback callback server and the redirect URI it answers on, so
/// the authorize path takes one argument instead of two.
struct BoundCallback {
    server: tiny_http::Server,
    redirect_uri: String,
}

/// Build the credential from the exchanged tokens and emit `Done`.
fn finish(
    info: &McpLoginInfo,
    server: &AuthServer,
    client_id: &str,
    tokens: &http::TokenSet,
    tx: &Sender<LoginEvent>,
) -> Result<(), OAuthLoginError> {
    let granted_scopes = credential::granted_scopes(tokens.scope.as_deref(), &info.oauth.scopes);
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
        account_id: None,
        client_id: Some(client_id.to_string()),
        client_secret: None,
        granted_scopes,
        token_type: tokens.token_type.clone(),
    };
    let message = build_add_credential_from_credential(info.credential_key.clone(), credential)
        .map_err(|e| OAuthLoginError::Decode(e.to_string()))?;
    let _ = tx.send(LoginEvent::Done {
        service: info.credential_key.clone(),
        message: Box::new(message),
    });
    Ok(())
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
}
