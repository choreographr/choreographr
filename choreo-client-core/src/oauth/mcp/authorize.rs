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
    /// The confidential client's secret, resolved from the configured
    /// environment variable, when one applies.
    pub(super) client_secret: Option<&'a str>,
    /// The bound loopback callback server and its `redirect_uri`.
    pub(super) bound: &'a BoundCallback,
    /// The flow's endpoint policy (discovered endpoints are vetted under it).
    pub(super) policy: http::HopPolicy,
}

/// The per-request inputs both the authorize URL and the token exchange carry:
/// the client registration, the redirect URI, the RFC 8707 resource indicator,
/// and the endpoint policy. Grouped so each builder stays under the argument
/// limit and the two cannot drift on the resource/redirect they send.
struct CodeRequest<'a> {
    client_id: &'a str,
    redirect_uri: &'a str,
    resource: &'a str,
    client_secret: Option<&'a str>,
    policy: http::HopPolicy,
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
        client_secret: ctx.client_secret,
        policy: ctx.policy,
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
    // RFC 9207: validate the authorization-server issuer before the code reaches
    // the token endpoint — a mix-up defence. Required when the server advertises
    // `iss` support; a present `iss` is compared even when support is not
    // advertised (the spec's local-policy provision).
    match callback.iss.as_deref() {
        Some(iss) if iss == ctx.server.issuer => {}
        Some(_) => return Err(OAuthLoginError::IssuerMismatch),
        None if ctx.server.authorization_response_iss_parameter_supported => {
            return Err(OAuthLoginError::IssuerMismatch);
        }
        None => {}
    }
    let _ = tx.send(LoginEvent::Progress {
        message: "Authorization received; exchanging the code for tokens…".to_string(),
    });
    exchange_code(&agent, ctx.server, &request, &callback.code, &verifier)
}

/// How the token endpoint authenticates the client (RFC 6749 §2.3.1).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ClientAuth {
    /// No client credential: a public client sends its `client_id` in the body.
    None,
    /// `client_secret_post`: the id and secret ride the request body.
    Post,
    /// `client_secret_basic`: the id and secret ride an HTTP Basic header.
    Basic,
}

/// Choose the token-endpoint client authentication from the methods the
/// authorization server advertises.
///
/// The rule matches the RFC 6749 §2.3.1 default (and the runtime refresh path,
/// `rmcp`): HTTP Basic unless the server advertises `client_secret_post` and
/// **not** `client_secret_basic`. A server that advertises neither (or the
/// field absent) gets Basic, the OAuth default; a public client — no secret —
/// sends no client credential at all.
fn client_auth_method(server: &AuthServer, has_secret: bool) -> ClientAuth {
    if !has_secret {
        return ClientAuth::None;
    }
    let methods = &server.token_endpoint_auth_methods_supported;
    let has_post = methods.iter().any(|m| m == "client_secret_post");
    let has_basic = methods.iter().any(|m| m == "client_secret_basic");
    if has_post && !has_basic {
        ClientAuth::Post
    } else {
        ClientAuth::Basic
    }
}

/// Build the authorization URL with PKCE S256 and the resource indicator.
///
/// The authorization endpoint (taken from discovered metadata) is vetted with
/// the flow's [`http::HopPolicy`] first, so an `http://` remote endpoint a
/// hostile or misconfigured server advertises can never be opened in the
/// browser — and, when the resource was remote, a loopback or private IP-literal
/// endpoint the document names is refused too.
///
/// # Errors
///
/// Returns [`OAuthLoginError::InsecureEndpoint`] when the discovered
/// authorization endpoint is neither `https` nor (when allowed) a loopback
/// `http` address, or is a private/link-local IP literal under a strict policy.
fn build_authorize_url(
    server: &AuthServer,
    request: &CodeRequest<'_>,
    challenge: &str,
    state: &str,
    scopes: &[String],
) -> Result<String, OAuthLoginError> {
    request.policy.check(&server.authorization_endpoint)?;
    let mut params = vec![
        ("response_type", "code"),
        ("client_id", request.client_id),
        ("redirect_uri", request.redirect_uri),
        ("code_challenge", challenge),
        ("code_challenge_method", "S256"),
        ("state", state),
        ("resource", request.resource),
    ];
    // Request only the resolved scopes; omit the parameter entirely when there
    // are none (per the spec's scope-selection strategy).
    let scope = scopes.join(" ");
    if !scope.is_empty() {
        params.push(("scope", scope.as_str()));
    }
    Ok(http::authorize_url(&server.authorization_endpoint, &params))
}

/// Exchange the authorization code at the token endpoint (form-encoded).
///
/// Routed through the shared [`http::post_with_basic_auth`] so the request
/// carries the same `Accept`, `Cache-Control: no-store`, and redirect policy as
/// the provider flow, and so a confidential client can authenticate with HTTP
/// Basic when the server prefers it.
///
/// # Errors
///
/// Returns [`OAuthLoginError`] on an insecure endpoint, a network failure, a
/// malformed body, or an OAuth error from the authorization server.
fn exchange_code(
    agent: &ureq::Agent,
    server: &AuthServer,
    request: &CodeRequest<'_>,
    code: &str,
    verifier: &str,
) -> Result<http::TokenSet, OAuthLoginError> {
    let auth = client_auth_method(server, request.client_secret.is_some());
    let params = code_exchange_pairs(request, code, verifier, auth);
    let body = http::form_encode(&params);
    // `client_secret_basic` puts the credential in the header; `client_secret_post`
    // already carries it in `params`.
    let basic = match auth {
        ClientAuth::Basic => request
            .client_secret
            .map(|secret| (request.client_id, secret)),
        ClientAuth::None | ClientAuth::Post => None,
    };
    let response = http::post_with_basic_auth(
        agent,
        &server.token_endpoint,
        "application/x-www-form-urlencoded",
        &body,
        request.policy,
        basic,
    )?;
    http::read_token_response(response)
}

