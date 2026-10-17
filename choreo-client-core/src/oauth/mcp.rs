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

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use choreo_keystore::{OAuthKind, ServiceCredential};
use choreo_proto::McpOAuthConfig;
use crossbeam_channel::Sender;
use serde_json::{Value, json};
use tracing::warn;

use super::{LoginEvent, LoginSession, OAuthLoginError, callback, pkce};
use crate::credentials::build_add_credential_from_credential;

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
    let (tx, rx) = crossbeam_channel::unbounded();
    let (stop, stop_rx) = crossbeam_channel::unbounded();
    let _ = &stop_rx; // MCP has no device-code wait; the stop channel is unused.
    let cancel = Arc::new(AtomicBool::new(false));
    let cancel_worker = Arc::clone(&cancel);
    let worker_tx = tx.clone();
    let handle = std::thread::Builder::new()
        .name("mcp-oauth-login".to_string())
        .spawn(move || {
            if let Err(error) = run(&info, &options, &worker_tx, &cancel_worker) {
                let message = error.to_string();
                warn!("mcp oauth login failed: {message}");
                let _ = worker_tx.send(LoginEvent::Error { message });
            }
        });
    let handle = match handle {
        Ok(handle) => Some(handle),
        Err(e) => {
            let _ = tx.send(LoginEvent::Error {
                message: format!("could not start the login worker: {e}"),
            });
            None
        }
    };
    LoginSession {
        events: rx,
        cancel,
        stop,
        handle,
    }
}

/// The worker entry point.
fn run(
    info: &McpLoginInfo,
    options: &McpLoginOptions,
    tx: &Sender<LoginEvent>,
    cancel: &AtomicBool,
) -> Result<(), OAuthLoginError> {
    let agent = agent();
    let _ = tx.send(LoginEvent::Progress {
        message: "Discovering the authorization server…".to_string(),
    });
    let server = discover(&agent, &info.resource)?;
    // Bind the loopback callback FIRST so the redirect URI is known, then use
    // that exact URI for both dynamic registration and the authorize request.
    // A server that validates `redirect_uri` against the registered value must
    // see the same string, including the port actually bound (which may be the
    // fallback, not the preferred one).
    let ports = preferred_ports(info.oauth.callback_port);
    let (callback_server, port) = callback::bind(&ports)?;
    let bound = BoundCallback {
        server: callback_server,
        redirect_uri: format!("http://127.0.0.1:{port}{}", callback::CALLBACK_PATH),
    };
    let client_id = match info.oauth.client_id.clone() {
        Some(id) => id,
        None => register_client(&agent, &server, &bound.redirect_uri, tx)?,
    };
    let tokens = authorize(info, options, &server, &client_id, &bound, tx, cancel)?;
    finish(info, &server, &client_id, &tokens, tx)?;
    Ok(())
}

/// The bound loopback callback server and the redirect URI it answers on, so
/// the authorize path takes one argument instead of two.
struct BoundCallback {
    server: tiny_http::Server,
    redirect_uri: String,
}

/// One discovered authorization server.
struct AuthServer {
    /// The authorization-server issuer (validated against the metadata).
    issuer: String,
    /// The authorization endpoint.
    authorization_endpoint: String,
    /// The token endpoint.
    token_endpoint: String,
    /// The dynamic client registration endpoint, when the server advertises one.
    registration_endpoint: Option<String>,
    /// The scopes the server advertises support for.
    scopes_supported: Vec<String>,
}

/// A ureq agent: JSON bodies, a bounded timeout, and no redirect following.
fn agent() -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(30)))
        .http_status_as_error(false)
        .max_redirects(0)
        .build();
    ureq::Agent::new_with_config(config)
}

/// Fetch and parse an `https` (or loopback `http`) JSON document.
fn get_json(agent: &ureq::Agent, url: &str) -> Result<Value, OAuthLoginError> {
    super::http::ensure_secure(url)?;
    let response = agent
        .get(url)
        .header("Accept", "application/json")
        .call()
        .map_err(|e| OAuthLoginError::Network(e.to_string()))?;
    response
        .into_body()
        .read_json()
        .map_err(|e| OAuthLoginError::Decode(format!("{url}: {e}")))
}

/// The `scheme://host[:port]` origin of `url`, for the well-known paths.
fn origin_of(url: &str) -> Result<String, OAuthLoginError> {
    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| OAuthLoginError::Decode(format!("not an absolute URL: {url}")))?;
    let authority = rest.split('/').next().unwrap_or(rest);
    Ok(format!("{scheme}://{authority}"))
}

