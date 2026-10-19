//! Integration tests for the `choreo-mcp` OAuth half: target-scoped credential
//! keying, the daemon-facing connect path, and (driven by an in-repo mock
//! authorization server) connect-time refresh, revocation, and scope handling.
//!
//! Everything here is hermetic: the mock authorization server and MCP endpoint
//! are a single loopback `TcpListener` this module owns, and the credential
//! store is a temp file. No real authorization server is ever contacted.
//!
//! These bind a local socket, so they belong here and are marked `#[ignore]`
//! per the workspace test discipline (`cargo test` runs only unit tests;
//! `cargo test-integration` runs these).

// The helpers below (mock server, backend) use `expect`; clippy's
// allow-*-in-tests config only covers `#[test]`-annotated functions, so a
// file-level expectation is required for the helper fns.
#![expect(clippy::expect_used)]

use crate::common::watchdog;
use choreo_mcp::{
    CredentialBackend, McpAuthWiring, McpError, McpOAuthConfig, McpProtocolMode, McpServer,
    McpServerConfig, McpTransport, RefreshGuard, credential_key_for,
};
use choreo_shared::{OAuthKind, ServiceCredential};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// An HTTP (Streamable HTTP) server config for `slug` at `url`.
fn http_config(slug: &str, url: &str) -> McpServerConfig {
    McpServerConfig {
        slug: slug.to_string(),
        transport: McpTransport::Http {
            url: url.to_string(),
            headers: HashMap::new(),
            oauth: None,
        },
        enabled: true,
        timeout: Some(Duration::from_secs(10)),
        protocol: McpProtocolMode::Auto,
        max_concurrent_calls: None,
        max_restarts: None,
        disabled_tools: Vec::new(),
    }
}

/// A stdio server config (never carries an OAuth credential key).
fn stdio_config(slug: &str) -> McpServerConfig {
    McpServerConfig {
        slug: slug.to_string(),
        transport: McpTransport::Stdio {
            command: "true".to_string(),
            args: Vec::new(),
            env: HashMap::new(),
            cwd: None,
            log_path: None,
        },
        enabled: true,
        timeout: None,
        protocol: McpProtocolMode::Auto,
        max_concurrent_calls: None,
        max_restarts: None,
        disabled_tools: Vec::new(),
    }
}

// ── A file-backed credential backend ────────────────────────────────────

/// A `CredentialBackend` that persists the durable credential to a temp file,
/// so the adapter's round-trip is observable on disk. This is the "real
/// backend" the daemon's fire-and-forget writes eventually land in.
struct FileBackend {
    path: std::path::PathBuf,
    reauth: AtomicBool,
}

impl FileBackend {
    fn new(path: std::path::PathBuf) -> Arc<Self> {
        Arc::new(Self {
            path,
            reauth: AtomicBool::new(false),
        })
    }

    /// Read the persisted credential, if any.
    fn read(path: &std::path::Path) -> Option<ServiceCredential> {
        let bytes = std::fs::read(path).ok()?;
        serde_json::from_slice(&bytes).ok()
    }
}

impl CredentialBackend for FileBackend {
    fn load(&self) -> Result<Option<ServiceCredential>, String> {
        Ok(Self::read(&self.path))
    }

    fn save(
        &self,
        credential: ServiceCredential,
        _expected_refresh_token: Option<String>,
    ) -> Result<(), String> {
        let bytes = serde_json::to_vec(&credential).map_err(|e| e.to_string())?;
        std::fs::write(&self.path, bytes).map_err(|e| e.to_string())
    }

    fn clear(&self) -> Result<(), String> {
        let _ = std::fs::remove_file(&self.path);
        Ok(())
    }

    fn acquire_refresh_guard(&self) -> Result<Option<RefreshGuard>, String> {
        Ok(None)
    }

    fn mark_reauthentication(&self) -> Result<(), String> {
        self.reauth.store(true, Ordering::SeqCst);
        Ok(())
    }
}

/// Seed `path` with a stored MCP OAuth grant for `issuer`/`resource`.
fn seed_mcp_grant(path: &std::path::Path, issuer: &str, resource: &str) {
    let credential = ServiceCredential::OAuth {
        kind: OAuthKind::Mcp,
        issuer: Some(issuer.to_string()),
        resource: Some(resource.to_string()),
        authorization_server: Some(issuer.to_string()),
        token_endpoint: None,
        refresh_encoding: None,
        refresh_token: Some("rt-initial".to_string()),
        access_token: None,
        account_id: None,
        client_id: Some("client-1".to_string()),
        client_secret: None,
        granted_scopes: vec!["openid".to_string()],
        token_type: Some("Bearer".to_string()),
    };
    std::fs::write(
        path,
        serde_json::to_vec(&credential).expect("serialize seed"),
    )
    .expect("seed");
}

