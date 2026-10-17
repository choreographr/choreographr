//! RFC 9728 protected-resource and RFC 8414 / OIDC authorization-server
//! metadata discovery for the MCP login flow.
//!
//! The client starts from the server's resource URL, fetches its
//! protected-resource metadata to learn which authorization server it trusts,
//! then reads that authorization server's metadata for the endpoints. The
//! well-known URLs follow the RFCs: the well-known segment is inserted *before*
//! the URL's path component, not appended after it, so a path-bearing issuer or
//! resource resolves correctly; a root-only document remains the fallback (the
//! in-repo mock authorization servers serve only the root form).

use serde_json::Value;
use url::Url;

use super::super::OAuthLoginError;
use super::super::http;

/// The RFC 9728 protected-resource metadata well-known segment.
const PROTECTED_RESOURCE: &str = "/.well-known/oauth-protected-resource";
/// The RFC 8414 authorization-server metadata well-known segment.
const OAUTH_AUTHORIZATION_SERVER: &str = "/.well-known/oauth-authorization-server";
/// The OIDC discovery well-known segment (RFC 8414 §3 / `OpenID` Discovery).
const OPENID_CONFIGURATION: &str = "/.well-known/openid-configuration";

/// One discovered authorization server.
pub(super) struct AuthServer {
    /// The authorization-server issuer (validated against the metadata).
    pub(super) issuer: String,
    /// The authorization endpoint.
    pub(super) authorization_endpoint: String,
    /// The token endpoint.
    pub(super) token_endpoint: String,
    /// The dynamic client registration endpoint, when the server advertises one.
    pub(super) registration_endpoint: Option<String>,
    /// The scopes the server advertises support for.
    pub(super) scopes_supported: Vec<String>,
}

/// The most redirects a discovery GET will follow. Each hop is re-vetted with
/// the endpoint policy, so a redirect cannot steer the fetch onto a non-`https`
/// remote host.
const MAX_DISCOVERY_REDIRECTS: usize = 5;

/// Fetch and parse an `https` (or loopback `http`) JSON document, following a
/// small, bounded number of redirects — **re-vetting every hop** with
/// [`http::ensure_secure`] before it is dialed, so a redirect cannot downgrade
/// the fetch to cleartext. The caller's `discovery_agent` follows no redirects
/// itself, which is what lets this loop police each target.
fn get_json(agent: &ureq::Agent, url: &str) -> Result<Value, OAuthLoginError> {
    let mut current = parse_url(url, "discovery")?;
    for _ in 0..=MAX_DISCOVERY_REDIRECTS {
        http::ensure_secure(current.as_str())?;
        let response = agent
            .get(current.as_str())
            .header("Accept", "application/json")
            .call()
            .map_err(|e| OAuthLoginError::Network(e.to_string()))?;
        if response.status().is_redirection() {
            // A credential-free metadata GET may redirect (e.g. to a tenant
            // host); resolve the `Location` against the current URL and loop so
            // the next hop is vetted before it is sent.
            let target = response
                .headers()
                .get("location")
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| {
                    OAuthLoginError::Decode(format!("{current}: redirect with no Location header"))
                })?;
            current = current.join(target).map_err(|e| {
                OAuthLoginError::Decode(format!("{current}: bad redirect target ({e})"))
            })?;
            continue;
        }
        return response
            .into_body()
            .read_json()
            .map_err(|e| OAuthLoginError::Decode(format!("{current}: {e}")));
    }
    Err(OAuthLoginError::Decode(format!(
        "{url}: too many redirects"
    )))
}

/// Parse `raw` into a URL, naming `what` in the failure for a readable error.
fn parse_url(raw: &str, what: &str) -> Result<Url, OAuthLoginError> {
    Url::parse(raw).map_err(|e| OAuthLoginError::Decode(format!("invalid {what} URL ({raw}): {e}")))
}

/// Build a well-known metadata URL: insert `prefix` before `base`'s path, per
/// RFC 8414 §3.1 / RFC 9728 §3 (the well-known segment goes *before* the path
/// component, not after it), and drop any query/fragment.
fn well_known(base: &Url, prefix: &str) -> Url {
    let mut url = base.clone();
    let path = base.path().trim_end_matches('/');
    url.set_path(&format!("{prefix}{path}"));
    url.set_query(None);
    url.set_fragment(None);
    url
}

/// The origin-rooted form of a well-known metadata URL (`prefix` with no path
/// suffix), the fallback for a server that serves only the root document.
fn well_known_root(base: &Url, prefix: &str) -> Url {
    let mut url = base.clone();
    url.set_path(prefix);
    url.set_query(None);
    url.set_fragment(None);
    url
}

