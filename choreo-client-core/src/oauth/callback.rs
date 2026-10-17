//! The loopback OAuth callback server.
//!
//! The interactive flow redirects the browser to a loopback server the library
//! owns — bound to the same name the `redirect_uri` advertises (`localhost` for
//! a provider, `127.0.0.1` for MCP) — which captures the authorization `code`
//! and `state` and then stops. The server never binds a routable interface, tries
//! the caller's ports in order (falling back on `AddrInUse`), answers any
//! non-callback path with `400` (a browser's `favicon.ico`/preconnect probe must
//! not be mistaken for a callback), and serves one fixed success page.
//!
//! The accept loop uses `tiny_http`'s `recv_timeout` so it can poll the flow's
//! single-bit cancel flag between accepts: a blocking `accept` cannot be
//! interrupted by a channel message, so a tiny lock-free flag is the only way
//! to observe a cancel promptly (the daemon's metrics server is the same
//! pattern).

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crossbeam_channel::Sender;

use super::LoginEvent;
use super::OAuthLoginError;
use choreo_shared::form::parse_query;

/// The callback path the provider redirects to (the `ChatGPT`/Codex backend
/// allow-lists `/auth/callback`).
pub(crate) const CALLBACK_PATH: &str = "/auth/callback";

/// How long each `recv_timeout` waits before the loop re-checks the cancel
/// flag. Short enough that a cancel is observed promptly, long enough that the
/// loop is not a busy-wait.
const POLL_INTERVAL: Duration = Duration::from_millis(200);

/// The fixed success page served once the code and state are captured.
const SUCCESS_PAGE: &str = "<!doctype html><html><head><meta charset=\"utf-8\">\
<title>Choreographr sign-in complete</title></head><body>\
<h1>Sign-in complete</h1><p>You can close this window and return to Choreographr.</p>\
</body></html>";

/// The `code`, `state`, and issuer captured from the redirect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Callback {
    /// The authorization code to exchange.
    pub(crate) code: String,
    /// The `state` value the flow sent, to validate against a CSRF mismatch.
    pub(crate) state: String,
    /// A client id the provider returned on the callback, when it issues one
    /// (the `OpenAI` dynamic-registration route). Preferred over the value sent
    /// when persisting the credential's client registration.
    pub(crate) client_id: Option<String>,
    /// The authorization-server issuer (`iss`) the response carries, when the
    /// server includes it (RFC 9207). Validated against the discovered issuer to
    /// defeat an authorization-server mix-up.
    pub(crate) iss: Option<String>,
}

/// Bind the callback server on the first free port from `ports`, on `host`.
///
/// `host` is the same name the caller advertises in the `redirect_uri`
/// (`localhost` for the provider flow, `127.0.0.1` for MCP), so the server binds
/// the address the browser will actually dial — a host whose `localhost`
/// prefers `::1` cannot leave the server bound on `127.0.0.1` alone.
///
/// Falls back to the next port when one is in use (`AddrInUse`) — the
/// `:1455` then `:1457` sequence — and returns the port actually bound so the
/// caller can build the `redirect_uri`.
///
/// # Errors
///
/// Returns [`OAuthLoginError::CallbackBind`] when no port could be bound (an
/// empty list, or every port in use).
pub(crate) fn bind(host: &str, ports: &[u16]) -> Result<(tiny_http::Server, u16), OAuthLoginError> {
    first_bound(ports, |port| {
        let server =
            tiny_http::Server::http((host, port)).map_err(|e| format!("{host}:{port}: {e}"))?;
        // Report the ACTUAL bound port: a request for port 0 binds an ephemeral
        // port, and the redirect_uri must carry that real port (a literal `:0`
        // would be unreachable).
        let bound = server
            .server_addr()
            .to_ip()
            .map_or(port, |addr| addr.port());
        Ok((server, bound))
    })
}

/// Try each port in order, returning the first one `attempt` accepts and the
/// port it reported. Extracted from [`bind`] (which is the only caller) so the
/// fallback policy is unit-testable without binding a socket.
fn first_bound<T>(
    ports: &[u16],
    mut attempt: impl FnMut(u16) -> Result<(T, u16), String>,
) -> Result<(T, u16), OAuthLoginError> {
    let mut last_error = String::from("no redirect ports configured");
    for &port in ports {
        match attempt(port) {
            Ok(bound) => return Ok(bound),
            Err(e) => last_error = e,
        }
    }
    Err(OAuthLoginError::CallbackBind(last_error))
}

