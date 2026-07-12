use super::{ToolError, truncate_tool_output};
use serde::Deserialize;
use std::{collections::HashMap, path::Path, time::Duration};

#[derive(Debug, Deserialize)]
pub struct HttpRequestArgs {
    pub method: String,
    pub url: String,
    #[serde(default)]
    pub headers: HashMap<String, String>,
    pub body: Option<String>,
    pub timeout_secs: Option<u64>,
}

pub fn execute_http_request_tool(
    args: &HttpRequestArgs,
    _working_dir: Option<&Path>,
) -> Result<String, ToolError> {
    match args.method.as_str() {
        "GET" | "POST" | "HEAD" => {}
        other => return Err(ToolError::UnsupportedMethod(other.to_string())),
    }

    let parsed_url =
        url::Url::parse(&args.url).map_err(|e| ToolError::InvalidUrl(e.to_string()))?;
    match parsed_url.scheme() {
        "http" | "https" => {}
        other => return Err(ToolError::UnsupportedUrlScheme(other.to_string())),
    }

    let timeout_secs = args.timeout_secs.unwrap_or(10).clamp(1, 30);
    let agent = ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(timeout_secs)))
            .http_status_as_error(false)
            .build(),
    );

    let response = match args.method.as_str() {
        "GET" => {
            let mut req = agent.get(&args.url);
            req = req.header("User-Agent", "tai-daemon/0.1");
            for (name, value) in &args.headers {
                req = req.header(name.as_str(), value.as_str());
            }
            req.call()
        }
        "POST" => {
            let mut req = agent.post(&args.url);
            req = req.header("User-Agent", "tai-daemon/0.1");
            for (name, value) in &args.headers {
                req = req.header(name.as_str(), value.as_str());
            }
            if let Some(body) = &args.body {
                req.send(body.as_str())
            } else {
                req.send_empty()
            }
        }
        "HEAD" => {
            let mut req = agent.head(&args.url);
            req = req.header("User-Agent", "tai-daemon/0.1");
            for (name, value) in &args.headers {
                req = req.header(name.as_str(), value.as_str());
            }
            req.call()
        }
        other => return Err(ToolError::UnsupportedMethod(other.to_string())),
    }
    .map_err(|e| ToolError::RequestFailed(e.to_string()))?;

    let status = response.status();
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    // Collect headers before consuming the body with into_body()
    let headers: Vec<(String, String)> = response
        .headers()
        .iter()
        .map(|(name, value)| {
            (
                name.as_str().to_ascii_lowercase(),
                value.to_str().unwrap_or("<non-utf8>").to_string(),
            )
        })
        .collect();

    let body = if args.method == "HEAD" {
        String::new()
    } else if is_text_content_type(&content_type) {
        match response.into_body().read_to_string() {
            Ok(text) => truncate_tool_output(&text),
            Err(error) => format!("body omitted: failed to decode response text: {error}"),
        }
    } else {
        "body omitted: non-text response".to_string()
    };

    Ok(format_http_response(status, &headers, &body))
}

fn is_text_content_type(content_type: &str) -> bool {
    let mime = content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    mime.starts_with("text/")
        || matches!(
            mime.as_str(),
            "application/json"
                | "application/xml"
                | "application/javascript"
                | "application/x-javascript"
                | "application/x-ndjson"
                | "application/graphql-response+json"
        )
        || mime.ends_with("+json")
        || mime.ends_with("+xml")
}

fn format_http_response(
    status: ureq::http::StatusCode,
    headers: &[(String, String)],
    body: &str,
) -> String {
    let mut output = format!("status: {status}");

    let mut sorted = headers.to_vec();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));

    for (name, value) in &sorted {
        output.push('\n');
        output.push_str(name);
        output.push_str(": ");
        output.push_str(value);
    }

    output.push_str("\n\n");
    output.push_str(body);
    output
}

pub(crate) struct HttpRequest;

define_tool!(
    HttpRequest,
    "http_request",
    "Make an HTTP request to an absolute URL and return status, response headers, and response body text. Supports custom headers such as Range for partial content requests.",
    HttpRequestArgs,
    execute_http_request_tool,
    serde_json::json!({
        "type": "object",
        "properties": {
            "method": {
                "type": "string",
                "enum": ["GET", "POST", "HEAD"]
            },
            "url": {
                "type": "string",
                "description": "Absolute http or https URL"
            },
            "headers": {
                "type": "object",
                "description": "Optional request headers, including Range",
                "additionalProperties": {
                    "type": "string"
                }
            },
            "body": {
                "type": "string",
                "description": "Optional UTF-8 request body"
            },
            "timeout_secs": {
                "type": "integer",
                "minimum": 1,
                "maximum": 30,
                "default": 10
            }
        },
        "required": ["method", "url"],
        "additionalProperties": false
    }),
    "core"
);
