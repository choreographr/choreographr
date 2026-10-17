//! The OAuth refresh **request** side: the parameters a token-endpoint exchange
//! carries and their serialization.
//!
//! The request holds two secrets (the durable refresh token and, for a
//! confidential client, the client secret), so it has a redacting
//! [`Debug`](std::fmt::Debug) and its encoded body is wipe-on-drop. Encoding is
//! kept apart from the HTTP call so the form-vs-JSON contract is unit testable
//! without a socket.

use serde::Serialize;
use zeroize::Zeroizing;

use crate::catalog::GrantEncoding;

/// A provider OAuth refresh request: everything the exchange needs, derived
/// from the stored credential's durable grant fields (the token endpoint and
/// refresh encoding recorded at login, the client registration, and the
/// refresh token), never from the live catalog profile.
///
/// The `token_endpoint` is the absolute token URL recorded at login; the
/// `encoding` is the credential's recorded `refresh_encoding`. `client_secret`
/// is present only for a confidential client.
#[derive(Clone)]
pub struct OAuthRefreshRequest<'a> {
    /// The absolute token endpoint URL.
    pub token_endpoint: &'a str,
    /// The request-body encoding the endpoint expects.
    pub encoding: GrantEncoding,
    /// The client id recorded at login.
    pub client_id: Option<&'a str>,
    /// The client secret, for a confidential client.
    pub client_secret: Option<&'a str>,
    /// The refresh token to present (the durable grant root).
    pub refresh_token: &'a str,
    /// The scopes to request; empty to omit the `scope` parameter.
    pub scopes: &'a [String],
}

/// Redacting `Debug`: the refresh token and any client secret are secret, so a
/// logged request must not expose them. Only the non-secret routing fields and
/// presence flags are printed.
impl std::fmt::Debug for OAuthRefreshRequest<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuthRefreshRequest")
            .field("token_endpoint", &self.token_endpoint)
            .field("encoding", &self.encoding)
            .field("has_client_id", &self.client_id.is_some())
            .field("has_client_secret", &self.client_secret.is_some())
            .field("has_refresh_token", &(!self.refresh_token.is_empty()))
            .field("scopes", &self.scopes.len())
            .finish()
    }
}

/// The JSON refresh body. A typed struct with borrowed fields serializes
/// straight to the request body, so the refresh token and client secret are
/// never copied into an intermediate owned `Value`/map that would hold a second
/// copy of the secrets in memory. Mirrors the login path's `CodeExchangeBody`.
#[derive(Serialize)]
struct RefreshBody<'a> {
    grant_type: &'static str,
    refresh_token: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    client_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    client_secret: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    scope: Option<&'a str>,
}

/// Serialize a refresh request body per the endpoint's encoding.
///
/// Kept separate from the HTTP call so the form-vs-JSON contract is unit
/// testable without a socket. The returned body holds the refresh token, so it
/// is wipe-on-drop; both branches borrow the secrets straight from `request`,
/// so no non-zeroizing copy of the refresh token or client secret exists at any
/// point. Visible to the parent module so `refresh_oauth_token` can build the
/// POST body.
pub(super) fn encode_refresh_body(request: &OAuthRefreshRequest<'_>) -> Zeroizing<Vec<u8>> {
    // The spec's `scope` parameter is a space-delimited list; omitted when empty
    // (an empty scope asks for none and some servers reject it).
    let scope = request.scopes.join(" ");
    let scope = (!scope.is_empty()).then_some(scope.as_str());
    match request.encoding {
        GrantEncoding::Form => {
            // Build borrowed pairs and hand them to the shared encoder, the same
            // one the login path uses, so form escaping cannot drift between the
            // two paths — and no secret is copied to a temporary `String`.
            let mut params: Vec<(&str, &str)> = vec![
                ("grant_type", "refresh_token"),
                ("refresh_token", request.refresh_token),
            ];
            if let Some(client_id) = request.client_id {
                params.push(("client_id", client_id));
            }
            if let Some(client_secret) = request.client_secret {
                params.push(("client_secret", client_secret));
            }
            if let Some(scope) = scope {
                params.push(("scope", scope));
            }
            Zeroizing::new(choreo_shared::form::form_encode(&params).into_bytes())
        }
        GrantEncoding::Json => {
            let body = RefreshBody {
                grant_type: "refresh_token",
                refresh_token: request.refresh_token,
                client_id: request.client_id,
                client_secret: request.client_secret,
                scope,
            };
            Zeroizing::new(serde_json::to_vec(&body).unwrap_or_default())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(encoding: GrantEncoding) -> OAuthRefreshRequest<'static> {
        OAuthRefreshRequest {
            token_endpoint: "https://auth.example/token",
            encoding,
            client_id: Some("client-abc"),
            client_secret: None,
            refresh_token: "refresh-xyz",
            scopes: &[],
        }
    }

    #[test]
    fn form_encoding_serializes_a_url_encoded_body() {
        let body = encode_refresh_body(&request(GrantEncoding::Form));
        let text = std::str::from_utf8(&body).expect("utf-8 body");
        assert!(text.contains("grant_type=refresh_token"), "{text}");
        assert!(text.contains("refresh_token=refresh-xyz"), "{text}");
        assert!(text.contains("client_id=client-abc"), "{text}");
        // No scope parameter when the scope list is empty.
        assert!(!text.contains("scope="), "{text}");
    }

    #[test]
    fn form_encoding_includes_scope_and_secret_when_present() {
        let scopes = vec!["openid".to_string(), "offline_access".to_string()];
        let req = OAuthRefreshRequest {
            client_secret: Some("shh"),
            scopes: &scopes,
            ..request(GrantEncoding::Form)
        };
        let body = encode_refresh_body(&req);
        let text = std::str::from_utf8(&body).expect("utf-8 body");
        assert!(text.contains("client_secret=shh"), "{text}");
        // Scope is space-delimited and URL-encoded ('+' or '%20').
        assert!(
            text.contains("scope=openid+offline_access")
                || text.contains("scope=openid%20offline_access"),
            "{text}"
        );
    }

    #[test]
    fn json_encoding_serializes_a_json_object() {
        let body = encode_refresh_body(&request(GrantEncoding::Json));
        let value: serde_json::Value = serde_json::from_slice(&body).expect("json body");
        assert_eq!(value["grant_type"], "refresh_token");
        assert_eq!(value["refresh_token"], "refresh-xyz");
        assert_eq!(value["client_id"], "client-abc");
        assert!(value.get("scope").is_none());
    }
}
