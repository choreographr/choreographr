//! Slash-tolerant URL joining: the one place a base URL is combined with a
//! path segment.
//!
//! Provider discovery, the MCP login, and the runtime refresh material all join
//! a base URL (an issuer or authorization endpoint) with a path that may or may
//! not carry a leading slash, against a base that may or may not carry a
//! trailing slash. [`join_path`] owns that tolerance so the paths cannot drift
//! into different results for the same input.

/// Join a base URL and a path into one URL, tolerating a trailing slash on
/// `base` and/or a leading slash on `path`.
///
/// Exactly one separator `/` sits between the two once joined: `base`'s trailing
/// slashes are stripped and a leading slash on `path` is absorbed, so
/// `("https://as.example/", "/oauth/token")`, `("https://as.example",
/// "/oauth/token")`, and `("https://as.example/", "oauth/token")` all yield
/// `https://as.example/oauth/token`. A `path` with no leading slash is inserted
/// after a single `/`; an empty `path` therefore yields `base` with that
/// trailing separator (`https://as.example/`). Only the trailing slashes of
/// `base` are trimmed — the path is otherwise passed through unchanged, so a
/// query string or fragment on `path` survives.
#[must_use]
pub fn join_path(base: &str, path: &str) -> String {
    let base = base.trim_end_matches('/');
    if path.starts_with('/') {
        format!("{base}{path}")
    } else {
        format!("{base}/{path}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_path_tolerates_either_slash() {
        // Neither side carries a slash.
        assert_eq!(
            join_path("https://as.example", "oauth/token"),
            "https://as.example/oauth/token"
        );
        // Trailing slash on the base.
        assert_eq!(
            join_path("https://as.example/", "oauth/token"),
            "https://as.example/oauth/token"
        );
        // Leading slash on the path.
        assert_eq!(
            join_path("https://as.example", "/oauth/token"),
            "https://as.example/oauth/token"
        );
        // Both.
        assert_eq!(
            join_path("https://as.example/", "/oauth/token"),
            "https://as.example/oauth/token"
        );
    }

    #[test]
    fn join_path_empty_path_keeps_the_separator() {
        assert_eq!(join_path("https://as.example", ""), "https://as.example/");
        assert_eq!(join_path("https://as.example/", ""), "https://as.example/");
    }

    #[test]
    fn join_path_passes_the_path_through_unchanged() {
        assert_eq!(
            join_path("https://as.example", "token?grant=1"),
            "https://as.example/token?grant=1"
        );
    }
}
