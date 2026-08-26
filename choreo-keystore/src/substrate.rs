//! Polkadot account store — imports a Polkadot-JS `KeyringPairJson` keystore
//! export into a [`ServiceCredential::Substrate`] credential and rebuilds
//! credentials from raw sr25519 material.
//!
//! The Polkadot-JS keyring export format (`@polkadot/util-crypto`) stores the
//! account as a password-encrypted `pkcs8`/`sr25519` secret using scrypt key
//! derivation followed by NaCl's `crypto_secretbox` (XSalsa20-Poly1305). On
//! import we decrypt the payload, validate its fixed STRUCTURE (a DER-ish
//! header, the 64-byte expanded ed25519 secret, a static divider tag, and the
//! 32-byte public key), and verify the material is a self-consistent sr25519
//! keypair before handing it to the daemon.

use crate::{ServiceCredential, error::KeystoreError};

use base64::Engine as _;
use crypto_secretbox::{
    XSalsa20Poly1305,
    aead::{AeadInPlace, KeyInit, generic_array::GenericArray},
};
use serde::Deserialize;
use sp_core::crypto::{Ss58AddressFormat, Ss58Codec};
use sp_core::sr25519::Public as Sr25519Public;
use tracing::{debug, warn};

/// Length of the salt prepended to the encrypted keystore payload.
const SALT_LEN: usize = 32;
/// Byte offset where the scrypt/secretbox parameters begin (salt + n + p + r).
const PARAMS_LEN: usize = SALT_LEN + 12;
/// The 24-byte secretbox nonce follows the 12 bytes of scrypt parameters.
const NONCE_LEN: usize = 24;
/// Total length of the parameter block preceding the ciphertext:
/// salt(32) || n(4) || p(4) || r(4) || nonce(24) = 68 bytes.
const PREFIX_LEN: usize = PARAMS_LEN + NONCE_LEN;

/// Decrypted keystore payload layout: header(16) || secret_key(64) ||
/// div(5) || public_key(32) = 117 bytes.
const PLAINTEXT_LEN: usize = 117;

/// DER-ish ASN.1 header that must prefix a pkcs8 sr25519 secret.
const HEADER: [u8; 16] = [
    0x30, 0x53, 0x02, 0x01, 0x01, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04, 0x20,
];
/// Static divider tag that separates the expanded secret from the public key.
const DIV: [u8; 5] = [0xa1, 0x23, 0x03, 0x21, 0x00];

/// The (N, p, r) scrypt cost parameters Polkadot-JS accepts for keyring
/// exports. `N` is the CPU cost (a power of two, so `log_n` is derivable), `p`
/// the parallelisation cost and `r` the block size.
const ALLOWED_SCRYPT_PARAMS: &[(u32, u32, u32)] = &[
    (1 << 13, 10, 8),
    (1 << 14, 5, 8),
    (1 << 15, 3, 8),
    (1 << 15, 1, 8),
    (1 << 16, 2, 8),
    (1 << 17, 1, 8),
];

/// String value that `base64::engine::general_purpose::STANDARD` uses for
/// decoding — the Polkadot-JS export's `encoded` field.
const SS58_SUBSTRATE_PREFIX: u16 = 42;

/// Borrowed view of a Substrate credential's fields. Mirrors the borrowed
/// view used for the `X` variant; avoids allocating a separate struct.
#[derive(Debug, Clone, Copy)]
pub struct SubstrateCredentialView<'a> {
    pub name: &'a str,
    pub account_id: &'a str,
    pub secret: &'a [u8],
    pub public: &'a [u8],
}

/// Parsed (and only lightly validated) Polkadot-JS `KeyringPair` JSON export.
/// Unknown fields (e.g. `meta`) are ignored by serde.
#[derive(Debug, Deserialize)]
struct KeyringPairJson {
    encoded: String,
    encoding: Encoding,
    #[allow(dead_code)]
    address: String,
}

/// The `encoding` block declares the encryption scheme used to protect the
/// keypair. Polkadot-JS emits `content: ["pkcs8", "sr25519"]`,
/// `type: ["scrypt", "xsalsa20-poly1305"]` and `version: "3"`.
#[derive(Debug, Deserialize)]
struct Encoding {
    content: Vec<String>,
    #[serde(rename = "type")]
    crypto_type: Vec<String>,
    version: String,
}

