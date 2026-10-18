//! Target canonicalization and the target-scoped credential key.
//!
//! An MCP OAuth credential is keyed by the server's **canonical** endpoint
//! identity (`mcp:<slug>|<target-hash>`) rather than the raw configured URL, so
//! two spellings of one endpoint key the same credential and a re-pointed URL
//! keys a different one. [`credential_matches_target`] is the fail-closed check
//! the daemon applies on load, so a `git pull` that re-points a repo-controlled
//! server URL never receives the stored token.
//!
//! The `mcp:` prefix is **reserved** for these target-scoped MCP keys: a provider
//! account must never be named `mcp:<slug>|…`, because a name in that form would
//! be read as an MCP credential key and could collide with a server's key.

use choreo_shared::{OAuthKind, ServiceCredential};

use crate::config::{McpServerConfig, McpTransport};
use crate::naming::short_hash;

/// Canonicalize a remote server URL for credential keying.
///
/// Lowercases the scheme and host, drops a default port, strips a trailing
/// slash, and drops any fragment and userinfo. Canonicalization matters more
/// than the hash function: a hash cannot fix a non-canonical input, so two
/// spellings of one endpoint must reduce to the same string here. An
/// unparseable URL is returned trimmed and unchanged (a best effort).
#[must_use]
pub fn canonical_target(url: &str) -> String {
    let trimmed = url.trim();
    let Ok(mut parsed) = reqwest::Url::parse(trimmed) else {
        return trimmed.to_string();
    };
    // Drop the fragment and any userinfo; neither identifies the server.
    parsed.set_fragment(None);
    if !parsed.username().is_empty() {
        let _ = parsed.set_username("");
    }
    let _ = parsed.set_password(None);
    // Normalize the path's trailing slash: the url crate keeps `/` for an
    // empty path and preserves any other trailing slash, but `…/mcp` and
    // `…/mcp/` name one endpoint.
    let normalized = parsed.path().trim_end_matches('/').to_owned();
    if normalized.is_empty() {
        parsed.set_path("/");
    } else {
        parsed.set_path(&normalized);
    }
    // A known default port is already dropped by `Url::parse`; a non-default
    // port is meaningful and kept.
    let mut out = parsed.to_string();
    if out.ends_with('/') {
        out.pop();
    }
    out
}

/// Whether a stored MCP credential may be used for a server with the given
/// canonical resource and optional expected issuer (fail-closed, SEP-2352).
///
/// On load the stored `resource` (the server URL) is compared with the
/// currently-configured server URL and the stored `issuer` with the expected
/// issuer (when known): a `git pull` that re-points a repo-controlled server
/// URL must NOT receive the stored token, and an issuer that changed must not
/// either. Any mismatch — or a missing/!MCP credential — returns `false`, so
/// the caller treats it as no credential (quarantine → re-login).
#[must_use]
pub fn credential_matches_target(
    credential: &ServiceCredential,
    canonical_resource: &str,
    expected_issuer: Option<&str>,
) -> bool {
    let Some(view) = credential.as_oauth() else {
        return false;
    };
    if view.kind != OAuthKind::Mcp {
        return false;
    }
    let resource_matches = view
        .resource
        .is_some_and(|r| canonical_target(r) == canonical_resource);
    let issuer_matches = expected_issuer.is_none_or(|iss| {
        view.issuer
            .is_some_and(|stored| choreo_shared::oauth_endpoint::issuers_match(stored, iss))
    });
    resource_matches && issuer_matches
}

/// Build the target-scoped credential key for a canonical target identity and
/// slug: `mcp:<slug>|<short-hash>`.
///
/// The `mcp:` prefix is **reserved** for these target-scoped MCP keys: a provider
/// account must never be named `mcp:<slug>|…`, because a name in that form is
/// read as an MCP credential key and could collide with a server's key.
///
/// The hash is the repo's stable FNV-1a ([`short_hash`]) of the canonical
/// identity joined to the slug with a `\0`, so two servers of one slug pointing
/// at different endpoints key distinctly while the same endpoint keys the same.
/// The hash is only an index: [`credential_matches_target`]'s canonical-resource
/// re-validation on load is the authoritative guard against cross-target use, so
/// a hash collision cannot hand one server another's token.
#[must_use]
pub fn credential_key(slug: &str, identity: &str) -> String {
    format!("mcp:{slug}|{}", short_hash(&format!("{identity}\0{slug}")))
}

/// The target-scoped credential key for a resolved server config, or `None`
/// for a stdio server (which never carries an OAuth credential).
#[must_use]
pub fn credential_key_for(config: &McpServerConfig) -> Option<String> {
    match &config.transport {
        McpTransport::Http { url, .. } => {
            let identity = format!("http\0{}", canonical_target(url));
            Some(credential_key(&config.slug, &identity))
        }
        McpTransport::Stdio { .. } => None,
    }
}

