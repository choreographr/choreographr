//! JWT claim parsing for the identity values an OAuth grant needs.
//!
//! The login library reads a small, fixed set of claims from an `id_token` (or,
//! as a fallback, the `access_token`) to fill the credential's durable identity
//! fields — the `ChatGPT` account id, the plan type, and the email. **No
//! signature verification is performed**: the token arrived over TLS directly
//! from the provider's token endpoint, so its provenance is trusted and only
//! the claims are needed. The raw JWT is never stored (see the credential
//! model); only the extracted claim values are.
//!
//! Every function here is total and returns `Option`: a malformed token, a
//! missing segment, or a missing claim yields `None` rather than an error, so a
//! provider that omits a claim degrades to "no value" instead of failing the
//! whole login.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::Value;

/// The `OpenAI` auth namespace that carries the `ChatGPT` identity claims. The
/// `chatgpt_account_id` lives under `https://api.openai.com/auth`; some
/// providers carry it only in the access token rather than the `id_token`, which
/// is why the caller tries the `id_token` first and this fallback second.
const OPENAI_AUTH_NAMESPACE: &str = "https://api.openai.com/auth";

/// The `OpenAI` profile namespace carrying the email claim.
const OPENAI_PROFILE_NAMESPACE: &str = "https://api.openai.com/profile";

/// Decode a JWT's JSON claims payload (`header.payload.signature`), or `None`
/// when the token is malformed. The signature segment is ignored.
#[must_use]
pub fn decode_claims(jwt: &str) -> Option<Value> {
    // A JWT has three dot-separated base64url segments; only the payload
    // (segment 1) is decoded. `split` (not `splitn`) so a token with the wrong
    // segment count is rejected rather than silently accepted.
    let mut segments = jwt.split('.');
    let _header = segments.next()?;
    let payload = segments.next()?;
    segments.next()?;
    // A fourth segment (or a trailing dot) is not a JWT.
    if segments.next().is_some() {
        return None;
    }
    let decoded = URL_SAFE_NO_PAD.decode(payload).ok()?;
    serde_json::from_slice(&decoded).ok()
}

/// Extract the `ChatGPT` account id (`chatgpt_account_id`) from a decoded JWT
/// claims object: `["https://api.openai.com/auth"]["chatgpt_account_id"]`.
#[must_use]
pub fn chatgpt_account_id(claims: &Value) -> Option<String> {
    claims
        .get(OPENAI_AUTH_NAMESPACE)?
        .get("chatgpt_account_id")?
        .as_str()
        .map(str::to_string)
}

/// Extract the `ChatGPT` plan type (`chatgpt_plan_type`), e.g. `plus`/`pro`.
#[must_use]
pub fn chatgpt_plan_type(claims: &Value) -> Option<String> {
    claims
        .get(OPENAI_AUTH_NAMESPACE)?
        .get("chatgpt_plan_type")?
        .as_str()
        .map(str::to_string)
}

/// Extract the account email: the profile namespace's `email` when present,
/// else a top-level `email` claim (OIDC providers place it at the root).
#[must_use]
pub fn email(claims: &Value) -> Option<String> {
    claims
        .get(OPENAI_PROFILE_NAMESPACE)
        .and_then(|profile| profile.get("email"))
        .or_else(|| claims.get("email"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a JWT with the given JSON claims payload (header/signature are
    /// irrelevant dummies since neither is verified).
    fn jwt_with_claims(claims: &Value) -> String {
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"none","typ":"JWT"}"#);
        let payload = URL_SAFE_NO_PAD.encode(claims.to_string().as_bytes());
        format!("{header}.{payload}.sig")
    }

    #[test]
    fn well_formed_token_claims_are_extracted() {
        let claims = serde_json::json!({
            "https://api.openai.com/auth": {
                "chatgpt_account_id": "acct_123",
                "chatgpt_plan_type": "plus",
            },
            "https://api.openai.com/profile": { "email": "user@example.com" },
        });
        let token = jwt_with_claims(&claims);
        let decoded = decode_claims(&token).expect("claims decode");
        assert_eq!(chatgpt_account_id(&decoded).as_deref(), Some("acct_123"));
        assert_eq!(chatgpt_plan_type(&decoded).as_deref(), Some("plus"));
        assert_eq!(email(&decoded).as_deref(), Some("user@example.com"));
    }

    #[test]
    fn top_level_email_is_a_fallback() {
        let token = jwt_with_claims(&serde_json::json!({ "email": "root@example.com" }));
        let decoded = decode_claims(&token).expect("claims decode");
        assert_eq!(email(&decoded).as_deref(), Some("root@example.com"));
    }

    #[test]
    fn malformed_tokens_are_rejected() {
        assert!(decode_claims("").is_none());
        assert!(decode_claims("only-one-segment").is_none());
        assert!(decode_claims("a.b").is_none(), "two segments is not a JWT");
        assert!(
            decode_claims("a.b.c.d").is_none(),
            "four segments is not a JWT"
        );
        // A payload that is not base64url JSON.
        assert!(decode_claims("a.!!!.c").is_none());
        assert!(
            decode_claims("a.bm90LWpzb24.c").is_none(),
            "payload not JSON"
        );
    }

    #[test]
    fn missing_claims_yield_none() {
        let token = jwt_with_claims(&serde_json::json!({ "sub": "x" }));
        let decoded = decode_claims(&token).expect("claims decode");
        assert!(chatgpt_account_id(&decoded).is_none());
        assert!(chatgpt_plan_type(&decoded).is_none());
        assert!(email(&decoded).is_none());
    }

    #[test]
    fn non_string_claim_is_ignored() {
        let token = jwt_with_claims(&serde_json::json!({
            "https://api.openai.com/auth": { "chatgpt_account_id": 42 },
        }));
        let decoded = decode_claims(&token).expect("claims decode");
        assert!(chatgpt_account_id(&decoded).is_none());
    }
}
