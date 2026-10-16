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

/// The callback path the provider redirects to (`<issuer redirect>:/auth/callback`
/// per the `ChatGPT`/Codex allow-list).
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
    let mut last_error = String::from("no redirect ports configured");
    for &port in ports {
        match tiny_http::Server::http(("127.0.0.1", port)) {
            Ok(server) => {
                // Report the ACTUAL bound port: a request for port 0 binds an
                // ephemeral port, and the redirect_uri must carry that real
                // port (a literal `:0` would be unreachable).
                let bound = server
                    .server_addr()
                    .to_ip()
                    .map_or(port, |addr| addr.port());
                return Ok((server, bound));
            }
            Err(e) => last_error = format!("127.0.0.1:{port}: {e}"),
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

/// Parse an `application/x-www-form-urlencoded` query string into decoded
/// key/value pairs. Invalid percent-escapes are dropped rather than failing the
/// whole parse.
fn parse_query(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .filter_map(|pair| {
            let (key, value) = pair.split_once('=')?;
            Some((percent_decode(key), percent_decode(value)))
        })
        .collect()
}

/// Percent-decode one URL component, mapping `+` to a space and an invalid
/// escape to a literal `%` (the byte sequence is kept as-is).
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
                if let (Some(hi), Some(lo)) = (
                    bytes.get(i + 1).and_then(|b| hex_value(*b)),
                    bytes.get(i + 2).and_then(|b| hex_value(*b)),
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

/// The value of one hex digit, or `None` for a non-hex byte.
fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_decode_handles_escapes_and_plus() {
        assert_eq!(percent_decode("a%20b"), "a b");
        assert_eq!(percent_decode("a+b"), "a b");
        assert_eq!(percent_decode("code%2Fwith%2Fslashes"), "code/with/slashes");
        // A stray `%` without two following hex digits is kept literally.
        assert_eq!(percent_decode("50%"), "50%");
        assert_eq!(percent_decode("%zz"), "%zz");
    }

    #[test]
    fn query_parsing_extracts_code_and_state() {
        let pairs = parse_query("code=abc123&state=xyz&scope=openid%20profile");
        assert_eq!(
            pairs,
            vec![
                ("code".to_string(), "abc123".to_string()),
                ("state".to_string(), "xyz".to_string()),
                ("scope".to_string(), "openid profile".to_string()),
            ]
        );
    }

    #[test]
    fn bind_falls_back_and_binds_loopback() {
        // Bind on an ephemeral first port, hold it, and confirm the binder
        // moves to the second when the first is in use.
        let held = tiny_http::Server::http("127.0.0.1:0").expect("ephemeral bind");
        let busy = held.server_addr().to_ip().expect("ip addr").port();
        let (server, port) = bind(&[busy, 0]).expect("falls back to port 0");
        assert_ne!(port, busy, "must move past the busy port");
        assert_eq!(
            server
                .server_addr()
                .to_ip()
                .expect("ip addr")
                .ip()
                .to_string(),
            "127.0.0.1",
            "must bind loopback only"
        );
    }
}
