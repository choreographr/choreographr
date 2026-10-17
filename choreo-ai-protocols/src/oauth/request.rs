//! The OAuth refresh **request** side: the parameters a token-endpoint exchange
//! carries and their serialization.
//!
//! The request holds two secrets (the durable refresh token and, for a
//! confidential client, the client secret), so it has a redacting
//! [`Debug`](std::fmt::Debug) and its encoded body is wipe-on-drop. Encoding is
//! kept apart from the HTTP call so the form-vs-JSON contract is unit testable
//! without a socket.

use zeroize::Zeroizing;

use crate::catalog::GrantEncoding;

/// A provider OAuth refresh request: everything the exchange needs, derived
/// from the stored credential and the provider's catalog `oauth` profile.
///
/// The `token_endpoint` is the absolute token URL (issuer + token path); the
/// `encoding` is the profile's `refresh_encoding`. `client_secret` is present
/// only for a confidential client.
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

/// Serialize a refresh request body per the endpoint's encoding.
///
/// Kept separate from the HTTP call so the form-vs-JSON contract is unit
/// testable without a socket. The returned body holds the refresh token, so it
/// is wipe-on-drop. Visible to the parent module so `refresh_oauth_token` can
/// build the POST body.
pub(super) fn encode_refresh_body(request: &OAuthRefreshRequest<'_>) -> Zeroizing<Vec<u8>> {
    // The spec's `scope` parameter is a space-delimited list; omitted when empty
    // (an empty scope asks for none and some servers reject it).
    let scope = request.scopes.join(" ");
    match request.encoding {
        GrantEncoding::Form => {
            let mut serializer = url::form_urlencoded::Serializer::new(String::new());
            serializer.append_pair("grant_type", "refresh_token");
            serializer.append_pair("refresh_token", request.refresh_token);
            if let Some(client_id) = request.client_id {
                serializer.append_pair("client_id", client_id);
            }
            if let Some(client_secret) = request.client_secret {
                serializer.append_pair("client_secret", client_secret);
            }
            if !scope.is_empty() {
                serializer.append_pair("scope", &scope);
            }
            Zeroizing::new(serializer.finish().into_bytes())
        }
        GrantEncoding::Json => {
            // Build the JSON object field-by-field; `serde_json::json!` would
            // copy the secrets into an intermediate `Value`, so a small manual
            // map keeps them in one place that zeroizes.
            let mut map = serde_json::Map::new();
            map.insert("grant_type".into(), "refresh_token".into());
            map.insert("refresh_token".into(), request.refresh_token.into());
            if let Some(client_id) = request.client_id {
                map.insert("client_id".into(), client_id.into());
            }
            if let Some(client_secret) = request.client_secret {
                map.insert("client_secret".into(), client_secret.into());
            }
            if !scope.is_empty() {
                map.insert("scope".into(), scope.into());
            }
            Zeroizing::new(
                serde_json::to_vec(&serde_json::Value::Object(map)).unwrap_or_else(|_| Vec::new()),
            )
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
