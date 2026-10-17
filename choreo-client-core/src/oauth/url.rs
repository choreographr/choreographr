//! `application/x-www-form-urlencoded` component decoding, shared by the
//! pasted-callback-URL parser and the loopback callback server.
//!
//! A provider redirect reaches the login library two ways — as the query of a
//! pasted callback/authorize URL, or as the request line the loopback server
//! receives — and both must decode it with identical rules. Keeping the decoder
//! here means the pasted-URL path and the loopback path cannot drift: `+` maps
//! to a space, and an invalid `%` escape is kept literally rather than failing
//! the whole parse.

/// Decode an `application/x-www-form-urlencoded` query into key/value pairs,
/// percent-decoding each side. An empty pair, or one with no `=`, is skipped.
#[must_use]
pub(super) fn parse_query(query: &str) -> Vec<(String, String)> {
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
pub(super) fn percent_decode(input: &str) -> String {
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
}
