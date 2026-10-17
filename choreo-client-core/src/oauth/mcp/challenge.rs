//! The `WWW-Authenticate` challenge reader (RFC 9728 §5.1 / RFC 6750 §3).
//!
//! An MCP server that requires authorization answers an unauthenticated request
//! with `401 Unauthorized` and a `WWW-Authenticate: Bearer …` challenge. RFC
//! 9728 §5.1 defines a `resource_metadata="…"` parameter naming the
//! protected-resource metadata document — the authoritative location of that
//! document — and RFC 6750 §3 defines a `scope="…"` parameter naming the scopes
//! the operation needs. Reading them lets a client discover a server whose
//! metadata is not served at the resource's well-known path, and request the
//! scopes the server actually wants; the well-known probing in
//! [`super::discovery`] is only the fallback for a challenge that omits the
//! parameter.

/// Extract the RFC 9728 `resource_metadata` URL from a `WWW-Authenticate`
/// challenge, if present.
///
/// The parameter name is matched case-insensitively; its value may be a
/// quoted-string (the norm) or a bare token. A challenge that carries no such
/// parameter — e.g. a bare metadata URL with no `Bearer` scheme — yields `None`,
/// so the caller falls back to well-known discovery.
#[must_use]
pub(super) fn resource_metadata_url(header: &str) -> Option<String> {
    param_value(header, "resource_metadata").filter(|value| !value.is_empty())
}

/// Extract the space-delimited `scope` list from a `WWW-Authenticate` challenge
/// (RFC 6750 §3), if present. An absent, empty, or whitespace-only scope yields
/// `None`.
#[must_use]
pub(super) fn scope(header: &str) -> Option<Vec<String>> {
    let value = param_value(header, "scope")?;
    let scopes: Vec<String> = value.split_whitespace().map(str::to_string).collect();
    (!scopes.is_empty()).then_some(scopes)
}

/// The value of an `auth-param` in a challenge header, matched
/// case-insensitively and read as a quoted-string or a bare token (unquoted,
/// terminated by a comma or whitespace).
///
/// The parameter name must begin an `auth-param` — at the header start or just
/// after one of the separators that bound parameters (a comma, whitespace, or a
/// semicolon). Without that boundary check a `name=` occurring inside another
/// parameter's value (a `scope=` inside a `resource_metadata` URL) or as the
/// tail of a longer name (`insufficient_scope=`) would be misread as the
/// parameter itself.
fn param_value(header: &str, name: &str) -> Option<String> {
    let needle = format!("{name}=");
    // `to_ascii_lowercase` maps ASCII letters 1:1 (leaving other bytes
    // untouched), so a byte offset found in the lowercased copy is valid in the
    // original; the ASCII needle's offset is a char boundary.
    let lower = header.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut from = 0;
    let at = loop {
        let rel = lower.get(from..)?.find(&needle)?;
        let idx = from + rel;
        let boundary = idx == 0
            || bytes
                .get(idx - 1)
                .is_some_and(|b| matches!(*b, b' ' | b'\t' | b',' | b';'));
        if boundary {
            break idx;
        }
        // Not a parameter boundary: skip past this occurrence and keep looking,
        // so a `name=` inside a value cannot shadow the real parameter.
        from = idx + 1;
    };
    let after = header.get(at + needle.len()..)?;
    let after = after.trim_start();
    if let Some(quoted) = after.strip_prefix('"') {
        let end = quoted.find('"')?;
        return quoted.get(..end).map(str::to_string);
    }
    let end = after.find([',', ' ']).unwrap_or(after.len());
    after.get(..end).map(str::to_string)
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

    #[test]
    fn extracts_the_scope_list() {
        assert_eq!(
            scope(r#"Bearer resource_metadata="https://x.example/meta", scope="read write""#),
            Some(vec!["read".to_string(), "write".to_string()])
        );
        // An absent, empty, or whitespace-only scope yields `None`.
        assert_eq!(scope(r#"Bearer realm="OAuth""#), None);
        assert_eq!(scope(r#"Bearer scope="""#), None);
        assert_eq!(scope(r#"Bearer scope="   ""#), None);
    }

    #[test]
    fn parameter_name_inside_a_value_is_not_a_parameter() {
        // A `resource_metadata=` appearing inside another parameter's value must
        // not shadow the real parameter that follows.
        assert_eq!(
            resource_metadata_url(
                r#"Bearer scope="resource_metadata=https://evil", resource_metadata="https://good""#
            )
            .as_deref(),
            Some("https://good")
        );
        // A `scope=` inside a `resource_metadata` URL must not be read as the
        // parameter when the real `scope` follows.
        assert_eq!(
            scope(r#"Bearer resource_metadata="https://x/scope=a", scope="read""#),
            Some(vec!["read".to_string()])
        );
    }

    #[test]
    fn parameter_name_suffix_is_not_a_parameter() {
        // `x_scope=` is not the `scope` parameter (the preceding `_` is not a
        // boundary); the tail of a longer name is not the parameter.
        assert_eq!(scope(r#"Bearer x_scope="admin""#), None);
        assert_eq!(
            resource_metadata_url(r#"Bearer not_resource_metadata="https://x""#),
            None
        );
    }
}
