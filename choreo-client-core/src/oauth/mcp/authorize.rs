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

/// The flow-wide inputs `authorize` runs against, grouped so the entry point
/// takes one value rather than a long positional list: the login info, the
/// options, the discovered server, the client id, the bound callback, and the
/// flow's endpoint policy.
pub(super) struct AuthorizeContext<'a> {
    /// The server's login info (its resource indicator and oauth block).
    pub(super) info: &'a McpLoginInfo,
    /// The UI options (browser prompt, pasted callback URL).
    pub(super) options: &'a McpLoginOptions,
    /// The discovered authorization server.
    pub(super) server: &'a AuthServer,
    /// The client id (pre-registered or dynamically registered).
    pub(super) client_id: &'a str,
    /// The bound loopback callback server and its `redirect_uri`.
    pub(super) bound: &'a BoundCallback,
    /// Whether a loopback `http` endpoint is allowed (the flow began at a
    /// loopback resource).
    pub(super) allow_loopback_http: bool,
}

/// The per-request inputs both the authorize URL and the token exchange carry:
/// the client registration, the redirect URI, the RFC 8707 resource indicator,
/// and the endpoint policy. Grouped so each builder stays under the argument
/// limit and the two cannot drift on the resource/redirect they send.
struct CodeRequest<'a> {
    client_id: &'a str,
    redirect_uri: &'a str,
    resource: &'a str,
    allow_loopback_http: bool,
}

/// Run the PKCE loopback (or paste-URL) authorization-code flow over the
/// already-bound callback server and its `redirect_uri`.
pub(super) fn authorize(
    ctx: &AuthorizeContext<'_>,
    tx: &Sender<LoginEvent>,
    cancel: &AtomicBool,
) -> Result<http::TokenSet, OAuthLoginError> {
    let agent = http::agent();
    let verifier = pkce::generate_verifier();
    let challenge = pkce::challenge_s256(&verifier);
    let state = pkce::generate_state();
    let scopes = default_scopes(ctx.server, &ctx.info.oauth.scopes);
    let request = CodeRequest {
        client_id: ctx.client_id,
        redirect_uri: &ctx.bound.redirect_uri,
        resource: &ctx.info.resource,
        allow_loopback_http: ctx.allow_loopback_http,
    };

    let auth_url = build_authorize_url(ctx.server, &request, &challenge, &state, &scopes)?;
    // The shared scaffold emits the URL, opens the browser (or prompts), and
    // captures the callback from the loopback server or a pasted URL.
    let callback = callback::capture_loopback_callback(
        &auth_url,
        ctx.options.no_browser,
        ctx.options.callback_url.as_deref(),
        &ctx.bound.server,
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
        &ctx.server.token_endpoint,
        &request,
        &callback.code,
        &verifier,
    )
}

/// Build the authorization URL with PKCE S256 and the resource indicator.
///
/// The authorization endpoint (taken from discovered metadata) is vetted with
/// the shared `https`-or-loopback rule first, under the flow's endpoint policy,
/// so an `http://` remote endpoint a hostile or misconfigured server advertises
/// can never be opened in the browser — and, when the resource was remote, a
/// loopback endpoint the document names is refused too.
///
/// # Errors
///
/// Returns [`OAuthLoginError::InsecureEndpoint`] when the discovered
/// authorization endpoint is neither `https` nor (when allowed) a loopback
/// `http` address.
fn build_authorize_url(
    server: &AuthServer,
    request: &CodeRequest<'_>,
    challenge: &str,
    state: &str,
    scopes: &[String],
) -> Result<String, OAuthLoginError> {
    http::ensure_secure_with(&server.authorization_endpoint, request.allow_loopback_http)?;
    let scope = scopes.join(" ");
    let params = [
        ("response_type", "code"),
        ("client_id", request.client_id),
        ("redirect_uri", request.redirect_uri),
        ("code_challenge", challenge),
        ("code_challenge_method", "S256"),
        ("state", state),
        ("scope", scope.as_str()),
        ("resource", request.resource),
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
    request: &CodeRequest<'_>,
    code: &str,
    verifier: &str,
) -> Result<http::TokenSet, OAuthLoginError> {
    let params = [
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", request.redirect_uri),
        ("client_id", request.client_id),
        ("code_verifier", verifier),
        ("resource", request.resource),
    ];
    let body = http::form_encode(&params);
    let response = http::post(
        agent,
        token_endpoint,
        "application/x-www-form-urlencoded",
        &body,
        request.allow_loopback_http,
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

    fn request(resource: &str, allow_loopback_http: bool) -> CodeRequest<'_> {
        CodeRequest {
            client_id: "client-1",
            redirect_uri: "http://127.0.0.1:1455/auth/callback",
            resource,
            allow_loopback_http,
        }
    }

    #[test]
    fn authorize_url_carries_pkce_and_resource() {
        let url = build_authorize_url(
            &server("https://as.example/authorize"),
            &request("https://example.com/mcp", true),
            "challenge",
            "state-1",
            &["openid".to_string(), "offline_access".to_string()],
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
                &request("https://example.com/mcp", true),
                "challenge",
                "state-1",
                &["openid".to_string()],
            ),
            Err(OAuthLoginError::InsecureEndpoint(_))
        ));
        assert!(
            build_authorize_url(
                &server("https://as.example/authorize"),
                &request("https://example.com/mcp", true),
                "challenge",
                "state-1",
                &["openid".to_string()],
            )
            .is_ok()
        );
    }

    #[test]
    fn authorize_url_refuses_a_loopback_endpoint_under_the_strict_policy() {
        // A remote resource must not be steered onto a local endpoint: under the
        // strict policy a loopback `http` authorize endpoint the metadata named
        // is refused, not opened.
        assert!(matches!(
            build_authorize_url(
                &server("http://127.0.0.1:8080/authorize"),
                &request("https://example.com/mcp", false),
                "challenge",
                "state-1",
                &["openid".to_string()],
            ),
            Err(OAuthLoginError::InsecureEndpoint(_))
        ));
        // The permissive policy (a loopback resource) still accepts it.
        assert!(
            build_authorize_url(
                &server("http://127.0.0.1:8080/authorize"),
                &request("http://127.0.0.1:8080/mcp", true),
                "challenge",
                "state-1",
                &["openid".to_string()],
            )
            .is_ok()
        );
    }
}
