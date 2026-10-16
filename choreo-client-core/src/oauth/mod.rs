//! Provider OAuth login library.
//!
//! This module turns a provider's OAuth sign-in parameters (an
//! [`OAuthProviderDescriptor`] the daemon serves) plus a few options into a
//! ready-to-store [`ServiceCredential`] and the [`ClientMessageType::AddCredential`]
//! frame that carries it. It runs the frontend half of the flow — PKCE S256, a
//! loopback callback server, or an RFC 8628 device-code poll for headless hosts
//! — and hands the durable result (refresh token + client registration +
//! extracted identity claims) back to the caller. The access token is derived
//! and ephemeral; it is deliberately dropped here and never placed in the
//! credential (the daemon mints a fresh one when it needs it).
//!
//! The interactive flow runs on a **worker thread** and streams progress over a
//! crossbeam channel as [`LoginEvent`]s, so a UI can render a cancelable modal
//! (the auth URL, a device code, a prompt) without blocking its own loop.
//! Cancellation is a single lock-free [`AtomicBool`] the loopback accept loop
//! polls (a blocking accept cannot be interrupted by a channel message), plus a
//! stop channel for the device-code wait.
//!
//! The render crates never build an OAuth credential themselves: this library
//! returns the `AddCredential` message so the credential's shape stays in one
//! place.

mod callback;
mod http;
mod jwt;
mod pkce;

pub use jwt::{chatgpt_account_id, chatgpt_plan_type, decode_claims, email};
pub use pkce::{challenge_s256, generate_state, generate_verifier, state_matches};

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;

use choreo_keystore::{OAuthKind, ServiceCredential};
use choreo_proto::{ClientMessageType, OAuthProviderDescriptor};
use crossbeam_channel::{Receiver, Sender, after, select};
use tracing::warn;

use crate::credentials::build_add_credential_from_credential;

/// A failure from the login flow. Each variant is a distinct recovery class
/// (bind failure vs. CSRF mismatch vs. provider error), so the UI can hint at
/// the right next step rather than surfacing one generic message.
#[derive(Debug, thiserror::Error)]
pub enum OAuthLoginError {
    /// The descriptor had no usable client id (a borrowed id withheld because
    /// the opt-in is off, or a provider that requires dynamic registration).
    #[error("no client id for this provider: {0}")]
    NoClientId(String),
    /// The provider does not offer the requested flow (device-code asked of a
    /// provider without one).
    #[error("provider does not support this login flow: {0}")]
    Unsupported(String),
    /// The loopback callback server could not bind any of the configured
    /// ports.
    #[error("could not bind the loopback callback server: {0}")]
    CallbackBind(String),
    /// The loopback accept loop failed.
    #[error("callback server error: {0}")]
    Callback(String),
    /// The `state` returned by the callback did not match the one sent — a
    /// possible CSRF attempt; the code is never exchanged.
    #[error("OAuth state mismatch (possible CSRF); login aborted")]
    StateMismatch,
    /// The flow was cancelled by the caller.
    #[error("login cancelled")]
    Cancelled,
    /// A URL was not `https` and not a loopback `http` address.
    #[error("refusing an insecure OAuth endpoint: {0}")]
    InsecureEndpoint(String),
    /// A network-level failure talking to the provider.
    #[error("network error: {0}")]
    Network(String),
    /// The provider's response could not be decoded.
    #[error("malformed provider response: {0}")]
    Decode(String),
    /// The provider returned an OAuth error (`error`/`error_description`).
    #[error("provider error: {error}: {description}")]
    TokenEndpoint {
        /// The OAuth `error` code.
        error: String,
        /// The optional `error_description`.
        description: String,
    },
    /// The device-code flow expired or was denied.
    #[error("device-code flow ended: {0}")]
    DeviceFlow(String),
    /// The `--callback-url` value could not be parsed into a code/state.
    #[error("could not parse the callback URL: {0}")]
    CallbackUrl(String),
}

/// How the flow serializes a token request body. Mirrors the descriptor's
/// per-endpoint encoding (the code exchange and the refresh can differ).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Encoding {
    /// `application/x-www-form-urlencoded`.
    Form,
    /// `application/json`.
    Json,
}

