//! Integration tests for the provider OAuth login library.
//!
//! These drive the real loopback callback server and the RFC 8628 device-code
//! poller against an **in-repo mock authorization server** — a local TCP
//! listener that serves the authorize/token/device endpoints — so no test ever
//! contacts a real provider (`auth.openai.com`). Marked `#[ignore]` per the
//! crate's integration-test convention; run with
//! `cargo nextest run -p choreo-client-core --run-ignored only`.
#![expect(clippy::expect_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use choreo_client_core::oauth::{LoginEvent, LoginOptions, start_login};
use choreo_proto::{
    ClientMessageType, OAuthClientIdentity, OAuthGrantEncoding, OAuthProviderDescriptor,
};
use serde_json::json;

/// The success/refresh tokens the mock token endpoint returns, with an `id_token`
/// JWT carrying the `ChatGPT` account-id claim.
fn token_response() -> String {
    let claims = json!({
        "https://api.openai.com/auth": {
            "chatgpt_account_id": "acct_mock_1",
            "chatgpt_plan_type": "plus",
        },
        "https://api.openai.com/profile": { "email": "mock@example.com" },
    });
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"none","typ":"JWT"}"#);
    let payload = URL_SAFE_NO_PAD.encode(claims.to_string().as_bytes());
    let id_token = format!("{header}.{payload}.sig");
    json!({
        "access_token": "access-mock",
        "refresh_token": "refresh-mock",
        "id_token": id_token,
        "token_type": "Bearer",
        "scope": "openid offline_access",
    })
    .to_string()
}

/// A local mock authorization server: `/authorize`, `/token`, and the two
/// device endpoints. It runs on its own thread and never contacts the network
/// beyond loopback.
struct MockServer {
    issuer: String,
    device_polls: Arc<AtomicUsize>,
}

impl MockServer {
    fn start() -> Self {
        let server = tiny_http::Server::http("127.0.0.1:0").expect("bind mock server");
        let port = server.server_addr().to_ip().expect("mock ip addr").port();
        let issuer = format!("http://127.0.0.1:{port}");
        let device_polls = Arc::new(AtomicUsize::new(0));
        let polls = Arc::clone(&device_polls);
        std::thread::spawn(move || {
            loop {
                match server.recv_timeout(Duration::from_secs(30)) {
                    Ok(Some(request)) => handle(request, &polls),
                    Ok(None) => {}
                    Err(_) => break,
                }
            }
        });
        Self {
            issuer,
            device_polls,
        }
    }
}

/// Route one mock request. The authorize handler plays the browser: after
/// reading the `redirect_uri`/`state`, it issues a GET to the library's
/// loopback callback carrying a `code`, which is what unblocks the flow.
fn handle(request: tiny_http::Request, device_polls: &AtomicUsize) {
    let url = request.url().to_string();
    let (path, query) = url
        .split_once('?')
        .map_or((url.as_str(), ""), |(p, q)| (p, q));
    let response = match path {
        "/authorize" => {
            let mut redirect_uri = None;
            let mut state = None;
            for pair in query.split('&') {
                if let Some((key, value)) = pair.split_once('=') {
                    match key {
                        "redirect_uri" => redirect_uri = Some(percent_decode(value)),
                        "state" => state = Some(percent_decode(value)),
                        _ => {}
                    }
                }
            }
            if let (Some(redirect_uri), Some(state)) = (redirect_uri, state) {
                let callback = format!("{redirect_uri}?code=mock-code&state={state}");
                // Act as the browser following the redirect.
                let _ = ureq::get(&callback).call();
            }
            tiny_http::Response::from_string("authorized").with_status_code(200)
        }
        "/token" => tiny_http::Response::from_string(token_response()).with_status_code(200),
        "/deviceauth/usercode" => tiny_http::Response::from_string(
            json!({
                "device_code": "device-code-1",
                "user_code": "ABCD-EFGH",
                "verification_uri": "http://127.0.0.1/device",
                "interval": 1,
            })
            .to_string(),
        )
        .with_status_code(200),
        "/deviceauth/token" => {
            let poll = device_polls.fetch_add(1, Ordering::SeqCst);
            if poll == 0 {
                tiny_http::Response::from_string(
                    json!({ "error": "authorization_pending" }).to_string(),
                )
                .with_status_code(200)
            } else {
                tiny_http::Response::from_string(token_response()).with_status_code(200)
            }
        }
        _ => tiny_http::Response::from_string("not found").with_status_code(404),
    };
    let _ = request.respond(response);
}