// ── The in-repo mock authorization server + MCP endpoint ────────────────

/// How the mock token endpoint answers a refresh.
#[derive(Clone, Copy)]
enum TokenMode {
    /// Mint a fresh access token and rotate the refresh token.
    Rotate,
    /// Reject the refresh token (`invalid_grant`) — a revoked grant.
    InvalidGrant,
}

/// Shared state a mock server connection handler reads.
struct MockState {
    issuer: String,
    valid_token: String,
    token_mode: TokenMode,
    /// The MCP endpoint demands a scope the client never holds (403).
    require_scope: bool,
    token_hits: AtomicUsize,
    /// The `Authorization` header of each POST to the MCP endpoint, in order.
    mcp_auth_headers: Mutex<Vec<String>>,
}

/// A running mock authorization server plus the MCP endpoint on one loopback
/// listener.
struct MockAuthServer {
    issuer: String,
    mcp_url: String,
    state: Arc<MockState>,
    _join: std::thread::JoinHandle<()>,
}

impl MockAuthServer {
    fn start(token_mode: TokenMode, require_scope: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock server");
        let addr = listener.local_addr().expect("mock addr");
        let issuer = format!("http://{addr}");
        let state = Arc::new(MockState {
            issuer: issuer.clone(),
            valid_token: "at-1".to_string(),
            token_mode,
            require_scope,
            token_hits: AtomicUsize::new(0),
            mcp_auth_headers: Mutex::new(Vec::new()),
        });
        let handler_state = Arc::clone(&state);
        let join = std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let state = Arc::clone(&handler_state);
                std::thread::spawn(move || handle_connection(stream, &state));
            }
        });
        Self {
            mcp_url: format!("{issuer}/mcp"),
            issuer,
            state,
            _join: join,
        }
    }

    fn token_hits(&self) -> usize {
        self.state.token_hits.load(Ordering::SeqCst)
    }

    fn mcp_auth_headers(&self) -> Vec<String> {
        self.state.mcp_auth_headers.lock().expect("lock").clone()
    }
}

/// One parsed HTTP/1.1 request.
struct Request {
    method: String,
    path: String,
    headers: HashMap<String, String>,
    body: String,
}