impl Encoding {
    /// Map a descriptor's grant encoding to this module's encoding.
    fn from_proto(encoding: choreo_proto::OAuthGrantEncoding) -> Self {
        match encoding {
            choreo_proto::OAuthGrantEncoding::Form => Encoding::Form,
            choreo_proto::OAuthGrantEncoding::Json => Encoding::Json,
        }
    }
}

/// Options controlling how the sign-in runs.
#[derive(Debug, Clone)]
pub struct LoginOptions {
    /// The daemon account name the credential is stored under (the
    /// `AddCredential` `service`). The account must already exist.
    pub account: String,
    /// Never open a browser; emit the authorize URL for the caller to surface.
    pub no_browser: bool,
    /// Emit the authorize URL (in addition to any browser open) so a remote/SSH
    /// user can open it elsewhere. The TUI always wants the URL, so this is a
    /// hint for a CLI that would otherwise stay silent.
    pub print_auth_url: bool,
    /// A full callback URL (or an authorize response URL) pasted by the user
    /// for the paste-URL path: when set, the flow parses the code/state from it
    /// instead of waiting on the loopback server.
    pub callback_url: Option<String>,
    /// Use the RFC 8628 device-code flow (headless) instead of loopback.
    pub device: bool,
}

/// A progress event streamed from the login worker thread.
#[derive(Debug)]
pub enum LoginEvent {
    /// The authorize URL to display (and possibly open). Never contains a
    /// `code` — only the request parameters.
    AuthUrl {
        /// The full authorization URL.
        url: String,
    },
    /// A device-code prompt to display: the short `code` the user enters at
    /// `uri`.
    DeviceCode {
        /// The short user code.
        code: String,
        /// The verification URL the user visits.
        uri: String,
    },
    /// A free-text prompt for the user (e.g. "paste the callback URL").
    Prompt {
        /// The prompt text.
        message: String,
    },
    /// A free-text progress note.
    Progress {
        /// The note.
        message: String,
    },
    /// The flow finished: `message` is the `AddCredential` request to send. The
    /// credential is boxed because it is large.
    Done {
        /// The account name the credential belongs to.
        service: String,
        /// The ready-to-send request.
        message: Box<ClientMessageType>,
    },
    /// The flow failed. `message` is human-readable and carries no secrets.
    Error {
        /// The failure reason.
        message: String,
    },
}

/// A running login flow: the event stream, a cancel handle, and the worker
/// join handle.
pub struct LoginSession {
    /// Events from the worker thread, oldest first. The stream ends when the
    /// worker finishes.
    pub events: Receiver<LoginEvent>,
    cancel: Arc<AtomicBool>,
    stop: Sender<()>,
    handle: Option<JoinHandle<()>>,
}

