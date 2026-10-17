//! The two interactive authorization flows: loopback PKCE and RFC 8628
//! device-code.
//!
//! Both return the exchanged `TokenSet` and the client id to persist, so the
//! caller (`crate::oauth`'s `run`) hands them to the one credential-building
//! path. They are split from the public types so the transport detail (bind a
//! loopback server, poll a device endpoint) stays out of the module that exposes
//! the API.

use std::sync::atomic::{AtomicBool, Ordering};

use choreo_proto::OAuthProviderDescriptor;
use crossbeam_channel::{Receiver, Sender, after, select_biased};

use super::{LoginEvent, LoginOptions, OAuthLoginError, callback, http, pkce};

/// How many consecutive transient network failures the device-code poll
/// tolerates before giving up. A one-off blip is retried; a persistently
/// unreachable token endpoint still fails rather than looping forever.
const MAX_DEVICE_POLL_ERRORS: u8 = 5;

/// The PKCE loopback flow: authorize URL → browser/manual → callback → code
/// exchange.
pub(super) fn run_loopback(
    descriptor: &OAuthProviderDescriptor,
    options: &LoginOptions,
    tx: &Sender<LoginEvent>,
    cancel: &AtomicBool,
) -> Result<(http::TokenSet, String), OAuthLoginError> {
    let client_id = effective_client_id(descriptor)?;
    let verifier = pkce::generate_verifier();
    let challenge = pkce::challenge_s256(&verifier);
    let state = pkce::generate_state();

    // Bind before building the URL so the redirect port is known. Bind the SAME
    // name the redirect URI advertises (`localhost`): `tiny_http` resolves it
    // the way the browser will, so a host whose `localhost` prefers `::1` cannot
    // leave the callback bound on `127.0.0.1` alone and unreachable.
    let (server, port) = callback::bind("localhost", &descriptor.redirect_ports)?;
    let redirect_uri = format!("http://localhost:{port}{}", callback::CALLBACK_PATH);
    let auth_url = build_authorize_url(descriptor, &client_id, &redirect_uri, &challenge, &state)?;
    // The shared scaffold emits the URL, opens the browser (or prompts), and
    // captures the callback from the loopback server or a pasted URL.
    let callback = callback::capture_loopback_callback(
        &auth_url,
        options.no_browser || options.print_auth_url,
        options.callback_url.as_deref(),
        &server,
        cancel,
        tx,
    )?;

    // CSRF guard: never exchange a code whose `state` does not match.
    if !pkce::state_matches(&state, &callback.state) {
        return Err(OAuthLoginError::StateMismatch);
    }
    let _ = tx.send(LoginEvent::Progress {
        message: "Authorization received; exchanging the code for tokens…".to_string(),
    });
    let tokens = http::exchange_code(
        &descriptor.issuer,
        &descriptor.token_path,
        descriptor.code_exchange_encoding,
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

/// The RFC 8628 device-code flow: usercode request → poll → tokens. Returns the
/// tokens and the client id to persist (the descriptor's).
pub(super) fn run_device(
    descriptor: &OAuthProviderDescriptor,
    tx: &Sender<LoginEvent>,
    cancel: &AtomicBool,
    stop: &Receiver<()>,
) -> Result<(http::TokenSet, String), OAuthLoginError> {
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
    // The interval starts at the server's suggestion (defaulting to 5s) and is
    // bumped by 5s on each `slow_down`, per RFC 8628 §3.5.
    let mut interval = std::time::Duration::from_secs(code.interval.unwrap_or(5).max(1));
    // A transient transport error during the (minutes-long) poll must not abort
    // the flow: tolerate a short run of them before giving up, so a one-off blip
    // is retried but a permanently unreachable endpoint still fails.
    let mut transient_errors: u8 = 0;
    loop {
        wait_interval(stop, interval, cancel)?;
        match http::device_poll(
            &descriptor.issuer,
            device_token_path,
            &client_id,
            &code.device_code,
        ) {
            Ok(http::PollOutcome::Tokens(tokens)) => return Ok((tokens, client_id)),
            // A pending poll is a successful round-trip, so it clears the
            // transient-error run: only *consecutive* transport failures count.
            Ok(http::PollOutcome::Pending) => transient_errors = 0,
            Ok(http::PollOutcome::SlowDown) => {
                transient_errors = 0;
                interval = interval.saturating_add(std::time::Duration::from_secs(5));
                let _ = tx.send(LoginEvent::Progress {
                    message: "The provider asked to slow down; waiting a little longer…"
                        .to_string(),
                });
            }
            Ok(http::PollOutcome::Expired(reason)) => {
                return Err(OAuthLoginError::DeviceFlow(reason));
            }
            // A transient network failure: report progress and keep polling.
            Err(OAuthLoginError::Network(e)) if transient_errors < MAX_DEVICE_POLL_ERRORS => {
                transient_errors += 1;
                let _ = tx.send(LoginEvent::Progress {
                    message: format!(
                        "transient error while waiting for authorization; retrying ({e})"
                    ),
                });
            }
            Err(e) => return Err(e),
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
    // the biased select makes the stop arm win when both are ready, so a cancel
    // is observed instantly rather than after up to a full interval.
    select_biased! {
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
/// the code. The authorization endpoint is vetted with the shared `https`-or-
/// loopback rule first, so a discovery/config-driven `http://` remote endpoint
/// can never be opened in the browser. Query parameters are percent-encoded by
/// the shared form encoder.
///
/// # Errors
///
/// Returns [`OAuthLoginError::InsecureEndpoint`] when the authorization endpoint
/// is neither `https` nor a loopback `http` address.
fn build_authorize_url(
    descriptor: &OAuthProviderDescriptor,
    client_id: &str,
    redirect_uri: &str,
    code_challenge: &str,
    state: &str,
) -> Result<String, OAuthLoginError> {
    let endpoint = choreo_shared::url::join_path(&descriptor.issuer, &descriptor.authorize_path);
    http::ensure_secure(&endpoint)?;
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
    Ok(http::authorize_url(&endpoint, &params))
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

    #[test]
    fn authorize_url_carries_pkce_and_state() {
        let url = build_authorize_url(
            &descriptor(),
            "client-abc",
            "http://localhost:1455/auth/callback",
            "CHALLENGE",
            "STATE123",
        )
        .expect("secure endpoint");
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
    fn authorize_url_refuses_a_cleartext_remote_endpoint() {
        // A `http://` remote authorize endpoint must be refused before the
        // browser is pointed at it; an `https://` one is accepted.
        let mut desc = descriptor();
        desc.issuer = "http://evil.example".into();
        assert!(matches!(
            build_authorize_url(&desc, "client-abc", "http://localhost:1455/cb", "C", "S"),
            Err(OAuthLoginError::InsecureEndpoint(_))
        ));
        assert!(
            build_authorize_url(
                &descriptor(),
                "client-abc",
                "http://localhost:1455/cb",
                "C",
                "S"
            )
            .is_ok()
        );
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
}