/// Percent-decode a query value, mapping `+` to a space.
fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let byte = bytes.get(i).copied().unwrap_or(0);
        match byte {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hi = bytes.get(i + 1).and_then(|b| hex(*b));
                let lo = bytes.get(i + 2).and_then(|b| hex(*b));
                if let (Some(hi), Some(lo)) = (hi, lo) {
                    out.push((hi << 4) | lo);
                    i += 3;
                } else {
                    out.push(b'%');
                    i += 1;
                }
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The hex value of one ASCII digit, or `None`.
fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// A loopback-capable descriptor pointing at `issuer`.
fn descriptor(issuer: &str, device: bool) -> OAuthProviderDescriptor {
    OAuthProviderDescriptor {
        provider: "mock".into(),
        issuer: issuer.to_string(),
        authorize_path: "/authorize".into(),
        token_path: "/token".into(),
        client_id: Some("client-abc".into()),
        scopes: vec!["openid".into(), "offline_access".into()],
        redirect_ports: vec![0],
        code_exchange_encoding: OAuthGrantEncoding::Form,
        refresh_encoding: OAuthGrantEncoding::Json,
        client_identity: OAuthClientIdentity::Registered,
        device_code: device,
        device_authorization_path: device.then(|| "/deviceauth/usercode".to_string()),
        device_token_path: device.then(|| "/deviceauth/token".to_string()),
    }
}

#[test]
#[ignore = "integration: binds loopback sockets and drives a mock HTTP server"]
fn loopback_flow_exchanges_and_builds_a_provider_credential() {
    let mock = MockServer::start();
    let session = start_login(
        descriptor(&mock.issuer, false),
        LoginOptions {
            account: "mock-account".into(),
            no_browser: true,
            callback_url: None,
            device: false,
        },
    );
    // Read until the authorize URL arrives, then act as the browser by hitting
    // the mock authorize endpoint (which redirects back to the callback).
    let mut opened = false;
    for event in &session.events {
        match event {
            LoginEvent::AuthUrl { url } => {
                assert!(
                    url.starts_with(&mock.issuer),
                    "authorize URL uses the issuer"
                );
                assert!(url.contains("code_challenge_method=S256"));
                assert!(url.contains("state="));
                ureq::get(&url).call().expect("get authorize URL");
                opened = true;
            }
            LoginEvent::Done { message, .. } => {
                let credential = match *message {
                    ClientMessageType::AddCredential { credential, .. } => credential,
                    other => panic!("expected AddCredential, got {other:?}"),
                };
                assert!(opened, "authorize URL must arrive before done");
                let oauth = credential.as_oauth().expect("OAuth credential");
                assert_eq!(oauth.kind, choreo_keystore::OAuthKind::Provider);
                assert_eq!(oauth.refresh_token, Some("refresh-mock"));
                assert_eq!(oauth.account_id, Some("acct_mock_1"));
                assert_eq!(oauth.client_id, Some("client-abc"));
                assert_eq!(oauth.granted_scopes, ["openid", "offline_access"]);
                return;
            }
            LoginEvent::Error { message } => panic!("login failed: {message}"),
            _ => {}
        }
    }
    panic!("event stream ended without a terminal event");
}

#[test]
#[ignore = "integration: binds loopback sockets and drives a mock HTTP server"]
fn device_flow_polls_until_authorized() {
    let mock = MockServer::start();
    let session = start_login(
        descriptor(&mock.issuer, true),
        LoginOptions {
            account: "mock-account".into(),
            no_browser: true,
            callback_url: None,
            device: true,
        },
    );
    let mut saw_device_code = false;
    for event in &session.events {
        match event {
            LoginEvent::DeviceCode { code, uri } => {
                assert_eq!(code, "ABCD-EFGH");
                assert_eq!(uri, "http://127.0.0.1/device");
                saw_device_code = true;
            }
            LoginEvent::Done { message, .. } => {
                assert!(saw_device_code, "device code must arrive before done");
                let credential = match *message {
                    ClientMessageType::AddCredential { credential, .. } => credential,
                    other => panic!("expected AddCredential, got {other:?}"),
                };
                let oauth = credential.as_oauth().expect("OAuth credential");
                assert_eq!(oauth.refresh_token, Some("refresh-mock"));
                // The mock reports one pending poll before the tokens.
                assert!(mock.device_polls.load(Ordering::SeqCst) >= 2);
                return;
            }
            LoginEvent::Error { message } => panic!("login failed: {message}"),
            _ => {}
        }
    }
    panic!("event stream ended without a terminal event");
}

#[test]
#[ignore = "integration: binds loopback sockets and drives a mock HTTP server"]
fn cancelling_the_loopback_flow_stops_the_wait() {
    let mock = MockServer::start();
    let session = start_login(
        descriptor(&mock.issuer, false),
        LoginOptions {
            account: "mock-account".into(),
            no_browser: true,
            callback_url: None,
            device: false,
        },
    );
    // Wait for the authorize URL (proof the callback server is up), then
    // cancel; the accept loop must observe the flag and end the flow.
    let url = session
        .events
        .recv_timeout(Duration::from_secs(5))
        .expect("an event");
    assert!(matches!(url, LoginEvent::AuthUrl { .. }));
    session.cancel();
    // Drain the informational events the no-browser path emits before the
    // terminal cancellation surfaces.
    loop {
        let event = session
            .events
            .recv_timeout(Duration::from_secs(5))
            .expect("a terminal event after cancel");
        match event {
            LoginEvent::Error { message } => {
                assert!(message.contains("cancelled"), "{message}");
                return;
            }
            LoginEvent::Done { .. } => panic!("a cancelled flow must not finish"),
            _ => {}
        }
    }
}