/// Read one HTTP/1.1 request off `stream`.
fn read_request(stream: &TcpStream) -> Option<Request> {
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line).ok()?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_string();
    let path = parts.next()?.to_string();

    let mut headers = HashMap::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 {
            break;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((key, value)) = line.split_once(':') {
            headers.insert(key.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }
    let length: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let mut body = vec![0u8; length];
    if length > 0 {
        reader.read_exact(&mut body).ok()?;
    }
    Some(Request {
        method,
        path,
        headers,
        body: String::from_utf8_lossy(&body).into_owned(),
    })
}

/// Write an HTTP/1.1 response with optional extra headers and close.
fn write_response(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    extra_headers: &str,
    body: &str,
) {
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\n{extra_headers}Content-Length: {len}\r\nConnection: close\r\n\r\n{body}",
        len = body.len()
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

/// Serve one connection: route discovery, token, and MCP traffic by path.
fn handle_connection(mut stream: TcpStream, state: &MockState) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let Some(request) = read_request(&stream) else {
        return;
    };
    match (request.method.as_str(), request.path.as_str()) {
        // RFC 8414 authorization-server metadata at the origin root. The
        // path-inserted and OIDC candidates the client tries first fall through
        // to the 404 arm, so this is the document discovery settles on.
        ("GET", "/.well-known/oauth-authorization-server") => {
            let body = serde_json::json!({
                "issuer": state.issuer,
                "authorization_endpoint": format!("{}/authorize", state.issuer),
                "token_endpoint": format!("{}/token", state.issuer),
                "registration_endpoint": format!("{}/register", state.issuer),
                "scopes_supported": ["openid"],
                "code_challenge_methods_supported": ["S256"],
                "response_types_supported": ["code"],
            })
            .to_string();
            write_response(&mut stream, "200 OK", "application/json", "", &body);
        }
        // An authorization server advertised only at an explicitly-named subpath
        // (the OpenID Connect appended form `{issuer}/.well-known/openid-configuration`)
        // — the shape the `authorizationServer` override exists for. Nothing at
        // the resource's paths leads here, so a connect can reach it only through
        // the override.
        ("GET", "/as/.well-known/openid-configuration") => {
            let body = serde_json::json!({
                "issuer": format!("{}/as", state.issuer),
                "authorization_endpoint": format!("{}/as/authorize", state.issuer),
                "token_endpoint": format!("{}/as/token", state.issuer),
                "registration_endpoint": format!("{}/as/register", state.issuer),
                "scopes_supported": ["openid"],
            })
            .to_string();
            write_response(&mut stream, "200 OK", "application/json", "", &body);
        }
        // Any other GET (the protected-resource probes, the OIDC candidates,
        // the resource endpoint itself) is absent, so discovery moves on.
        ("GET", _) => write_response(&mut stream, "404 Not Found", "text/plain", "", "no"),
        ("POST", "/token") => handle_token(&mut stream, state),
        ("POST", "/as/token") => handle_token(&mut stream, state),
        ("POST", "/mcp") => handle_mcp(&mut stream, state, &request),
        _ => write_response(&mut stream, "404 Not Found", "text/plain", "", "no"),
    }
}

/// Answer a token-endpoint refresh.
fn handle_token(stream: &mut TcpStream, state: &MockState) {
    state.token_hits.fetch_add(1, Ordering::SeqCst);
    match state.token_mode {
        TokenMode::Rotate => {
            let body = serde_json::json!({
                "access_token": state.valid_token,
                "token_type": "Bearer",
                "expires_in": 3600,
                "refresh_token": "rt-rotated",
            })
            .to_string();
            write_response(stream, "200 OK", "application/json", "", &body);
        }
        TokenMode::InvalidGrant => {
            let body = r#"{"error":"invalid_grant","error_description":"revoked"}"#;
            write_response(stream, "400 Bad Request", "application/json", "", body);
        }
    }
}

/// Answer an MCP request, challenging an absent/insufficient token.
fn handle_mcp(stream: &mut TcpStream, state: &MockState, request: &Request) {
    let auth = request
        .headers
        .get("authorization")
        .cloned()
        .unwrap_or_default();
    state
        .mcp_auth_headers
        .lock()
        .expect("lock")
        .push(auth.clone());

    // A server that needs a scope the client does not hold: 403 with a
    // `WWW-Authenticate: Bearer error="insufficient_scope"` challenge.
    if state.require_scope {
        write_response(
            stream,
            "403 Forbidden",
            "application/json",
            "WWW-Authenticate: Bearer error=\"insufficient_scope\", scope=\"admin\"\r\n",
            "",
        );
        return;
    }

    // A missing or wrong bearer gets a 401 challenge, which drives the
    // client's one-shot refresh-and-retry.
    if auth != format!("Bearer {}", state.valid_token) {
        write_response(
            stream,
            "401 Unauthorized",
            "application/json",
            "WWW-Authenticate: Bearer error=\"invalid_token\"\r\n",
            "",
        );
        return;
    }

    let value: serde_json::Value =
        serde_json::from_str(&request.body).unwrap_or(serde_json::Value::Null);
    let method = value.get("method").and_then(|m| m.as_str()).unwrap_or("");
    let id = value.get("id").cloned().unwrap_or(serde_json::Value::Null);
    let result = match method {
        "server/discover" => Some(serde_json::json!({
            "resultType": "complete",
            "supportedVersions": ["2026-07-28"],
            "capabilities": {"tools": {}},
            "ttlMs": 0,
            "cacheScope": "public",
            "_meta": {
                "io.modelcontextprotocol/serverInfo": {
                    "name": "oauth-fixture",
                    "version": "0.1.0"
                }
            }
        })),
        "initialize" => Some(serde_json::json!({
            "protocolVersion": "2025-11-25",
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "oauth-fixture", "version": "0.1.0"}
        })),
        "tools/list" => Some(serde_json::json!({"tools": []})),
        _ => None,
    };
    match result {
        Some(result) => {
            let body =
                serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string();
            write_response(stream, "200 OK", "application/json", "", &body);
        }
        // A notification (no id) expects only an acknowledgement.
        None => write_response(stream, "202 Accepted", "text/plain", "", ""),
    }
}

// ── Tests ────────────────────────────────────────────────────────────────

