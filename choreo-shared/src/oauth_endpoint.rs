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
/// A URL whose authority carries userinfo (an `@`, as in `user:pass@host`) is
/// refused outright: RFC 3986 places the host *after* the `@`, so the real host
/// is remote even when the text before the `@` names a loopback host.
///
/// The parse is deliberately conservative — it over-refuses rather than
/// under-refuses, because a false negative only blocks a legitimate endpoint
/// while a false positive would leak a secret. A loopback `http` URL whose
/// authority is not immediately followed by a `/` (e.g. `http://localhost?x=1`,
/// a query with no path) is therefore refused rather than accepted: the host is
/// read from the text before the first `/`, and without one that text is the
/// whole authority-plus-query. Real token and device endpoints carry a path, so
/// this only ever affects a hand-written test endpoint.
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
    // The authority is everything before the first `/`. Userinfo (`user:pass@host`)
    // decodes to a remote host, so reading the host from before the `@` would
    // select a loopback name for a URL that actually targets a remote host;
    // refuse the whole URL rather than trying to parse past the userinfo.
    let authority = rest.split('/').next().unwrap_or_default();
    if authority.contains('@') {
        return false;
    }
    // A bracketed IPv6 host (`[::1]:port`) is unwrapped; a bracket with no
    // closing `]` is malformed and refused rather than read as the host.
    let host = if let Some(after_bracket) = authority.strip_prefix('[') {
        match after_bracket.split_once(']') {
            Some((inner, _rest)) => inner,
            None => return false,
        }
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
    fn malformed_bracketed_host_is_refused() {
        // A bracketed IPv6 authority with no closing `]` is malformed; it must
        // not be read as the host `::1` and accepted.
        assert!(!is_secure_endpoint("http://[::1/token"));
        assert!(!is_secure_endpoint("http://[::1"));
        assert!(!is_secure_endpoint("http://[localhost/token"));
    }

    #[test]
    fn userinfo_before_the_host_does_not_select_a_loopback_host() {
        // `http://localhost@evil.example/` has host `evil.example`, not
        // `localhost`, so the real host is remote; every spelling that carries
        // userinfo (with or without a port, and the bracketed-IPv6 form) must be
        // refused, not read as a loopback host.
        assert!(!is_secure_endpoint("http://localhost@evil.example/token"));
        assert!(!is_secure_endpoint(
            "http://127.0.0.1:80@evil.example/token"
        ));
        assert!(!is_secure_endpoint("http://localhost:1@evil.example/t"));
        assert!(!is_secure_endpoint("http://[::1]@evil.example/t"));
    }

    #[test]
    fn loopback_with_a_port_is_still_secure() {
        assert!(is_secure_endpoint("http://localhost:8080/token"));
        assert!(is_secure_endpoint("http://[::1]:8080/token"));
    }

    #[test]
    fn a_loopback_url_without_a_path_is_conservatively_refused() {
        // The host is read before the first `/`; with no `/` the whole
        // authority-and-query string is inspected, so a path-less loopback URL
        // is refused rather than accepted. Over-refusal is the safe direction
        // (it cannot leak a secret), and every real endpoint carries a path.
        assert!(!is_secure_endpoint("http://localhost?x=1"));
        assert!(!is_secure_endpoint("http://127.0.0.1?a=@b"));
    }
}
