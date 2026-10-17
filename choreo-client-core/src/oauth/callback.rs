//! The loopback OAuth callback server.
//!
//! The interactive flow redirects the browser to a `127.0.0.1` server the
//! library owns, which captures the authorization `code` and `state` and then
//! stops. The server binds `127.0.0.1` only (never a routable interface), tries
//! the descriptor's ports in order (falling back on `AddrInUse`), answers any
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

use super::OAuthLoginError;
use super::url::parse_query;

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

/// The `code` and `state` captured from the redirect.
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
}

/// Bind the callback server on the first free `127.0.0.1` port from `ports`.
///
/// Falls back to the next port when one is in use (`AddrInUse`) — the
/// `:1455` then `:1457` sequence — and returns the port actually bound so the
/// caller can build the `redirect_uri`.
///
/// # Errors
///
/// Returns [`OAuthLoginError::CallbackBind`] when no port could be bound (an
/// empty list, or every port in use).
pub(crate) fn bind(ports: &[u16]) -> Result<(tiny_http::Server, u16), OAuthLoginError> {
    first_bound(ports, |port| {
        let server = tiny_http::Server::http(("127.0.0.1", port))
            .map_err(|e| format!("127.0.0.1:{port}: {e}"))?;
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
    let mut code = None;
    let mut state = None;
    let mut client_id = None;
    let mut error = None;
    let mut error_description = None;
    for (key, value) in parse_query(query) {
        match key.as_str() {
            "code" => code = Some(value),
            "state" => state = Some(value),
            "client_id" => client_id = Some(value),
            "error" => error = Some(value),
            "error_description" => error_description = Some(value),
            _ => {}
        }
    }
    // The success page is served for the callback regardless of outcome so the
    // user's browser tab settles; the library still reports a provider error.
    let mut response = tiny_http::Response::from_string(SUCCESS_PAGE).with_status_code(200);
    if let Ok(header) = tiny_http::Header::from_bytes("Content-Type", "text/html; charset=utf-8") {
        response = response.with_header(header);
    }
    match (code, state, error) {
        (Some(code), Some(state), _) => {
            let _ = request.respond(response);
            Ok(Some(Callback {
                code,
                state,
                client_id,
            }))
        }
        (_, _, Some(error)) => {
            let _ = request.respond(response);
            Err(OAuthLoginError::Callback(format!(
                "provider returned error: {error} {}",
                error_description.unwrap_or_default()
            )))
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