/// The path of `url` (everything after the authority), or `/`.
fn path_of(url: &str) -> String {
    let Some((_, rest)) = url.split_once("://") else {
        return "/".to_string();
    };
    rest.split_once('/')
        .map_or_else(|| "/".to_string(), |(_, path)| format!("/{path}"))
}

/// Discover the authorization server from the server URL (RFC 9728 → 8414).
fn discover(agent: &ureq::Agent, resource: &str) -> Result<AuthServer, OAuthLoginError> {
    let origin = origin_of(resource)?;
    // RFC 9728 protected-resource metadata: try the origin-rooted document,
    // then the path-suffixed form.
    let path = path_of(resource);
    let mut candidates = vec![format!("{origin}/.well-known/oauth-protected-resource")];
    if path != "/" {
        candidates.push(format!(
            "{origin}/.well-known/oauth-protected-resource{}",
            path.trim_end_matches('/')
        ));
    }
    let mut resource_metadata = None;
    for url in &candidates {
        if let Ok(value) = get_json(agent, url) {
            resource_metadata = Some(value);
            break;
        }
    }
    let authorization_server = resource_metadata
        .as_ref()
        .and_then(|v| v.get("authorization_servers"))
        .and_then(|v| v.as_array())
        .and_then(|servers| servers.first())
        .and_then(|v| v.as_str())
        .map(str::to_string)
        // No protected-resource metadata: assume the resource host is the AS.
        .unwrap_or(origin);
    fetch_as_metadata(agent, &authorization_server)
}

/// Fetch RFC 8414 / OIDC authorization-server metadata.
fn fetch_as_metadata(
    agent: &ureq::Agent,
    authorization_server: &str,
) -> Result<AuthServer, OAuthLoginError> {
    let base = authorization_server.trim_end_matches('/');
    let url = format!("{base}/.well-known/oauth-authorization-server");
    let oidc = format!("{base}/.well-known/openid-configuration");
    let metadata = get_json(agent, &url)
        .or_else(|_| get_json(agent, &oidc))
        .map_err(|e| {
            OAuthLoginError::Decode(format!(
                "authorization-server metadata discovery failed for {authorization_server}: {e}"
            ))
        })?;
    let string = |key: &str| {
        metadata
            .get(key)
            .and_then(|v| v.as_str())
            .map(str::to_string)
    };
    let authorization_endpoint = string("authorization_endpoint").ok_or_else(|| {
        OAuthLoginError::Decode("metadata has no authorization_endpoint".to_string())
    })?;
    let token_endpoint = string("token_endpoint")
        .ok_or_else(|| OAuthLoginError::Decode("metadata has no token_endpoint".to_string()))?;
    let issuer = string("issuer").unwrap_or_else(|| authorization_server.to_string());
    let scopes_supported = metadata
        .get("scopes_supported")
        .and_then(|v| v.as_array())
        .map_or_else(Vec::new, |a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        });
    Ok(AuthServer {
        issuer,
        authorization_endpoint,
        token_endpoint,
        registration_endpoint: string("registration_endpoint"),
        scopes_supported,
    })
}

