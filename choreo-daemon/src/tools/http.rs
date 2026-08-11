use super::{MAX_TOOL_OUTPUT_BYTES, sanitize_multiline, sanitize_name, truncate_tool_output};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::{collections::HashMap, path::Path, time::Duration};
use tracing::debug;
use ureq::RequestBuilder;

/// Hard cap on the response body bytes read for a text response. This is the
/// *memory* bound: it limits what a hostile server can make the daemon buffer
/// to a bounded prefix, no matter how large the advertised body is. The
/// prefix deliberately exceeds [`MAX_TOOL_OUTPUT_BYTES`] (the final content
/// cap) so a just-under-budget body is read whole and the strict decode path
/// applies; escaping can expand a Cf/control-heavy body past the slack, but
/// the final `truncate_tool_output` cap keeps the tool result at the budget
/// regardless.
const MAX_HTTP_BODY_BYTES: usize = MAX_TOOL_OUTPUT_BYTES + 64 * 1024;

/// HTTP tool errors — a structured error type for http_request failures.
#[derive(Debug, Serialize, Deserialize, thiserror::Error)]
pub enum HttpError {
    #[error("unsupported method: {0}")]
    UnsupportedMethod(String),
    #[error("invalid url: {0}")]
    InvalidUrl(String),
    #[error("unsupported URL scheme: {0}")]
    UnsupportedUrlScheme(String),
    #[error("invalid header {name}: {error}")]
    InvalidHeader { name: String, error: String },
    #[error("request failed: {0}")]
    RequestFailed(String),
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct HttpRequestArgs {
    /// HTTP method (GET, POST, PUT, DELETE, PATCH, HEAD)
    pub method: String,
    /// Request URL
    pub url: String,
    /// Optional HTTP headers as key-value pairs
    #[serde(default)]
    pub headers: HashMap<String, String>,
    /// Optional request body
    pub body: Option<String>,
    /// Optional timeout in seconds (default 30)
    pub timeout_secs: Option<u64>,
}

pub fn execute_http_request_tool(
    args: &HttpRequestArgs,
    _working_dir: Option<&Path>,
) -> Result<String, HttpError> {
    match args.method.as_str() {
        "GET" | "POST" | "PUT" | "DELETE" | "PATCH" | "HEAD" => {}
        other => return Err(HttpError::UnsupportedMethod(other.to_string())),
    }

    let parsed_url =
        url::Url::parse(&args.url).map_err(|e| HttpError::InvalidUrl(e.to_string()))?;
    match parsed_url.scheme() {
        "http" | "https" => {}
        other => return Err(HttpError::UnsupportedUrlScheme(other.to_string())),
    }

    let timeout_secs = args.timeout_secs.unwrap_or(10).clamp(1, 30);
    let agent = ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(timeout_secs)))
            .http_status_as_error(false)
            .build(),
    );

    // Validate header names/values before making the request so structured
    // errors surface instead of ureq panicking on invalid input.  Reject
    // empty names, control characters and colons in names, and newlines in
    // values (preventing header injection).
    for (name, value) in &args.headers {
        if name.is_empty() {
            return Err(HttpError::InvalidHeader {
                name: "(empty)".into(),
                error: "header name must not be empty".into(),
            });
        }
        if name.bytes().any(|b| b <= 0x1f || b == 0x7f || b == b':') {
            return Err(HttpError::InvalidHeader {
                name: name.clone(),
                error: "header name contains invalid characters".into(),
            });
        }
        if value.bytes().any(|b| b == b'\n' || b == b'\r') {
            return Err(HttpError::InvalidHeader {
                name: name.clone(),
                error: "header value contains newline characters".into(),
            });
        }
    }

    let response = match args.method.as_str() {
        "GET" => apply_headers(agent.get(&args.url), &args.headers).call(),
        "POST" => {
            let req = apply_headers(agent.post(&args.url), &args.headers);
            if let Some(body) = &args.body {
                req.send(body.as_str())
            } else {
                req.send_empty()
            }
        }
        "PUT" => {
            let req = apply_headers(agent.put(&args.url), &args.headers);
            if let Some(body) = &args.body {
                req.send(body.as_str())
            } else {
                req.send_empty()
            }
        }
        "DELETE" => apply_headers(agent.delete(&args.url), &args.headers).call(),
        "PATCH" => {
            let req = apply_headers(agent.patch(&args.url), &args.headers);
            if let Some(body) = &args.body {
                req.send(body.as_str())
            } else {
                req.send_empty()
            }
        }
        "HEAD" => apply_headers(agent.head(&args.url), &args.headers).call(),
        other => return Err(HttpError::UnsupportedMethod(other.to_string())),
    }
    .map_err(|e| HttpError::RequestFailed(e.to_string()))?;

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
        read_bounded_text_body(response)
    } else {
        "body omitted: non-text response".to_string()
    };

    Ok(format_http_response(status, &headers, &body))
}

