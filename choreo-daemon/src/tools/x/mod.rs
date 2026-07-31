use choreo_keystore::{ServiceCredential, XCredentialView};

mod post;
mod search_recent;
mod user_lookup;

pub(crate) use post::XPost;
pub(crate) use search_recent::XSearchRecent;
pub(crate) use user_lookup::XUserLookup;

fn get_x_credentials(x_credentials: Option<&ServiceCredential>) -> Option<XCredentialView<'_>> {
    x_credentials.and_then(ServiceCredential::as_x)
}

const X_API_BASE: &str = "https://api.twitter.com";

fn urlencode(s: &str) -> String {
    let mut result = String::with_capacity(s.len() * 3);
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                result.push(byte as char);
            }
            _ => {
                result.push_str(&format!("%{:02X}", byte));
            }
        }
    }
    result
}

fn hmac_sha1(key: &[u8], data: &str) -> String {
    use hmac::{Hmac, KeyInit, Mac};
    use sha1::Sha1;
    let mut key_buf = [0u8; 64];
    if !key.is_empty() {
        let len = key.len().min(64);
        key_buf[..len].copy_from_slice(&key[..len]);
    }
    let mut mac = Hmac::<Sha1>::new((&key_buf).into());
    mac.update(data.as_bytes());
    let result = mac.finalize();
    base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        result.into_bytes().as_slice(),
    )
}

fn build_oauth1_header(
    method: &str,
    url: &str,
    creds: &XCredentialView<'_>,
    params: &[(&str, &str)],
) -> String {
    use std::time::{SystemTime, UNIX_EPOCH};

    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .to_string();

    let nonce: String = (0..32)
        .map(|_| {
            let b: u8 = rand::random();
            format!("{:02x}", b)
        })
        .collect();

    let mut oauth_params: Vec<(String, String)> = vec![
        ("oauth_consumer_key".to_string(), creds.api_key.to_string()),
        ("oauth_nonce".to_string(), nonce),
        (
            "oauth_signature_method".to_string(),
            "HMAC-SHA1".to_string(),
        ),
        ("oauth_timestamp".to_string(), timestamp),
        ("oauth_token".to_string(), creds.access_token.to_string()),
        ("oauth_version".to_string(), "1.0".to_string()),
    ];

    let mut all_params: Vec<(String, String)> = oauth_params.clone();
    for (k, v) in params {
        all_params.push((k.to_string(), v.to_string()));
    }

    all_params.sort_by(|a, b| {
        let key_cmp = a.0.cmp(&b.0);
        if key_cmp.is_eq() {
            a.1.cmp(&b.1)
        } else {
            key_cmp
        }
    });

    let param_string = all_params
        .iter()
        .map(|(k, v)| format!("{}={}", urlencode(k), urlencode(v)))
        .collect::<Vec<_>>()
        .join("&");

    let signature_base = format!(
        "{}&{}&{}",
        method.to_uppercase(),
        urlencode(url),
        urlencode(&param_string)
    );

    let signing_key = format!(
        "{}&{}",
        urlencode(creds.api_key_secret),
        urlencode(creds.access_token_secret)
    );

    let signature = hmac_sha1(signing_key.as_bytes(), &signature_base);

    oauth_params.push(("oauth_signature".to_string(), signature));
    oauth_params.sort_by(|a, b| {
        let key_cmp = a.0.cmp(&b.0);
        if key_cmp.is_eq() {
            a.1.cmp(&b.1)
        } else {
            key_cmp
        }
    });

    let header_value = oauth_params
        .iter()
        .map(|(k, v)| format!("{}=\"{}\"", urlencode(k), urlencode(v)))
        .collect::<Vec<_>>()
        .join(", ");

    format!("OAuth {header_value}")
}

fn x_api_get(
    path: &str,
    params: &[(&str, &str)],
    x_credentials: Option<&ServiceCredential>,
) -> Result<String, String> {
    let creds = get_x_credentials(x_credentials).ok_or("X credentials not configured")?;
    let url = format!("{X_API_BASE}{path}");
    let auth_header = build_oauth1_header("GET", &url, &creds, params);

    let agent = ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .http_status_as_error(false)
            .build(),
    );
    let response = agent
        .get(&url)
        .header("Authorization", &auth_header)
        .call()
        .map_err(|e| format!("X API request failed: {e}"))?;

    let status = response.status().as_u16();
    let body = response
        .into_body()
        .read_to_string()
        .map_err(|e| format!("failed to read response: {e}"))?;

    if !(200..300).contains(&status) {
        return Err(format!("X API error (status {status}): {body}"));
    }

    Ok(body)
}

fn x_api_post(
    path: &str,
    body_json: &str,
    x_credentials: Option<&ServiceCredential>,
) -> Result<String, String> {
    let creds = get_x_credentials(x_credentials).ok_or("X credentials not configured")?;
    let url = format!("{X_API_BASE}{path}");
    let auth_header = build_oauth1_header("POST", &url, &creds, &[]);

    let agent = ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .http_status_as_error(false)
            .build(),
    );
    let response = agent
        .post(&url)
        .header("Authorization", &auth_header)
        .header("Content-Type", "application/json")
        .send(body_json)
        .map_err(|e| format!("X API request failed: {e}"))?;

    let status = response.status().as_u16();
    let body = response
        .into_body()
        .read_to_string()
        .map_err(|e| format!("failed to read response: {e}"))?;

    if !(200..300).contains(&status) {
        return Err(format!("X API error (status {status}): {body}"));
    }

    Ok(body)
}

fn format_x_api_response(response: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(response) {
        Ok(v) => serde_json::to_string_pretty(&v).unwrap_or_else(|_| response.to_string()),
        Err(_) => response.to_string(),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn urlencode_preserves_unreserved_chars() {
        assert_eq!(super::urlencode("ABCabc123-_."), "ABCabc123-_.");
        assert_eq!(super::urlencode("~"), "~");
    }

    #[test]
    fn urlencode_encodes_space_as_percent_20() {
        assert_eq!(super::urlencode("hello world"), "hello%20world");
    }

    #[test]
    fn urlencode_encodes_special_chars() {
        assert_eq!(super::urlencode("a&b=c+d/e"), "a%26b%3Dc%2Bd%2Fe");
    }

    #[test]
    fn urlencode_handles_empty_string() {
        assert_eq!(super::urlencode(""), "");
    }

    #[test]
    fn hmac_sha1_is_deterministic() {
        let key = b"sekret";
        let data = "hello";
        let a = super::hmac_sha1(key, data);
        let b = super::hmac_sha1(key, data);
        assert_eq!(a, b);
    }

    #[test]
    fn hmac_sha1_different_keys_produce_different_output() {
        let a = super::hmac_sha1(b"key1", "hello");
        let b = super::hmac_sha1(b"key2", "hello");
        assert_ne!(a, b);
    }

    #[test]
    fn hmac_sha1_returns_base64_encoded_string() {
        let result = super::hmac_sha1(b"key", "data");
        assert!(
            result
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '/' || c == '=')
        );
        assert!(!result.is_empty());
    }
}