/// Dynamically register a client (SEP-837), returning its `client_id`.
///
/// `redirect_uri` is the URI the flow will actually use (the callback is already
/// bound), so the registered `redirect_uris` matches the authorize request
/// exactly.
fn register_client(
    agent: &ureq::Agent,
    server: &AuthServer,
    redirect_uri: &str,
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
    super::http::ensure_secure(endpoint)?;
    let body = json!({
        "client_name": "Choreographr",
        "redirect_uris": [redirect_uri],
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": "none",
        "scope": default_scopes(server, &[]).join(" "),
    });
    let response = agent
        .post(endpoint)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
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
fn default_scopes(server: &AuthServer, configured: &[String]) -> Vec<String> {
    if !configured.is_empty() {
        return configured.to_vec();
    }
    if server.scopes_supported.is_empty() {
        vec!["openid".to_string()]
    } else {
        server.scopes_supported.clone()
    }
}

/// Run the PKCE loopback (or paste-URL) authorization-code flow over the
/// already-bound callback server and its `redirect_uri`.
fn authorize(
    info: &McpLoginInfo,
    options: &McpLoginOptions,
    server: &AuthServer,
    client_id: &str,
    bound: &BoundCallback,
    tx: &Sender<LoginEvent>,
    cancel: &AtomicBool,
) -> Result<super::http::TokenSet, OAuthLoginError> {
    let agent = agent();
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
    );
    let _ = tx.send(LoginEvent::AuthUrl {
        url: auth_url.clone(),
    });

    let callback = if let Some(callback_url) = options.callback_url.as_deref() {
        super::parse_callback_url(callback_url)?
    } else {
        if options.no_browser {
            let _ = tx.send(LoginEvent::Prompt {
                message: format!("Open this URL to sign in: {auth_url}"),
            });
        } else {
            match webbrowser::open(&auth_url) {
                Ok(()) => {
                    let _ = tx.send(LoginEvent::Progress {
                        message: "Opened the sign-in page in your browser.".to_string(),
                    });
                }
                Err(e) => {
                    let _ = tx.send(LoginEvent::Prompt {
                        message: format!("Open this URL to sign in: {auth_url} ({e})"),
                    });
                }
            }
        }
        callback::wait_for_callback(&bound.server, cancel)?
    };

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

/// Build the authorization URL with PKCE S256 and the resource indicator.
fn build_authorize_url(
    server: &AuthServer,
    client_id: &str,
    redirect_uri: &str,
    challenge: &str,
    state: &str,
    scopes: &[String],
    resource: &str,
) -> String {
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
    let query = super::http::form_encode(&params);
    let sep = if server.authorization_endpoint.contains('?') {
        '&'
    } else {
        '?'
    };
    format!("{}{sep}{query}", server.authorization_endpoint)
}

/// Exchange the authorization code at the token endpoint (form-encoded).
fn exchange_code(
    agent: &ureq::Agent,
    token_endpoint: &str,
    client_id: &str,
    code: &str,
    redirect_uri: &str,
    verifier: &str,
    resource: &str,
) -> Result<super::http::TokenSet, OAuthLoginError> {
    super::http::ensure_secure(token_endpoint)?;
    let params = vec![
        ("grant_type".to_string(), "authorization_code".to_string()),
        ("code".to_string(), code.to_string()),
        ("redirect_uri".to_string(), redirect_uri.to_string()),
        ("client_id".to_string(), client_id.to_string()),
        ("code_verifier".to_string(), verifier.to_string()),
        ("resource".to_string(), resource.to_string()),
    ];
    let body = super::http::form_encode(&params);
    let response = agent
        .post(token_endpoint)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .send(body.as_bytes())
        .map_err(|e| OAuthLoginError::Network(e.to_string()))?;
    super::http::read_token_response(response)
}

/// Build the credential from the exchanged tokens and emit `Done`.
fn finish(
    info: &McpLoginInfo,
    server: &AuthServer,
    client_id: &str,
    tokens: &super::http::TokenSet,
    tx: &Sender<LoginEvent>,
) -> Result<(), OAuthLoginError> {
    let granted_scopes = tokens.scope.as_deref().map_or_else(
        || info.oauth.scopes.clone(),
        |s| s.split_whitespace().map(str::to_string).collect(),
    );
    let credential = ServiceCredential::OAuth {
        kind: OAuthKind::Mcp,
        issuer: Some(server.issuer.clone()),
        resource: Some(info.resource.clone()),
        authorization_server: Some(server.issuer.clone()),
        refresh_token: tokens.refresh_token.clone(),
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
    fn origin_and_path_split_absolute_urls() {
        assert_eq!(
            origin_of("https://example.com/mcp").unwrap(),
            "https://example.com"
        );
        assert_eq!(
            origin_of("https://example.com:8443/a/b").unwrap(),
            "https://example.com:8443"
        );
        assert_eq!(path_of("https://example.com/mcp"), "/mcp");
        assert_eq!(path_of("https://example.com"), "/");
        assert!(origin_of("not-a-url").is_err());
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
        );
        assert!(url.starts_with("https://as.example/authorize?"));
        assert!(url.contains("code_challenge=challenge"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("state=state-1"));
        assert!(url.contains("resource=https%3A%2F%2Fexample.com%2Fmcp"));
        assert!(url.contains("scope=openid%20offline_access"));
    }

    #[test]
    fn preferred_ports_lead_with_the_configured_port() {
        assert_eq!(preferred_ports(Some(9000)), vec![9000, 1455, 1457]);
        assert_eq!(preferred_ports(Some(1457)), vec![1457, 1455]);
        assert_eq!(preferred_ports(None), vec![1455, 1457]);
    }
}
