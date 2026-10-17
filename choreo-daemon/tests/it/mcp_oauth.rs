// Only compiled with the `mcp` feature: the config loader and trust store live
// behind it.
#![cfg(feature = "mcp")]

//! Integration coverage for MCP OAuth at the daemon tier: the project-root
//! trust gate on a server's `oauth` block, the guarantee that untrusting a
//! project never deletes a stored credential row, and the interactive sign-in
//! driving RFC 9728/8414 discovery, dynamic client registration, and the PKCE
//! code exchange against an in-repo mock authorization server.
//!
//! These touch the filesystem boundary (project config, trust store) and bind
//! local sockets (the mock authorization server + the loopback callback), so
//! they belong here and are marked `#[ignore = "integration"]` per AGENTS.md.

// The mock-server, login-driver, and config helpers below use `expect`/`panic!`;
// clippy's allow-*-in-tests config only covers `#[test]`-annotated functions, so
// a file-level expectation is required for the helper fns.
#![expect(clippy::expect_used)]
#![expect(clippy::panic)]

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};

use choreo_client_core::oauth::{LoginEvent, McpLoginInfo, McpLoginOptions, start_mcp_login};
use choreo_daemon::daemon::DaemonCommand;
use choreo_daemon::mcp::config::load_project_config;
use choreo_keystore::{OAuthKind, ServiceCredential};
use choreo_mcp::{McpOAuthConfig, McpServerConfig, McpTransport};
use choreo_proto::McpOAuthConfig as WireOAuthConfig;

/// The `oauth` block of an HTTP server config, if any.
fn oauth_of(config: &McpServerConfig) -> Option<&McpOAuthConfig> {
    match &config.transport {
        McpTransport::Http { oauth, .. } => oauth.as_ref(),
        McpTransport::Stdio { .. } => None,
    }
}

/// Write a project `.mcp.json` with one HTTP server carrying an `oauth` block
/// and a `${VAR}`-templated header (the header proves expansion is gated too).
fn write_project_mcp(root: &std::path::Path) {
    std::fs::write(
        root.join(".mcp.json"),
        r#"{"mcpServers":{"docs":{"url":"https://example.com/mcp",
            "headers":{"Authorization":"Bearer ${MCP_PROJECT_TOKEN}"},
            "oauth":{"clientId":"pre-registered","callbackPort":1455,"scopes":["openid"]}}}}"#,
    )
    .expect("write .mcp.json");
}

/// The project trust gate drops an untrusted server's `oauth` block entirely,
/// so it is never expanded, dialed, or offered as a login option; a trusted
/// load resolves it. This is the "cannot be signed into while untrusted" rule.
#[test]
#[ignore = "integration"]
fn untrusted_project_oauth_block_is_not_offered() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    write_project_mcp(root);

    // Trusted: the block is resolved and the templated header is expanded.
    let trusted = load_project_config(root, true)
        .expect("load")
        .expect("present");
    assert_eq!(trusted.len(), 1);
    let oauth =
        oauth_of(&trusted[0].config).expect("a trusted project server keeps its oauth block");
    assert_eq!(oauth.client_id.as_deref(), Some("pre-registered"));
    assert_eq!(oauth.callback_port, Some(1455));
    assert_eq!(oauth.scopes, vec!["openid".to_string()]);

    // Untrusted: the `oauth` block is dropped — the server is read for status
    // only and can never be logged into.
    let untrusted = load_project_config(root, false)
        .expect("load")
        .expect("present");
    assert_eq!(untrusted.len(), 1);
    assert!(
        oauth_of(&untrusted[0].config).is_none(),
        "an untrusted project server must not expose its oauth block"
    );
}