/// The server slug encoded in a target-scoped credential key
/// (`mcp:<slug>|<target-hash>`), or `None` for any other service key.
///
/// The inverse of [`credential_key`], kept beside it so the two cannot drift:
/// it strips the reserved `mcp:` prefix and drops the target hash after the
/// LAST `|`. The hash is [`short_hash`] hex and never contains a `|`, so a slug
/// that itself contains one is recovered intact.
#[must_use]
pub fn slug_from_credential_key(key: &str) -> Option<&str> {
    key.strip_prefix("mcp:")?
        .rsplit_once('|')
        .map(|(slug, _hash)| slug)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn durable_oauth() -> ServiceCredential {
        ServiceCredential::OAuth {
            kind: OAuthKind::Mcp,
            issuer: Some("https://as.example".into()),
            resource: Some("https://example.com/mcp".into()),
            authorization_server: Some("https://as.example".into()),
            token_endpoint: None,
            refresh_encoding: None,
            refresh_token: Some("refresh-secret".into()),
            account_id: None,
            client_id: Some("client-1".into()),
            client_secret: Some("client-secret".into()),
            granted_scopes: vec!["openid".into()],
            token_type: Some("Bearer".into()),
        }
    }

    #[test]
    fn credential_matches_target_is_fail_closed() {
        let cred = durable_oauth();
        let resource = canonical_target("https://example.com/mcp");
        // A matching resource (in any spelling) and no expected issuer passes.
        assert!(credential_matches_target(&cred, &resource, None));
        assert!(credential_matches_target(
            &cred,
            &canonical_target("HTTPS://Example.com/mcp/"),
            None
        ));
        // A re-pointed resource is rejected.
        assert!(!credential_matches_target(
            &cred,
            &canonical_target("https://other.example/mcp"),
            None
        ));
        // A matching expected issuer passes; a mismatched one is rejected.
        assert!(credential_matches_target(
            &cred,
            &resource,
            Some("https://as.example")
        ));
        // A trailing-slash-only difference in the expected issuer is tolerated
        // (the shared comparison), so it does not force a needless re-login.
        assert!(credential_matches_target(
            &cred,
            &resource,
            Some("https://as.example/")
        ));
        assert!(!credential_matches_target(
            &cred,
            &resource,
            Some("https://evil.example")
        ));
        // A non-MCP credential never matches.
        assert!(!credential_matches_target(
            &ServiceCredential::ApiKey { key: "k".into() },
            &resource,
            None
        ));
    }

    #[test]
    fn canonical_target_normalizes_scheme_host_port_and_slash() {
        assert_eq!(
            canonical_target("HTTPS://Example.COM/mcp/"),
            "https://example.com/mcp"
        );
        assert_eq!(
            canonical_target("https://example.com:443/mcp"),
            "https://example.com/mcp"
        );
        assert_eq!(
            canonical_target("https://example.com/"),
            "https://example.com"
        );
        assert_eq!(
            canonical_target("https://example.com"),
            "https://example.com"
        );
        // Fragment and userinfo are dropped.
        assert_eq!(
            canonical_target("https://u:p@example.com/mcp#frag"),
            "https://example.com/mcp"
        );
        // A non-default port is meaningful and kept.
        assert_eq!(
            canonical_target("http://127.0.0.1:8765/mcp"),
            "http://127.0.0.1:8765/mcp"
        );
    }

    #[test]
    fn canonical_target_is_spelling_insensitive() {
        assert_eq!(
            canonical_target("https://MCP.Example.com:443/tools/"),
            canonical_target("https://mcp.example.com/tools")
        );
    }

    #[test]
    fn credential_key_is_stable_and_target_scoped() {
        let a = credential_key("docs", "http\0https://example.com/mcp");
        let b = credential_key("docs", "http\0https://example.com/mcp");
        assert_eq!(a, b, "same identity keys the same");
        let other = credential_key("docs", "http\0https://other.example/mcp");
        assert_ne!(a, other, "different targets key distinctly");
        assert!(a.starts_with("mcp:docs|"));
    }

    #[test]
    fn slug_from_credential_key_inverts_credential_key() {
        // The round trip: the slug that built the key is what the parser
        // recovers, whatever the target.
        let key = credential_key("docs", "http\0https://example.com/mcp");
        assert_eq!(slug_from_credential_key(&key), Some("docs"));
        // A slug that itself contains the separator is recovered intact: only
        // the hash after the LAST `|` is dropped.
        assert_eq!(slug_from_credential_key("mcp:a|b|c0ffee"), Some("a|b"));
    }

    #[test]
    fn slug_from_credential_key_rejects_non_mcp_keys() {
        // A provider account name is never an MCP key.
        assert_eq!(slug_from_credential_key("openai"), None);
        // The prefix alone, or with no separator, is not a target-scoped key.
        assert_eq!(slug_from_credential_key("mcp:"), None);
        assert_eq!(slug_from_credential_key("mcp:notion"), None);
    }

    #[test]
    fn credential_key_for_http_only() {
        let http = McpServerConfig {
            slug: "docs".into(),
            transport: McpTransport::Http {
                url: "https://example.com/mcp".into(),
                headers: HashMap::new(),
                oauth: None,
            },
            enabled: true,
            timeout: None,
            protocol: crate::McpProtocolMode::Auto,
            max_concurrent_calls: None,
            max_restarts: None,
            disabled_tools: Vec::new(),
        };
        let key = credential_key_for(&http).expect("http has a key");
        assert!(key.starts_with("mcp:docs|"));

        let stdio = McpServerConfig {
            slug: "docs".into(),
            transport: McpTransport::Stdio {
                command: "npx".into(),
                args: Vec::new(),
                env: HashMap::new(),
                cwd: None,
                log_path: None,
            },
            enabled: true,
            timeout: None,
            protocol: crate::McpProtocolMode::Auto,
            max_concurrent_calls: None,
            max_restarts: None,
            disabled_tools: Vec::new(),
        };
        assert!(credential_key_for(&stdio).is_none());
    }
}