/// Read a text response body with a hard byte cap ([`MAX_HTTP_BODY_BYTES`]),
/// then sanitize it per line (the same policy as `grep` on matched lines:
/// C0/C1 controls and format chars escaped, structural newlines preserved) and
/// cap it at the shared tool-output budget.
///
/// The body is attacker-controlled (the URL is arbitrary): a hostile response
/// could embed ESC or a Unicode bidi override that would inject terminal
/// escapes or spoof text in the tool transcript / TUI. The read cap bounds
/// daemon memory first; the sanitizer then neutralizes the content.
fn read_bounded_text_body(response: ureq::http::Response<ureq::Body>) -> String {
    // Read at most MAX_HTTP_BODY_BYTES so a hostile server cannot balloon
    // daemon memory. The final content is capped at MAX_TOOL_OUTPUT_BYTES
    // anyway; the slack absorbs most of the sanitizer's escaping expansion
    // (a worst-case Cf-heavy body can still outgrow it — the final cap is
    // the backstop, not the slack). `into_reader` yields an owned `Read`;
    // `take` bounds it so a multi-gigabyte body is never buffered.
    let mut bytes = Vec::with_capacity(64 * 1024);
    let mut reader = response
        .into_body()
        .into_reader()
        .take(MAX_HTTP_BODY_BYTES as u64);
    if let Err(e) = reader.read_to_end(&mut bytes) {
        debug!(
            error = %e,
            bytes_read = bytes.len(),
            "http: body read failed before the byte cap"
        );
        return format!("body omitted: failed to read response body: {e}");
    }

    // Distinguish "the cap cut the body short" from "the server sent the
    // whole body": a cut body is decoded lossily (a mid-UTF-8-char cut at
    // the cap must not fail the decode — the incomplete char renders as
    // U+FFFD and the sanitizer handles the rest); a fully-read body keeps the
    // strict decode semantics (genuinely invalid UTF-8 → body omitted).
    let truncated = bytes.len() as u64 >= MAX_HTTP_BODY_BYTES as u64;
    if truncated {
        debug!(
            cap_bytes = MAX_HTTP_BODY_BYTES,
            "http: response body cut at the read cap (hostile or huge body)"
        );
    }
    let text = if truncated {
        String::from_utf8_lossy(&bytes).into_owned()
    } else {
        match String::from_utf8(bytes) {
            Ok(s) => s,
            Err(e) => return format!("body omitted: failed to decode response text: {e}"),
        }
    };

    truncate_tool_output(&sanitize_multiline(&text))
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

/// Apply User-Agent and user-supplied headers to a ureq request builder.
fn apply_headers<B>(
    req: RequestBuilder<B>,
    headers: &HashMap<String, String>,
) -> RequestBuilder<B> {
    let mut req = req.header("User-Agent", "choreographr/0.1");
    for (name, value) in headers {
        req = req.header(name.as_str(), value.as_str());
    }
    req
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
        // Header values come from the remote server (untrusted): a hostile
        // value containing a newline would split the header onto extra lines
        // (breaking the line-oriented output) and ESC/bidi chars could inject
        // terminal sequences. `sanitize_name` escapes all of those; header
        // *names* are already lowercased ASCII (validated before the request
        // and produced by ureq), so only values need sanitizing.
        output.push_str(&sanitize_name(value));
    }

    output.push_str("\n\n");
    output.push_str(body);
    output
}

