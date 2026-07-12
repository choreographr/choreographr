use crate::tools::{ToolError, context::ToolContext, truncate_tool_output};
use serde::Deserialize;
use std::path::Path;
use tai_keystore::{ServiceCredential, XCredentialView};

#[derive(Debug, Deserialize)]
pub struct XPostArgs {
    pub text: String,
}

#[derive(Debug, Deserialize)]
pub struct XSearchRecentArgs {
    pub query: String,
    pub max_results: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub struct XUserLookupArgs {
    pub username: String,
}

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

fn execute_x_post_tool(
    args: &XPostArgs,
    x_credentials: Option<&ServiceCredential>,
) -> Result<String, ToolError> {
    let text = args.text.trim();
    if text.is_empty() {
        return Err(ToolError::Other("text must not be empty".to_string()));
    }

    let body = serde_json::json!({ "text": text }).to_string();

    x_api_post("/2/tweets", &body, x_credentials)
        .map(|response| {
            truncate_tool_output(&format!(
                "tweet posted successfully:\n{}",
                format_x_api_response(&response)
            ))
        })
        .map_err(|e| ToolError::Other(truncate_tool_output(&e)))
}

fn execute_x_search_recent_tool(
    args: &XSearchRecentArgs,
    x_credentials: Option<&ServiceCredential>,
) -> Result<String, ToolError> {
    let query = args.query.trim();
    if query.is_empty() {
        return Err(ToolError::Other("query must not be empty".to_string()));
    }

    let max_results = args.max_results.unwrap_or(10).clamp(10, 100);
    let max_results_str = max_results.to_string();

    let params = vec![
        ("query", query),
        ("max_results", &max_results_str),
        ("tweet.fields", "created_at,author_id,public_metrics"),
    ];

    x_api_get("/2/tweets/search/recent", &params, x_credentials)
        .map(|response| {
            truncate_tool_output(&format!(
                "search results for '{query}':\n{}",
                format_x_api_response(&response)
            ))
        })
        .map_err(|e| ToolError::Other(truncate_tool_output(&e)))
}

fn execute_x_user_lookup_tool(
    args: &XUserLookupArgs,
    x_credentials: Option<&ServiceCredential>,
) -> Result<String, ToolError> {
    let username = args.username.trim();
    if username.is_empty() {
        return Err(ToolError::Other("username must not be empty".to_string()));
    }

    let params = vec![("user.fields", "description,public_metrics,created_at")];

    x_api_get(
        &format!("/2/users/by/username/{username}"),
        &params,
        x_credentials,
    )
    .map(|response| {
        truncate_tool_output(&format!(
            "user lookup for @{username}:\n{}",
            format_x_api_response(&response)
        ))
    })
    .map_err(|e| ToolError::Other(truncate_tool_output(&e)))
}

pub(crate) struct XPost;

impl super::Tool for XPost {
    type Args = XPostArgs;
    type Return = String;

    fn name(&self) -> &'static str {
        "x_post"
    }

    fn group(&self) -> &'static str {
        "x"
    }

    fn description(&self) -> &'static str {
        "Post a tweet to X (Twitter). Requires X credentials to be configured via the keystore."
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "text": {
                    "type": "string",
                    "description": "The text content of the tweet (max 280 characters)"
                }
            },
            "required": ["text"]
        })
    }

    fn execute(
        &self,
        args: Self::Args,
        x_credentials: Option<&ServiceCredential>,
        _cwd: Option<&Path>,
        _ctx: Option<&ToolContext>,
    ) -> Result<String, ToolError> {
        execute_x_post_tool(&args, x_credentials)
    }
}

pub(crate) struct XSearchRecent;

impl super::Tool for XSearchRecent {
    type Args = XSearchRecentArgs;
    type Return = String;

    fn name(&self) -> &'static str {
        "x_search_recent"
    }

    fn group(&self) -> &'static str {
        "x"
    }

    fn description(&self) -> &'static str {
        "Search recent tweets on X (Twitter). Requires X credentials to be configured via the keystore."
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "The search query (max 512 characters)"
                },
                "max_results": {
                    "type": "number",
                    "description": "Maximum number of results to return (10-100, default 10)"
                }
            },
            "required": ["query"]
        })
    }

    fn execute(
        &self,
        args: Self::Args,
        x_credentials: Option<&ServiceCredential>,
        _cwd: Option<&Path>,
        _ctx: Option<&ToolContext>,
    ) -> Result<String, ToolError> {
        execute_x_search_recent_tool(&args, x_credentials)
    }
}

pub(crate) struct XUserLookup;

impl super::Tool for XUserLookup {
    type Args = XUserLookupArgs;
    type Return = String;

    fn name(&self) -> &'static str {
        "x_user_lookup"
    }

    fn group(&self) -> &'static str {
        "x"
    }

    fn description(&self) -> &'static str {
        "Look up a user on X (Twitter) by username or ID. Requires X credentials to be configured via the keystore."
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "username": {
                    "type": "string",
                    "description": "The X username (without @) to look up"
                }
            },
            "required": ["username"]
        })
    }

    fn execute(
        &self,
        args: Self::Args,
        x_credentials: Option<&ServiceCredential>,
        _cwd: Option<&Path>,
        _ctx: Option<&ToolContext>,
    ) -> Result<String, ToolError> {
        execute_x_user_lookup_tool(&args, x_credentials)
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
