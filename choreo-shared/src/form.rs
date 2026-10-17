//! `application/x-www-form-urlencoded` encoding: the one place an OAuth request
//! body or query is serialized from key/value pairs.
//!
//! The provider login library (code exchange, device calls, authorize query) and
//! the runtime refresh client (refresh body) all serialize the same request
//! shape, so the encoder lives here rather than hand-rolled once per path — the
//! two cannot drift into different escaping for the same input.

/// Percent-encode key/value pairs into `application/x-www-form-urlencoded`,
/// leaving the RFC 3986 unreserved set untouched and encoding a space as `%20`.
///
/// Pairs are joined with `&` and each side is `=`-separated; the encoding of a
/// space as `%20` (rather than `+`) is accepted identically by authorization
/// servers, and `%20` is unambiguous in a URL query as well as a request body.
#[must_use]
pub fn form_encode(params: &[(String, String)]) -> String {
    let mut out = String::new();
    for (i, (key, value)) in params.iter().enumerate() {
        if i > 0 {
            out.push('&');
        }
        encode_into(&mut out, key);
        out.push('=');
        encode_into(&mut out, value);
    }
    out
}

/// Append `value` percent-encoded per `application/x-www-form-urlencoded`.
fn encode_into(out: &mut String, value: &str) {
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(char::from(byte));
            }
            b' ' => out.push_str("%20"),
            _ => {
                out.push('%');
                out.push(hex_digit(byte >> 4));
                out.push(hex_digit(byte & 0x0f));
            }
        }
    }
}

/// The uppercase hex digit for a nibble.
fn hex_digit(nibble: u8) -> char {
    char::from_digit(u32::from(nibble & 0x0f), 16).map_or('0', |c| c.to_ascii_uppercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn form_encoding_matches_urlencoded_rules() {
        let pairs = vec![
            ("grant_type".to_string(), "authorization_code".to_string()),
            ("scope".to_string(), "openid offline_access".to_string()),
        ];
        assert_eq!(
            form_encode(&pairs),
            "grant_type=authorization_code&scope=openid%20offline_access"
        );
    }

    #[test]
    fn form_encoding_escapes_reserved_bytes() {
        let pairs = vec![("state".to_string(), "a+b/c=d".to_string())];
        assert_eq!(form_encode(&pairs), "state=a%2Bb%2Fc%3Dd");
    }

    #[test]
    fn form_encoding_of_no_pairs_is_empty() {
        assert_eq!(form_encode(&[]), "");
    }
}