/// Derive a 32-byte scrypt key from `password` with the stored (salt, n, p, r)
/// parameters. Returns `InvalidKeystoreData` if the parameters are not in the
/// Polkadot-JS allowed set.
fn derive_scrypt_key(
    password: &str,
    salt: &[u8; SALT_LEN],
    n: u32,
    p: u32,
    r: u32,
) -> Result<[u8; 32], KeystoreError> {
    // Polkadot-JS only accepts a fixed set of (N, p, r) triples; anything else
    // is rejected so we never feed arbitrary / potentially expensive params
    // into scrypt.
    let log_n = ALLOWED_SCRYPT_PARAMS
        .iter()
        .find(|&&(nn, pp, rr)| nn == n && pp == p && rr == r)
        .map(|&(nn, _, _)| nn.trailing_zeros() as u8)
        .ok_or(KeystoreError::UnsupportedKeystoreFormat)?;

    // Note: scrypt 0.12's `Params::new` takes only (log_n, r, p); the output
    // length for the derived key is fixed by the `output` buffer we pass to
    // `scrypt::scrypt` (32 bytes below).
    let params =
        scrypt::Params::new(log_n, r, p).map_err(|_| KeystoreError::InvalidKeystoreData)?;

    let mut key = [0u8; 32];
    scrypt::scrypt(password.as_bytes(), salt, &params, &mut key)
        .map_err(|_| KeystoreError::InvalidKeystoreData)?;
    debug!(log_n, p, r, "derived scrypt key");
    Ok(key)
}

/// Decrypt a NaCl secretbox payload with `key` + 24-byte `nonce`.
///
/// The Polkadot-JS ciphertext layout prepends the 16-byte Poly1305 tag to the
/// message (exactly the NaCl `crypto_secretbox` wire format). `decrypt_in_place`
/// verifies the tag then decrypts the remaining bytes in place, leaving the
/// 117-byte plaintext in the returned buffer.
fn decrypt_secretbox(
    key: &[u8; 32],
    nonce: &[u8; NONCE_LEN],
    ciphertext: &[u8],
) -> Result<Vec<u8>, KeystoreError> {
    debug!(ct_len = ciphertext.len(), "decrypting secretbox ciphertext");
    if ciphertext.len() < XSalsa20Poly1305::TAG_SIZE {
        return Err(KeystoreError::InvalidKeystoreData);
    }
    let cipher = XSalsa20Poly1305::new(GenericArray::from_slice(key));
    let nonce = GenericArray::from_slice(nonce);
    let mut buffer = ciphertext.to_vec();
    cipher
        .decrypt_in_place(nonce, b"", &mut buffer)
        .map_err(|_| KeystoreError::DecryptionFailed)?;
    Ok(buffer)
}

/// Derive the SS58-check display address (prefix 42) for a raw public key.
fn ss58_address(public: &[u8; 32]) -> Result<String, KeystoreError> {
    let pk = Sr25519Public::from(*public);
    let format = Ss58AddressFormat::from(SS58_SUBSTRATE_PREFIX);
    Ok(pk.to_ss58check_with_version(format))
}

