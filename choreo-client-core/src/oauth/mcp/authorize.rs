//! The MCP PKCE loopback (or paste-URL) authorization-code flow.
//!
//! Builds the authorize URL from the discovered endpoints, captures the
//! callback via the shared scaffold, and exchanges the code at the token
//! endpoint. The flow is loopback + paste-URL only — MCP has no device-code
//! flow.

use std::sync::atomic::AtomicBool;

use crossbeam_channel::Sender;

use super::super::http;
use super::super::{LoginEvent, OAuthLoginError, callback, pkce};
use super::discovery::AuthServer;
use super::register::default_scopes;
use super::{BoundCallback, McpLoginInfo, McpLoginOptions};

/// Run the PKCE loopback (or paste-URL) authorization-code flow over the
/// already-bound callback server and its `redirect_uri`.
pub(super) fn authorize(
    info: &McpLoginInfo,
    options: &McpLoginOptions,
    server: &AuthServer,
    client_id: &str,
    bound: &BoundCallback,
    tx: &Sender<LoginEvent>,
    cancel: &AtomicBool,
) -> Result<http::TokenSet, OAuthLoginError> {
    let agent = http::agent();
    let verifier = pkce::generate_verifier();
    let challenge = pkce::challenge_s256(&verifier);
    let state = pkce::generate_state();
    let scopes = default_scopes(server, &info.oauth.scopes);

    let auth_url = build_authorize_url(
        server,
        client_id,
        &bound.redirect_uri,
        &challenge,
        &state,
        &scopes,
        &info.resource,
    )?;
    // The shared scaffold emits the URL, opens the browser (or prompts), and
    // captures the callback from the loopback server or a pasted URL.
    let callback = callback::capture_loopback_callback(
        &auth_url,
        options.no_browser,
        options.callback_url.as_deref(),
        &bound.server,
        cancel,
        tx,
    )?;

    if !pkce::state_matches(&state, &callback.state) {
        return Err(OAuthLoginError::StateMismatch);
    }
    let _ = tx.send(LoginEvent::Progress {
        message: "Authorization received; exchanging the code for tokens…".to_string(),
    });
    exchange_code(
        &agent,
        &server.token_endpoint,
        client_id,
        &callback.code,
        &bound.redirect_uri,
        &verifier,
        &info.resource,
    )
}

/// Build the authorization URL with PKCE S256 and the resource indicator.
///
/// The authorization endpoint (taken from discovered metadata) is vetted with
/// the shared `https`-or-loopback rule first, so an `http://` remote endpoint a
/// hostile or misconfigured server advertises can never be opened in the
/// browser.
///
/// # Errors
///
/// Returns [`OAuthLoginError::InsecureEndpoint`] when the discovered
/// authorization endpoint is neither `https` nor a loopback `http` address.
fn build_authorize_url(
    server: &AuthServer,
    client_id: &str,
    redirect_uri: &str,
    challenge: &str,
    state: &str,
    scopes: &[String],
    resource: &str,
) -> Result<String, OAuthLoginError> {
    http::ensure_secure(&server.authorization_endpoint)?;
    let params = vec![
        ("response_type".to_string(), "code".to_string()),
        ("client_id".to_string(), client_id.to_string()),
        ("redirect_uri".to_string(), redirect_uri.to_string()),
        ("code_challenge".to_string(), challenge.to_string()),
        ("code_challenge_method".to_string(), "S256".to_string()),
        ("state".to_string(), state.to_string()),
        ("scope".to_string(), scopes.join(" ")),
        ("resource".to_string(), resource.to_string()),
    ];
    Ok(http::authorize_url(&server.authorization_endpoint, &params))
}

/// Exchange the authorization code at the token endpoint (form-encoded).
///
/// Routed through the shared [`http::post`] so the request carries the same
/// `Accept`, `Cache-Control: no-store`, and redirect policy as the provider
/// flow.
fn exchange_code(
    agent: &ureq::Agent,
    token_endpoint: &str,
    client_id: &str,
    code: &str,
    redirect_uri: &str,
    verifier: &str,
    resource: &str,
) -> Result<http::TokenSet, OAuthLoginError> {
    let params = vec![
        ("grant_type".to_string(), "authorization_code".to_string()),
        ("code".to_string(), code.to_string()),
        ("redirect_uri".to_string(), redirect_uri.to_string()),
        ("client_id".to_string(), client_id.to_string()),
        ("code_verifier".to_string(), verifier.to_string()),
        ("resource".to_string(), resource.to_string()),
    ];
    let body = http::form_encode(&params);
    let response = http::post(
        agent,
        token_endpoint,
        "application/x-www-form-urlencoded",
        &body,
    )?;
    http::read_token_response(response)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server(authorization_endpoint: &str) -> AuthServer {
        AuthServer {
            issuer: "https://as.example".to_string(),
            authorization_endpoint: authorization_endpoint.to_string(),
            token_endpoint: "https://as.example/token".to_string(),
            registration_endpoint: None,
            scopes_supported: vec!["openid".to_string()],
        }
    }

    #[test]
    fn authorize_url_carries_pkce_and_resource() {
        let url = build_authorize_url(
            &server("https://as.example/authorize"),
            "client-1",
            "http://127.0.0.1:1455/auth/callback",
            "challenge",
            "state-1",
            &["openid".to_string(), "offline_access".to_string()],
            "https://example.com/mcp",
        )
        .expect("secure endpoint");
        assert!(url.starts_with("https://as.example/authorize?"));
        assert!(url.contains("code_challenge=challenge"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("state=state-1"));
        assert!(url.contains("resource=https%3A%2F%2Fexample.com%2Fmcp"));
        assert!(url.contains("scope=openid%20offline_access"));
    }

    #[test]
    fn authorize_url_refuses_a_cleartext_remote_endpoint() {
        // A discovered `http://` remote authorize endpoint must be refused
        // before the browser is pointed at it; an `https://` one is accepted.
        assert!(matches!(
            build_authorize_url(
                &server("http://evil.example/authorize"),
                "client-1",
                "http://127.0.0.1:1455/auth/callback",
                "challenge",
                "state-1",
                &["openid".to_string()],
                "https://example.com/mcp",
            ),
            Err(OAuthLoginError::InsecureEndpoint(_))
        ));
        assert!(
            build_authorize_url(
                &server("https://as.example/authorize"),
                "client-1",
                "http://127.0.0.1:1455/auth/callback",
                "challenge",
                "state-1",
                &["openid".to_string()],
                "https://example.com/mcp",
            )
            .is_ok()
        );
    }
}
