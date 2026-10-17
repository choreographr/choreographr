//! Dynamic client registration (SEP-837) for the MCP login flow.
//!
//! When the server's config carries no pre-registered client id, the flow
//! registers one at the authorization server's advertised registration
//! endpoint. The registration carries the already-bound redirect URI and the
//! same scope the authorize request will ask for, so the two never disagree.

use crossbeam_channel::Sender;
use serde_json::{Value, json};

use super::super::http;
use super::super::{LoginEvent, OAuthLoginError};
use super::discovery::AuthServer;

/// Dynamically register a client (SEP-837), returning its `client_id`.
///
/// `redirect_uri` is the URI the flow will actually use (the callback is already
/// bound), so the registered `redirect_uris` matches the authorize request
/// exactly. `scopes` is the same configured set the authorize request will ask
/// for (falling back to the server's advertised scopes), so the registered
/// client and the authorization request never disagree on scope.
///
/// `allow_loopback_http` is the flow's endpoint policy: the registration
/// endpoint came from a fetched document, so it is vetted under the same rule as
/// the rest of the flow (a remote resource cannot register against a local
/// endpoint).
pub(super) fn register_client(
    agent: &ureq::Agent,
    server: &AuthServer,
    redirect_uri: &str,
    scopes: &[String],
    allow_loopback_http: bool,
    tx: &Sender<LoginEvent>,
) -> Result<String, OAuthLoginError> {
    let Some(endpoint) = server.registration_endpoint.as_deref() else {
        return Err(OAuthLoginError::NoClientId(
            "no dynamic client registration endpoint; set an oauth.clientId in the server config"
                .to_string(),
        ));
    };
    let _ = tx.send(LoginEvent::Progress {
        message: "Registering this client with the authorization server…".to_string(),
    });
    http::ensure_secure_with(endpoint, allow_loopback_http)?;
    let body = json!({
        "client_name": "Choreographr",
        "redirect_uris": [redirect_uri],
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": "none",
        "scope": default_scopes(server, scopes).join(" "),
    });
    let response = agent
        .post(endpoint)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        // Anonymous dynamic registration carries no secret, but match the other
        // credential-free OAuth POSTs' no-store policy.
        .header("Cache-Control", "no-store")
        .send(body.to_string())
        .map_err(|e| OAuthLoginError::Network(e.to_string()))?;
    let value: Value = response
        .into_body()
        .read_json()
        .map_err(|e| OAuthLoginError::Decode(format!("registration response: {e}")))?;
    value
        .get("client_id")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or_else(|| {
            OAuthLoginError::Decode("registration response had no client_id".to_string())
        })
}

/// The scopes to request: the configured set when non-empty, else the
/// authorization server's advertised `scopes_supported`, else `openid`.
pub(super) fn default_scopes(server: &AuthServer, configured: &[String]) -> Vec<String> {
    if !configured.is_empty() {
        return configured.to_vec();
    }
    if server.scopes_supported.is_empty() {
        vec!["openid".to_string()]
    } else {
        server.scopes_supported.clone()
    }
}
