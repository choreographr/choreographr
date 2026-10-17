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
/// `policy` is the flow's endpoint policy: the registration endpoint came from
/// a fetched document, so it is vetted under the same rule as the rest of the
/// flow (a remote resource cannot register against a loopback or private
/// endpoint).
pub(super) fn register_client(
    agent: &ureq::Agent,
    server: &AuthServer,
    redirect_uri: &str,
    scopes: &[String],
    policy: http::HopPolicy,
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
    policy.check(endpoint)?;
    let mut body = json!({
        "client_name": "Choreographr",
        "redirect_uris": [redirect_uri],
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": "none",
    });
    // Omit `scope` entirely when there is nothing to request (per the spec's
    // scope-selection strategy), rather than registering an empty scope set.
    let scope = default_scopes(server, scopes).join(" ");
    if !scope.is_empty()
        && let Some(object) = body.as_object_mut()
    {
        object.insert("scope".to_string(), serde_json::Value::String(scope));
    }
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

/// The scopes to request, in the spec's scope-selection precedence: the
/// client-configured set when non-empty, else the challenge's scopes (RFC 6750
/// §3), else the protected resource's `scopes_supported`, else the authorization
/// server's own advertised scopes (a compatibility fallback for a server that
/// advertises them only on itself). An empty result means "request no scope" —
/// the callers omit the `scope` parameter rather than inventing a default.
pub(super) fn default_scopes(server: &AuthServer, configured: &[String]) -> Vec<String> {
    if !configured.is_empty() {
        return configured.to_vec();
    }
    if let Some(challenge) = &server.challenge_scope {
        return challenge.clone();
    }
    if !server.resource_scopes_supported.is_empty() {
        return server.resource_scopes_supported.clone();
    }
    server.scopes_supported.clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server(challenge: Option<&[&str]>, resource: &[&str], advertised: &[&str]) -> AuthServer {
        AuthServer {
            issuer: "https://as.example".to_string(),
            authorization_endpoint: "https://as.example/authorize".to_string(),
            token_endpoint: "https://as.example/token".to_string(),
            registration_endpoint: Some("https://as.example/register".to_string()),
            scopes_supported: advertised.iter().map(ToString::to_string).collect(),
            token_endpoint_auth_methods_supported: Vec::new(),
            challenge_scope: challenge.map(|s| s.iter().map(ToString::to_string).collect()),
            resource_scopes_supported: resource.iter().map(ToString::to_string).collect(),
            authorization_response_iss_parameter_supported: false,
        }
    }

    #[test]
    fn default_scopes_follow_the_selection_precedence() {
        let owned = |scopes: &[&str]| scopes.iter().map(ToString::to_string).collect::<Vec<_>>();
        // Configured scopes win over every source.
        assert_eq!(
            default_scopes(
                &server(Some(&["c"]), &["r"], &["a"]),
                &owned(&["configured"])
            ),
            owned(&["configured"])
        );
        // Else the challenge's scope.
        assert_eq!(
            default_scopes(&server(Some(&["c"]), &["r"], &["a"]), &[]),
            owned(&["c"])
        );
        // Else the resource's scopes.
        assert_eq!(
            default_scopes(&server(None, &["r"], &["a"]), &[]),
            owned(&["r"])
        );
        // Else the AS's advertised scopes (compatibility fallback).
        assert_eq!(
            default_scopes(&server(None, &[], &["a"]), &[]),
            owned(&["a"])
        );
        // Else nothing — the callers omit `scope`.
        assert_eq!(
            default_scopes(&server(None, &[], &[]), &[]),
            Vec::<String>::new()
        );
    }
}
