use super::{ToolError, ToolResult, tool_ok, truncate_tool_output};
use reqwest::{
    Method, StatusCode, Url,
    header::{HeaderMap, HeaderName, HeaderValue},
};
use serde::Deserialize;
use std::{collections::HashMap, time::Duration};

#[derive(Debug, Deserialize)]
struct HttpRequestArgs {
    method: String,
    url: String,
    #[serde(default)]
    headers: HashMap<String, String>,
    body: Option<String>,
    timeout_secs: Option<u64>,
}

pub(crate) fn execute_http_request_tool(arguments_json: &str) -> ToolResult {
    match execute_http_request_tool_inner(arguments_json) {
        Ok(content) => tool_ok(content),
        Err(error) => error.into(),
    }
}

fn execute_http_request_tool_inner(arguments_json: &str) -> Result<String, ToolError> {
    let args: HttpRequestArgs = serde_json::from_str(arguments_json)?;

    let method = match args.method.as_str() {
        "GET" => Method::GET,
        "POST" => Method::POST,
        "HEAD" => Method::HEAD,
        other => return Err(ToolError::UnsupportedMethod(other.to_string())),
    };

    let url = Url::parse(&args.url).map_err(|e| ToolError::InvalidUrl(e.to_string()))?;
    match url.scheme() {
        "http" | "https" => {}
        other => return Err(ToolError::UnsupportedUrlScheme(other.to_string())),
    }

    let timeout_secs = args.timeout_secs.unwrap_or(10).clamp(1, 30);
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(timeout_secs))
        .build()
        .map_err(|e| ToolError::Other(format!("failed to build http client: {e}")))?;

    let headers = build_http_request_headers(args.headers)?;

    let mut request = client.request(method.clone(), url).headers(headers);
    if method != Method::GET
        && method != Method::HEAD
        && let Some(body) = args.body
    {
        request = request.body(body);
    }

    let response = request
        .send()
        .map_err(|e| ToolError::RequestFailed(e.to_string()))?;

    let status = response.status();
    let headers = response.headers().clone();
    let content_type = headers
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let body = if method == Method::HEAD {
        String::new()
    } else if is_text_content_type(&content_type) {
        match response.text() {
            Ok(text) => truncate_tool_output(&text),
            Err(error) => format!("body omitted: failed to decode response text: {error}"),
        }
    } else {
        "body omitted: non-text response".to_string()
    };

    Ok(format_http_response(status, &headers, &body))
}

fn build_http_request_headers(headers: HashMap<String, String>) -> Result<HeaderMap, ToolError> {
    let mut request_headers = HeaderMap::new();
    for (name, value) in headers {
        let header_name =
            HeaderName::try_from(name.as_str()).map_err(|error| ToolError::InvalidHeader {
                name: name.clone(),
                error: error.to_string(),
            })?;
        let header_value =
            HeaderValue::from_str(&value).map_err(|error| ToolError::InvalidHeader {
                name: name.clone(),
                error: error.to_string(),
            })?;
        request_headers.insert(header_name, header_value);
    }
    Ok(request_headers)
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

fn format_http_response(status: StatusCode, headers: &HeaderMap, body: &str) -> String {
    let mut output = format!("status: {}", status);

    let mut entries = headers
        .iter()
        .map(|(name, value)| {
            (
                name.as_str().to_ascii_lowercase(),
                value.to_str().unwrap_or("<non-utf8>").to_string(),
            )
        })
        .collect::<Vec<_>>();
    entries.sort_by(|a, b| a.0.cmp(&b.0));

    for (name, value) in entries {
        output.push('\n');
        output.push_str(&name);
        output.push_str(": ");
        output.push_str(&value);
    }

    output.push_str("\n\n");
    output.push_str(body);
    output
}

define_tool!(
    HttpRequest,
    "http_request",
    "Make an HTTP request to an absolute URL and return status, response headers, and response body text. Supports custom headers such as Range for partial content requests.",
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