/// A stored MCP credential row survives untrusting its project root: untrust
/// gates *use*, never existence. The row is present before, during, and after
/// the trust flip, so a re-trust needs no fresh login.
#[test]
#[ignore = "integration"]
fn untrusting_a_project_keeps_the_stored_credential_row() {
    use choreo_daemon::mcp::trust::McpTrustStore;

    let mut state = crate::common::test_daemon_state();
    // Bind (TOFU) so the daemon is unlocked and the save is not dropped.
    let key = vec![7u8; 32];
    let (bind_ack, _bind_rx) = std::sync::mpsc::channel();
    state.handle_command(DaemonCommand::BindKeystore {
        key,
        reply: None,
        ack: bind_ack,
    });
    assert!(!state.locked, "bind leaves the keystore unlocked");

    // A signed-in MCP grant stored under its target-scoped key, persisted
    // through the interactive-login path (`SaveCredential`, the realistic
    // first write — `OAuthSave` is reserved for a refresh of an
    // already-stored grant).
    let credential_key = "mcp:docs|abc123".to_string();
    let (save_ack, _save_rx) = std::sync::mpsc::channel();
    state.handle_command(DaemonCommand::SaveCredential {
        service: credential_key.clone(),
        credential: ServiceCredential::OAuth {
            kind: OAuthKind::Mcp,
            issuer: Some("https://as.example".into()),
            resource: Some("https://example.com/mcp".into()),
            authorization_server: Some("https://as.example".into()),
            token_endpoint: None,
            refresh_encoding: None,
            refresh_token: Some("rt".into()),
            account_id: None,
            client_id: Some("client-1".into()),
            client_secret: None,
            granted_scopes: vec!["openid".into()],
            token_type: Some("Bearer".into()),
        },
        reply: None,
        ack: save_ack,
    });
    assert!(
        state.credentials.contains_key(&credential_key),
        "the grant is stored once logged in"
    );

    // The trust store is the sole authority untrust flips; it never touches the
    // keystore, so the row must be untouched.
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let trust_path = root.join("trust.toml");
    let mut trust = McpTrustStore::load(trust_path.clone());
    trust.trust(root).expect("trust");
    assert!(trust.is_trusted(root));

    trust.untrust(root).expect("untrust");
    assert!(!trust.is_trusted(root), "the project is now untrusted");
    assert!(
        state.credentials.contains_key(&credential_key),
        "untrusting must never delete the stored credential row"
    );

    // Re-trusting needs no new login: the same row is still there.
    trust.trust(root).expect("re-trust");
    assert!(trust.is_trusted(root));
    assert!(
        state.credentials.contains_key(&credential_key),
        "the grant is usable again after re-trust with no new login"
    );
}

// ── A mock authorization server ──────────────────────────────────────────

/// One parsed HTTP/1.1 request (the body is captured so the token endpoint can
/// inspect the form it carries).
struct Request {
    method: String,
    path: String,
    body: String,
}

/// Read one HTTP/1.1 request off `stream`.
fn read_request(stream: &TcpStream) -> Option<Request> {
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let mut parts = line.split_whitespace();
    let method = parts.next()?.to_string();
    let path = parts.next()?.to_string();
    let mut length = 0usize;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header).ok()? == 0 {
            break;
        }
        let header = header.trim_end();
        if header.is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':')
            && name.trim().eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse().unwrap_or(0);
        }
    }
    // Drain the body so the response is written against a clean stream.
    let mut body = vec![0u8; length];
    if length > 0 {
        reader.read_exact(&mut body).ok()?;
    }
    Some(Request {
        method,
        path,
        body: String::from_utf8_lossy(&body).into_owned(),
    })
}