/// Import a Polkadot-JS `KeyringPairJson` keystore export.
///
/// Decrypts `json` with `password` and returns a [`ServiceCredential::Substrate`]
/// carrying the account name, SS58 address, expanded 64-byte ed25519 secret and
/// 32-byte public key.
pub fn import_from_json(
    json: &str,
    name: &str,
    password: &str,
) -> Result<ServiceCredential, KeystoreError> {
    let pair: KeyringPairJson = serde_json::from_str(json).map_err(|e| {
        warn!(error = %e, "failed to parse Polkadot-JS keyring JSON");
        KeystoreError::InvalidKeystoreData
    })?;

    // Reject anything that is not the pkcs8/sr25519 + scrypt/xsalsa20-poly1305
    // v3 encoding we know how to decrypt.
    if pair.encoding.content.as_slice() != ["pkcs8", "sr25519"]
        || pair.encoding.crypto_type.as_slice() != ["scrypt", "xsalsa20-poly1305"]
        || pair.encoding.version != "3"
    {
        warn!(
            content = ?pair.encoding.content,
            crypto_type = ?pair.encoding.crypto_type,
            version = %pair.encoding.version,
            "unsupported keyring encoding"
        );
        return Err(KeystoreError::UnsupportedKeystoreFormat);
    }

    let decoded = base64::engine::general_purpose::STANDARD
        .decode(&pair.encoded)
        .map_err(|e| {
            warn!(error = %e, "failed to base64-decode keyring payload");
            KeystoreError::InvalidKeystoreData
        })?;
    if decoded.len() < PREFIX_LEN + XSalsa20Poly1305::TAG_SIZE {
        warn!(payload_len = decoded.len(), "keyring payload too short");
        return Err(KeystoreError::InvalidKeystoreData);
    }

    let salt: [u8; SALT_LEN] = decoded[..SALT_LEN]
        .try_into()
        .map_err(|_| KeystoreError::InvalidKeystoreData)?;
    let n = u32::from_le_bytes(
        decoded[SALT_LEN..SALT_LEN + 4]
            .try_into()
            .map_err(|_| KeystoreError::InvalidKeystoreData)?,
    );
    let p = u32::from_le_bytes(
        decoded[SALT_LEN + 4..SALT_LEN + 8]
            .try_into()
            .map_err(|_| KeystoreError::InvalidKeystoreData)?,
    );
    let r = u32::from_le_bytes(
        decoded[SALT_LEN + 8..SALT_LEN + 12]
            .try_into()
            .map_err(|_| KeystoreError::InvalidKeystoreData)?,
    );
    let nonce: [u8; NONCE_LEN] = decoded[PARAMS_LEN..PREFIX_LEN]
        .try_into()
        .map_err(|_| KeystoreError::InvalidKeystoreData)?;
    let ciphertext = &decoded[PREFIX_LEN..];

    let key = derive_scrypt_key(password, &salt, n, p, r)?;
    let plaintext = decrypt_secretbox(&key, &nonce, ciphertext)?;

    if plaintext.len() != PLAINTEXT_LEN {
        warn!(
            plaintext_len = plaintext.len(),
            "unexpected decrypted keystore length"
        );
        return Err(KeystoreError::InvalidKeystoreData);
    }

    if plaintext[..HEADER.len()] != HEADER {
        return Err(KeystoreError::InvalidKeystoreData);
    }
    let secret_key: [u8; 64] = plaintext[16..80]
        .try_into()
        .map_err(|_| KeystoreError::InvalidKeystoreData)?;
    if plaintext[80..85] != DIV {
        return Err(KeystoreError::InvalidKeystoreData);
    }
    let public_key: [u8; 32] = plaintext[85..117]
        .try_into()
        .map_err(|_| KeystoreError::InvalidKeystoreData)?;

    // Cross-check that the expanded ed25519 secret genuinely derives the
    // claimed public key — an integrity failure here would silently produce a
    // credential that signs with a different key than the address advertises.
    let secret = schnorrkel::SecretKey::from_ed25519_bytes(&secret_key)
        .map_err(|_| KeystoreError::InvalidKeystoreData)?;
    if secret.to_public().to_bytes() != public_key {
        return Err(KeystoreError::InvalidKeystoreData);
    }

    // Cross-check that the export's advertised `address` is the SS58 encoding
    // of that public key. A mismatched `address` (a paste error or a tampered
    // export) would otherwise yield a credential whose stored `account_id`
    // does not match the key that actually signs. `from_raw` derives the
    // address from the public key for exactly this reason.
    let derived_address = ss58_address(&public_key)?;
    if derived_address != pair.address {
        warn!(
            advertised = %pair.address,
            derived = %derived_address,
            "keystore address does not match the derived public key"
        );
        return Err(KeystoreError::InvalidKeystoreData);
    }

    debug!(
        account_id = %pair.address,
        "imported Polkadot keyring credential"
    );

    Ok(ServiceCredential::Substrate {
        name: name.to_owned(),
        account_id: pair.address,
        secret: secret_key.to_vec(),
        public: public_key.to_vec(),
    })
}

/// Build a Substrate credential from raw sr25519 material.
///
/// Validates that `public` actually derives from `secret` and that the SS58
/// address is a valid prefix-42 encoding of that public key.
pub fn from_raw(
    name: &str,
    secret: &[u8; 64],
    public: &[u8; 32],
) -> Result<ServiceCredential, KeystoreError> {
    let secret_key = schnorrkel::SecretKey::from_ed25519_bytes(secret)
        .map_err(|_| KeystoreError::InvalidKeystoreData)?;
    if secret_key.to_public().to_bytes() != *public {
        return Err(KeystoreError::InvalidKeystoreData);
    }
    let account_id = ss58_address(public)?;

    debug!(account_id = %account_id, "built Substrate credential from raw material");

    Ok(ServiceCredential::Substrate {
        name: name.to_owned(),
        account_id,
        secret: secret.to_vec(),
        public: public.to_vec(),
    })
}

