//! Endpoint security policy for OAuth requests that carry a code or a token.
//!
//! A credential-bearing OAuth request — a token exchange, a refresh, a device
//! poll — must never put its secret on the wire in cleartext. The one policy
//! both the frontend login library (`choreo-client-core`) and the runtime
//! refresh client (`choreo-ai-protocols`) enforce is: **`https`, or a loopback
//! `http` URL** (the in-repo mock server and a local test issuer). Anything else
//! — a plain-`http` remote host, or any non-http scheme — is refused.
//!
//! The predicate lives here, in one place, so login and refresh cannot drift
//! into two different security rules for the same request shape. Callers wrap it
//! in their own error type: the login library maps a rejection to
//! `OAuthLoginError::InsecureEndpoint`, the refresh client to
//! `OAuthRefreshError::InsecureEndpoint`.

/// Whether `url` is safe to send an OAuth code or token to: `https`, or a
/// loopback `http` URL (`127.0.0.1`, `localhost`, or `[::1]`).
///
/// The scheme and host are compared case-insensitively: a URL scheme is
/// case-insensitive by RFC 3986, so `HTTPS://…` is accepted just like
/// `https://…`. An unparseable prefix (any scheme other than http/https) is
/// refused, so a misconfigured endpoint can never leak a secret over cleartext.
#[must_use]
pub fn is_secure_endpoint(url: &str) -> bool {
    // Lowercase only enough to normalize the scheme and host; the path's case is
    // irrelevant to this decision and copying the whole URL is cheap for the
    // handful of calls made per login/refresh.
    let lower = url.to_ascii_lowercase();
    if lower.starts_with("https://") {
        return true;
    }
    let Some(rest) = lower.strip_prefix("http://") else {
        return false;
    };
    // The authority is everything before the first `/`; a bracketed IPv6 host
    // (`[::1]:port`) is unwrapped, otherwise the host is up to the port colon.
    let authority = rest.split('/').next().unwrap_or_default();
    let host = if let Some(after_bracket) = authority.strip_prefix('[') {
        after_bracket.split(']').next().unwrap_or_default()
    } else {
        authority.split(':').next().unwrap_or_default()
    };
    matches!(host, "127.0.0.1" | "localhost" | "::1")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn https_is_secure() {
        assert!(is_secure_endpoint("https://auth.openai.com/oauth/token"));
        // The scheme is case-insensitive (RFC 3986).
        assert!(is_secure_endpoint("HTTPS://auth.openai.com/oauth/token"));
    }

    #[test]
    fn loopback_http_is_secure() {
        assert!(is_secure_endpoint("http://127.0.0.1:8080/token"));
        assert!(is_secure_endpoint("http://localhost:8080/token"));
        assert!(is_secure_endpoint("http://localhost/token"));
        assert!(is_secure_endpoint("http://[::1]:8080/token"));
        // Host and scheme case-insensitivity.
        assert!(is_secure_endpoint("HTTP://LOCALHOST:8080/token"));
    }

    #[test]
    fn remote_cleartext_is_refused() {
        assert!(!is_secure_endpoint("http://evil.example/token"));
        assert!(!is_secure_endpoint("http://127.0.0.1.evil.example/token"));
        assert!(!is_secure_endpoint("http://notlocalhost/token"));
    }

    #[test]
    fn non_http_schemes_are_refused() {
        assert!(!is_secure_endpoint("ftp://127.0.0.1/token"));
        assert!(!is_secure_endpoint("file:///etc/passwd"));
        assert!(!is_secure_endpoint(""));
        assert!(!is_secure_endpoint("auth.openai.com/token"));
    }

    #[test]
    fn userinfo_before_the_host_does_not_select_a_loopback_host() {
        // `http://localhost@evil.example/` has host `evil.example`, not
        // `localhost`; the naive split must not be fooled into accepting it.
        assert!(!is_secure_endpoint("http://localhost@evil.example/token"));
    }
}
