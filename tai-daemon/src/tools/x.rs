use crate::tools::{ToolExecutionOutput, ToolResult};
use async_trait::async_trait;
use reqwest::Client;
use sha1::Sha1;
use std::sync::RwLock;
use tai_keystore::XCredentials;

static X_CREDENTIALS: std::sync::OnceLock<RwLock<Option<XCredentials>>> = std::sync::OnceLock::new();

pub(crate) fn set_x_credentials(creds: XCredentials) {
    let lock = X_CREDENTIALS.get_or_init(|| RwLock::new(None));
    *lock.write().unwrap() = Some(creds);
}

pub(crate) fn clear_x_credentials() {
    if let Some(lock) = X_CREDENTIALS.get() {
        *lock.write().unwrap() = None;
    }
}

fn get_x_credentials() -> Option<XCredentials> {
    X_CREDENTIALS.get()?.read().unwrap().clone()
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

#[allow(deprecated)]
fn hmac_sha1(key: &[u8], data: &str) -> String {
    use hmac::{Hmac, Mac};
    type HmacSha1 = Hmac<Sha1>;
    let mut mac = HmacSha1::new_from_slice(key).expect("HMAC can take key of any size");
    mac.update(data.as_bytes());
    let result = mac.finalize();
    base64::Engine::encode(&base64::engine::general_purpose::STANDARD, result.into_bytes().as_slice())
}

fn build_oauth1_header(
    method: &str,
    url: &str,
    creds: &XCredentials,
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
        ("oauth_consumer_key".to_string(), creds.api_key.clone()),
        ("oauth_nonce".to_string(), nonce),
        (
            "oauth_signature_method".to_string(),
            "HMAC-SHA1".to_string(),
        ),
        ("oauth_timestamp".to_string(), timestamp),
        ("oauth_token".to_string(), creds.access_token.clone()),
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
        urlencode(&creds.api_key_secret),
        urlencode(&creds.access_token_secret)
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

async fn x_api_get(path: &str, params: &[(&str, &str)]) -> Result<String, String> {
    let creds = get_x_credentials().ok_or("X credentials not configured")?;
    let url = format!("{X_API_BASE}{path}");
    let auth_header = build_oauth1_header("GET", &url, &creds, params);

    let client = Client::new();
    let response = client
        .get(&url)
        .header("Authorization", &auth_header)
        .send()
        .await
        .map_err(|e| format!("X API request failed: {e}"))?;

    let status = response.status();
    let body = response.text().await.map_err(|e| format!("failed to read response: {e}"))?;

    if !status.is_success() {
        return Err(format!("X API error (status {status}): {body}"));
    }

    Ok(body)
}

async fn x_api_post(path: &str, body_json: &str) -> Result<String, String> {
    let creds = get_x_credentials().ok_or("X credentials not configured")?;
    let url = format!("{X_API_BASE}{path}");
    let auth_header = build_oauth1_header("POST", &url, &creds, &[]);

    let client = Client::new();
    let response = client
        .post(&url)
        .header("Authorization", &auth_header)
        .header("Content-Type", "application/json")
        .body(body_json.to_string())
        .send()
        .await
        .map_err(|e| format!("X API request failed: {e}"))?;

    let status = response.status();
    let body = response.text().await.map_err(|e| format!("failed to read response: {e}"))?;

    if !status.is_success() {
        return Err(format!("X API error (status {status}): {body}"));
    }

    Ok(body)
}

pub(crate) struct XPost;

#[async_trait]
impl super::Tool for XPost {
    fn name(&self) -> &'static str {
        "x_post"
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

    async fn execute(&self, arguments_json: &str) -> ToolExecutionOutput {
        let args: serde_json::Value = match serde_json::from_str(arguments_json) {
            Ok(v) => v,
            Err(e) => {
                return ToolExecutionOutput {
                    result: ToolResult {
                        content: format!("invalid arguments: {e}"),
                        is_error: true,
                    },
                    image: None,
                };
            }
        };

        let text = args["text"].as_str().unwrap_or_default();
        if text.is_empty() {
            return ToolExecutionOutput {
                result: ToolResult {
                    content: "text must not be empty".to_string(),
                    is_error: true,
                },
                image: None,
            };
        }

        let body = serde_json::json!({ "text": text }).to_string();

        match x_api_post("/2/tweets", &body).await {
            Ok(response) => {
                let formatted = match serde_json::from_str::<serde_json::Value>(&response) {
                    Ok(v) => serde_json::to_string_pretty(&v).unwrap_or(response),
                    Err(_) => response,
                };
                ToolExecutionOutput {
                    result: ToolResult {
                        content: crate::tools::truncate_tool_output(&format!(
                            "tweet posted successfully:\n{formatted}"
                        )),
                        is_error: false,
                    },
                    image: None,
                }
            }
            Err(e) => ToolExecutionOutput {
                result: ToolResult {
                    content: crate::tools::truncate_tool_output(&e),
                    is_error: true,
                },
                image: None,
            },
        }
    }
}

pub(crate) struct XSearchRecent;

#[async_trait]
impl super::Tool for XSearchRecent {
    fn name(&self) -> &'static str {
        "x_search_recent"
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

    async fn execute(&self, arguments_json: &str) -> ToolExecutionOutput {
        let args: serde_json::Value = match serde_json::from_str(arguments_json) {
            Ok(v) => v,
            Err(e) => {
                return ToolExecutionOutput {
                    result: ToolResult {
                        content: format!("invalid arguments: {e}"),
                        is_error: true,
                    },
                    image: None,
                };
            }
        };

        let query = args["query"].as_str().unwrap_or_default();
        if query.is_empty() {
            return ToolExecutionOutput {
                result: ToolResult {
                    content: "query must not be empty".to_string(),
                    is_error: true,
                },
                image: None,
            };
        }

        let max_results = args["max_results"]
            .as_u64()
            .unwrap_or(10)
            .clamp(10, 100);
        let max_results_str = max_results.to_string();

        let params = vec![
            ("query", query),
            ("max_results", &max_results_str),
            ("tweet.fields", "created_at,author_id,public_metrics"),
        ];

        match x_api_get("/2/tweets/search/recent", &params).await {
            Ok(response) => {
                let formatted = match serde_json::from_str::<serde_json::Value>(&response) {
                    Ok(v) => serde_json::to_string_pretty(&v).unwrap_or(response),
                    Err(_) => response,
                };
                ToolExecutionOutput {
                    result: ToolResult {
                        content: crate::tools::truncate_tool_output(&format!(
                            "search results for '{query}':\n{formatted}"
                        )),
                        is_error: false,
                    },
                    image: None,
                }
            }
            Err(e) => ToolExecutionOutput {
                result: ToolResult {
                    content: crate::tools::truncate_tool_output(&e),
                    is_error: true,
                },
                image: None,
            },
        }
    }
}

pub(crate) struct XUserLookup;

#[async_trait]
impl super::Tool for XUserLookup {
    fn name(&self) -> &'static str {
        "x_user_lookup"
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

    async fn execute(&self, arguments_json: &str) -> ToolExecutionOutput {
        let args: serde_json::Value = match serde_json::from_str(arguments_json) {
            Ok(v) => v,
            Err(e) => {
                return ToolExecutionOutput {
                    result: ToolResult {
                        content: format!("invalid arguments: {e}"),
                        is_error: true,
                    },
                    image: None,
                };
            }
        };

        let username = args["username"].as_str().unwrap_or_default();
        if username.is_empty() {
            return ToolExecutionOutput {
                result: ToolResult {
                    content: "username must not be empty".to_string(),
                    is_error: true,
                },
                image: None,
            };
        }

        let params = vec![("user.fields", "description,public_metrics,created_at")];

        match x_api_get(&format!("/2/users/by/username/{username}"), &params).await {
            Ok(response) => {
                let formatted = match serde_json::from_str::<serde_json::Value>(&response) {
                    Ok(v) => serde_json::to_string_pretty(&v).unwrap_or(response),
                    Err(_) => response,
                };
                ToolExecutionOutput {
                    result: ToolResult {
                        content: crate::tools::truncate_tool_output(&format!(
                            "user lookup for @{username}:\n{formatted}"
                        )),
                        is_error: false,
                    },
                    image: None,
                }
            }
            Err(e) => ToolExecutionOutput {
                result: ToolResult {
                    content: crate::tools::truncate_tool_output(&e),
                    is_error: true,
                },
                image: None,
            },
        }
    }
}