/// Write an HTTP/1.1 JSON response and close.
fn write_json(stream: &mut TcpStream, status: &str, body: &str) {
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n{body}",
        len = body.len()
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

/// A running mock authorization server.
struct MockAuthorizationServer {
    origin: String,
    mcp_url: String,
    _join: std::thread::JoinHandle<()>,
}

impl MockAuthorizationServer {
    /// Start the mock. `require_client_secret` makes the token endpoint accept a
    /// code exchange only when it carries `client_secret=right-secret`
    /// (`client_secret_post`) — a confidential-client authorization server
    /// (Shopify Customer Accounts, Entra, Keycloak). `advertise_challenge` makes
    /// the `/mcp` endpoint answer an unauthenticated request with a `401` and a
    /// `WWW-Authenticate: Bearer resource_metadata="…"` challenge, so the sign-in
    /// must discover the authorization server from the challenge.
    fn start(require_client_secret: bool, advertise_challenge: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock AS");
        let origin = format!("http://{}", listener.local_addr().expect("addr"));
        let state = origin.clone();
        let join = std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let origin = state.clone();
                std::thread::spawn(move || {
                    handle(stream, &origin, require_client_secret, advertise_challenge);
                });
            }
        });
        Self {
            mcp_url: format!("{origin}/mcp"),
            origin,
            _join: join,
        }
    }
}