/// The form pairs for the authorization-code exchange.
///
/// A `client_secret_basic` request carries the client id in the `Authorization`
/// header (RFC 6749 §2.3.1 forbids two client-auth methods in one request), so
/// its body omits `client_id`; `client_secret_post` puts both id and secret in
/// the body, and a public client sends the id alone. Extracted so the
/// presence/absence of the secret is unit-testable without a network call.
fn code_exchange_pairs<'a>(
    request: &CodeRequest<'a>,
    code: &'a str,
    verifier: &'a str,
    auth: ClientAuth,
) -> Vec<(&'a str, &'a str)> {
    let mut params = vec![
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", request.redirect_uri),
        ("code_verifier", verifier),
        ("resource", request.resource),
    ];
    match auth {
        ClientAuth::Basic => {}
        ClientAuth::Post => {
            params.push(("client_id", request.client_id));
            if let Some(secret) = request.client_secret {
                params.push(("client_secret", secret));
            }
        }
        ClientAuth::None => params.push(("client_id", request.client_id)),
    }
    params
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
            token_endpoint_auth_methods_supported: Vec::new(),
            challenge_scope: None,
            resource_scopes_supported: Vec::new(),
            authorization_response_iss_parameter_supported: false,
        }
    }

    fn request(resource: &str, allow_loopback_http: bool) -> CodeRequest<'_> {
        CodeRequest {
            client_id: "client-1",
            redirect_uri: "http://127.0.0.1:1455/auth/callback",
            resource,
            client_secret: None,
            // A loopback resource keeps both allowances; a remote one keeps
            // neither, which is what the strict-policy tests exercise.
            policy: http::HopPolicy {
                allow_loopback_http,
                allow_private_hosts: allow_loopback_http,
            },
        }
    }

    #[test]
    fn code_exchange_pairs_follow_the_client_auth_method() {
        let mut req = request("https://example.com/mcp", true);
        req.client_secret = Some("s3cret");

        // client_secret_post: id and secret ride the body.
        let post = code_exchange_pairs(&req, "the-code", "the-verifier", ClientAuth::Post);
        assert!(post.contains(&("client_id", "client-1")));
        assert!(post.contains(&("client_secret", "s3cret")));

        // client_secret_basic: neither id nor secret rides the body (they go in
        // the Authorization header).
        let basic = code_exchange_pairs(&req, "the-code", "the-verifier", ClientAuth::Basic);
        assert!(!basic.iter().any(|(k, _)| *k == "client_id"));
        assert!(!basic.iter().any(|(k, _)| *k == "client_secret"));

        // A public client sends its id (no secret) in the body.
        req.client_secret = None;
        let public = code_exchange_pairs(&req, "the-code", "the-verifier", ClientAuth::None);
        assert!(public.contains(&("client_id", "client-1")));
        assert!(!public.iter().any(|(k, _)| *k == "client_secret"));

        // The mandatory pairs are always present.
        assert!(post.contains(&("grant_type", "authorization_code")));
        assert!(post.contains(&("code", "the-code")));
        assert!(post.contains(&("code_verifier", "the-verifier")));
    }

    #[test]
    fn client_auth_method_follows_the_advertised_methods() {
        let mut srv = server("https://as.example/authorize");
        // Nothing advertised: HTTP Basic, the RFC 6749 §2.3.1 default.
        assert_eq!(client_auth_method(&srv, true), ClientAuth::Basic);
        // post-only → Post.
        srv.token_endpoint_auth_methods_supported = vec!["client_secret_post".to_string()];
        assert_eq!(client_auth_method(&srv, true), ClientAuth::Post);
        // Both advertised → Basic (the OAuth default; matches the refresh path).
        srv.token_endpoint_auth_methods_supported = vec![
            "client_secret_post".to_string(),
            "client_secret_basic".to_string(),
        ];
        assert_eq!(client_auth_method(&srv, true), ClientAuth::Basic);
        // basic-only → Basic.
        srv.token_endpoint_auth_methods_supported = vec!["client_secret_basic".to_string()];
        assert_eq!(client_auth_method(&srv, true), ClientAuth::Basic);
        // A public client (no secret) sends no client credential at all.
        assert_eq!(client_auth_method(&srv, false), ClientAuth::None);
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
    fn authorize_url_refuses_a_private_ip_literal_under_the_strict_policy() {
        // A remote resource's document must not steer the browser at an
        // internal https service either: under the strict policy a
        // private/link-local IP-literal endpoint is refused.
        for endpoint in ["https://10.0.0.5/authorize", "https://169.254.169.254/auth"] {
            assert!(
                matches!(
                    build_authorize_url(
                        &server(endpoint),
                        &request("https://example.com/mcp", false),
                        "challenge",
                        "state-1",
                        &["openid".to_string()],
                    ),
                    Err(OAuthLoginError::InsecureEndpoint(_))
                ),
                "{endpoint} must be refused under the strict policy"
            );
        }
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
