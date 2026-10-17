//! PKCE (RFC 7636) and the OAuth `state` value for the provider login flow.
//!
//! Only the S256 method is offered: a plain `code_challenge` is a weaker
//! substitute (the verifier would ride the authorize URL verbatim), and every
//! provider Choreographr targets accepts S256. The verifier is a fresh 32-byte
//! CSPRNG value, base64url-encoded without padding (43 characters), and the
//! challenge is the base64url of its SHA-256 digest — the login library's whole
//! cryptographic surface, since the `id_token` is trusted by TLS provenance
//! from the token endpoint rather than by signature verification.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq as _;

/// Bytes of CSPRNG entropy in a PKCE verifier or a `state` value. 32 bytes
/// (256 bits) comfortably clears RFC 7636's 32-character minimum for the
/// verifier and makes a `state` collision or forgery infeasible.
const ENTROPY_BYTES: usize = 32;

/// Generate a fresh PKCE `code_verifier`: 32 CSPRNG bytes as 43 base64url
/// characters (no padding).
#[must_use]
pub fn generate_verifier() -> String {
    let bytes: [u8; ENTROPY_BYTES] = rand::random();
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Derive the S256 `code_challenge` for `verifier`: base64url(SHA-256(verifier)),
/// no padding.
#[must_use]
pub fn challenge_s256(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    URL_SAFE_NO_PAD.encode(digest)
}

/// Generate a fresh random `state` value (CSRF correlation between the
/// authorize request and the callback).
#[must_use]
pub fn generate_state() -> String {
    let bytes: [u8; ENTROPY_BYTES] = rand::random();
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Whether a callback's returned `state` matches the value the flow sent.
///
/// A mismatch is a CSRF signal (an attacker-supplied callback), so the caller
/// treats it as a hard failure and never exchanges the accompanying code. The
/// comparison is constant-time (`subtle`'s [`ConstantTimeEq`](subtle::ConstantTimeEq),
/// which also fails an unequal-length pair) to avoid leaking the expected value
/// byte by byte; `state` is not a secret, but the cost is negligible.
#[must_use]
pub fn state_matches(expected: &str, returned: &str) -> bool {
    bool::from(expected.as_bytes().ct_eq(returned.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn challenge_matches_the_rfc7636_s256_example() {
        // RFC 7636 Appendix B: verifier "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"
        // has challenge "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM".
        let challenge = challenge_s256("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk");
        assert_eq!(challenge, "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    #[test]
    fn verifier_and_state_are_fresh_and_unpadded_base64url() {
        let a = generate_verifier();
        let b = generate_verifier();
        assert_ne!(a, b, "two verifiers must not collide");
        assert_eq!(a.len(), 43, "32 bytes of base64url is 43 chars");
        assert!(!a.contains('='), "padding must be stripped");
        assert!(
            a.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        );

        let state = generate_state();
        assert_eq!(state.len(), 43);
        assert_ne!(generate_state(), state);
    }

    #[test]
    fn state_comparison_accepts_only_the_exact_value() {
        assert!(state_matches("abc123", "abc123"));
        assert!(!state_matches("abc123", "abc124"));
        assert!(!state_matches("abc123", "abc12"));
        assert!(!state_matches("abc123", ""));
    }
}
