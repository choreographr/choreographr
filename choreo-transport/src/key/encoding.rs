//! The textual form of a transport key: standard-alphabet base64.
//!
//! One encoding serves every key this crate reads or writes — the public key an
//! operator copies out of `choreographr pubkey`, the ACL entry that enrolls a
//! client, the `known_servers.toml` pin, and the on-disk secret. A 32-byte key
//! is exactly 44 base64 characters, so a key has one spelling and comparing two
//! of them is comparing two strings.
//!
//! Space-grouped "fingerprint" renderings are deliberately absent: regrouping
//! the same bytes adds no security property and gives one key two spellings that
//! a paste can silently mix.

use base64::Engine as _;
use zeroize::Zeroizing;

use super::TransportSecretKey;
use crate::error::TransportError;

/// Encode a 32-byte key as standard-alphabet base64 (44 characters).
///
/// This is the canonical — and only — textual form of a transport public key:
/// the ACL stores pubkeys in exactly this form, and the value an operator reads
/// out of `choreographr pubkey` is byte-for-byte this string.
#[must_use]
pub fn pubkey_base64(pk: &[u8; 32]) -> String {
    base64::engine::general_purpose::STANDARD.encode(pk)
}

/// Parse a base64-encoded 32-byte key, trimming surrounding whitespace.
///
/// The inverse of [`pubkey_base64`], and tolerant of the trailing newline the
/// on-disk form carries.
///
/// Only the canonical form is accepted: the value must decode to exactly 32
/// bytes. A space-grouped key is not base64 and is refused with a message that
/// says so, so an operator who pasted a grouped value is told to send the plain
/// form rather than silently connecting to nothing.
///
/// # Errors
///
/// Returns [`TransportError::Io`] (with [`std::io::ErrorKind::InvalidData`])
/// when the text is not valid base64 or does not decode to exactly 32 bytes.
pub fn parse_pubkey_base64(text: &str) -> Result<[u8; 32], TransportError> {
    let key = decode_key_base64(text).map_err(|detail| {
        TransportError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("invalid base64 key: {detail}"),
        ))
    })?;
    // A public key is not secret, so the copy out of the wiping wrapper is the
    // caller's own array and needs no further care.
    Ok(*key)
}

/// Decode standard-alphabet base64 key text into exactly 32 bytes.
///
/// The single decode implementation behind both the public key parser and the
/// secret-file loader. The decoded bytes may be a SECRET (the loader's caller),
/// so they are handed back inside a [`Zeroizing`] buffer and never exist as a
/// bare array on this path; the intermediate `Vec` the decoder fills is wiped
/// too. The failure is a message rather than an error type so each caller wraps
/// it in the error its context calls for (a public key parse is a plain
/// I/O-shaped error; a secret file names its path and refuses regeneration).
/// Messages carry lengths and decoder positions only — never the decoded bytes.
fn decode_key_base64(text: &str) -> Result<Zeroizing<[u8; 32]>, String> {
    // `decode_vec` fills a caller-provided buffer, so the decoder's output can be
    // a `Zeroizing` from the first byte rather than a `Vec` that is copied out
    // and dropped unwiped.
    let mut decoded = Zeroizing::new(Vec::new());
    base64::engine::general_purpose::STANDARD
        .decode_vec(text.trim(), &mut decoded)
        .map_err(|e| format!("not valid base64: {e}"))?;
    let len = decoded.len();
    // The conversion copies into a fixed array, which is then MOVED into the
    // wiping wrapper, so the only unwiped lifetime is the freed `decoded`
    // allocation — and that one is a `Zeroizing` too.
    let key: [u8; 32] = decoded
        .as_slice()
        .try_into()
        .map_err(|_| format!("decodes to {len} bytes, expected 32"))?;
    Ok(Zeroizing::new(key))
}

/// Encode a secret key for on-disk storage: standard base64 plus a trailing
/// newline, so the file is a well-formed text file (the loader trims it).
///
/// The result is [`Zeroizing`] because base64 of a secret IS the secret: a plain
/// `String` would be freed un-erased the moment the write returned.
pub(super) fn encode_secret(sk: &[u8; 32]) -> Zeroizing<String> {
    let mut text = base64::engine::general_purpose::STANDARD.encode(sk);
    text.push('\n');
    Zeroizing::new(text)
}