/// The origin of `url` (`scheme://host[:port]/`), the assumed authorization
/// server when the resource advertises no protected-resource metadata.
fn origin_url(url: &Url) -> Url {
    let mut origin = url.clone();
    origin.set_path("/");
    origin.set_query(None);
    origin.set_fragment(None);
    origin
}

/// Drop duplicate candidates while preserving their order, so a base with no
/// path (where the path-inserted and root forms coincide) tries each URL once.
fn dedup(urls: Vec<Url>) -> Vec<Url> {
    let mut unique: Vec<Url> = Vec::new();
    for url in urls {
        if !unique.contains(&url) {
            unique.push(url);
        }
    }
    unique
}

/// The protected-resource metadata candidates for `resource`, in order: the
/// path-inserted form first, then the origin-rooted form.
fn protected_resource_candidates(resource: &Url) -> Vec<Url> {
    dedup(vec![
        well_known(resource, PROTECTED_RESOURCE),
        well_known_root(resource, PROTECTED_RESOURCE),
    ])
}

/// The authorization-server metadata candidates for `issuer`, in order: the
/// path-inserted RFC 8414 and OIDC forms, then their root fallbacks.
fn authorization_server_candidates(issuer: &Url) -> Vec<Url> {
    dedup(vec![
        well_known(issuer, OAUTH_AUTHORIZATION_SERVER),
        well_known(issuer, OPENID_CONFIGURATION),
        well_known_root(issuer, OAUTH_AUTHORIZATION_SERVER),
        well_known_root(issuer, OPENID_CONFIGURATION),
    ])
}

/// Strip at most one trailing slash, so a single trailing-slash difference
/// between two otherwise-identical URLs is tolerated.
fn strip_one_trailing_slash(url: &str) -> &str {
    url.strip_suffix('/').unwrap_or(url)
}

/// Reject a metadata document whose `issuer` does not identify the
/// authorization-server URL it was fetched from (RFC 8414 §3.3), tolerating a
/// single trailing-slash difference. This is the standard defence against an
/// issuer mix-up, where a hostile document names a different issuer than the
/// one the client asked.
fn validate_issuer(issuer: &str, authorization_server: &Url) -> Result<(), OAuthLoginError> {
    let issuer_url = parse_url(issuer, "metadata issuer")?;
    if strip_one_trailing_slash(issuer_url.as_str())
        == strip_one_trailing_slash(authorization_server.as_str())
    {
        Ok(())
    } else {
        Err(OAuthLoginError::Decode(format!(
            "metadata issuer does not match the authorization server URL: {issuer} != {authorization_server}"
        )))
    }
}

/// Discover the authorization server from the server URL (RFC 9728 → 8414).
pub(super) fn discover(agent: &ureq::Agent, resource: &str) -> Result<AuthServer, OAuthLoginError> {
    let resource_url = parse_url(resource, "resource")?;
    // RFC 9728 protected-resource metadata: try the path-inserted document,
    // then the origin-rooted form.
    let mut resource_metadata = None;
    for candidate in protected_resource_candidates(&resource_url) {
        if let Ok(value) = get_json(agent, candidate.as_str()) {
            resource_metadata = Some(value);
            break;
        }
    }
    let authorization_server = resource_metadata
        .as_ref()
        .and_then(|v| v.get("authorization_servers"))
        .and_then(|v| v.as_array())
        .and_then(|servers| servers.first())
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let authorization_server = match authorization_server {
        Some(server) => parse_url(&server, "authorization server")?,
        // No protected-resource metadata: assume the resource host is the AS.
        None => origin_url(&resource_url),
    };
    fetch_as_metadata(agent, &authorization_server)
}

/// Whether an authorization-server metadata document carries the two endpoints
/// the flow needs, as strings (a document that omits or mistypes them cannot be
/// used, and the caller should try the next well-known candidate).
fn has_required_endpoints(metadata: &Value) -> bool {
    ["authorization_endpoint", "token_endpoint"]
        .iter()
        .all(|key| metadata.get(key).and_then(Value::as_str).is_some())
}