/// Serve the callback until a `code`+`state` pair is captured, the flow is
/// cancelled, or the server errors.
///
/// # Errors
///
/// Returns [`OAuthLoginError::Cancelled`] when `cancel` is set, and
/// [`OAuthLoginError::Callback`] when the accept loop fails or the provider
/// redirected with an `error` parameter.
pub(crate) fn wait_for_callback(
    server: &tiny_http::Server,
    cancel: &AtomicBool,
) -> Result<Callback, OAuthLoginError> {
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(OAuthLoginError::Cancelled);
        }
        match server.recv_timeout(POLL_INTERVAL) {
            Ok(Some(request)) => {
                if let Some(callback) = handle_request(request)? {
                    return Ok(callback);
                }
            }
            // Timed out with no request: loop and re-check the cancel flag.
            Ok(None) => {}
            Err(e) => return Err(OAuthLoginError::Callback(e.to_string())),
        }
    }
}

/// The parsed parameters of a provider's redirect: the authorization `code` and
/// `state`, an optional provider-issued `client_id` (dynamic registration), and
/// the OAuth `error`/`error_description` pair a denial carries. One shape read
/// by both the pasted-URL parser and the loopback request handler, so the two
/// decode the same keys the same way.
#[derive(Debug, Default)]
pub(crate) struct CallbackParams {
    /// The authorization code to exchange.
    pub(crate) code: Option<String>,
    /// The `state` value the flow sent, to validate against a CSRF mismatch.
    pub(crate) state: Option<String>,
    /// A client id the provider returned on the callback, when it issues one.
    pub(crate) client_id: Option<String>,
    /// The authorization-server issuer (`iss`) the response carries (RFC 9207).
    pub(crate) iss: Option<String>,
    /// The OAuth error code on a denied authorization.
    pub(crate) error: Option<String>,
    /// The human-readable error description, when supplied.
    pub(crate) error_description: Option<String>,
}

/// Parse the redirect's query into [`CallbackParams`].
#[must_use]
fn parse_callback_params(query: &str) -> CallbackParams {
    let mut params = CallbackParams::default();
    for (key, value) in parse_query(query) {
        match key.as_str() {
            "code" => params.code = Some(value),
            "state" => params.state = Some(value),
            "client_id" => params.client_id = Some(value),
            "iss" => params.iss = Some(value),
            "error" => params.error = Some(value),
            "error_description" => params.error_description = Some(value),
            _ => {}
        }
    }
    params
}

/// Parse a pasted callback URL (or a bare `code=..&state=..` query) into a
/// [`Callback`]. Also captures an optional provider-issued `client_id`.
///
/// Shared by the provider flow's paste-URL path and MCP's, so both accept the
/// same shapes.
///
/// # Errors
///
/// Returns [`OAuthLoginError::CallbackUrl`] when the URL carries no `code`/`state`
/// pair.
pub(crate) fn parse_callback_url(url: &str) -> Result<Callback, OAuthLoginError> {
    let query = url.split_once('?').map_or(url, |(_, query)| query);
    let params = parse_callback_params(query);
    match (params.code, params.state) {
        (Some(code), Some(state)) => Ok(Callback {
            code,
            state,
            client_id: params.client_id,
            iss: params.iss,
        }),
        _ => Err(OAuthLoginError::CallbackUrl(
            "the URL has no code/state parameters".to_string(),
        )),
    }
}