/// A slug is not globally unique across tiers, so the credential row key is
/// derived from the server's target, not its tier: a daemon-tier `foo` and a
/// project-tier `foo` pointing at different endpoints key distinctly, while two
/// servers at the same endpoint share one key.
#[test]
#[ignore = "integration: exercises the target-scoped key derivation"]
fn shared_slug_keys_by_target_not_tier() {
    // The same shape a daemon-tier `mcp.json` entry and a project-tier
    // `.mcp.json` entry both resolve to — the tier is not a field of the
    // resolved config, so it cannot leak into the key.
    let daemon = http_config("foo", "https://a.example/mcp");
    let project_same_target = http_config("foo", "https://a.example/mcp");
    let project_other_target = http_config("foo", "https://b.example/mcp");

    let daemon_key = credential_key_for(&daemon).expect("daemon key");
    let same_key = credential_key_for(&project_same_target).expect("project key");
    let other_key = credential_key_for(&project_other_target).expect("project key");

    assert!(
        daemon_key.starts_with("mcp:foo|"),
        "the key is namespaced by the human-facing slug: {daemon_key}"
    );
    assert_eq!(
        daemon_key, same_key,
        "the same target keys the same regardless of tier"
    );
    assert_ne!(
        daemon_key, other_key,
        "a different target keys distinctly even at the same slug"
    );
    // A stdio server carries no OAuth credential, so it has no key.
    assert!(credential_key_for(&stdio_config("foo")).is_none());
}

/// The connect path refreshes a cold grant against the mock authorization
/// server, sends the refreshed bearer to the MCP endpoint, and persists the
/// rotated refresh token *without* the access token.
#[test]
#[ignore = "integration: binds a local mock authorization server"]
fn connect_refreshes_and_persists_without_the_access_token() {
    watchdog();
    let mock = MockAuthServer::start(TokenMode::Rotate, false);

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("credential.json");
    seed_mcp_grant(&path, &mock.issuer, &mock.mcp_url);
    let backend = FileBackend::new(path.clone());

    let config = http_config("docs", &mock.mcp_url);
    let wiring = McpAuthWiring {
        key: credential_key_for(&config).expect("key"),
        backend: backend.clone(),
    };

    let server = McpServer::connect_with_auth(&config, None, Some(wiring))
        .expect("connect with OAuth against the mock server");
    assert_eq!(server.name(), "oauth-fixture");

    // The cold start refreshes exactly once, and the refreshed bearer reaches
    // the MCP endpoint on the handshake that succeeds.
    assert_eq!(mock.token_hits(), 1, "a cold start refreshes exactly once");
    assert!(
        mock.mcp_auth_headers().iter().any(|h| h == "Bearer at-1"),
        "the refreshed access token must reach the MCP endpoint: {:?}",
        mock.mcp_auth_headers()
    );

    // The adapter persisted the rotated refresh token and stripped the access
    // token: the durable credential on disk carries the new refresh token and
    // none of the access token.
    let stored = FileBackend::read(&path).expect("credential persisted");
    let view = stored.as_oauth().expect("oauth credential");
    assert_eq!(view.refresh_token, Some("rt-rotated"));
    let raw = std::fs::read_to_string(&path).expect("read raw");
    assert!(
        !raw.contains("at-1"),
        "the access token must never be persisted: {raw}"
    );
}

/// A stored grant whose authorization server is reachable ONLY through the
/// configured `authorizationServer` override refreshes at connect: discovery
/// from the resource URL leads nowhere, so the refresh must resolve the AS from
/// the override, send the refreshed bearer, and persist the rotation.
#[test]
#[ignore = "integration: binds a local mock authorization server"]
fn connect_honors_the_authorization_server_override() {
    watchdog();
    let mock = MockAuthServer::start(TokenMode::Rotate, false);
    let as_url = format!("{}/as", mock.issuer);

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("credential.json");
    // The stored grant names the subpath issuer, so a connect that instead
    // discovered the resource's root AS would withhold it.
    seed_mcp_grant(&path, &as_url, &mock.mcp_url);
    let backend = FileBackend::new(path.clone());

    let mut config = http_config("docs", &mock.mcp_url);
    if let McpTransport::Http { oauth, .. } = &mut config.transport {
        *oauth = Some(McpOAuthConfig {
            authorization_server: Some(as_url.clone()),
            ..McpOAuthConfig::default()
        });
    }
    let wiring = McpAuthWiring {
        key: credential_key_for(&config).expect("key"),
        backend: backend.clone(),
    };

    let server = McpServer::connect_with_auth(&config, None, Some(wiring))
        .expect("connect through the authorizationServer override");
    assert_eq!(server.name(), "oauth-fixture");
    assert_eq!(
        mock.token_hits(),
        1,
        "the cold start refreshes exactly once"
    );
    assert!(
        mock.mcp_auth_headers().iter().any(|h| h == "Bearer at-1"),
        "the refreshed access token must reach the MCP endpoint: {:?}",
        mock.mcp_auth_headers()
    );
    let stored = FileBackend::read(&path).expect("credential persisted");
    assert_eq!(
        stored.as_oauth().expect("oauth credential").refresh_token,
        Some("rt-rotated"),
        "the refresh completed and rotated the durable grant"
    );
}