/// Return the stored SS58 `account_id` for a Substrate credential.
///
/// Returns the empty string for any non-Substrate credential (the caller is
/// expected to have already verified the variant via the `as_substrate`
/// helper).
pub fn credential_address(cred: &ServiceCredential) -> String {
    match cred {
        ServiceCredential::Substrate { account_id, .. } => account_id.clone(),
        _ => {
            warn!(credential = %cred, "credential_address called on non-Substrate credential");
            String::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::Rng as _;

    // Polkadot-JS keyring export for the well-known sr25519 "Alice" dev
    // account (password `whoisalice`).
    const ALICE_JSON: &str = r#"{
        "encoded": "DumgApKCTqoCty1OZW/8WS+sgo6RdpHhCwAkA2IoDBMAgAAAAQAAAAgAAAB6IG/q24EeVf0JqWqcBd5m2tKq5BlyY84IQ8oamLn9DZe9Ouhgunr7i36J1XxUnTI801axqL/ym1gil0U8440Qvj0lFVKwGuxq38zuifgoj0B3Yru0CI6QKEvQPU5xxj4MpyxdSxP+2PnTzYao0HDH0fulaGvlAYXfqtU89xrx2/z9z7IjSwS3oDFPXRQ9kAdDebtyCVreZ9Otw9v3",
        "encoding": {
            "content": ["pkcs8", "sr25519"],
            "type": ["scrypt", "xsalsa20-poly1305"],
            "version": "3"
        },
        "address": "5GrwvaEF5zXb26Fz9rcQpDWS57CtERHpNehXCPcNoHGKutQY",
        "meta": { "name": "Alice", "source": "development" }
    }"#;

    // Polkadot-JS keyring export for the well-known sr25519 "Bob" dev
    // account (password `whoisbob`).
    const BOB_JSON: &str = r#"{
        "encoded": "J2FFcPHAY11Pmq/38eqbwfUv9OPitYJs+oYgahBvlagAAAIAAQAAAAgAAAB5o0DwXCWDblsH+9pc++RaBO4fpHBHzUirHFHFE9yS3sDzgAIQjhgvPqJ3ODrMR2gy7vk0VZg1fyirIvmsrfjGbWnOI8YU0joX0tYytroyWaykFKtZJMmE0pNKcJ5dJmDxscbK53Ac+7ld2UdH07yKPXxmPuYNNw3vKx8cg9CdQgifKfzQxHnC+EUpOoHPLwGlHsFEYtIlQtngqd9n",
        "encoding": {
            "content": ["pkcs8", "sr25519"],
            "type": ["scrypt", "xsalsa20-poly1305"],
            "version": "3"
        },
        "address": "5CfWTDh7XxJ2yrayqQ2aJnnZAH5v5XaF1oJFfH5QCpbfP9v8",
        "meta": { "name": "Bob", "source": "development" }
    }"#;

    #[test]
    fn import_alice_from_json() {
        let cred = import_from_json(ALICE_JSON, "main", "whoisalice").expect("import Alice");
        let view = cred.as_substrate().expect("expected Substrate view");
        assert_eq!(
            view.account_id,
            "5GrwvaEF5zXb26Fz9rcQpDWS57CtERHpNehXCPcNoHGKutQY"
        );
        assert_eq!(view.secret.len(), 64, "expanded secret must be 64 bytes");
        assert_eq!(view.public.len(), 32, "public key must be 32 bytes");
        assert_eq!(view.name, "main");

        // The stored public key must round-trip back to the same SS58 address.
        let public_key: [u8; 32] = view.public.try_into().expect("32-byte public");
        assert_eq!(ss58_address(&public_key).unwrap(), view.account_id);
    }

    #[test]
    fn import_bob_from_json() {
        let cred = import_from_json(BOB_JSON, "bob", "whoisbob").expect("import Bob");
        let view = cred.as_substrate().expect("expected Substrate view");
        assert_eq!(
            view.account_id,
            "5CfWTDh7XxJ2yrayqQ2aJnnZAH5v5XaF1oJFfH5QCpbfP9v8"
        );
        assert_eq!(view.secret.len(), 64);
        assert_eq!(view.public.len(), 32);
    }

    #[test]
    fn import_rejects_wrong_password() {
        let result = import_from_json(ALICE_JSON, "main", "not-whoisalice");
        assert!(
            matches!(result, Err(KeystoreError::DecryptionFailed)),
            "expected DecryptionFailed, got {:?}",
            result
        );
    }

    #[test]
    fn import_rejects_mismatched_address() {
        // A valid keyring ciphertext whose advertised `address` does not match
        // the public key encoded in the decrypted payload must be rejected so
        // the credential's account id always corresponds to the signing key.
        let json = ALICE_JSON.replace(
            "5GrwvaEF5zXb26Fz9rcQpDWS57CtERHpNehXCPcNoHGKutQY",
            "5CfWTDh7XxJ2yrayqQ2aJnnZAH5v5XaF1oJFfH5QCpbfP9v8",
        );
        let result = import_from_json(&json, "main", "whoisalice");
        assert!(
            matches!(result, Err(KeystoreError::InvalidKeystoreData)),
            "expected InvalidKeystoreData for a mismatched address, got {:?}",
            result
        );
    }

    #[test]
    fn import_rejects_unsupported_encoding() {
        let json = r#"{
            "encoded": "AAAA",
            "encoding": {
                "content": ["pkcs8", "sr25519"],
                "type": ["scrypt", "ecdsa"],
                "version": "3"
            },
            "address": "5GrwvaEF5zXb26Fz9rcQpDWS57CtERHpNehXCPcNoHGKutQY"
        }"#;
        let result = import_from_json(json, "main", "whoisalice");
        assert!(matches!(
            result,
            Err(KeystoreError::UnsupportedKeystoreFormat)
        ));
    }

    // Generate a guaranteed-valid sr25519 (secret_key, public_key) pair.
    // `SecretKey::from_ed25519_bytes` on arbitrary random bytes often fails
    // (a random 256-bit value is not always a canonical scalar), so we derive
    // the expanded secret from a random mini-secret instead, which always
    // produces a valid key.
    fn gen_valid_secret() -> ([u8; 64], [u8; 32]) {
        let mut mini = [0u8; 32];
        rand::rng().fill_bytes(&mut mini);
        let mini_key = schnorrkel::MiniSecretKey::from_bytes(&mini).expect("valid mini secret");
        let secret_key = mini_key.expand(schnorrkel::ExpansionMode::Ed25519);
        (
            secret_key.to_ed25519_bytes(),
            secret_key.to_public().to_bytes(),
        )
    }

    #[test]
    fn from_raw_round_trip() {
        let (secret_seed, public) = gen_valid_secret();

        let cred = from_raw("fresh", &secret_seed, &public).expect("from_raw should succeed");
        let view = cred.as_substrate().expect("expected Substrate view");
        assert_eq!(view.name, "fresh");
        assert_eq!(view.secret, secret_seed.as_slice());
        assert_eq!(view.public, public.as_slice());
        // The derived address must be a valid prefix-42 ss58 address.
        let parsed = Sr25519Public::from_ss58check(view.account_id).expect("valid ss58");
        let parsed_bytes: &[u8] = parsed.as_ref();
        assert_eq!(parsed_bytes, public.as_slice());
    }

    #[test]
    fn from_raw_rejects_mismatched_public() {
        let (secret_seed, mut public) = gen_valid_secret();
        public[0] ^= 0xff; // corrupt one byte -> no longer derived from secret

        let result = from_raw("bad", &secret_seed, &public);
        assert!(matches!(result, Err(KeystoreError::InvalidKeystoreData)));
    }

    #[test]
    fn credential_address_returns_account_id() {
        let cred = import_from_json(ALICE_JSON, "main", "whoisalice").expect("import Alice");
        assert_eq!(
            credential_address(&cred),
            "5GrwvaEF5zXb26Fz9rcQpDWS57CtERHpNehXCPcNoHGKutQY"
        );
    }

    #[test]
    fn decrypts_input_vectors_round_trip_length() {
        // Sanity-check the base64 round trip we rely on for the vectors above.
        let decoded = base64::engine::general_purpose::STANDARD
            .decode("DumgApKCTqoCty1OZW/8WS+sgo6RdpHhCwAkA2IoDBMAgAAAAQAAAAgAAAB6IG/q24EeVf0JqWqcBd5m2tKq5BlyY84IQ8oamLn9DZe9Ouhgunr7i36J1XxUnTI801axqL/ym1gil0U8440Qvj0lFVKwGuxq38zuifgoj0B3Yru0CI6QKEvQPU5xxj4MpyxdSxP+2PnTzYao0HDH0fulaGvlAYXfqtU89xrx2/z9z7IjSwS3oDFPXRQ9kAdDebtyCVreZ9Otw9v3")
            .expect("valid base64");
        // 68-byte params prefix + 16-byte tag + 117-byte plaintext.
        assert_eq!(decoded.len(), 68 + 16 + 117);
    }
}