/// Surface the authorize URL and capture the redirect callback, shared by the
/// provider and MCP loopback flows.
///
/// Emits the [`LoginEvent::AuthUrl`], opens the browser unless `prompt_only`
/// (then it emits the URL as a [`LoginEvent::Prompt`] instead), and returns the
/// captured [`Callback`] — from a pasted URL when one was supplied, otherwise
/// from the already-bound `server`. The `state` check and the code exchange stay
/// with the caller (they differ per flow). Keeping this one scaffold means the
/// two loopback flows cannot drift in how they present the URL or wait for the
/// redirect.
///
/// # Errors
///
/// Returns [`OAuthLoginError::Cancelled`] when `cancel` is set, and
/// [`OAuthLoginError::Callback`]/[`OAuthLoginError::CallbackUrl`] on a loopback
/// failure or an unparseable pasted URL.
pub(crate) fn capture_loopback_callback(
    auth_url: &str,
    prompt_only: bool,
    callback_url: Option<&str>,
    server: &tiny_http::Server,
    cancel: &AtomicBool,
    tx: &Sender<LoginEvent>,
) -> Result<Callback, OAuthLoginError> {
    // The URL carries the request parameters only; it never carries a code.
    let _ = tx.send(LoginEvent::AuthUrl {
        url: auth_url.to_string(),
    });
    if let Some(callback_url) = callback_url {
        // Paste-URL path: the caller supplied the redirect URL a remote browser
        // landed on, so the loopback server is unused.
        return parse_callback_url(callback_url);
    }
    if prompt_only {
        let _ = tx.send(LoginEvent::Prompt {
            message: format!("Open this URL to sign in: {auth_url}"),
        });
    } else {
        // Best-effort: a headless/SSH host has no browser, so a failed open
        // falls back to the emitted URL + prompt.
        match webbrowser::open(auth_url) {
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
    wait_for_callback(server, cancel)
}

/// Handle one HTTP request: answer a non-callback path with `400` and a
/// callback carrying `code`+`state` with the success page, returning the
/// captured pair. A callback with an `error` parameter is a terminal failure.
fn handle_request(request: tiny_http::Request) -> Result<Option<Callback>, OAuthLoginError> {
    let url = request.url().to_string();
    let (path, query) = match url.split_once('?') {
        Some((path, query)) => (path, query),
        None => (url.as_str(), ""),
    };
    if path != CALLBACK_PATH {
        // A browser's favicon/preconnect probe, or anything else: not a
        // callback. Answer 400 so it cannot be mistaken for one.
        let response = tiny_http::Response::from_string("not found").with_status_code(400);
        let _ = request.respond(response);
        return Ok(None);
    }
    let params = parse_callback_params(query);
    // The success page is served for the callback regardless of outcome so the
    // user's browser tab settles; the library still reports a provider error.
    let mut response = tiny_http::Response::from_string(SUCCESS_PAGE).with_status_code(200);
    if let Ok(header) = tiny_http::Header::from_bytes("Content-Type", "text/html; charset=utf-8") {
        response = response.with_header(header);
    }
    match (params.code, params.state, params.error) {
        (Some(code), Some(state), _) => {
            let _ = request.respond(response);
            Ok(Some(Callback {
                code,
                state,
                client_id: params.client_id,
                iss: params.iss,
            }))
        }
        (_, _, Some(error)) => {
            let _ = request.respond(response);
            // A provider denial is an OAuth `error`/`error_description` pair, not
            // a loopback-server failure, so it is surfaced as the same
            // provider-error class as a token-endpoint rejection.
            Err(OAuthLoginError::TokenEndpoint {
                error,
                description: params.error_description.unwrap_or_default(),
            })
        }
        _ => {
            // A callback path without a code or an error: keep waiting (some
            // providers send a preliminary probe), but do not loop forever on
            // a malformed redirect — respond and continue.
            let response = tiny_http::Response::from_string("missing code").with_status_code(400);
            let _ = request.respond(response);
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn first_bound_skips_a_busy_port_and_reports_the_next() {
        // The binder tries each port in order; a busy port falls through to the
        // next, and the port reported is the one that actually bound.
        let mut tried = Vec::new();
        let (payload, port) = first_bound(&[1455, 1457], |port| {
            tried.push(port);
            if port == 1455 {
                Err(format!("127.0.0.1:{port}: in use"))
            } else {
                Ok(("server", port))
            }
        })
        .expect("the second port binds");
        assert_eq!(payload, "server");
        assert_eq!(port, 1457);
        assert_eq!(tried, [1455, 1457], "ports are tried in order");
    }

    #[test]
    fn first_bound_reports_the_last_error_when_no_port_binds() {
        let error = first_bound(&[1455, 1457], |port| {
            Err::<((), u16), _>(format!("busy {port}"))
        })
        .expect_err("no port binds");
        match error {
            OAuthLoginError::CallbackBind(detail) => assert!(detail.contains("1457"), "{detail}"),
            other => panic!("expected CallbackBind, got {other:?}"),
        }
        // An empty port list reports the distinct "none configured" message.
        assert!(matches!(
            first_bound::<()>(&[], |_| Err::<((), u16), String>(
                "never called".to_string()
            )),
            Err(OAuthLoginError::CallbackBind(_))
        ));
    }
}