/// A refresh token the authorization server rejects (`invalid_grant`) is
/// terminal: the connect surfaces an actionable authorization-required error
/// and the backend records that a re-login is needed.
#[test]
#[ignore = "integration: binds a local mock authorization server"]
fn revoked_refresh_token_surfaces_reauth_and_never_sends_a_token() {
    watchdog();
    let mock = MockAuthServer::start(TokenMode::InvalidGrant, false);

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("credential.json");
    seed_mcp_grant(&path, &mock.issuer, &mock.mcp_url);
    let backend = FileBackend::new(path);

    let config = http_config("docs", &mock.mcp_url);
    let wiring = McpAuthWiring {
        key: credential_key_for(&config).expect("key"),
        backend: backend.clone(),
    };

    let err = McpServer::connect_with_auth(&config, None, Some(wiring))
        .err()
        .expect("a revoked grant must fail the connect");
    assert!(
        matches!(err, McpError::AuthRequired { .. }),
        "expected AuthRequired, got: {err:?}"
    );
    assert!(
        backend.reauth.load(Ordering::SeqCst),
        "a rejected refresh token must be recorded for the re-login surface"
    );
    assert_eq!(mock.token_hits(), 1, "the refresh is attempted once");
}

/// A server that challenges for a scope the client does not hold (403
/// `insufficient_scope`) surfaces an actionable authorization-required error
/// rather than an opaque failure.
#[test]
#[ignore = "integration: binds a local mock authorization server"]
fn insufficient_scope_surfaces_auth_required() {
    watchdog();
    let mock = MockAuthServer::start(TokenMode::Rotate, true);

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("credential.json");
    seed_mcp_grant(&path, &mock.issuer, &mock.mcp_url);
    let backend = FileBackend::new(path);

    let config = http_config("docs", &mock.mcp_url);
    let wiring = McpAuthWiring {
        key: credential_key_for(&config).expect("key"),
        backend: backend.clone(),
    };

    let err = McpServer::connect_with_auth(&config, None, Some(wiring))
        .err()
        .expect("an insufficient-scope challenge must fail the connect");
    assert!(
        matches!(err, McpError::AuthRequired { .. }),
        "expected AuthRequired, got: {err:?}"
    );
    // The connect refreshes the access token up front (the cold load serves an
    // already-expired refresh-only grant), so the token endpoint is hit once;
    // the fresh bearer is then rejected with the scope challenge, which is what
    // surfaces the authorization-required error.
    assert_eq!(
        mock.token_hits(),
        1,
        "the cold start refreshes once before the request"
    );
}

/// A stored grant whose issuer no longer matches the authorization server
/// re-discovered at connect is withheld: the connect fails closed and the
/// token never reaches the endpoint.
#[test]
#[ignore = "integration: binds a local mock authorization server"]
fn issuer_mismatch_quarantines_the_stored_grant() {
    watchdog();
    let mock = MockAuthServer::start(TokenMode::Rotate, false);

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("credential.json");
    // The stored issuer names a *different* authorization server than the one
    // this endpoint discovers, so the grant must not be used.
    seed_mcp_grant(&path, "https://previous.example", &mock.mcp_url);
    let backend = FileBackend::new(path);

    let config = http_config("docs", &mock.mcp_url);
    let wiring = McpAuthWiring {
        key: credential_key_for(&config).expect("key"),
        backend: backend.clone(),
    };

    let err = McpServer::connect_with_auth(&config, None, Some(wiring))
        .err()
        .expect("a mismatched issuer must fail closed");
    assert!(
        matches!(err, McpError::AuthRequired { .. }),
        "expected AuthRequired, got: {err:?}"
    );
    // The withheld grant means the token endpoint is never used and no bearer
    // was ever presented to the endpoint.
    assert_eq!(
        mock.token_hits(),
        0,
        "a quarantined grant is never refreshed"
    );
    assert!(
        mock.mcp_auth_headers().iter().all(|h| h != "Bearer at-1"),
        "a quarantined grant must never be sent"
    );
}