pub(crate) struct HttpRequest;

impl crate::tools::Tool for HttpRequest {
    type Args = HttpRequestArgs;
    type Return = String;
    type Error = HttpError;

    fn name(&self) -> &'static str {
        "http_request"
    }

    fn group(&self) -> &'static str {
        "core"
    }

    fn description(&self) -> &'static str {
        "Make an HTTP request to an absolute URL and return status, response headers, and response body text. Supports custom headers such as Range for partial content requests."
    }

    fn describe_invocation(&self, args: &Self::Args) -> String {
        let mut parts = vec![format!(
            "Making {} HTTP request to {}.",
            args.method, args.url
        )];
        if !args.headers.is_empty() {
            parts.push(format!(" {} header(s).", args.headers.len()));
        }
        if let Some(ref body) = args.body {
            parts.push(format!(" Body: {} bytes.", body.len()));
        }
        parts.push(format!(" Timeout: {}s.", args.timeout_secs.unwrap_or(30)));
        parts.concat()
    }

    fn execute(
        &self,
        args: Self::Args,
        _x_credentials: Option<&crate::tools::ServiceCredential>,
        working_dir: Option<&std::path::Path>,
        _ctx: Option<&crate::tools::context::ToolContext>,
    ) -> Result<Self::Return, Self::Error> {
        execute_http_request_tool(&args, working_dir)
    }

    fn return_string(ret: &Self::Return) -> String {
        ret.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::Tool;

    // ── header validation tests ─────────────────────────────────────

    #[test]
    fn header_validation_rejects_empty_name() {
        let args = HttpRequestArgs {
            method: "GET".into(),
            url: "http://example.com".into(),
            headers: [("".into(), "value".into())].into(),
            body: None,
            timeout_secs: None,
        };
        let err = execute_http_request_tool(&args, None).unwrap_err();
        assert!(matches!(err, HttpError::InvalidHeader { .. }));
    }

    #[test]
    fn header_validation_rejects_colon_in_name() {
        let args = HttpRequestArgs {
            method: "GET".into(),
            url: "http://example.com".into(),
            headers: [("bad:header".into(), "value".into())].into(),
            body: None,
            timeout_secs: None,
        };
        let err = execute_http_request_tool(&args, None).unwrap_err();
        assert!(matches!(err, HttpError::InvalidHeader { .. }));
    }

    #[test]
    fn header_validation_rejects_newline_in_value() {
        let args = HttpRequestArgs {
            method: "GET".into(),
            url: "http://example.com".into(),
            headers: [("name".into(), "value\ninjected".into())].into(),
            body: None,
            timeout_secs: None,
        };
        let err = execute_http_request_tool(&args, None).unwrap_err();
        assert!(matches!(err, HttpError::InvalidHeader { .. }));
    }

    #[test]
    fn header_validation_rejects_carriage_return_in_value() {
        let args = HttpRequestArgs {
            method: "GET".into(),
            url: "http://example.com".into(),
            headers: [("name".into(), "value\rinjected".into())].into(),
            body: None,
            timeout_secs: None,
        };
        let err = execute_http_request_tool(&args, None).unwrap_err();
        assert!(matches!(err, HttpError::InvalidHeader { .. }));
    }

    #[test]
    fn header_validation_rejects_control_char_in_name() {
        let args = HttpRequestArgs {
            method: "GET".into(),
            url: "http://example.com".into(),
            headers: [("header\x00name".into(), "value".into())].into(),
            body: None,
            timeout_secs: None,
        };
        let err = execute_http_request_tool(&args, None).unwrap_err();
        assert!(matches!(err, HttpError::InvalidHeader { .. }));
    }

    #[test]
    fn header_validation_accepts_valid_headers() {
        let args = HttpRequestArgs {
            method: "GET".into(),
            url: "http://example.com".into(),
            headers: [("Accept".into(), "text/html".into())].into(),
            body: None,
            timeout_secs: None,
        };
        // Should pass header validation — only fail at the network layer
        // (RequestFailed) or succeed. Never an InvalidHeader error.
        let result = execute_http_request_tool(&args, None);
        match result {
            Ok(_) => {} // network succeeded
            Err(e) => assert!(
                matches!(e, HttpError::RequestFailed(_)),
                "expected RequestFailed, got {e}"
            ),
        }
    }

    // ── method validation tests ─────────────────────────────────────

    #[test]
    fn unsupported_method_rejected() {
        let args = HttpRequestArgs {
            method: "OPTIONS".into(),
            url: "http://example.com".into(),
            headers: [].into(),
            body: None,
            timeout_secs: None,
        };
        let err = execute_http_request_tool(&args, None).unwrap_err();
        assert!(matches!(err, HttpError::UnsupportedMethod(_)));
    }

    #[test]
    fn invalid_url_rejected() {
        let args = HttpRequestArgs {
            method: "GET".into(),
            url: "\0invalid".into(),
            headers: [].into(),
            body: None,
            timeout_secs: None,
        };
        let err = execute_http_request_tool(&args, None).unwrap_err();
        assert!(matches!(err, HttpError::InvalidUrl(_)));
    }

    #[test]
    fn unsupported_scheme_rejected() {
        let args = HttpRequestArgs {
            method: "GET".into(),
            url: "ftp://example.com".into(),
            headers: [].into(),
            body: None,
            timeout_secs: None,
        };
        let err = execute_http_request_tool(&args, None).unwrap_err();
        assert!(matches!(err, HttpError::UnsupportedUrlScheme(_)));
    }

    // ── HttpError postcard round trip ───────────────────────────────

    #[test]
    fn http_error_postcard_round_trip() {
        let errors = vec![
            HttpError::UnsupportedMethod("PATCH".into()),
            HttpError::InvalidUrl("bad".into()),
            HttpError::UnsupportedUrlScheme("file".into()),
            HttpError::InvalidHeader {
                name: "X-Foo".into(),
                error: "bad value".into(),
            },
            HttpError::RequestFailed("timeout".into()),
        ];
        for err in &errors {
            let encoded = postcard::to_allocvec(err).unwrap();
            let decoded: HttpError = postcard::from_bytes(&encoded).unwrap();
            assert_eq!(err.to_string(), decoded.to_string());
        }
    }

    // ── format_http_response tests ──────────────────────────────────

    #[test]
    fn format_http_response_includes_status_body() {
        let status = ureq::http::StatusCode::OK;
        let headers = vec![("content-type".into(), "text/plain".into())];
        let body = "hello";
        let output = format_http_response(status, &headers, body);
        assert!(output.contains("200 OK"));
        assert!(output.contains("content-type: text/plain"));
        assert!(output.contains("hello"));
    }

    #[test]
    fn format_http_response_sorts_headers() {
        let status = ureq::http::StatusCode::OK;
        let headers = vec![
            ("z-header".into(), "z".into()),
            ("a-header".into(), "a".into()),
        ];
        let output = format_http_response(status, &headers, "");
        let a_pos = output.find("a-header").unwrap();
        let z_pos = output.find("z-header").unwrap();
        assert!(a_pos < z_pos, "headers should be sorted alphabetically");
    }

    #[test]
    fn describe_invocation_includes_method_and_url() {
        let tool = HttpRequest;
        let args = HttpRequestArgs {
            method: "POST".into(),
            url: "https://api.example.com/data".into(),
            headers: [("Authorization".into(), "Bearer token123".into())].into(),
            body: Some("{\"key\":\"value\"}".into()),
            timeout_secs: Some(60),
        };
        let desc = tool.describe_invocation(&args);
        assert!(desc.contains("Making POST HTTP request to https://api.example.com/data."));
        assert!(desc.contains("1 header(s)."));
        assert!(desc.contains("Body: 15 bytes."));
        assert!(desc.contains("Timeout: 60s."));
    }

    #[test]
    fn describe_invocation_no_body() {
        let tool = HttpRequest;
        let args = HttpRequestArgs {
            method: "GET".into(),
            url: "https://example.com".into(),
            headers: std::collections::HashMap::new(),
            body: None,
            timeout_secs: None,
        };
        let desc = tool.describe_invocation(&args);
        assert!(desc.contains("Making GET HTTP request to https://example.com."));
        assert!(desc.contains("Timeout: 30s."));
    }
}
