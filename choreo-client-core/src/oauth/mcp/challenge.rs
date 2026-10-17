//! The `WWW-Authenticate` challenge reader (RFC 9728 §5.1).
//!
//! An MCP server that requires authorization answers an unauthenticated request
//! with `401 Unauthorized` and a `WWW-Authenticate: Bearer …` challenge. RFC
//! 9728 §5.1 defines a `resource_metadata="…"` parameter naming the
//! protected-resource metadata document — the authoritative location of that
//! document. Reading it is what lets a client discover a server whose metadata
//! is not served at the resource's well-known path; the well-known probing in
//! [`super::discovery`] is only the fallback for a challenge that omits the
//! parameter.

/// Extract the RFC 9728 `resource_metadata` URL from a `WWW-Authenticate`
/// challenge, if present.
///
/// The parameter name is matched case-insensitively; its value may be a
/// quoted-string (the norm) or a bare token (unquoted, terminated by a comma or
/// whitespace). A challenge that carries no such parameter — e.g. a bare
/// metadata URL with no `Bearer` scheme — yields `None`, so the caller falls
/// back to well-known discovery.
#[must_use]
pub(super) fn resource_metadata_url(header: &str) -> Option<String> {
    // `to_ascii_lowercase` leaves non-ASCII bytes untouched and maps ASCII
    // letters 1:1, so a byte offset found in the lowercased copy is valid in
    // the original. The needle is pure ASCII, so its offset is a char boundary.
    let needle = header.to_ascii_lowercase().find("resource_metadata=")?;
    let after = header.get(needle + "resource_metadata=".len()..)?;
    let after = after.trim_start();
    if let Some(quoted) = after.strip_prefix('"') {
        let end = quoted.find('"')?;
        return quoted
            .get(..end)
            .map(str::to_string)
            .filter(|value| !value.is_empty());
    }
    // An unquoted token value ends at the next comma or whitespace.
    let end = after.find([',', ' ']).unwrap_or(after.len());
    after
        .get(..end)
        .map(str::to_string)
        .filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_a_quoted_resource_metadata_url() {
        // The shape a conformant server emits (Linear-style).
        assert_eq!(
            resource_metadata_url(
                r#"Bearer realm="OAuth", resource_metadata="https://mcp.example.com/.well-known/oauth-protected-resource/mcp", scope="read write""#
            )
            .as_deref(),
            Some("https://mcp.example.com/.well-known/oauth-protected-resource/mcp")
        );
    }

    #[test]
    fn extracts_an_unquoted_token_value() {
        assert_eq!(
            resource_metadata_url(
                "Bearer resource_metadata=https://example.com/.well-known/oauth-protected-resource"
            )
            .as_deref(),
            Some("https://example.com/.well-known/oauth-protected-resource")
        );
        // A following parameter (comma-separated) bounds the bare token.
        assert_eq!(
            resource_metadata_url("Bearer resource_metadata=https://a.example/meta, scope=x")
                .as_deref(),
            Some("https://a.example/meta")
        );
    }

    #[test]
    fn parameter_name_match_is_case_insensitive() {
        assert_eq!(
            resource_metadata_url(r#"bearer Resource_Metadata="https://x.example/meta""#)
                .as_deref(),
            Some("https://x.example/meta")
        );
    }

    #[test]
    fn absent_parameter_yields_none() {
        // A challenge with no `resource_metadata` parameter.
        assert_eq!(resource_metadata_url(r#"Bearer realm="OAuth""#), None);
        assert_eq!(resource_metadata_url(""), None);
        // A bare metadata URL (no scheme token, no parameter) is not this form;
        // the caller falls back to well-known discovery.
        assert_eq!(
            resource_metadata_url(
                "https://shopify.com/authentication/1/.well-known/openid-configuration"
            ),
            None
        );
    }

    #[test]
    fn empty_or_unterminated_values_yield_none() {
        assert_eq!(resource_metadata_url("Bearer resource_metadata="), None);
        assert_eq!(resource_metadata_url(r#"Bearer resource_metadata=""#), None);
        assert_eq!(
            resource_metadata_url(r#"Bearer resource_metadata="""#),
            None
        );
    }
}