/// Fetch RFC 8414 / OIDC authorization-server metadata.
pub(super) fn fetch_as_metadata(
    agent: &ureq::Agent,
    authorization_server: &Url,
) -> Result<AuthServer, OAuthLoginError> {
    let mut metadata = None;
    let mut last_error = OAuthLoginError::Decode("no metadata candidate".to_string());
    for candidate in authorization_server_candidates(authorization_server) {
        match get_json(agent, candidate.as_str()) {
            Ok(value) => {
                // A well-known document that exists but omits the required
                // endpoints must not stop the search: some servers serve an
                // empty RFC 8414 path-inserted document and the real endpoints
                // only at the OIDC or root form, so continue to the next
                // candidate rather than failing on the first 200.
                if has_required_endpoints(&value) {
                    metadata = Some(value);
                    break;
                }
                last_error = OAuthLoginError::Decode(format!(
                    "{candidate}: metadata has no authorization_endpoint/token_endpoint"
                ));
            }
            Err(error) => last_error = error,
        }
    }
    let metadata = metadata.ok_or_else(|| {
        OAuthLoginError::Decode(format!(
            "authorization-server metadata discovery failed for {authorization_server}: {last_error}"
        ))
    })?;
    let string = |key: &str| {
        metadata
            .get(key)
            .and_then(|v| v.as_str())
            .map(str::to_string)
    };
    let authorization_endpoint = string("authorization_endpoint").ok_or_else(|| {
        OAuthLoginError::Decode("metadata has no authorization_endpoint".to_string())
    })?;
    let token_endpoint = string("token_endpoint")
        .ok_or_else(|| OAuthLoginError::Decode("metadata has no token_endpoint".to_string()))?;
    // The document's `issuer` must identify the same authorization server it
    // was fetched from (RFC 8414 §3.3); a document with no `issuer` is accepted
    // as the server we asked for.
    let issuer = string("issuer").unwrap_or_else(|| authorization_server.to_string());
    validate_issuer(&issuer, authorization_server)?;
    let scopes_supported = metadata
        .get("scopes_supported")
        .and_then(|v| v.as_array())
        .map_or_else(Vec::new, |a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        });
    Ok(AuthServer {
        issuer,
        authorization_endpoint,
        token_endpoint,
        registration_endpoint: string("registration_endpoint"),
        scopes_supported,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(raw: &str) -> Url {
        Url::parse(raw).expect("valid URL")
    }

    #[test]
    fn well_known_inserts_the_segment_before_the_path() {
        // No path: the root form, with or without a trailing slash.
        assert_eq!(
            well_known(&url("https://example.com"), PROTECTED_RESOURCE).as_str(),
            "https://example.com/.well-known/oauth-protected-resource"
        );
        assert_eq!(
            well_known(&url("https://example.com/"), PROTECTED_RESOURCE).as_str(),
            "https://example.com/.well-known/oauth-protected-resource"
        );
        // A path-bearing resource: the segment goes BEFORE the path, never
        // appended after it.
        assert_eq!(
            well_known(&url("https://example.com/mcp"), PROTECTED_RESOURCE).as_str(),
            "https://example.com/.well-known/oauth-protected-resource/mcp"
        );
        // A trailing slash on the base path is trimmed, not carried into the
        // well-known URL.
        assert_eq!(
            well_known(&url("https://example.com/mcp/"), PROTECTED_RESOURCE).as_str(),
            "https://example.com/.well-known/oauth-protected-resource/mcp"
        );
    }

    #[test]
    fn well_known_root_drops_the_path() {
        assert_eq!(
            well_known_root(
                &url("https://example.com/tenant"),
                OAUTH_AUTHORIZATION_SERVER
            )
            .as_str(),
            "https://example.com/.well-known/oauth-authorization-server"
        );
    }

    #[test]
    fn candidate_lists_put_the_well_known_form_first_and_dedup() {
        // No path: path-inserted and root forms coincide, so each URL is tried
        // once.
        let root = authorization_server_candidates(&url("https://example.com"));
        assert_eq!(
            root.iter().map(Url::as_str).collect::<Vec<_>>(),
            vec![
                "https://example.com/.well-known/oauth-authorization-server",
                "https://example.com/.well-known/openid-configuration",
            ]
        );
        // A path-bearing issuer: the path-inserted forms first, then the root
        // fallbacks.
        let path = authorization_server_candidates(&url("https://example.com/tenant"));
        assert_eq!(
            path.iter().map(Url::as_str).collect::<Vec<_>>(),
            vec![
                "https://example.com/.well-known/oauth-authorization-server/tenant",
                "https://example.com/.well-known/openid-configuration/tenant",
                "https://example.com/.well-known/oauth-authorization-server",
                "https://example.com/.well-known/openid-configuration",
            ]
        );
    }

    #[test]
    fn issuer_validation_accepts_matches_and_tolerates_one_trailing_slash() {
        let as_url = url("https://example.com");
        assert!(validate_issuer("https://example.com", &as_url).is_ok());
        assert!(validate_issuer("https://example.com/", &as_url).is_ok());
        let as_path = url("https://example.com/tenant");
        assert!(validate_issuer("https://example.com/tenant", &as_path).is_ok());
        assert!(validate_issuer("https://example.com/tenant/", &as_path).is_ok());
    }

    #[test]
    fn issuer_validation_rejects_a_mismatch() {
        let as_url = url("https://example.com/tenant");
        assert!(matches!(
            validate_issuer("https://evil.example", &as_url),
            Err(OAuthLoginError::Decode(_))
        ));
        assert!(matches!(
            validate_issuer("not-a-url", &as_url),
            Err(OAuthLoginError::Decode(_))
        ));
    }
}