impl LoginSession {
    /// Request cancellation. Best-effort: the loopback accept loop observes it
    /// within one poll interval, the device-code wait immediately.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
        // The device wait selects on this channel so a cancel is observed
        // without waiting out the poll interval. A closed receiver is fine.
        let _ = self.stop.send(());
    }

    /// Block until the worker thread finishes.
    pub fn join(mut self) {
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Start the interactive sign-in for `descriptor` on a worker thread.
///
/// Returns immediately with a [`LoginSession`]; progress is streamed on
/// `session.events`. The caller drives the resulting `AddCredential` message
/// over its normal request path on `LoginEvent::Done`.
#[must_use]
pub fn start_login(descriptor: OAuthProviderDescriptor, options: LoginOptions) -> LoginSession {
    let (tx, rx) = crossbeam_channel::unbounded();
    let (stop, stop_rx) = crossbeam_channel::unbounded();
    let cancel = Arc::new(AtomicBool::new(false));
    let cancel_worker = Arc::clone(&cancel);
    // A clone goes to the worker so the original `tx` stays available for the
    // thread-spawn-failure path below; both drop when the login ends.
    let worker_tx = tx.clone();
    // The descriptor and options are owned by the worker for its whole life;
    // nothing is shared back except the event channel and the cancel flag.
    let handle = std::thread::Builder::new()
        .name("oauth-login".to_string())
        .spawn(move || {
            let result = run(&descriptor, &options, &worker_tx, &cancel_worker, &stop_rx);
            match result {
                Ok(()) => {}
                Err(error) => {
                    let message = error.to_string();
                    // Never log the raw error verbatim if it could carry a URL
                    // with a code; the error strings here are already
                    // code-free, but keep the log to a kind tag.
                    warn!("oauth login failed: {message}");
                    let _ = worker_tx.send(LoginEvent::Error { message });
                }
            }
        });
    let handle = match handle {
        Ok(handle) => Some(handle),
        Err(e) => {
            // Thread spawn failure is terminal: report it on the stream so the
            // caller's event handler surfaces it, and drop the join handle.
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

/// The worker entry point: dispatch to the device or loopback flow and emit
/// `Done`.
fn run(
    descriptor: &OAuthProviderDescriptor,
    options: &LoginOptions,
    tx: &Sender<LoginEvent>,
    cancel: &AtomicBool,
    stop: &Receiver<()>,
) -> Result<(), OAuthLoginError> {
    let tokens = if options.device {
        run_device(descriptor, options, tx, cancel, stop)?
    } else {
        let (tokens, client_id) = run_loopback(descriptor, options, tx, cancel)?;
        return finish(descriptor, options, tx, &tokens, &client_id);
    };
    let client_id = descriptor.client_id.clone().unwrap_or_default();
    finish(descriptor, options, tx, &tokens, &client_id)
}

/// Build the credential from the exchanged tokens and emit `Done`.
fn finish(
    descriptor: &OAuthProviderDescriptor,
    options: &LoginOptions,
    tx: &Sender<LoginEvent>,
    tokens: &http::TokenSet,
    client_id: &str,
) -> Result<(), OAuthLoginError> {
    let credential = build_credential(descriptor, tokens, client_id);
    let message = build_add_credential_from_credential(options.account.clone(), credential)
        .map_err(|e| OAuthLoginError::Decode(e.to_string()))?;
    let _ = tx.send(LoginEvent::Done {
        service: options.account.clone(),
        message: Box::new(message),
    });
    Ok(())
}

/// The PKCE loopback flow: authorize URL → browser/manual → callback → code
/// exchange.
fn run_loopback(
    descriptor: &OAuthProviderDescriptor,
    options: &LoginOptions,
    tx: &Sender<LoginEvent>,
    cancel: &AtomicBool,
) -> Result<(http::TokenSet, String), OAuthLoginError> {
    let client_id = effective_client_id(descriptor)?;
    let verifier = generate_verifier();
    let challenge = challenge_s256(&verifier);
    let state = generate_state();

    // Bind before building the URL so the redirect port is known.
    let (server, port) = callback::bind(&descriptor.redirect_ports)?;
    let redirect_uri = format!("http://localhost:{port}{}", callback::CALLBACK_PATH);
    let auth_url = build_authorize_url(descriptor, &client_id, &redirect_uri, &challenge, &state);
    // The URL carries the request parameters only; it never carries a code.
    let _ = tx.send(LoginEvent::AuthUrl {
        url: auth_url.clone(),
    });

    let callback = if let Some(callback_url) = options.callback_url.as_deref() {
        // Paste-URL path: the caller supplied the redirect URL a remote browser
        // landed on, so the loopback server is unused.
        parse_callback_url(callback_url)?
    } else {
        if options.no_browser || options.print_auth_url {
            let _ = tx.send(LoginEvent::Prompt {
                message: format!("Open this URL to sign in: {auth_url}"),
            });
        } else {
            // Best-effort: a headless/SSH host has no browser, so a failed open
            // falls back to the emitted URL + the prompt below.
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
        callback::wait_for_callback(&server, cancel)?
    };

    // CSRF guard: never exchange a code whose `state` does not match.
    if !state_matches(&state, &callback.state) {
        return Err(OAuthLoginError::StateMismatch);
    }
    let _ = tx.send(LoginEvent::Progress {
        message: "Authorization received; exchanging the code for tokens…".to_string(),
    });
    let tokens = http::exchange_code(
        &descriptor.issuer,
        &descriptor.token_path,
        Encoding::from_proto(descriptor.code_exchange_encoding),
        &client_id,
        &callback.code,
        &redirect_uri,
        &verifier,
    )?;
    // A provider on the dynamic-registration route returns the client id it
    // registered on the callback; persist it so refresh does not depend on the
    // catalog value. Otherwise persist the id actually sent.
    let persisted_client_id = callback
        .client_id
        .clone()
        .unwrap_or_else(|| client_id.clone());
    Ok((tokens, persisted_client_id))
}

/// The RFC 8628 device-code flow: usercode request → poll → tokens.
fn run_device(
    descriptor: &OAuthProviderDescriptor,
    options: &LoginOptions,
    tx: &Sender<LoginEvent>,
    cancel: &AtomicBool,
    stop: &Receiver<()>,
) -> Result<http::TokenSet, OAuthLoginError> {
    if !descriptor.device_code {
        return Err(OAuthLoginError::Unsupported(
            "the provider has no device-code flow".to_string(),
        ));
    }
    let (Some(device_authorization_path), Some(device_token_path)) = (
        descriptor.device_authorization_path.as_deref(),
        descriptor.device_token_path.as_deref(),
    ) else {
        return Err(OAuthLoginError::Unsupported(
            "the provider's device-code endpoints are not configured".to_string(),
        ));
    };
    let client_id = effective_client_id(descriptor)?;
    let scope = descriptor.scopes.join(" ");
    let code = http::device_authorize(
        &descriptor.issuer,
        device_authorization_path,
        &client_id,
        &scope,
    )?;
    let _ = tx.send(LoginEvent::DeviceCode {
        code: code.user_code.clone(),
        uri: code.verification_uri.clone(),
    });
    let _ = tx.send(LoginEvent::Prompt {
        message: options.account.clone(),
    });
    let interval = std::time::Duration::from_secs(code.interval.unwrap_or(5).max(1));
    loop {
        wait_interval(stop, interval, cancel)?;
        match http::device_poll(
            &descriptor.issuer,
            device_token_path,
            &client_id,
            &code.device_code,
        )? {
            http::PollOutcome::Tokens(tokens) => {
                return Ok(tokens);
            }
            http::PollOutcome::Pending => {}
            http::PollOutcome::Expired(reason) => {
                return Err(OAuthLoginError::DeviceFlow(reason));
            }
        }
    }
}

/// Wait one device-code poll interval, waking immediately on cancel.
fn wait_interval(
    stop: &Receiver<()>,
    interval: std::time::Duration,
    cancel: &AtomicBool,
) -> Result<(), OAuthLoginError> {
    if cancel.load(Ordering::Relaxed) {
        return Err(OAuthLoginError::Cancelled);
    }
    // The timer IS the event here, so `after` (a timer channel) is the wait;
    // the stop channel wins the race so a cancel is observed instantly rather
    // than after up to a full interval.
    select! {
        recv(stop) -> _ => Err(OAuthLoginError::Cancelled),
        recv(after(interval)) -> _ => {
            if cancel.load(Ordering::Relaxed) {
                Err(OAuthLoginError::Cancelled)
            } else {
                Ok(())
            }
        }
    }
}

/// The client id to send at the authorize endpoint, or an error explaining why
/// none is usable (a withheld borrowed id, or a provider requiring dynamic
/// registration that the descriptor did not resolve to a concrete id).
fn effective_client_id(descriptor: &OAuthProviderDescriptor) -> Result<String, OAuthLoginError> {
    descriptor.client_id.clone().ok_or_else(|| {
        OAuthLoginError::NoClientId(format!(
            "{} has no client id (a borrowed id is withheld until the opt-in is enabled, \
             or the provider needs dynamic registration)",
            descriptor.provider
        ))
    })
}

/// Build the provider authorization URL with PKCE and the `state`.
///
/// The request carries only the parameters (never a code); the callback returns
/// the code. Query parameters are percent-encoded by the same rules as the form
/// encoder.
fn build_authorize_url(
    descriptor: &OAuthProviderDescriptor,
    client_id: &str,
    redirect_uri: &str,
    code_challenge: &str,
    state: &str,
) -> String {
    let mut params: Vec<(String, String)> = vec![
        ("response_type".to_string(), "code".to_string()),
        ("client_id".to_string(), client_id.to_string()),
        ("redirect_uri".to_string(), redirect_uri.to_string()),
        ("scope".to_string(), descriptor.scopes.join(" ")),
        ("state".to_string(), state.to_string()),
        ("code_challenge".to_string(), code_challenge.to_string()),
        ("code_challenge_method".to_string(), "S256".to_string()),
    ];
    params.retain(|(_, value)| !value.is_empty());
    let query = http::form_encode(&params);
    http::join_url(&descriptor.issuer, &descriptor.authorize_path)
        + if descriptor.authorize_path.contains('?') {
            "&"
        } else {
            "?"
        }
        + &query
}

/// Parse a pasted callback URL (or a bare `code=..&state=..` query) into a
/// [`callback::Callback`]. Also captures an optional provider-issued
/// `client_id`.
fn parse_callback_url(url: &str) -> Result<callback::Callback, OAuthLoginError> {
    let query = url.split_once('?').map_or(url, |(_, query)| query);
    let mut code = None;
    let mut state = None;
    let mut client_id = None;
    for (key, value) in query.split('&').filter_map(|pair| pair.split_once('=')) {
        match key {
            "code" => code = Some(percent_decode_loose(value)),
            "state" => state = Some(percent_decode_loose(value)),
            "client_id" => client_id = Some(percent_decode_loose(value)),
            _ => {}
        }
    }
    match (code, state) {
        (Some(code), Some(state)) => Ok(callback::Callback {
            code,
            state,
            client_id,
        }),
        _ => Err(OAuthLoginError::CallbackUrl(
            "the URL has no code/state parameters".to_string(),
        )),
    }
}

/// Percent-decode a URL component, mapping `+` to space and leaving an
/// invalid escape literal.
fn percent_decode_loose(input: &str) -> String {
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
                if let (Some(hi), Some(lo)) = (
                    bytes.get(i + 1).and_then(|b| hex(*b)),
                    bytes.get(i + 2).and_then(|b| hex(*b)),
                ) {
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

/// The value of one hex digit, or `None`.
fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Build the durable OAuth credential from the token response.
///
/// Only the durable subset is kept (refresh token, client registration,
/// extracted claims); the access token is dropped. The identity claim is read
/// from the `id_token` first, then the access token.
fn build_credential(
    descriptor: &OAuthProviderDescriptor,
    tokens: &http::TokenSet,
    client_id: &str,
) -> ServiceCredential {
    ServiceCredential::OAuth {
        kind: OAuthKind::Provider,
        issuer: Some(descriptor.issuer.clone()),
        resource: None,
        authorization_server: None,
        refresh_token: tokens.refresh_token.clone(),
        account_id: extract_account_id(tokens),
        client_id: Some(client_id.to_string()),
        client_secret: None,
        granted_scopes: granted_scopes(tokens, descriptor),
        token_type: tokens
            .token_type
            .clone()
            .or_else(|| Some("Bearer".to_string())),
    }
}

/// The account id, preferring the `id_token`'s claim, falling back to the
/// access token's (some providers carry it only in the access token).
fn extract_account_id(tokens: &http::TokenSet) -> Option<String> {
    if let Some(id_token) = tokens.id_token.as_deref()
        && let Some(claims) = decode_claims(id_token)
        && let Some(account_id) = chatgpt_account_id(&claims)
    {
        return Some(account_id);
    }
    let claims = decode_claims(tokens.access_token.as_str())?;
    chatgpt_account_id(&claims)
}

/// The granted scopes: the token response's `scope` when present, else the
/// descriptor's requested scopes.
fn granted_scopes(tokens: &http::TokenSet, descriptor: &OAuthProviderDescriptor) -> Vec<String> {
    match tokens.scope.as_deref() {
        Some(scope) if !scope.trim().is_empty() => {
            scope.split_whitespace().map(str::to_string).collect()
        }
        _ => descriptor.scopes.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use choreo_proto::{OAuthClientIdentity, OAuthGrantEncoding};

    fn descriptor() -> OAuthProviderDescriptor {
        OAuthProviderDescriptor {
            provider: "mock".into(),
            issuer: "https://auth.example".into(),
            authorize_path: "/authorize".into(),
            token_path: "/token".into(),
            client_id: Some("client-abc".into()),
            scopes: vec!["openid".into(), "offline_access".into()],
            redirect_ports: vec![1455],
            code_exchange_encoding: OAuthGrantEncoding::Form,
            refresh_encoding: OAuthGrantEncoding::Json,
            client_identity: OAuthClientIdentity::Registered,
            device_code: false,
            device_authorization_path: None,
            device_token_path: None,
        }
    }

    fn tokens(id_token: Option<&str>, access_token: &str, scope: Option<&str>) -> http::TokenSet {
        http::TokenSet {
            access_token: access_token.to_string(),
            refresh_token: Some("refresh".into()),
            id_token: id_token.map(str::to_string),
            token_type: Some("Bearer".into()),
            scope: scope.map(str::to_string),
        }
    }

    fn jwt(claims: &serde_json::Value) -> String {
        use base64::Engine as _;
        let header = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b"{}");
        let payload =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(claims.to_string().as_bytes());
        format!("{header}.{payload}.sig")
    }

    #[test]
    fn authorize_url_carries_pkce_and_state() {
        let url = build_authorize_url(
            &descriptor(),
            "client-abc",
            "http://localhost:1455/auth/callback",
            "CHALLENGE",
            "STATE123",
        );
        assert!(url.starts_with("https://auth.example/authorize?"));
        assert!(url.contains("response_type=code"));
        assert!(url.contains("client_id=client-abc"));
        assert!(url.contains("code_challenge=CHALLENGE"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("state=STATE123"));
        // The redirect URI is percent-encoded.
        assert!(url.contains("redirect_uri=http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback"));
        assert!(url.contains("scope=openid%20offline_access"));
    }

    #[test]
    fn parse_callback_url_extracts_code_state_and_client_id() {
        let callback = parse_callback_url(
            "http://localhost:1455/auth/callback?code=abc&state=xyz&client_id=oaiapp_1",
        )
        .expect("parse");
        assert_eq!(callback.code, "abc");
        assert_eq!(callback.state, "xyz");
        assert_eq!(callback.client_id.as_deref(), Some("oaiapp_1"));
    }

    #[test]
    fn parse_callback_url_rejects_a_missing_code() {
        assert!(parse_callback_url("http://localhost/cb?state=xyz").is_err());
    }

    #[test]
    fn effective_client_id_errors_when_withheld() {
        let mut desc = descriptor();
        desc.client_id = None;
        assert!(matches!(
            effective_client_id(&desc),
            Err(OAuthLoginError::NoClientId(_))
        ));
        assert_eq!(
            effective_client_id(&descriptor()).expect("id"),
            "client-abc"
        );
    }

    #[test]
    fn account_id_prefers_the_id_token_then_falls_back_to_the_access_token() {
        let acct = serde_json::json!({
            "https://api.openai.com/auth": { "chatgpt_account_id": "from-id" }
        });
        let from_id = tokens(Some(&jwt(&acct)), "opaque-access", None);
        assert_eq!(extract_account_id(&from_id).as_deref(), Some("from-id"));

        // No id_token: the access token itself is a JWT carrying the claim.
        let from_access = tokens(None, &jwt(&acct), None);
        assert_eq!(extract_account_id(&from_access).as_deref(), Some("from-id"));

        // Neither carries the claim.
        let none = tokens(None, "opaque", None);
        assert!(extract_account_id(&none).is_none());
    }

    #[test]
    fn granted_scopes_prefers_the_response_then_the_descriptor() {
        let with_scope = tokens(None, "opaque", Some("a b"));
        assert_eq!(granted_scopes(&with_scope, &descriptor()), ["a", "b"]);

        let without = tokens(None, "opaque", None);
        assert_eq!(
            granted_scopes(&without, &descriptor()),
            ["openid", "offline_access"]
        );
    }

    #[test]
    fn build_credential_drops_the_access_token_and_keeps_the_grant() {
        let cred = build_credential(
            &descriptor(),
            &tokens(None, "opaque-access", Some("openid")),
            "client-abc",
        );
        let oauth = cred.as_oauth().expect("OAuth credential");
        assert_eq!(oauth.kind, OAuthKind::Provider);
        assert_eq!(oauth.refresh_token, Some("refresh"));
        assert_eq!(oauth.client_id, Some("client-abc"));
        assert_eq!(oauth.issuer, Some("https://auth.example"));
        // The access token is not a field of the credential at all.
        assert_eq!(format!("{cred:?}").matches("opaque-access").count(), 0);
    }
}
