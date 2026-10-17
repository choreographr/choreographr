//! `application/x-www-form-urlencoded`: the one place an OAuth request body or
//! query is serialized from key/value pairs — and the paired decoder.
//!
//! The provider login library (code exchange, device calls, authorize query) and
//! the runtime refresh client (refresh body) all serialize the same request
//! shape, so the encoder lives here rather than hand-rolled once per path — the
//! two cannot drift into different escaping for the same input. The decoder is
//! the mirror the callback parsers use, so a value encoded by one path decodes
//! identically on another.

/// Percent-encode a single `application/x-www-form-urlencoded` value, the exact
/// escaping [`form_encode`] applies to each side of a pair.
///
/// Used to build the RFC 6749 §2.3.1 HTTP Basic credential for a confidential
/// client, whose id and secret must be url-encoded before base64 rather than
/// concatenated raw (a secret containing `:` or a space would otherwise
/// corrupt the credential).
#[must_use]
pub fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    encode_into(&mut out, value);
    out
}

/// Percent-encode key/value pairs into `application/x-www-form-urlencoded`,
/// leaving the RFC 3986 unreserved set untouched and encoding a space as `%20`.
///
/// The pairs are borrowed, so a secret (a refresh token, a code verifier) is
/// never copied into an intermediate owned map on its way to the request body —
/// the caller keeps it in a single wipe-on-drop buffer. Pairs are joined with
/// `&` and each side is `=`-separated; the encoding of a space as `%20` (rather
/// than `+`) is accepted identically by authorization servers, and `%20` is
/// unambiguous in a URL query as well as a request body.
#[must_use]
pub fn form_encode(params: &[(&str, &str)]) -> String {
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

/// Decode an `application/x-www-form-urlencoded` query into key/value pairs,
/// percent-decoding each side. An empty pair, or one with no `=`, is skipped.
///
/// The mirror of [`form_encode`]: a value percent-encoded by that function
/// decodes back to itself here. `+` maps to a space (the form convention), and
/// an invalid `%` escape is kept literally rather than failing the whole parse.
#[must_use]
pub fn parse_query(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .filter_map(|pair| {
            let (key, value) = pair.split_once('=')?;
            Some((percent_decode(key), percent_decode(value)))
        })
        .collect()
}

/// Percent-decode one URL component, mapping `+` to a space and leaving an
/// invalid `%` escape as a literal `%` (the byte sequence is kept as-is).
#[must_use]
pub fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let byte = bytes.get(i).copied().unwrap_or(0);
        match byte {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                if let (Some(hi), Some(lo)) = (
                    bytes.get(i + 1).and_then(|b| hex_value(*b)),
                    bytes.get(i + 2).and_then(|b| hex_value(*b)),
                ) {
                    out.push((hi << 4) | lo);
                    i += 3;
                } else {
                    out.push(b'%');
                    i += 1;
                }
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
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

/// The value of one hex digit, or `None` for a non-hex byte.
fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn form_encoding_matches_urlencoded_rules() {
        assert_eq!(
            form_encode(&[
                ("grant_type", "authorization_code"),
                ("scope", "openid offline_access"),
            ]),
            "grant_type=authorization_code&scope=openid%20offline_access"
        );
    }

    #[test]
    fn form_encoding_escapes_reserved_bytes() {
        assert_eq!(form_encode(&[("state", "a+b/c=d")]), "state=a%2Bb%2Fc%3Dd");
    }

    #[test]
    fn form_encoding_of_no_pairs_is_empty() {
        assert_eq!(form_encode(&[]), "");
    }

    #[test]
    fn single_value_encoding_matches_a_pair_side() {
        // The single-value encoder applies the same escaping form_encode does to
        // each side of a pair.
        assert_eq!(percent_encode("a b/c=d:e"), "a%20b%2Fc%3Dd%3Ae");
        assert_eq!(percent_encode("plain-_.~"), "plain-_.~");
    }

    #[test]
    fn percent_decode_handles_escapes_and_plus() {
        assert_eq!(percent_decode("a%20b"), "a b");
        assert_eq!(percent_decode("a+b"), "a b");
        assert_eq!(percent_decode("code%2Fwith%2Fslashes"), "code/with/slashes");
        // A stray `%` without two following hex digits is kept literally.
        assert_eq!(percent_decode("50%"), "50%");
        assert_eq!(percent_decode("%zz"), "%zz");
    }

    #[test]
    fn query_parsing_extracts_decoded_pairs() {
        let pairs = parse_query("code=abc123&state=xyz&scope=openid%20profile");
        assert_eq!(
            pairs,
            vec![
                ("code".to_string(), "abc123".to_string()),
                ("state".to_string(), "xyz".to_string()),
                ("scope".to_string(), "openid profile".to_string()),
            ]
        );
        // Empty pairs and pairs without `=` are skipped.
        assert_eq!(parse_query("&&novalue&"), Vec::<(String, String)>::new());
    }

    #[test]
    fn encode_then_decode_round_trips() {
        let value = "a+b/c=d e";
        let encoded = form_encode(&[("k", value)]);
        assert_eq!(
            parse_query(&encoded),
            vec![("k".to_string(), value.to_string())]
        );
    }
}