/// Serve one mock-authorization-server request. When `require_client_secret` is
/// set, a token exchange without the expected `client_secret` is rejected
/// (`401 invalid_client`), so a client that cannot authenticate a confidential
/// client fails loudly. When `advertise_challenge` is set, the MCP endpoint
/// answers an unauthenticated request with an RFC 9728 §5.1 challenge.
fn handle(
    mut stream: TcpStream,
    origin: &str,
    require_client_secret: bool,
    advertise_challenge: bool,
) {
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(10)));
    let Some(request) = read_request(&stream) else {
        return;
    };
    match (request.method.as_str(), request.path.as_str()) {
        // The MCP endpoint: an unauthenticated request is challenged with the
        // protected-resource metadata URL it should use.
        ("POST", "/mcp") if advertise_challenge => {
            let response = format!(
                "HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Bearer resource_metadata=\"{origin}/.well-known/oauth-protected-resource\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
        // RFC 9728 protected-resource metadata naming this origin as the AS.
        ("GET", "/.well-known/oauth-protected-resource") => {
            let body = serde_json::json!({
                "resource": format!("{origin}/mcp"),
                "authorization_servers": [origin],
            })
            .to_string();
            write_json(&mut stream, "200 OK", &body);
        }
        // RFC 8414 authorization-server metadata.
        ("GET", "/.well-known/oauth-authorization-server") => {
            let body = serde_json::json!({
                "issuer": origin,
                "authorization_endpoint": format!("{origin}/authorize"),
                "token_endpoint": format!("{origin}/token"),
                "registration_endpoint": format!("{origin}/register"),
                "scopes_supported": ["openid", "offline_access"],
            })
            .to_string();
            write_json(&mut stream, "200 OK", &body);
        }
        // Dynamic client registration (SEP-837); issues a fresh client id.
        ("POST", "/register") => {
            write_json(
                &mut stream,
                "200 OK",
                &serde_json::json!({"client_id": "dcr-client-1"}).to_string(),
            );
        }
        // The PKCE code exchange; returns the durable grant. A confidential
        // mode rejects an exchange that omits the client secret.
        ("POST", "/token") => {
            if require_client_secret && !request.body.contains("client_secret=right-secret") {
                write_json(
                    &mut stream,
                    "401 Unauthorized",
                    &serde_json::json!({"error": "invalid_client"}).to_string(),
                );
                return;
            }
            let body = serde_json::json!({
                "access_token": "access-token-secret",
                "token_type": "Bearer",
                "refresh_token": "rt-mock",
                "scope": "openid",
                "expires_in": 3600,
            })
            .to_string();
            write_json(&mut stream, "200 OK", &body);
        }
        _ => write_json(&mut stream, "404 Not Found", "{}"),
    }
}

/// Drive one interactive MCP sign-in to completion against the mock: wait for
/// the authorize URL (asserting it carries PKCE S256 and the RFC 8707 resource
/// indicator), simulate the browser landing on the loopback callback with the
/// code and matching state, and return the `AddCredential` service key and the
/// credential it carries.
fn drive_login(info: McpLoginInfo) -> (String, ServiceCredential) {
    let session = start_mcp_login(
        info,
        McpLoginOptions {
            no_browser: true,
            callback_url: None,
        },
    );
    let (state, port) = loop {
        let event = session
            .events
            .recv_timeout(std::time::Duration::from_secs(20))
            .unwrap_or_else(|e| panic!("timed out waiting for the authorize URL: {e:?}"));
        match event {
            LoginEvent::AuthUrl { url } => {
                assert!(
                    url.contains("code_challenge_method=S256"),
                    "the authorize URL must carry a PKCE S256 challenge: {url}"
                );
                assert!(
                    url.contains("resource="),
                    "the authorize URL must carry the RFC 8707 resource indicator: {url}"
                );
                let query = url.split_once('?').map_or("", |(_, q)| q);
                let params: HashMap<&str, &str> = query
                    .split('&')
                    .filter_map(|pair| pair.split_once('='))
                    .collect();
                let state = params.get("state").expect("state param").to_string();
                let redirect = params.get("redirect_uri").expect("redirect_uri param");
                let redirect = redirect.replace("%3A", ":").replace("%2F", "/");
                let port = redirect
                    .rsplit_once(':')
                    .and_then(|(_, tail)| tail.split('/').next())
                    .and_then(|p| p.parse::<u16>().ok())
                    .expect("redirect port");
                break (state, port);
            }
            LoginEvent::Error { message } => panic!("login failed: {message}"),
            _ => {}
        }
    };
    {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect callback");
        let request = format!(
            "GET /auth/callback?code=test-code&state={state} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n"
        );
        stream
            .write_all(request.as_bytes())
            .expect("write callback request");
        let mut response = String::new();
        let _ = stream.read_to_string(&mut response);
    }
    let (service, message) = loop {
        let event = session
            .events
            .recv_timeout(std::time::Duration::from_secs(20))
            .unwrap_or_else(|e| panic!("timed out waiting for login completion: {e:?}"));
        match event {
            LoginEvent::Done { service, message } => break (service, message),
            LoginEvent::Error { message } => panic!("login failed: {message}"),
            _ => {}
        }
    };
    session.join();
    let choreo_proto::ClientMessageType::AddCredential { credential, .. } = *message else {
        panic!("expected an AddCredential request");
    };
    (service, credential)
}

/// The interactive MCP sign-in completes against the mock authorization
/// server: it discovers the AS, dynamically registers a client, derives a PKCE
/// S256 challenge, exchanges the code, and produces an `AddCredential` request
/// carrying the durable MCP grant under the daemon-supplied credential key.
#[test]
#[ignore = "integration: binds a local mock authorization server"]
fn mcp_login_discovers_registers_and_exchanges_pkce() {
    let mock = MockAuthorizationServer::start(false, false);
    let credential_key = "mcp:docs|abc123".to_string();
    let info = McpLoginInfo {
        slug: "docs".to_string(),
        resource: mock.mcp_url.clone(),
        credential_key: credential_key.clone(),
        // No pre-registered id, so the flow discovers the registration endpoint
        // and runs dynamic client registration. Port 0 binds an ephemeral
        // loopback port (the redirect_uri carries the real port).
        oauth: WireOAuthConfig {
            client_id: None,
            callback_port: Some(0),
            scopes: Vec::new(),
            authorization_server: None,
            client_secret_env: None,
        },
    };
    let (service, credential) = drive_login(info);
    assert_eq!(
        service, credential_key,
        "the grant keys under the daemon key"
    );
    let view = credential.as_oauth().expect("an OAuth credential");
    assert_eq!(view.kind, OAuthKind::Mcp);
    assert_eq!(
        view.issuer,
        Some(mock.origin.as_str()),
        "the discovered issuer is recorded"
    );
    assert_eq!(view.resource, Some(mock.mcp_url.as_str()));
    assert_eq!(view.refresh_token, Some("rt-mock"));
    assert_eq!(
        view.client_id,
        Some("dcr-client-1"),
        "the dynamically registered client id is recorded"
    );
    assert!(
        view.client_secret.is_none(),
        "a public client stores no secret"
    );
    // The access token is never part of the durable credential.
    assert!(
        !format!("{credential:?}").contains("access-token-secret"),
        "the access token must not be stored"
    );
}

/// A confidential-client sign-in: the authorization server is named explicitly
/// (no protected-resource probing), the client is pre-registered (no dynamic
/// registration), and the token endpoint accepts the exchange only when it
/// carries the secret from the configured environment variable — which is sent
/// (`client_secret_post`) and stored in the durable grant.
#[test]
#[ignore = "integration: binds a local mock authorization server"]
fn mcp_login_confidential_client_sends_secret() {
    let mock = MockAuthorizationServer::start(true, false);
    let credential_key = "mcp:shopify|abc123".to_string();
    // SAFETY: nextest runs each test in its own process, and this is the only
    // setter of this variable, so no other test observes the mutation.
    unsafe { std::env::set_var("CHOREO_MCP_TEST_CLIENT_SECRET", "right-secret") };
    let info = McpLoginInfo {
        slug: "shopify".to_string(),
        resource: mock.mcp_url.clone(),
        credential_key: credential_key.clone(),
        oauth: WireOAuthConfig {
            // A pre-registered client id (no dynamic registration) and an
            // explicit authorization-server issuer (no protected-resource
            // probing) — the Shopify Customer Accounts shape.
            client_id: Some("app-1".to_string()),
            callback_port: Some(0),
            scopes: vec!["customer-account-mcp-api:full".to_string()],
            authorization_server: Some(mock.origin.clone()),
            client_secret_env: Some("CHOREO_MCP_TEST_CLIENT_SECRET".to_string()),
        },
    };
    let (service, credential) = drive_login(info);
    assert_eq!(service, credential_key);
    let view = credential.as_oauth().expect("an OAuth credential");
    assert_eq!(view.kind, OAuthKind::Mcp);
    assert_eq!(
        view.issuer,
        Some(mock.origin.as_str()),
        "the explicit authorization-server issuer is recorded"
    );
    assert_eq!(
        view.client_id,
        Some("app-1"),
        "the pre-registered client id is recorded"
    );
    assert_eq!(
        view.client_secret,
        Some("right-secret"),
        "the confidential client's secret is sent and stored for refresh"
    );
    assert_eq!(view.refresh_token, Some("rt-mock"));
}

/// Discovery from the `WWW-Authenticate` challenge (RFC 9728 §5.1): the MCP
/// endpoint answers the unauthenticated probe with a `401` naming the
/// protected-resource metadata URL, and the flow completes with neither a
/// configured `authorizationServer` nor a well-known-path guess for the
/// resource.
#[test]
#[ignore = "integration: binds a local mock authorization server"]
fn mcp_login_discovers_via_www_authenticate_challenge() {
    let mock = MockAuthorizationServer::start(false, true);
    let credential_key = "mcp:linear|abc123".to_string();
    let info = McpLoginInfo {
        slug: "linear".to_string(),
        resource: mock.mcp_url.clone(),
        credential_key: credential_key.clone(),
        oauth: WireOAuthConfig {
            client_id: None,
            callback_port: Some(0),
            scopes: Vec::new(),
            authorization_server: None,
            client_secret_env: None,
        },
    };
    let (service, credential) = drive_login(info);
    assert_eq!(service, credential_key);
    let view = credential.as_oauth().expect("an OAuth credential");
    assert_eq!(view.kind, OAuthKind::Mcp);
    assert_eq!(
        view.issuer,
        Some(mock.origin.as_str()),
        "the AS named by the challenge is discovered and recorded"
    );
    assert_eq!(view.client_id, Some("dcr-client-1"));
}