/// Decode secret key file contents as either the legacy raw 32-byte form or the
/// current base64 text form.
///
/// A file of exactly 32 bytes IS the legacy raw key. The base64 form is 44
/// characters plus a newline (45 bytes), and a 32-character base64 string would
/// decode to 24 bytes — so the raw length can never be mistaken for text.
///
/// Returns the [`TransportSecretKey`] itself, so the decoded secret has exactly
/// one representation and it is always a wiping one.
pub(super) fn decode_secret(bytes: &[u8]) -> Result<TransportSecretKey, String> {
    if bytes.len() == 32 {
        // Exactly the raw form's length: convert rather than copy, so a
        // mismatch is an error instead of a panic (it cannot happen — the
        // length was just checked — but the no-panic policy should not rest on
        // a reviewer noticing that).
        let raw: [u8; 32] = <[u8; 32]>::try_from(bytes)
            .map_err(|_| "32-byte raw key did not convert".to_string())?;
        return Ok(TransportSecretKey::from(raw));
    }
    let len = bytes.len();
    let text = std::str::from_utf8(bytes).map_err(|_| {
        format!("{len} bytes of non-UTF-8 data, expected 32 raw bytes or base64 text")
    })?;
    let key = decode_key_base64(text).map_err(|detail| format!("{len}-byte file: {detail}"))?;
    Ok(TransportSecretKey::from(*key))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The canonical base64 helpers round-trip and reject malformed input.
    #[test]
    fn pubkey_base64_round_trips_and_rejects_malformed_input() {
        let pk = [0xA5u8; 32];
        let text = pubkey_base64(&pk);
        assert_eq!(text.len(), 44);
        assert_eq!(parse_pubkey_base64(&text).expect("decode"), pk);

        // The on-disk form carries a trailing newline; whitespace is trimmed.
        assert_eq!(
            parse_pubkey_base64(&format!("{text}\n")).expect("decode"),
            pk
        );

        assert!(parse_pubkey_base64("not base64!!").is_err());
        // Valid base64 of the wrong length decodes but must not be accepted.
        // The error must say so (lengths, never bytes) so an operator knows the
        // value was a well-formed key of the wrong size, not a typo.
        let short_err = parse_pubkey_base64("AAAA").expect_err("16 bits is not a key");
        assert!(
            short_err.to_string().contains("expected 32"),
            "a wrong-length key must name the expected length: {short_err}"
        );
    }

    /// Only the canonical 44-character base64 form parses. A space-grouped
    /// value (the grouping some tools add for human comparison) is NOT base64
    /// and is refused — the project has exactly one textual key form, so the
    /// parser must not paper over a grouped paste.
    #[test]
    fn parse_pubkey_base64_rejects_grouped_and_wrong_length_values() {
        let pk = [0xA5u8; 32];
        let canonical = pubkey_base64(&pk);
        assert_eq!(canonical.len(), 44, "a 32-byte key is 44 base64 chars");

        // A group-of-four rendering of the SAME key must be refused, even
        // though it carries the right characters — the spaces make it invalid
        // base64, and the parser accepts only the canonical form.
        let grouped: String = canonical
            .as_bytes()
            .chunks(4)
            .map(|c| String::from_utf8_lossy(c).into_owned())
            .collect::<Vec<_>>()
            .join(" ");
        let grouped_err = parse_pubkey_base64(&grouped).expect_err("grouped is not base64");
        assert!(
            grouped_err.to_string().contains("not valid base64"),
            "a grouped value must be reported as invalid base64: {grouped_err}"
        );

        // Too few / too many decoded bytes are both refused.
        assert!(parse_pubkey_base64("AAAA").is_err(), "4 chars is 3 bytes");
        assert!(
            parse_pubkey_base64(&format!("{canonical}AAAA")).is_err(),
            "48 chars is more than 32 bytes"
        );
    }

    /// The secret encoding is the same base64 plus the trailing newline the
    /// text-file form carries, and both the current and the legacy raw form
    /// decode to the same key.
    #[test]
    fn secret_encoding_round_trips_and_accepts_the_legacy_raw_form() {
        let sk = [0x11u8; 32];

        let encoded = encode_secret(&sk);
        assert_eq!(encoded.len(), 45, "44 base64 chars plus one newline");
        assert_eq!(encoded.as_bytes().last(), Some(&b'\n'));
        assert_eq!(
            decode_secret(encoded.as_bytes())
                .expect("decode")
                .as_bytes(),
            &sk
        );

        // The legacy raw form: exactly 32 bytes IS the key, not text that
        // happens to be 32 bytes long.
        assert_eq!(decode_secret(&sk).expect("decode raw").as_bytes(), &sk);

        // Content that decodes to neither form is refused, with the length in
        // the message (never the bytes).
        let err = decode_secret(b"this is not a transport secret key\n").expect_err("garbage");
        assert!(err.contains("not valid base64"), "{err}");
        let err = decode_secret(&[0xFFu8; 40]).expect_err("non-UTF-8");
        assert!(err.contains("40 bytes of non-UTF-8"), "{err}");
    }
}
